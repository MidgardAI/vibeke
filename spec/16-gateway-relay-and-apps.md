# 16 — Gateway, relay and the phone/desktop apps

How a phone (PWA), a desktop app (Electron) and, later, teammates reach a Vibeke host **without Tailscale and without an inbound port**, end-to-end encrypted so that every server we run sees ciphertext and routing metadata only. It also records the staged SaaS shape (accounts, rate limits, push, sync, teams, session share/handoff, preview links) so the first slices stay compatible with it.

This section makes the Phase 2 row "Vibeke Gateway + mobile/web app" of [12](12-phase-2-outlook.md) concrete. It changes no Phase 1 requirement: the gateway is an ordinary API client of the unchanged server ([07](07-api-cli-plugins.md)), and nothing in the server, TUI or CLI is modified by the first stages.

Crates and packages:

| Piece | Where | What |
|---|---|---|
| `vk-e2e` | `crates/vk-e2e` | Wire types, Noise channel (`snow`), pairing-link codec, framing. Shared by relay tests, gateway and conformance vectors. |
| `vk-relay` | `crates/vk-relay`, binary `vibeke-relay` | The dumb, self-hostable relay. Optional static hosting of the web app. |
| `vk-gateway` | `crates/vk-gateway`, binary `vibeke-gateway` | Runs next to the server on the host: dials the relay, terminates Noise, exposes the app API, sends Web Push. |
| `@vibeke/core` | `web/packages/core` | TypeScript: Noise (noble), relay transport, app-API client, store. No DOM, no React. |
| `@vibeke/ui` | `web/packages/ui` | React components and screens shared by every client. |
| `@vibeke/pwa` | `web/apps/pwa` | PWA shell: service worker, Web Push, install, IndexedDB key store. |
| `@vibeke/desktop` | `web/apps/desktop` | Electron shell (later stage): same UI, native notifications, OS keychain, optional local transport. |

When the gateway proves itself, `vibeke-gateway` and `vibeke-relay` fold into the main binary as `vibeke gateway` / `vibeke relay` (one-line wiring in `crates/vibeke`). Until then they are separate binaries so the Phase 1 code stays untouched.

---

## 0. Stages

| Stage | Contents | Status |
|---|---|---|
| **G1 relay** | `vibeke-relay`: host registration by key, client→host splice, limits, health, static app hosting. **No accounts.** | build now |
| **G2 gateway** | `vibeke-gateway`: host keys, QR pairing, Noise channel, device registry/revocation, app API over the server API, git changes, attachments, events, Web Push | build now |
| **G3 PWA** | React PWA with interaction inbox, quick actions, batch approvals, push | build now |
| G4 desktop | Electron shell over the same packages; LAN/local transport | next |
| G5 accounts + SaaS | login (device-code OAuth), host registration under an account, device tickets, plans, metering, billing | later |
| G6 share + handoff | live share tickets, turn-boundary handoff bundles, move-to-self | later |
| G7 ZK services | encrypted sync/history, team groups (MLS), signed audit chain, fragment-keyed preview links | later |
| G8 direct paths | LAN (mDNS), WebRTC for browsers, QUIC hole punching (iroh) for native | later |
| — | Hosted runners, Slack/Teams integrations | **out of scope for this spec's build**; design notes only (§13) |

---

## 1. Principles and invariants

1. **No inbound port on the host.** The gateway only makes outbound connections (relay, Web Push endpoints). A direct/LAN listener is opt-in (G8).
2. **Zero knowledge of content.** The relay and any Vibeke-operated service see ciphertext and routing metadata (host id, connection times, byte counts, client IP). Never keys, terminal content, prompts, code or answers. Where a feature cannot meet this (hosted runners, third-party integrations) it is labelled as an exception and is opt-in.
3. **Keys live on endpoints.** Host keys on the host, device keys on the device. The relay never issues, stores or can substitute an end-to-end key. Pairing pins keys out-of-band (QR).
4. **The gateway is a client.** It uses the public JSON-RPC API over the session socket like any CLI, plus read-only local helpers (git, files it is told about). It never reaches into server internals, so the server keeps sole authority over interactions, delivery and policy.
5. **Least capability per device.** Each paired device carries a scope; revocation is immediate and local to the host.
6. **One UI codebase.** All product UI lives in `@vibeke/ui`; app shells only provide platform services (storage, notifications, transport, install).
7. **Fail closed, degrade visibly.** Unknown protocol versions, failed handshakes and revoked devices close the connection with a reason code. The UI shows "host offline / relay unreachable / revoked", never a silent spinner.

---

## 2. Topology

```
 phone (PWA)                     relay (VPS / hosted)                 host (laptop, devbox)
┌───────────────┐   wss    ┌──────────────────────────┐   wss    ┌─────────────────┐  unix  ┌────────┐
│ @vibeke/ui    │─────────►│ /v1/connect?host=H       │◄─────────│ vibeke-gateway  │───────►│ vibeke │
│ @vibeke/core  │◄═════════╪══ opaque Noise frames ═══╪═════════►│  Noise responder│ JSON-  │ server │
│ Noise initiator│         │ /v1/host (control, auth) │          │  app API, push  │ RPC    └────────┘
└──────┬────────┘          │ static app at /          │          └───────┬─────────┘
       │ Web Push (RFC 8291, encrypted to the subscription)              │
       └◄──────────── Apple / Google / Mozilla push service ◄────────────┘ (host sends directly)
```

- The host keeps one **control WebSocket** to the relay. When a client connects for host `H`, the relay notifies the host, the host opens a **data WebSocket** for that connection, and the relay splices the two. Every message after the splice is a Noise message the relay cannot read.
- Web Push for the PWA goes **host → push service → browser** directly. The payload is encrypted to the browser's subscription keys (RFC 8291) and authenticated with the host's VAPID key. The relay is not involved, so push does not weaken zero knowledge and works for self-hosters. Native apps (later) need APNs/FCM credentials and therefore a push proxy (§10.5).

### 2.1 Where the relay runs

Anything with a public IP and TLS: a €4 VPS (Hetzner, DigitalOcean), Fly.io, or a container behind Caddy. It is stateless apart from in-memory routing, so one small instance serves thousands of hosts; restart only drops live connections, and hosts reconnect with backoff. The **default** relay will be a Vibeke-hosted instance (`relay.vibeke.dev`, G5 adds accounts to it). The relay never runs on the dev host itself: a host that can accept inbound connections doesn't need it (§11).

---

## 3. Keys and identities

| Key | Algorithm | Owner / storage | Purpose |
|---|---|---|---|
| Host static key | X25519 | gateway state dir, file 0600 (OS keychain later) | Noise responder static key; pinned by devices |
| Host relay key | Ed25519 | same | Proves host-id ownership to the relay |
| Host id | `base32(blake3(relay_pub))[..26]`, lowercase | derived | Routing address on the relay; not secret |
| VAPID key | P-256 (ECDSA) | same | Authenticates Web Push requests from this host |
| Device static key | X25519 | PWA: IndexedDB; Electron: OS keychain | Noise initiator static key; authorized on the host |
| Pairing secret | 32 random bytes, single use, 5 min TTL | host state dir (hashed) + QR | `psk` for the pairing handshake |

Gateway state dir: `$VIBEKE_GATEWAY_DIR`, else `<config dir>/vibeke/gateway/` (`~/Library/Application Support/vibeke/gateway` on macOS, `$XDG_CONFIG_HOME/vibeke/gateway` on Linux). Created 0700; files 0600; ownership checked on start (same rule as 09 §3.1).

Files: `host.json` (keys, base64url), `devices.json` (authorized devices), `pairings/*.json` (pending), `push.json` (subscriptions), `gateway.toml` (config).

Key rotation: `vibeke-gateway rotate` creates a new host static key and keeps the old one for 7 days so paired devices re-pin it over an authenticated session (`host.rotate` notification carries the new key, signed with the old channel). Rotating the relay key changes the host id, so devices learn the new id the same way.

---

## 4. Pairing

### 4.1 The link

`vibeke-gateway pair [--name "the maintainer's phone"] [--scope full|approve|view]` creates a pending pairing and prints a QR code plus the same URL as text:

```
https://<relay>/#/pair?d=<base64url(json)>
json = {"v":1,"relay":"wss://relay.example.com","host":"<host id>","hk":"<host static pub b64u>",
        "pid":"<pairing id>","psk":"<pairing secret b64u>","exp":<unix s>,"name":"devbox"}
```

The payload lives in the **URL fragment**, which browsers never send to the server, so the relay serving the app never sees the secret. The app's origin is the relay (or a static origin the user configured; §9.4).

### 4.2 The handshake

1. The device generates its static X25519 key (if it has none) and opens `wss://<relay>/v1/connect?host=<host id>`.
2. It sends a **hello** (plain JSON text frame), which is also the Noise **prologue**:
   `{"v":1,"proto":"vibeke-e2e/1","mode":"pair","pid":"<pairing id>"}`
3. Noise `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s`, initiator = device, responder = gateway, responder static = `hk` from the QR, `psk` = pairing secret. The gateway looks up `pid`, rejects expired/used/unknown ids, and uses that psk.
4. First transport message from the device: `pair.complete {name, platform, user_agent}`. The gateway stores `{device_id, name, pub, scope, paired_at}` in `devices.json`, deletes the pairing (single use) and replies `{device_id, host_name, scope}`.
5. The device stores `{host id, relay, hk, device_id, name}` in its host list. One device can pair with many hosts.

Pairing fails closed: a wrong psk or host key makes the Noise handshake fail; the gateway records the failure (rate-limited per pairing id; 5 failures burn the pairing).

### 4.3 Normal connections

Hello `{"v":1,"proto":"vibeke-e2e/1","mode":"device"}`, then `Noise_IK_25519_ChaChaPoly_BLAKE2s`. After message 1 the gateway knows the device static key and checks it against `devices.json`; unknown or revoked keys are closed with reason `unauthorized` before any app data flows.

### 4.4 Typed-code fallback (later)

When a camera is unavailable, pairing uses a short code (`4-word` or 8 digits) and a balanced PAKE (CPace) to derive the psk, magic-wormhole style. OPAQUE and SPAKE2+ are not used: they are augmented PAKEs for password logins against a server-stored verifier, and Vibeke has neither passwords nor a trusted server.

### 4.5 Revocation

`vibeke-gateway devices` lists devices; `vibeke-gateway revoke <device>` removes one and closes its live connections. The app's Settings → Devices does the same for devices other than itself (needs `full` scope). Revocation also deletes the device's push subscriptions.

---

## 5. The secure channel (`vibeke-e2e/1`)

- **Transport:** one WebSocket per client connection. Message 0 is the hello (text frame). Every subsequent frame is binary and is exactly one Noise message (handshake or transport).
- **Hello / prologue:** the exact UTF-8 bytes of the hello frame are the Noise prologue, so tampering by the relay breaks the handshake. Unknown `v`/`proto` → the gateway answers with a plain text frame `{"error":"unsupported_version","supported":[1]}` and closes.
- **Handshake payloads:** message 1 payload is empty; message 2 payload is `{"v":1,"host_name","server_version","gateway_version"}` (JSON).
- **Application framing:** Noise caps messages at 65535 bytes. App messages are UTF-8 JSON, split into chunks of ≤ 65000 bytes; each Noise plaintext is `flag:u8 || chunk`, `flag = 0` more follows, `1` final. Max reassembled message: 16 MiB (attachments larger than that are chunked at the API level).
- **Ordering and replay:** Noise transport nonces are implicit counters, so a reordered, dropped or replayed frame fails decryption and the connection closes. WebSocket over TCP gives order; reconnect creates a new session.
- **Keepalive:** app-level `ping`/`pong` every 25 s from the client; the relay also pings at the WS layer. Mobile OSes suspend sockets; the client reconnects on visibility change and resumes event streams by cursor (§7.3).
- **Rekey:** after 2^20 messages or 1 hour per direction, the sender sends `{"t":"rekey"}` and both sides call Noise `rekey` for that direction (later; sessions are short-lived on phones today).

---

## 6. The relay (`vibeke-relay`)

### 6.1 Endpoints

| Endpoint | Who | Behaviour |
|---|---|---|
| `GET /v1/host` (WS) | gateway | Control socket. Relay → `{"t":"challenge","nonce"}`; host → `{"t":"auth","host","pub","sig"}` with `sig = Ed25519(relay_pub_key_of_host, "vibeke-relay/1 host-auth" ‖ nonce ‖ relay_origin)`. Relay checks `host == id(pub)` and the signature, then `{"t":"ok"}`. Later: `{"t":"incoming","conn","ip_hash"}` per client. A second auth for the same host id replaces the first (newest wins) after it too proves ownership. |
| `GET /v1/accept?conn=<id>` (WS) | gateway | Data socket for one client connection. First text frame: `{"t":"accept","host","conn","sig"}` with `sig` over `"vibeke-relay/1 accept" ‖ conn`. Relay splices it with the waiting client. |
| `GET /v1/connect?host=<id>` (WS) | device | Waits ≤ 10 s for the host to accept; otherwise closes with `4404 host_offline` (no host control socket) or `4408 accept_timeout`. |
| `GET /v1/status?host=<id>` | device | `{"online":bool}`. Lets the app show host presence without a handshake. Rate-limited. |
| `GET /healthz` | ops | `ok` |
| `GET /` and assets | browser | The web app, when `--app-dir` is given (or embedded at build time later). `Cache-Control` and a strict CSP. |

Close codes: `4400 bad_request`, `4401 unauthorized`, `4404 host_offline`, `4408 accept_timeout`, `4413 too_large`, `4429 rate_limited`, `4503 draining`.

### 6.2 Limits (no accounts yet)

All configurable via flags/env; defaults:

- Max WS message 256 KiB (Noise frames are ≤ 64 KiB; margin for future).
- Per-IP: 30 new connections/min, 64 concurrent sockets.
- Per host id: 32 concurrent client connections, 8 pending accepts.
- Per spliced connection: token bucket 1 MiB/s sustained, 4 MiB burst, each direction; idle timeout 120 s without any frame.
- Global caps on hosts and connections, `--max-hosts`, `--max-conns`.
- Unauthenticated control sockets must authenticate within 10 s.

The relay only forwards frames between a client and the host that **authenticated** for the requested host id, so it cannot be used as an open tunnel; byte limits keep abuse cheap.

### 6.3 What the relay stores and logs

Nothing on disk. Logs (structured, `tracing`): host id prefix (8 chars), event, byte totals at close, close code. Client IPs are logged only as a keyed hash rotated daily (`--log-ip raw` for self-hosters who want it). No frame contents ever.

### 6.4 Extension points for accounts

`trait Authorizer { fn host_connect(&self, host_id, token: Option<&str>) -> Decision; fn client_connect(&self, host_id, ticket: Option<&str>, ip) -> Decision; fn usage(&self, host_id, bytes_in, bytes_out); }`. G1 ships `OpenAuthorizer` (allow, limits only) and `StaticTokenAuthorizer` (`--host-token` list) for private self-hosting. G5 adds the account-backed implementation (§10).

### 6.5 Deployment

`vibeke-relay --listen 0.0.0.0:8443 --app-dir web/apps/pwa/dist` behind Caddy (`reverse_proxy` handles TLS and WS). Graceful shutdown: `SIGTERM` → stop accepting, send `4503 draining` to controls so hosts reconnect elsewhere, wait ≤ 30 s.

---

## 7. The gateway (`vibeke-gateway`)

### 7.1 Process

```
vibeke-gateway run [--relay wss://…] [--session default] [--socket PATH]   # foreground daemon
vibeke-gateway pair [--name N] [--scope full|approve|view] [--qr|--no-qr]
vibeke-gateway devices | revoke <device-id|name> | status | rotate
vibeke-gateway push test [--device D]
```

`run` loads/creates keys, connects to the server socket (`$VIBEKE_SOCKET`, else the server's runtime-dir rule from 09 §3.1, else `--socket`), sends `client.hello {client:"vibeke-gateway", kind:"cli"}`, opens the relay control socket with exponential backoff (1 s → 60 s, jitter) and serves connections. It must be started from outside any pane (otherwise the server gives it pane scope and it cannot answer interactions; it checks `capabilities` in the hello result and refuses to run with a clear error).

Pending pairings are files in `pairings/`, so `pair` works while `run` is running (it re-reads the directory on each pairing hello).

### 7.2 Device scopes

| Scope | Allows |
|---|---|
| `view` | read: dashboard, panes, screens, history, changes, interactions, events, notifications |
| `approve` | `view` + answer interactions, quick replies to agents, interrupt |
| `full` | `approve` + raw keys/text to any pane, create tabs/agents/tasks, attachments, device management |

Enforced in the gateway before any server call. Every mutating call is logged locally with the device id (`audit.log` in the state dir; JSON lines), and the server sees `answered_by = "gateway:<device name>"` via the answer's `idempotency_key` prefix until the server grows a native actor field.

### 7.3 App API

JSON-RPC 2.0 over the channel (§5). Requests `{id, method, params}`, responses `{id, result|error}`, notifications `{method, params}`. Errors reuse server codes (`unsupported`, `not_found`, `conflict`, `invalid_params`) plus `forbidden` (scope) and `unavailable` (server down).

| Method | Params → result | Backed by |
|---|---|---|
| `hello` | `{client, version}` → `{host_name, device_id, scope, server_version, features}` | gateway |
| `dashboard.get` | `{}` → `{at, machine, workspaces[], panes[], runs[], interactions[], notifications_unread}` | `session.snapshot` + `notification.list` |
| `pane.read` | `{pane, source?, lines?}` → `{text, revision}` | `pane.read` |
| `pane.send_text` | `{pane, text, submit?}` → `{}` | `pane.send_text` (+ `Enter` key if `submit`) |
| `pane.send_keys` | `{pane, keys[]}` → `{}` | `pane.send_keys` |
| `agent.prompt` | `{target, text}` → `{}` | `agent.prompt` |
| `agent.interrupt` | `{target}` → `{}` | `agent.interrupt` |
| `agent.transcript` | `{target, limit?}` → `{turns}` | `agent.transcript`; fallback: gateway reads the session JSONL itself |
| `interaction.list` | `{status?}` → `{interactions}` | `interaction.list` |
| `interaction.answer` | `{interaction, decision?, choices?, text?}` → `{interaction, delivery}` | `interaction.answer` with `idempotency_key = gw:<device>:<uuid>` |
| `interaction.answer_batch` | `{interactions[], decision}` → `{results[]}` | gateway loops `interaction.answer`; only same-fingerprint approvals are offered by the UI |
| `changes.get` | `{pane}` → `{repo, branch, ahead, behind, files[{path,status,adds,dels}]}` | gateway runs `git status --porcelain=v2 -b` + `git diff --numstat` in the pane cwd, read-only |
| `changes.diff` | `{pane, path}` → `{diff, truncated}` | `git diff -- <path>` (and untracked file contents, capped 256 KiB) |
| `tab.create` | `{workspace, cwd?, command?}` → `{pane}` | `tab.create` |
| `agent.start` | `{pane?, workspace?, harness, prompt?}` → `{run}` | `agent.start` |
| `attachment.put` | `{name, mime, data_b64}` → `{path}` | `image.upload` (server inbox); the UI then inserts the path |
| `notification.list` / `notification.read` | passthrough | server |
| `events.subscribe` | `{after?}` → `{at}` then `event` notifications | gateway's single server subscription, fanned out; overflow → `events.reset` (client refetches dashboard) |
| `push.vapid_key` | `{}` → `{key}` | gateway |
| `push.subscribe` | `{subscription, prefs}` → `{}` | gateway `push.json` |
| `push.unsubscribe` / `push.test` | | gateway |
| `devices.list` / `devices.revoke` | | gateway |
| `ping` | `{}` → `{}` | gateway |

The gateway keeps **one** server event subscription and fans events out to all connected devices (filtered by scope), so phones never put load on the server proportional to their count.

### 7.4 Push triggers

The gateway turns server events into Web Push when no **foreground** client for that device is connected (the app reports visibility with `client.visibility {visible}`):

| Event | Push |
|---|---|
| `interaction.opened` (approval/question/plan) | "Codex · samplehub wants to run `pnpm test`" with `tag = interaction id` (replaces itself), urgency high |
| run → `needs_input` without interaction | "Claude · dashboard is waiting" |
| run → `done` / `error` | "Claude · backend finished" (normal urgency; per-device toggle) |
| `notification` with urgency ≥ normal | title/body |
| interaction resolved elsewhere | silent push that closes the notification by `tag` (where supported) |

**Payload privacy levels** (per device, default `summary`): `full` (title + redacted command/summary through `vk-redact`), `summary` (harness, workspace, kind; no command), `minimal` ("Vibeke: 1 agent needs you"). Payloads are encrypted (RFC 8291) regardless; the level limits what shows on a lock screen.

Notification actions (Android/desktop Chrome, not iOS): `Approve` / `Deny` for low/medium-risk approvals. The service worker opens the app at `#/i/<id>?do=allow`; the app connects, re-fetches the interaction (so a stale push never answers a superseded prompt) and asks for one confirming tap for high-risk items. Answering directly from the service worker without opening the app is a later improvement.

### 7.5 Coalescing

Pushes are coalesced per device: at most 1 per interaction, and identical-fingerprint approvals within 10 s are merged ("4 agents want `pnpm test`"). Quiet hours per device (`prefs.quiet`) downgrade to silent.

---

## 8. Client features (G3 PWA, reused by G4 desktop)

Parity target: everything an existing phone companion offers today, on Vibeke's structured data, plus the decision-first workflow.

### 8.1 Baseline features

- **Dashboard, needs-input first.** All agents across paired hosts, grouped by host → workspace; sorted by: open interaction (by risk, then wait time) → needs input → working → idle/done. Badges with counts; app badge (`navigator.setAppBadge`) = open interactions.
- **Agent/pane view.** Live screen text (monospace, ANSI colours stripped or rendered), auto-scroll with "jump to bottom", refresh on events plus 2 s polling while visible (we poll only the open pane).
- **Prompts → buttons.** Structured interactions render as buttons natively (no screen scraping). For screen-only harnesses the server's keystroke delivery with selection verification applies (07 §2.x); the app just answers the interaction.
- **Keypad.** Esc, Tab, Shift-Tab, ↑ ↓ ← →, Enter, Ctrl-C, Ctrl-D, y / n, 1-9, Backspace, Space; long-press for repeat. Sends `pane.send_keys`.
- **Reply composer.** Text box that sends with Enter (`submit`), multi-line toggle, recent-reply history, **quick replies** (configurable chips: "continue", "yes", "run the tests", "commit it", "explain").
- **Voice input.** Dictation through the Web Speech API where available, else the OS keyboard's dictation; always lands in the composer for review before sending.
- **Attachments.** Photo/file picker and paste → `attachment.put` → path inserted into the composer.
- **History.** Agent transcript (user / assistant / tool turns, collapsible tool output) from `agent.transcript`.
- **Changes.** Read-only git view per pane cwd: branch, ahead/behind, files with +/−, tap for diff.
- **New tab / new agent.** Pick workspace, harness and an optional first prompt.
- **Multi-host ("crews").** Pair several hosts; one dashboard aggregates them; per-host online state via `/v1/status`.
- **Device pairing.** Scan QR or open the link; Settings shows hosts, this device, other devices (revoke), push state.
- **Web Push** with deep links, **installable PWA**, offline shell (cached app; data views show "offline").

### 8.2 Decisions first

- **Inbox tab** (default when anything is open): every open interaction across hosts as a card with harness, workspace, wait time, risk badge, command/paths/diff preview, and buttons `Allow` / `Allow always` / `Deny` / per-option choices / free text.
- **Quick actions.** Swipe right = allow, left = deny (low/medium risk only; high risk requires the card's button and a confirm). Haptic feedback (`navigator.vibrate` where supported).
- **Batch approvals.** Approvals with the same action fingerprint (tool + normalized command) group into one card: "4 agents want `pnpm test`" → `Allow all` / review individually.
- **Plan review.** Plan markdown rendered; Approve / Request changes (text).
- **Delivery state.** After answering, the card shows `delivering → delivered`, or `failed / unknown` with "open pane" to fix it by hand. Never silently drops.
- **Answered elsewhere.** If the TUI answers first, the card animates away with "answered in terminal".
- **Push → card.** Tapping a notification opens that card directly.

### 8.3 UX rules

- Mobile-first, one-hand reachable primary actions (bottom), safe-area insets, dark/light from the OS.
- Never render raw terminal escape sequences; never auto-send anything without an explicit tap.
- Every mutating tap shows progress and the result; errors are actionable ("host offline — retry", "revoked — pair again").

---

## 9. Web codebase: shared by PWA and Electron

### 9.1 Layout

```
web/
  package.json            # bun workspaces
  packages/core/          # @vibeke/core — no DOM/React
    src/noise.ts          # Noise IK / IKpsk2 (25519, ChaChaPoly, BLAKE2s) on @noble/*
    src/channel.ts        # hello/prologue, framing (§5), reconnect
    src/transport.ts      # interface Transport + RelayTransport (WebSocket)
    src/rpc.ts            # JSON-RPC client over a channel; event subscription
    src/pairing.ts        # link codec, pairing flow
    src/hosts.ts          # multi-host manager (connect, status, backoff)
    src/keystore.ts       # interface KeyStore (platform-provided)
    src/model.ts          # app-API types mirroring §7.3
    src/inbox.ts          # ranking, fingerprint grouping
  packages/ui/            # @vibeke/ui — React components + screens; depends on core
    src/platform.ts       # interface Platform { keystore, notifications, push?, haptics, openExternal }
    src/screens/*         # Inbox, Dashboard, Pane, History, Changes, Settings, Pair
  apps/pwa/               # Vite + vite-plugin-pwa; IndexedDB keystore; Web Push; service worker
  apps/desktop/           # Electron; preload exposes Platform via contextBridge
```

### 9.2 Rules

- Product UI and logic go in `core`/`ui`. An app shell may only implement `Platform` and bootstrap. Code review rejects feature logic in shells.
- `core` has zero runtime dependencies besides `@noble/curves`, `@noble/ciphers`, `@noble/hashes`. It runs in browsers, Electron, Bun (tests) and service workers.
- State: a small store (`zustand`) in `ui`; no server-state library needed (data arrives via RPC + events).
- Styling: plain CSS modules with CSS variables (no runtime CSS-in-JS), so Electron and PWA share theming.
- Routing: hash-based (`#/inbox`, `#/p/<host>/<pane>`), so the PWA works from any static origin and Electron loads `file://`.

### 9.3 Electron specifics (G4)

- Same `RelayTransport`. Optional `LocalTransport`: the main process connects to a gateway on the same machine through a local Unix socket (no relay, same Noise channel so code paths match).
- Keys in the OS keychain (`safeStorage`), notifications via the main process (no Web Push in Electron), tray badge for open interactions.
- `contextIsolation: true`, `sandbox: true`, no `nodeIntegration`; the preload exposes only `Platform`.

### 9.4 Where the PWA is served from (code-trust)

A browser app is only as trustworthy as whoever serves its JavaScript. Stage G3: the relay serves the app (self-hosters trust their own relay). Hardening path: (1) a fixed static origin separate from the relay's API (`app.vibeke.dev`), (2) reproducible builds with published hashes, (3) SRI for every asset, (4) optional verifier extension. Native/Electron apps avoid the problem by shipping signed code. This caveat is stated in the app's About screen.

### 9.5 iOS constraints

Web Push requires the PWA installed to the home screen (iOS 16.4+); the app detects `standalone` and shows an install guide first. No notification actions on iOS; deep links only. WebSockets die in the background; the app reconnects on `visibilitychange`.

---

## 10. Accounts and the SaaS (G5, design)

### 10.1 Separation

Account auth decides **who may use a relay and how much**. End-to-end device keys decide **which device may talk to which host**. The relay never sees the second, so billing and limits never require plaintext.

### 10.2 Flow

1. `vibeke login` → OAuth device-code flow (GitHub/Google/email link); refresh token in the OS keychain.
2. The gateway registers its relay public key under the account → host id; gets short-lived (1 h) host access tokens for `/v1/host`.
3. Devices need no account. At pairing the host issues a **device ticket**: `{host, device_pub, scope, exp}` signed by the host relay key. The relay verifies the ticket against the registered host key on `/v1/connect` and attributes usage to the host's account. Revoking = host stops renewing tickets (exp 24 h) + immediate revocation list push.
4. Self-hosted relays keep `OpenAuthorizer` / static tokens.

### 10.3 Metering and limits

Per account: hosts, paired devices, concurrent connections, bytes/s (token bucket) and GB/month, push sends/day (native proxy). Counters batched from relays to the control plane (Postgres + Stripe); never on the frame path.

### 10.4 Tiers (sketch)

Free: self-hosted, or hosted relay with tight limits. Pro: higher limits, native push, preview links, encrypted sync. Team: shared inbox (MLS), SSO, audit. Usage add-ons: preview bandwidth.

### 10.5 Native push proxy

Native iOS/Android apps can only receive pushes sent with the vendor's APNs/FCM credentials, so the hosted service runs a **push proxy**: the host posts `{device push token handle, ciphertext}`; the proxy wraps it in an APNs/FCM message; the app's Notification Service Extension decrypts it with the device key. The proxy sees "wake device X" plus a padded ciphertext length.

---

## 11. Direct paths (G8, design)

- **LAN:** the gateway optionally advertises `_vibeke._tcp` via mDNS and accepts the same `vibeke-e2e/1` WebSocket on a LAN port; the app tries LAN first when on the same network (Electron; browsers cannot do mDNS, so the PWA uses a remembered LAN URL).
- **Hole punching:** both sides learn their public address from a STUN-like endpoint on the relay, exchange candidates over the relay, and send packets simultaneously so each NAT treats the other's packet as a reply. Works for most NATs; symmetric NATs (some carriers, corporate) fall back to the relay. Browsers can only do this through WebRTC data channels (relay as signalling); native apps can use QUIC via iroh. The same Noise channel runs over every path, so the app API never changes.

---

## 12. Share and handoff (G6, design)

1. **Live share:** `vibeke-gateway share <task|pane> --to <device or invite> --scope view|approve --ttl 2h` issues a scoped, time-limited device ticket (one-time invite link for people not yet paired). Session stays on the host; the geometry lease (01 §3) handles multiple viewers.
2. **Handoff:** at a turn boundary (agent idle or interrupted) the host builds a bundle: git bundle of the task branch + uncommitted changes, agent transcript + resume handle (04), task metadata, event slice, evidence. Transcripts pass through `vk-redact`; the sender reviews "N secrets redacted" before sending. The bundle is encrypted to the recipient's key (HPKE), uploaded to the relay's blob mailbox (or streamed if both are online), and the recipient resumes with their own harness subscription. Credentials never travel; absolute paths are rewritten.
3. **Move to self:** the same with the user's own other host; no confirmation of recipient identity is needed because devices share the user key (§13.1).

---

## 13. Zero-knowledge services (G7, design)

### 13.1 Key hierarchy

User key (per person, generated on first device) → wrapped to each device key at pairing. Team groups use **MLS** (RFC 9420, `openmls`), so removing a member rotates keys. Public-key directory on the service with fingerprint verification ("safety numbers") or admin-signed member keys; key transparency later. Recovery: printed recovery key, passkey PRF-wrapped user key, admin recovery for teams; losing all of these loses data, by design.

### 13.2 Services

| Service | How it stays zero-knowledge |
|---|---|
| Push | Payload encrypted to the subscription/device; vendor and proxy see wake-ups only |
| Sync & history | Client-side encrypted, content-addressed blobs (keyed hash so identical content doesn't link across users); search and dashboards computed on clients |
| Team inbox | MLS group per team; hosts post encrypted interaction events; clients rank locally; the host is the authority on first valid signed answer |
| Audit log | Hash chain signed by device keys; the service stores and can prove non-truncation but cannot read |
| Preview links | `https://p.vibeke.dev/<id>#k=<key>`; a static page installs a service worker that tunnels every request end-to-end to the host over the relay; the fragment never reaches the server. TLS passthrough by SNI is the alternative (certificate transparency makes mis-issuance detectable) |
| Web dashboard | Static origin, SRI, reproducible builds (§9.4) |

### 13.3 Exceptions (labelled, opt-in)

- **Third-party integrations (Slack/Teams/GitHub):** run from the host with the user's own tokens; inbound buttons carry a host-MAC'd token so our endpoint cannot forge an approval. The third party sees what is sent to it. *Not built in this spec's stages.*
- **Hosted runners:** computing on code means seeing it. Options: BYO cloud account (we orchestrate only), confidential VMs with attestation, or an explicit "runner sees your code, like CI" label. *Not built in this spec's stages.*

---

## 14. Threats (additions to 09)

| # | Threat | Mitigation |
|---|---|---|
| R1 | Malicious or compromised relay reads traffic | Noise E2E with pinned host key; relay sees ciphertext only |
| R2 | Relay tampers with hello/version | hello is the Noise prologue |
| R3 | Relay serves malicious PWA JS | §9.4 hardening path; native/Electron signed code; documented caveat |
| R4 | Host-id squatting / impersonation on relay | Ed25519 challenge; host id = hash of key; E2E still pins the static key, so even a squatter cannot complete a handshake |
| R5 | QR photographed by someone else | single use, 5 min TTL, burned after 5 failures; host shows newly paired devices in the TUI later and in `devices` now |
| R6 | Lost phone | `revoke`; scope `approve` by default for phones; high-risk approvals need an explicit confirm; app lock with WebAuthn/biometrics (later) |
| R7 | Stale push answers a superseded prompt | the app always re-fetches the interaction; server `decision_rev` and delivery states decide |
| R8 | Push content on lock screen | per-device privacy level; `vk-redact` on payloads |
| R9 | Relay abuse as free tunnel/DoS | limits §6.2; only host-authenticated splices; accounts later |
| R10 | Gateway compromise = host API access | gateway is a same-UID process like the CLI (T9 unchanged); device scopes limit what remote devices can do |

---

## 15. Testing and acceptance

- **Crypto conformance:** a fixed-key test vector set generated by `vk-e2e` (snow) is checked into `crates/vk-e2e/tests/vectors/` and replayed by `@vibeke/core` tests (Bun), both IK and IKpsk2, including framing/chunking. A live Rust↔TS handshake test runs the gateway's channel against the TS client.
- **Relay:** integration tests with real WebSockets: auth success/failure, wrong signature, offline host, accept timeout, splice both directions, per-conn byte limit, oversized frame, host replacement, draining.
- **Gateway:** pairing (valid, expired, reused, wrong psk), unauthorized device, revoked device closes live connection, scope enforcement per method, event fan-out, push trigger mapping and coalescing (with a mock push endpoint), `changes.get` on a temp git repo. Server calls tested against a running `vibeke` server in a temp runtime dir.
- **PWA:** component tests for inbox ranking/grouping and answer flow; Playwright smoke (pair via link → see dashboard → answer a fixture interaction → delivery shown) against relay + gateway + server.
- **Acceptance (G1–G3):** from a phone on mobile data, with the laptop behind NAT and no Tailscale: pair by QR in < 30 s; push for a new Claude approval arrives in < 5 s; approve from the inbox and see `delivered`; read screen, send keys, view history and changes; revoke the phone from the terminal and see the app drop to "revoked" immediately; relay logs contain no content.
