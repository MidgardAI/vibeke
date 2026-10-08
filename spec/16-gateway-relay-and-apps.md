# 16 — Gateway, relay and the phone/desktop apps

How a phone (PWA), a desktop app (Electron) and, later, teammates reach a Vibeke host **without Tailscale and without an inbound port**, end-to-end encrypted so that the relay sees ciphertext and routing metadata only. Appendix A records the staged SaaS shape (accounts, rate limits, native push, sync, teams, share/handoff, preview links, direct paths) as non-binding design notes so the first slices stay compatible with it.

This section makes the Phase 2 row "Vibeke Gateway + mobile/web app" of [12](12-phase-2-outlook.md) concrete. It changes no Phase 1 requirement. The gateway is an API client of the server ([07](07-api-cli-plugins.md)); the only server changes are **additive** methods listed in §7.7 (new read-only git methods and an answer actor label). The TUI and CLI are not modified.

Reviewed by Codex on 2026-10-06 (design review); resolutions are folded in and summarized in §14.

Crates and packages:

| Piece | Where | What |
|---|---|---|
| `vk-e2e` | `crates/vk-e2e` | Wire types, Noise channel (`snow`), pairing-link codec, framing, relay auth messages, conformance vectors. |
| `vk-relay` | `crates/vk-relay`, binary `vibeke-relay` | The dumb, self-hostable relay. Optional static hosting of the web app for self-hosters. |
| `vk-gateway` | `crates/vk-gateway`, binary `vibeke-gateway` | Runs next to the server on the host: dials the relay, terminates Noise, exposes the app API, sends Web Push. |
| `@vibeke/core` | `web/packages/core` | TypeScript: Noise (noble), channel, app-API client, multi-host manager, inbox logic. No DOM, no React. |
| `@vibeke/ui` | `web/packages/ui` | React components and screens shared by every client. |
| `@vibeke/pwa` | `web/apps/pwa` | PWA shell: service worker, Web Push, install, IndexedDB key store. |
| `@vibeke/desktop` | `web/apps/desktop` | Electron shell (G4): same UI, native notifications, encrypted key storage, local transport. |

When the gateway proves itself, `vibeke-gateway` and `vibeke-relay` fold into the main binary as `vibeke gateway` / `vibeke relay`. Until then they are separate binaries.

---

## 0. Stages

| Stage | Contents | Status |
|---|---|---|
| **G1 relay** | `vibeke-relay`: host registration by key, client→host splice, limits, health, optional static app hosting. **No accounts.** | build now |
| **G2 gateway** | `vibeke-gateway`: host keys, QR pairing with host confirmation, Noise channel, device registry/revocation, app API, events, Web Push; server additions §7.7 | build now |
| **G3 PWA** | React PWA with interaction inbox, quick actions, batch approvals, push | build now |
| **G4 desktop** | §16: Electron app over the shared packages; local transport, menu-bar quick approvals, native notifications, shortcuts, deep links | build now |
| **G6 share + handoff** | §15: scoped expiring share invitations; turn-boundary handoff via the app between hosts or to a teammate | build now |
| G5, G7, G8 | accounts/SaaS, zero-knowledge services, direct paths | design notes only (Appendix A) |
| — | Hosted runners, Slack/Teams integrations | **not built**; design notes only (A.6) |

---

## 1. Principles and invariants

1. **No inbound port on the host.** The gateway only makes outbound connections (relay, push services). A LAN listener is opt-in and later (A.5).
2. **The relay learns no content.** It sees ciphertext and routing metadata (host id, connection times, byte counts, client IP). Never keys, terminal content, prompts, code or answers.
3. **Two separate trusts.** *Transport trust* (the relay) is zero: a malicious relay can drop or delay traffic, nothing more. *App-publisher trust* (whoever serves the web app's JavaScript) is real: that code holds the device key. The two are separated (§9.4) and the UI says which origin it trusts.
4. **Keys live on endpoints and are pinned out-of-band.** Host keys on the host, device keys on the device. Pairing pins the host key from the QR; the host confirms the device key fingerprint.
5. **The server stays the authority.** The gateway never reaches into server internals; interactions, delivery state and policy are decided by the server. The gateway adds authorization (device scopes), fan-out and push.
6. **Least capability per device, enforced on the host.** Scopes and batch eligibility are enforced in the gateway, never only in the UI.
7. **Never replay uncertain input.** Terminal input and answers are not retried automatically after a lost response; the client refetches state and asks the user.
8. **One UI codebase.** All product UI lives in `@vibeke/ui`; app shells only provide platform services.
9. **Fail closed, degrade visibly.** Version mismatches, failed handshakes and revoked devices close with an end-to-end reason; the UI shows "host offline / relay unreachable / revoked", never a silent spinner.

---

## 2. Topology

```
 phone (PWA from app origin)        relay (VPS / hosted)                  host (laptop, devbox)
┌────────────────┐   wss    ┌──────────────────────────┐   wss    ┌──────────────────┐  unix  ┌────────┐
│ @vibeke/ui     │─────────►│ /v1/connect?host=H       │◄─────────│ vibeke-gateway   │───────►│ vibeke │
│ @vibeke/core   │◄═════════╪══ opaque Noise frames ═══╪═════════►│  Noise responder │ JSON-  │ server │
│ Noise initiator│          │ /v1/host  (control)      │          │  app API, push   │ RPC    └────────┘
└───────┬────────┘          │ /v1/accept (data)        │          └────────┬─────────┘
        │                   └──────────────────────────┘                   │
        └◄──── Web Push (RFC 8291 encrypted, RFC 8292 VAPID) ◄── push service ◄┘ (host sends directly)
```

- The host keeps one authenticated **control WebSocket** to the relay. When a device connects for host `H`, the relay tells the host, the host opens a **data WebSocket** for that connection, and the relay splices the two. Everything after the splice is Noise ciphertext.
- Web Push goes **host → push service → browser** directly, encrypted to the browser's subscription keys and signed with the **device's** VAPID key (§8.1). The relay is not involved.

### 2.1 Where the relay runs

Anything with a public IP and TLS: a small VPS (Hetzner, DigitalOcean), Fly.io, or a container behind Caddy. It keeps only in-memory routing state; a restart drops live connections and hosts reconnect with backoff. The default will be a Vibeke-hosted instance; self-hosting is one binary. The relay never runs on the dev host itself (a host that accepts inbound connections uses a direct path, A.5).

---

## 3. Keys and identities

| Key | Algorithm | Owner / storage | Purpose |
|---|---|---|---|
| Host static key | X25519 | gateway state dir, 0600 | Noise responder static; pinned by devices |
| Host relay key | Ed25519 | same | Signs relay challenges with its private key to prove host-id ownership |
| Host id | lowercase base32 of `blake3(relay_pub)[..16]` (26 chars) | derived | Routing address; not secret |
| Device static key | X25519 | PWA: IndexedDB; Electron: `safeStorage`-encrypted file (§9.3) | Noise initiator static; authorized on the host |
| Device VAPID key | P-256 ECDSA | generated by the device; private half shared with each paired host over the channel | Signs Web Push to that device's subscription (§8.1) |
| Pairing secret | 32 random bytes, single use, 10 min TTL | `pairings/<pid>.json` (0600) on the host and the QR | Noise `psk` for pairing |

The pairing secret is stored **recoverably** (the responder needs the same psk); it is a short-lived credential protected like the host keys and deleted when the pairing completes, is rejected or expires.

Gateway state dir: `$VIBEKE_GATEWAY_DIR`, else `<config dir>/vibeke/gateway/` (`~/Library/Application Support/vibeke/gateway`, `$XDG_CONFIG_HOME/vibeke/gateway`). Created 0700; files 0600, written atomically (temp + rename); ownership and mode checked on start (09 §3.1). Files: `host.json`, `devices.json`, `pairings/`, `gateway.toml`, `audit.log`.

**Rotation (later):** `vibeke-gateway rotate` creates a new host static key; for 7 days the gateway accepts handshakes with either key (it tries the new key, then the old one, on message 1). Connected devices receive `host.key_rotated {hk}` over their authenticated channel and re-pin. Devices offline for longer re-pair. Rotating the relay key changes the host id and is announced the same way.

---

## 4. Pairing

### 4.1 The link

`vibeke-gateway pair [--name "the maintainer's phone"] [--scope full|approve|view] [--no-confirm]` creates a pending pairing, prints a QR code plus the URL, then **waits** for the claim (§4.3):

```
<app origin>/#/pair?d=<base64url(json)>
json = {"v":1,"relay":"wss://relay.example.com","host":"<host id>","hk":"<host static pub>",
        "pid":"<pairing id>","psk":"<pairing secret>","exp":<unix s>,"name":"devbox"}
```

The payload is in the URL **fragment**, never sent to the app origin's server. `<app origin>` is `gateway.toml: app_url` (§9.4).

### 4.2 Handshake

1. The device generates its static X25519 key if it has none and opens `wss://<relay>/v1/connect?host=<host id>`.
2. It sends the **hello** text message, whose exact bytes are the Noise **prologue**: `{"v":1,"proto":"vibeke-e2e/1","mode":"pair","pid":"<pid>"}`.
3. Noise `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s`: initiator = device, responder = gateway with static `hk`, `psk` = pairing secret (looked up by `pid`; unknown/expired → close `unauthorized`).
4. With psk2, the responder only learns that the device **holds the psk** when the device's first transport message decrypts. That message is the request `pair.claim {name, platform}`; until it arrives, nothing is recorded and no app method is available. Its result is `{status: "pending", fingerprint}`; the outcome follows as a notification `pair.done {device_id, host_name, host_id, scope}` or `pair.rejected`. The app compares `fingerprint` with its own key's fingerprint and aborts on mismatch. The pairing connection then closes; later connections use IK.

### 4.3 Host confirmation (the authorization boundary)

A photographed QR must not silently grant durable access, so a valid claim is not yet authorization:

1. The gateway marks the pairing `claimed {device_pub fingerprint, name, platform}` and replies `pair.pending {fingerprint}`. The app shows the same fingerprint (`abcd-efgh`, blake3 of the device static key).
2. The waiting `vibeke-gateway pair` prints `Pair "the maintainer's iPhone" (iOS) fingerprint abcd-efgh? [y/N]`. Only on `y` does the gateway **atomically** consume the pairing, recheck expiry, and persist the device to `devices.json`, then sends `pair.done {device_id, host_name, scope}`.
3. `N`, timeout (2 min) or closing the pair command → `pair.rejected`; the pairing stays usable until its own expiry so a hijacked claim does not lock the owner out.
4. `--no-confirm` skips step 2 and makes the QR an explicit **bearer invitation** (documented as such; useful for scripted setups).

Failures that do not prove psk possession (bad handshakes) never burn a pairing; they are rate-limited per pairing id and per relay connection (10/min). A second valid claim while one is pending is rejected.

### 4.4 Normal connections

Hello `{"v":1,"proto":"vibeke-e2e/1","mode":"device"}`, then `Noise_IK_25519_ChaChaPoly_BLAKE2s`. After message 1 the gateway knows the device static key and checks `devices.json`; unknown or revoked → it completes nothing and closes with a plaintext `{"error":"unauthorized"}` (the relay could forge this, so the app treats it as "maybe revoked", re-tries later, and only shows "revoked" after an authenticated `device.revoked` notification or three consecutive `unauthorized` closes).

One device can pair with many hosts.

### 4.5 Typed-code fallback (later)

Short code + balanced PAKE (CPace) deriving the psk, magic-wormhole style. OPAQUE and SPAKE2+ are not used: they are augmented PAKEs for password logins against a server-stored verifier, and here there is neither a password nor a trusted server.

### 4.6 Revocation

`vibeke-gateway devices` lists devices; `vibeke-gateway revoke <device>` removes it, deletes its VAPID key and subscriptions, sends an authenticated `device.revoked` to its live connections and closes them, and cancels any of its in-flight gateway operations. Revocation does not erase screens already cached on the device or notifications already delivered; the docs say so.

---

## 5. Channel (`vibeke-e2e/1`)

- **Transport:** one WebSocket per device connection. Message 0 is the hello (**text** message). Every later message is a **binary** WebSocket message carrying exactly one Noise message. A text message after the hello, or a binary message before it, is a protocol error.
- **Handshake payloads:** message 1: empty. Message 2: `{"v":1,"host_name","gateway_version","server_version"}`.
- **Framing:** application messages are UTF-8 JSON-RPC 2.0. Each is split into chunks of ≤ 65000 bytes; each Noise plaintext is `flag:u8 ‖ chunk` with `flag = 0x00` (more) or `0x01` (final); any other flag or an empty plaintext is a protocol error. Reassembly limit 16 MiB; a partial message older than 30 s is an error. Errors close the connection.
- **Replay and order:** Noise nonces are implicit counters; a dropped, reordered or replayed message fails decryption and closes the connection. Reconnect starts a new session.
- **Liveness:** the app sends `ping` every 20 s; either side closes when nothing authenticated arrives for 60 s. In-flight requests at close are reported to the UI as **unknown outcome**: mutations are not retried (§1.7); the client refetches.
- **Rekey (later):** after 2^20 messages or 1 h in one direction, the sender sends `{"jsonrpc":"2.0","method":"channel.rekey"}` under the current key, then calls Noise `rekey` for its sending direction only; the receiver rekeys its receiving direction after processing that message. Nonce counters continue. Until rekey ships, the gateway closes sessions older than 12 h and the app reconnects.

---

## 6. Relay (`vibeke-relay`)

### 6.1 Endpoints

| Endpoint | Who | Behaviour |
|---|---|---|
| `GET /v1/host` (WS) | gateway | Control socket (§6.2). |
| `GET /v1/accept` (WS) | gateway | Data socket for one pending client (§6.3). |
| `GET /v1/connect?host=<id>` (WS) | device | Waits ≤ 10 s for the host to accept, else closes `4404 host_offline` (no control socket) or `4408 accept_timeout`. |
| `GET /v1/status?host=<id>` | device | `{"online":bool}`; rate-limited. Untrusted hint only. |
| `GET /healthz` | ops | `ok` |
| `GET /` + assets | browser | The web app, only when started with `--app-dir` (self-hosters; §9.4). |

Close codes: `4400 bad_request`, `4401 unauthorized`, `4404 host_offline`, `4408 accept_timeout`, `4409 replaced`, `4413 too_large`, `4429 rate_limited`, `4503 draining`. These are relay-originated and untrusted by devices.

### 6.2 Host control state machine

1. On upgrade the relay sends `{"t":"challenge","nonce":<32 random bytes>,"origin":<canonical relay origin>}`. The nonce is bound to this socket.
2. The host checks that `origin` equals the canonical form of the URL **it dialed** (scheme + host + port, lowercase, default ports elided) and replies `{"t":"auth","host":<id>,"pub":<relay pub>,"sig":<Ed25519 signature with the host relay private key>}` over `"vibeke-relay/1 host-auth\0" ‖ origin ‖ "\0" ‖ nonce`.
3. The relay checks `host == host_id(pub)`, the signature, and that `origin` is one of its configured public origins (`--public-url`, repeatable). Failure → `4401`. Must complete within 10 s.
4. On success the relay assigns a **generation** (monotonic per host id) and replies `{"t":"ok","host","gen"}`. If a control socket for the same host already exists, the new one replaces it: the old socket gets `4409 replaced`; existing spliced connections are unaffected; pending connects are re-announced on the new control. Cleanup of the old socket is fenced by generation, so it can never remove the replacement's registration.
5. While registered the relay sends `{"t":"incoming","conn":<128-bit random id>,"gen"}` for each client.

### 6.3 Accept and splice

1. Pending connections live in a map `conn → {host, gen, client socket, deadline}`, state `pending`.
2. The host opens `/v1/accept` and sends `{"t":"accept","host","conn","gen","sig"}` with `sig` over `"vibeke-relay/1 accept\0" ‖ origin ‖ "\0" ‖ host ‖ "\0" ‖ gen ‖ "\0" ‖ conn`.
3. The relay atomically moves `pending → spliced` only if `conn` exists, belongs to `host` and `gen`, the signature verifies, and the deadline hasn't passed. Otherwise the accept socket is closed (`4401` / `4408`); a duplicate or late accept never touches an existing splice.
4. When the deadline passes first, the entry moves `pending → expired` and the client gets `4408`.
5. Spliced sockets forward WebSocket messages verbatim (text and binary, preserving type). Either side closing closes the other with the same code.

### 6.4 Limits

Defaults, all configurable:

| Bound | Default |
|---|---|
| WebSocket message | 128 KiB (Noise messages are ≤ 64 KiB + framing) |
| Per IP | 30 new sockets/min, 64 concurrent |
| Unauthenticated control / accept sockets | must authenticate in 10 s; ≤ 8 per IP |
| Per host | 32 spliced, 8 pending; `incoming` announcements ≤ 60/min |
| Per spliced connection | token bucket 1 MiB/s sustained, 4 MiB burst, ≤ 200 messages/s, each direction; idle 120 s |
| Queues | each forwarding direction is a bounded channel (64 messages); a slow reader back-pressures the sender's socket instead of buffering; a writer blocked > 30 s closes the pair |
| Global | `--max-hosts` 10 000, `--max-conns` 50 000 |

The relay only splices a client with the host that authenticated for that host id, so it is not an open tunnel; byte budgets keep abuse cheap. Self-generated host keys do not stop someone from running their own host as a free tunnel endpoint; accounts (A.1) address that for the hosted relay.

The **gateway** also limits independently of the relay: ≤ 16 concurrent devices connections, ≤ 32 in-flight RPCs per connection, ≤ 4 pairing handshakes/min, and it only accepts `incoming` announcements at ≤ 60/min.

### 6.5 What the relay stores and logs

Nothing on disk. Logs: host id prefix (8 chars), event, byte totals and close code. Client IPs only as a keyed hash rotated daily (`--log-ip raw` for self-hosters). Never message contents.

### 6.6 Extension point for accounts

`trait Authorizer { host_connect(host_id, token) -> Decision; client_connect(host_id, ticket, ip) -> Decision; usage(host_id, bytes_in, bytes_out) }`. G1 ships `Open` (limits only) and `StaticTokens` (`--host-token`, for private self-hosted relays). Accounts later (A.1).

### 6.7 Deployment

`vibeke-relay --listen 127.0.0.1:8787 --public-url https://relay.example.com [--app-dir web/apps/pwa/dist]` behind Caddy for TLS. `SIGTERM` → stop accepting, close controls with `4503` (hosts reconnect with backoff), wait ≤ 30 s for splices.

---

## 7. Gateway (`vibeke-gateway`)

### 7.1 Process

```
vibeke-gateway run [--relay wss://…] [--session NAME] [--socket PATH]
vibeke-gateway pair [--name N] [--scope full|approve|view] [--no-confirm]
vibeke-gateway devices | revoke <device> | status
```

Socket precedence: `--socket`, then `$VIBEKE_SOCKET` (only when `$VIBEKE_SESSION` matches `--session`), then the server's runtime-dir rule (`$VIBEKE_RUNTIME_DIR`, `$XDG_RUNTIME_DIR/vibeke`, `$TMPDIR/vibeke-$UID`) + `<session>/vibeke.sock`. The gateway sends `client.hello {client:"vibeke-gateway", kind:"cli"}` and refuses to run unless `capabilities` contains `*` (started inside a pane it would get pane scope).

It keeps two server connections: an RPC connection (pipelined requests, matched by id) and an **event connection** dedicated to `events.subscribe` (the server only accepts it on a raw connection). Both reconnect with backoff; while the server is down the app API returns `unavailable`.

`run` reconnects to the relay with exponential backoff (1 s → 60 s, full jitter). `pair` talks to `run` through the state directory: it writes `pairings/<pid>.json` and watches it for `claimed` / `done`, writing `confirmed: true|false` back.

### 7.2 Device scopes

| Scope | Allows |
|---|---|
| `view` | read everything: dashboard, panes, screens, history, changes, interactions, events, notifications |
| `approve` | `view` + `interaction.answer` (decisions, option choices and free-text answers **to an open interaction**), batch answers, `agent.interrupt` |
| `full` | `approve` + free-text prompts, raw keys/text to any pane, tabs/agents/tasks, attachments, rename/close, device management |

Default for `pair` is `full` (your own phone); `approve`/`view` are for shared or secondary devices. Enforcement is in the gateway per method before any server call (`forbidden` otherwise). Every mutating call is appended to `audit.log` (`{ts, device_id, method, target, op_id, outcome}`; parameters redacted with `vk-redact`).

### 7.3 Envelope, operations and retries

- JSON-RPC 2.0 (`"jsonrpc":"2.0"` on every message). Errors reuse server kinds (`unsupported`, `not_found`, `conflict`, `invalid_params`, `unavailable`) plus `forbidden`, `stale` and `too_large`.
- Every mutating request carries `op_id` (client-generated UUID). "Mutating" = every `approve`/`full` method plus `prefs.set`, `push.*` and `notification.read`; the gateway rejects them without one. The gateway keeps a per-device map `op_id → (params hash, result)` for 10 min: a retry with the same `op_id` and identical params returns the stored result; different params → `invalid_params`. The **app never auto-retries** a mutation whose response was lost (§1.7); it refetches state and shows the outcome.
- Answers pass `actor = "gateway:<device name>"` and `idempotency_key = "gw:<device_id>:<op_id>"` to the server (§7.7).

### 7.4 App API

| Method | Params → result | Scope | Backed by |
|---|---|---|---|
| `hello` | `{client, version, visible}` → `{host_name, device_id, scope, server_version, features}` | view | gateway |
| `client.visibility` | `{visible}` → `{}` | view | gateway; a lease that expires 60 s after the last `ping` |
| `dashboard.get` | `{}` → `{at, session, machine, workspaces, tabs, panes, runs, interactions, tasks, notifications_unread}` | view | `session.snapshot` + `notification.list`; enums normalized (§7.5) |
| `pane.read` | `{pane, source?, lines?}` → `{text, revision}` | view | `pane.read` (plain text today; a styled/ANSI source is a planned server addition so the mirror can show colour) |
| `pane.send_text` | `{pane, text, submit?, op_id}` → `{}` | full | `pane.send_text`; `submit` adds `pane.send_keys ["Enter"]` |
| `pane.send_keys` | `{pane, keys[], op_id}` → `{}` | full | `pane.send_keys` |
| `pane.rename` / `pane.close` / `pane.focus` | server params + `op_id` | full | server |
| `agent.prompt` | `{target, text, op_id}` → `{}` | full | `agent.prompt` |
| `agent.interrupt` | `{target, op_id}` → `{}` | approve | `agent.interrupt` |
| `agent.transcript` | `{target, limit?, skip?}` → `{turns, has_older}` | view | `agent.transcript`; `skip` = newest turns the client already has; the gateway asks for `skip + limit` and slices until the server pages natively |
| `agent.start` | `{workspace, cwd?, harness, prompt?, op_id}` → server `agent.start` result + `pane` (id) | full | `tab.create` → `root_pane`, then `agent.start {pane}` |
| `agent.harnesses` | `{}` → `{harnesses}` | view | server |
| `tab.create` | `{workspace, cwd?, op_id}` → `{tab, pane}` | full | `tab.create` |
| `interaction.list` / `interaction.get` | server params | view | server; normalized |
| `interaction.answer` | `{interaction, decision_rev, decision?, choices?: {question_id: [option_id]}, text?, op_id}` → `{interaction, delivery: {channel}}`; the delivery **state** is `interaction.delivery`, updated by `interaction.delivery_*` events | approve | gateway refetches the interaction; `stale` unless `status == open` and `decision_rev` matches; then `interaction.answer` |
| `interaction.answer_batch` | `{items[{interaction, decision_rev}], decision, op_id}` → `{results[]}` | approve | eligibility re-checked in the gateway (§7.6); each item answered individually; partial results reported |
| `git.status` / `git.diff` | §7.7 params; `git.diff` also takes `base` or `range` (07 §2.15a) | view | new server methods |
| `git.log` | `{pane, base?, limit?}` → `{commits[{sha, short, author, ts, subject}], truncated}` | view | server (07 §2.15a) |
| `fs.list` / `fs.read` | `{pane, path?}` → `{path, entries[{name, kind, size?, ignored, secret}], truncated}` / `{pane, path}` → `{path, text?, binary, truncated, size, secret}` | view | server (07 §2.15a) |
| `attention.list` | `{budget_ms?, effort?}` → server result | view | server (spec 15); limited devices get items of their panes/tasks only |
| `attention.update` | `{key, seen?, snooze_until_ms?, pin?, item_rev?, op_id}` | approve | server; `snooze_until_ms: null` is passed through (clears) |
| `task.review.get` / `task.review.candidates` / `task.review.diff` | `{task, subject?}` / `{task}` / `{task, subject?, path?, max_bytes?}` | view | server |
| `task.check.list` / `task.check.get` | `{task, subject?}` / `{check_run}` | view | server |
| `task.check.run` | `{task, subject, check, definition_digest?, authorize?, op_id}` | full | server; `idempotency_key = "gw:<device_id>:<op_id>"` |
| `preview.list` / `preview.get` / `preview.url` / `preview.status` | `{machine?, status?, all?, task?, pane?}` / `{preview, machine?}` / same / `{}` | view | server |
| `preview.open` / `preview.promote` / `preview.forget` | `{preview?, url?, machine?, window?, split?, focus?, pane?, op_id}` / `{preview, machine?, op_id}` | full | server |
| `worktree.list` | `{pane}` or `{workspace}` (or `{cwd}`) → `{worktrees}`; not for share devices | view | server `worktree.list {cwd}` with the pane's cwd or the workspace root |
| `tab.rename` / `tab.close` / `tab.focus` | `{tab, title?, op_id}` / `{tab, op_id}` | full | server |
| `attachment.put` | `{name, mime, data_b64, op_id}` → `{path}` (≤ 8 MiB) | full | `image.upload` |
| `notification.list` / `notification.read` | server params | view / approve | server |
| `events.subscribe` | `{after?}` → `{at}`; then `event` notifications | view | gateway ring buffer (§7.5) |
| `prefs.get` / `prefs.set` | `{device: {...}, host: {dnd_until?}}` | view / full | gateway (device prefs per device; DND host-wide) |
| `push.subscribe` | `{subscription, vapid_private, op_id}` → `{}` | view | gateway (§8.1) |
| `push.unsubscribe` / `push.test` | `{op_id}` | view | gateway |
| `stt.transcribe` | `{mime, data_b64, op_id}` → `{text}` | full | gateway, only if `gateway.toml: stt.command` is set (§8.4) |
| `devices.list` / `devices.revoke` | | view / full | gateway |
| `ping` | `{}` → `{}` | view | gateway |

### 7.5 Events and normalization

- The gateway keeps **one** server subscription (`events.subscribe {after}` on its event connection) and a ring buffer of the last 5 000 events with their server cursors. Device subscriptions are served from the ring: `events.subscribe {after: seq}` replays from the ring if `after` is inside it, else returns `{reset: true}` and the client calls `dashboard.get` again. Each device has a bounded outbound queue (1 000 events); overflow sends `events.reset` and drops the queue.
- **Snapshot barrier:** `dashboard.get` returns `at` (= `session.snapshot.at_seq`); the client subscribes with `after = at`, so nothing falls between snapshot and stream.
- On server `events.overflow`, server restart or cursor epoch change, the gateway resubscribes from its last cursor; on `truncated` it clears the ring and broadcasts `events.reset`.
- Events are forwarded as `event` notifications carrying the server event `{seq, ts, type, subject, actor, data}`, filtered by scope (all scopes may read all events today). Device cursors are plain `seq` numbers; `events.subscribe {after}` answers `{at}` or `{reset: true}`. Other notifications: `events.reset`, `device.revoked`.
- `dashboard.get` and `interaction.*` results add `harness` and `repo_root` to each interaction (from its run's cwd) so clients can group batches exactly as §7.6 checks them.
- **Normalization:** server enums serialize PascalCase (`"Approval"`, `"Open"`, `"Delivered"`) except `interaction.list`'s `kind`. The gateway rewrites every interaction to snake_case (`kind`, `status`, `delivery`, `action.risk`, `answer.decision`) so the app sees one form.

### 7.6 Batch eligibility

The UI only offers a batch the gateway would accept, and the gateway re-checks at answer time. Items are eligible together only if all hold:

- `kind == approval`, `status == open`, `answerable`, and `decision_rev` matches the request;
- the same **fingerprint**: `(harness, action.tool, normalized command or sorted paths, repo root of the pane cwd)`;
- `action.risk` is `low` or `medium` (`high` and `unknown` are never batched or swiped);
- the decision is `allow` or `deny` (never `allow_always` in a batch).

### 7.7 Server additions (additive)

| Method / change | Contract |
|---|---|
| `git.status {pane}` → `{repo_root, branch?, upstream?, ahead, behind, files[{path, x, y, kind, adds?, dels?, binary}], clean}` | Runs in the pane's cwd. Pane-scope callers may only target their own pane; `path` targets are allowed only for full-scope callers. |
| `git.diff {pane, file, staged?}` → `{diff, truncated, binary, untracked}` | Unified diff against `HEAD` (or the index if `staged`). Untracked files are rendered as additions by reading the file. Output capped at 512 KiB. |
| `interaction.answer` gains optional `actor` | Full-scope callers may label the answer (`answered_by = actor`); otherwise unchanged. Separates attribution from `idempotency_key`. |
| `git.log`, `git.diff {base|range}`, `fs.list`, `fs.read` | Read-only repository browsing for the workspace views; contracts in 07 §2.15a. Same execution and filesystem rules as below; refs are validated and passed after `--end-of-options`. |
| `agent.transcript` items gain `ts`; turns gain `duration_ms`, `tool_count`, `subagent_count` | Additive (07 §2.7). |

Git execution rules (git can run configured programs):
- Every git call: `git -c core.fsmonitor=false -c core.untrackedCache=false -c diff.external= -c core.pager=cat -c color.ui=false`, `--no-ext-diff --no-textconv`, `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`, stdin closed, 5 s timeout, 4 MiB stdout cap, killed on timeout.
- `file` must be a relative path listed by `git status` for that repo; it is resolved under `repo_root` with symlinks rejected at every component (`O_NOFOLLOW` walk) before an untracked file is read; files > 1 MiB or binary report `binary/truncated` only.
- Files matching the secret patterns of 09 §9 (`.env*`, `*.pem`, `id_*`, `*.key`, credentials files) report `{secret: true}` and no content.

### 7.8 Push triggers

| Server event | Push (when the device has no visible lease) |
|---|---|
| `interaction.opened` with `kind ∈ approval, question, plan_review` | "Codex · samplehub wants to run `pnpm test`" (privacy level permitting) |
| `agent.state_changed` to `idle` with `done_rev` increased, no open interaction | "Claude · backend finished" (default off, per-device toggle; debounced 30 s per pane, dropped if the pane is working again) |
| `agent.state_changed` to `error` / `rate_limited` | "Claude · dashboard stopped: rate limited" |
| `notification.created` with urgency ≥ normal and **not** generated for an interaction already pushed | title/body |

- One notification per host (`tag = vibeke:<host id>`), merged: one item shows its own text, several show "3 agents need you". `renotify` only when a new item is added.
- Visible notifications only (WebKit revokes push permission for silent pushes). When items resolve, the app closes stale notifications on its next foreground via `registration.getNotifications()`; no "clear" pushes.
- **Privacy levels** per device (default `summary`): `full` (command/summary through `vk-redact`), `summary` (harness, workspace, kind), `minimal` ("Vibeke: 1 agent needs you"). Payloads are RFC 8291-encrypted regardless.
- DND (host-wide, existing phone-companion semantics) **suppresses** pushes; nothing is queued. Quiet devices reconcile on foreground.
- Delivery: TTL 6 h, urgency `high` for interactions, `normal` otherwise; 404/410 deletes the subscription; 5 consecutive failures disable it and show a banner on next foreground; 429 honours `Retry-After`.
- Objective (not a guarantee): p50 < 5 s from `interaction.opened` to notification while the host is awake and online. A sleeping laptop sends nothing; the dashboard shows the host offline.

---

## 8. Push mechanics

### 8.1 One subscription per device, signed by the device's VAPID key

A browser push subscription is bound to one `applicationServerKey`, and an origin's service worker has one subscription. Per-host VAPID keys therefore cannot serve a PWA paired with several hosts. Instead **the device owns the VAPID key pair**: it generates P-256 keys, subscribes once with its public key, and sends `{subscription, vapid_private}` to every paired host in `push.subscribe` over the encrypted channel. Each host signs its pushes with that key. Consequences:

- One subscription, any number of hosts; adding a host needs no resubscribe.
- Revoking a host from the device: the device rotates its VAPID key, resubscribes, and re-sends to the remaining hosts.
- A host can only push to devices that chose to give it the key; compromise of a host leaks only that device's push capability.

### 8.2 SSRF guard

The subscription `endpoint` comes from a client, and the gateway makes requests to it, so: `https` only; host must match the allow-list (`*.push.apple.com`, `fcm.googleapis.com`, `*.push.services.mozilla.com`, `*.notify.windows.com`, configurable); resolved addresses must be public (no loopback, private, link-local, CGNAT or multicast); no redirects; 10 s timeout; ≤ 3 subscriptions per device; ≤ 120 sends/hour per device.

### 8.3 Platform notes

- iOS: Web Push only for home-screen PWAs (16.4+); the app shows an install guide first and requests permission only from a user gesture. No notification actions on iOS; tapping opens the deep link.
- Android/desktop Chrome: actions `Open` and, for low/medium risk approvals, `Approve…`. **No notification action ever answers directly**: `Approve…` opens the card with a confirm sheet; a crafted `#/i/<id>?do=allow` link only pre-selects.
- Electron: no Web Push; the main process shows native notifications while connected (G4).
- Subscriptions are refreshed on every app start (`pushManager.getSubscription()`); a changed endpoint is re-sent to all hosts.

### 8.4 Speech to text

- Web Speech API is used only after the user accepts a one-time notice that the browser's recognizer may send audio to Apple/Google. Otherwise, or by choice, the app records audio and calls `stt.transcribe` on the gateway, which runs the configured `stt.command` (e.g. a local whisper.cpp) with a 60 s timeout and 10 MiB input cap, deleting the temp file afterwards.
- The transcript always lands in the composer; sending requires a tap (no hands-free auto-send).

---

## 9. Client features and codebase

### 9.1 Baseline features

Inventory from an existing phone companion. It scrapes dialogs from pane text in the browser; Vibeke gets them as structured `Interaction`s (with the server's verified keystroke fallback for screen-only harnesses), so the app never parses terminal grids.

| Area | Parity features |
|---|---|
| Shell | Status band (connection banner amber ~4 s / red ~15 s with Retry, green flash on recovery); boot splash; busy bar; idle lock after 30 min visible-but-untouched (polling paused, "catching up" on resume); new-build detection and reload |
| Home | Host switcher and session label; footer tabs **Inbox**, **Panes**, **Focus** (only what needs you) and **Changes**; summary line "N need you" jumping to the first; colour wash on rows that need you; pins (long-press; pinned group on top; stored per device); hold menu: Pin, Rename, Close (double-tap), Focus in terminal; launch strip (harness × workspace); new agent / new tab sheet |
| Space | Workspaces with "needs you" dots; tabs; panes grouped by tab |
| Pane | ANSI terminal mirror (colours rendered, escapes never shown raw) with find, copy, wrap and text size; latest-reply card; ⋮ menu (find, history, copy, zen, rename, close, focus, pin); prev/next pane; **card dock** of open interactions above the action belt, collapsible to see the terminal |
| Action belt | **Keys**: keypad (Esc, Tab, sticky Shift/Ctrl/Alt off → once → locked, arrows, Ctrl-C, Space, Enter, Backspace), chord mode (queue keys as chips, send once), echo + ✓ per press. **Quick**: per-harness quick replies (tapped ✓, others dim). **Agent**: harness slash-command palette. **Display**: wrap, text size |
| Composer | Text box with clear / undo-clear and "You sent:" preview; destructive commands (`rm -rf`, `git push --force`, `drop table`…) need a second tap; attachments from photos/files/paste as numbered `#N` chips; voice (§8.4); notices for password prompts (no-echo detection), read-only scope, host offline |
| Zen | Chrome-free pane view; optional auto-zen in landscape |
| History | Transcript with load-older, find, jump prev/next between own messages; collapsible tool calls |
| Changes | Read-only git: files grouped by repo, filter by path/status, syntax-highlighted diff, prev/next file, refresh every 5 s while visible |
| Crew | Paired hosts with online state and "needs you" counts |
| Settings | Appearance (theme, terminal font size, belt size); Device (haptics, zen in landscape, privacy level); Alerts (push on/off, needs-input / finished, DND 30 m / 1 h / 4 h host-wide); System (hosts, devices with revoke, pair by QR or link, connection info, About with the trusted app origin) |
| Tour | One-time intro per device; push setup offered after it |
| Install / offline | `beforeinstallprompt` capture, iOS share-sheet guide; cached shell serves deep links offline; an offline pane shows its last mirror with "last seen" |

Deferred: i18n beyond English (strings in one typed dictionary), in-app self-update (PWA update replaces it), prompt-cache warnings, chat view.

### 9.2 Decisions first

- **Inbox** (default tab when anything is open): every open interaction across hosts as a card: harness, workspace, wait time, risk badge, command/paths/diff preview; `Allow` / `Allow always` / `Deny` / per-option choices / free text; plan markdown with Approve / Request changes.
- **Quick actions:** swipe right = allow, left = deny for low/medium risk; high/unknown risk require the button plus confirm. Haptics where supported.
- **Batch approvals:** cards with the same fingerprint (§7.6) group as "4 agents want `pnpm test` in samplehub" → `Allow all` / `Deny all` / expand.
- **Delivery state:** after answering, the card shows `delivering → delivered`, or `failed / unknown` with "open pane". `stale` → the card refreshes and asks again.
- **Answered elsewhere:** the card leaves with "answered in terminal".
- **Push → card:** a notification opens its card directly.

UX rules: mobile-first, primary actions bottom-reachable, safe-area insets, OS dark/light; nothing is ever sent without an explicit tap; every mutating tap shows progress and outcome; errors are actionable.

### 9.3 Codebase shared by PWA and Electron

```
web/                         # bun workspaces
  packages/core/             # @vibeke/core — no DOM/React; deps: @noble/curves, @noble/ciphers, @noble/hashes
    noise.ts channel.ts rpc.ts pairing.ts hosts.ts inbox.ts model.ts platform.ts
  packages/ui/               # @vibeke/ui — React 19 screens/components; depends on core
  apps/pwa/                  # Vite + service worker; IndexedDB KeyStore; Web Push
  apps/desktop/              # Electron; main-process KeyStore/transport; preload bridge
```

- `core` receives every platform capability by injection: `interface Platform { keystore: KeyStore; connect(url): Socket; clock: {now, setTimeout, clearTimeout}; random(n); platformName; lifecycle: {isVisible, onVisible, onHidden}; notify?; push? }`. It runs in browsers, service workers, Electron and Bun tests.
- Product logic lives in `core`/`ui`; shells implement `Platform` and bootstrap only.
- Stack: React 19, Tailwind v4, hash routing (`#/inbox`, `#/h/<host>/p/<pane>`), `useSyncExternalStore` stores (no server-state library: data comes from RPC + events).
- Rendering safety: terminal text is rendered through an ANSI → spans converter (no HTML); markdown (plans, transcripts) through a sanitizing renderer with raw HTML disabled; links open externally with `rel=noopener` and only `http(s)`.
- Electron (G4): `contextIsolation`, `sandbox`, no `nodeIntegration`; navigation and `window.open` denied except the bundled app; the preload exposes a narrow IPC (`keystore.get/set`, `notify`, `connect`) and the main process validates `event.senderFrame` origin on every call; device keys encrypted with `safeStorage` and refused when Linux reports the `basic_text` backend; a `LocalTransport` speaks the same channel to a gateway on the same machine.

### 9.4 Where the app is served from (app-publisher trust)

The JavaScript that runs the app holds the device key and sees plaintext, so whoever serves it is trusted. Rules:

- The pairing link and the app origin come from `gateway.toml: app_url`, chosen by the host owner. The relay origin is a separate setting.
- **Self-hosters** may serve the app from their own relay (`--app-dir`): they are the publisher, so nothing is lost.
- **The hosted service** serves the app from a dedicated static origin (`app.vibeke.dev`) built reproducibly from tagged sources with published asset hashes, **never** from the relay operator's infrastructure.
- Settings → About shows the app origin and build hash. Native/Electron builds ship signed code and avoid the question.
- SRI is not claimed as protection against the origin itself.

---

## 10. Threats (additions to 09)

| # | Threat | Mitigation |
|---|---|---|
| R1 | Malicious relay reads or alters traffic | Noise with pinned host key; prologue binds the hello; relay close codes untrusted |
| R2 | Malicious app publisher | §9.4 separation; publisher shown in About; native/Electron signed builds |
| R3 | Host-id squatting / control hijack | Ed25519 challenge bound to socket and canonical origin; generation-fenced replacement; E2E pins the static key anyway |
| R4 | Photographed QR | Host-side fingerprint confirmation; atomic consume; failures don't burn; bearer mode is explicit |
| R5 | Lost phone | `revoke`; `approve` scope for secondary devices; high-risk answers need confirm; app lock (WebAuthn) later |
| R6 | Stale or crafted approval | `decision_rev` check in the gateway; deep links only pre-select; batches re-validated |
| R7 | Lock-screen leakage | privacy levels; `vk-redact` on payloads |
| R8 | SSRF via push endpoints | §8.2 |
| R9 | Git as a code-execution or file-read vector | §7.7 rules |
| R10 | Relay/gateway resource exhaustion | §6.4 bounds on both sides |
| R11 | Gateway compromise | same-UID process like the CLI (T9 unchanged); device scopes bound remote devices |

---

## 11. Testing and acceptance

- **Crypto conformance:** fixed-key vectors (IK and IKpsk2, payloads, chunked transport messages) generated by `vk-e2e` are checked into `crates/vk-e2e/tests/vectors.json` and replayed by `@vibeke/core`; adversarial cases: wrong psk, wrong host key, altered prologue, replayed/reordered message, bad flag, oversize, truncated reassembly. A live Rust↔TS handshake runs the gateway against the TS client.
- **Relay:** real-WebSocket tests for auth (good, bad signature, wrong origin, timeout), offline host, accept timeout, late/duplicate accept, host replacement while splices live (old cleanup must not remove the new registration), splice in both directions with message type preserved, byte/rate limits, oversize message, slow reader back-pressure, draining.
- **Gateway:** pairing (valid + confirm, reject, expired, reused, wrong psk does not burn, concurrent claims, crash between claim and confirm), unauthorized and revoked devices (live connection closed, queued mutations refused), scope matrix per method, `op_id` retry semantics, stale `decision_rev`, batch re-validation, event ring replay/reset/overflow, snapshot barrier, push mapping/coalescing/DND with a mock push service, SSRF guard, two hosts pushing to one subscription (signed with the device VAPID key).
- **Server additions:** `git.status`/`git.diff` on temp repos incl. symlink escape, secret files, external diff/textconv/fsmonitor configured to run a marker script (must not run), huge files, timeouts; `actor` attribution.
- **PWA:** unit tests for inbox ranking/grouping and answer flow; Playwright smoke against relay + gateway + server (pair → confirm → dashboard → answer a fixture interaction → delivered); manual matrix on an installed iOS PWA and Android Chrome.
- **Acceptance (G1–G3):** on mobile data, laptop behind NAT, no Tailscale: pair by QR with confirmation in < 60 s; an approval push arrives (p50 < 5 s over 20 trials, host awake); approve from the inbox and see `delivered`; read the screen, send keys, view history and changes; revoke from the terminal and see the app show "revoked"; relay logs contain no content.

---

## 12. Implementation status (2026-10-06)

| Piece | State |
|---|---|
| `vk-e2e` | Noise IK/IKpsk2 over `snow`, framing, hello, link, relay messages; fixed-key vectors in `tests/vectors.json` (incl. fingerprint and host id) replayed byte-for-byte by `@vibeke/core` |
| `vk-relay` / `vibeke-relay` | §6 complete without accounts: challenge + origin-bound signatures, generations, fenced replacement, atomic accept, limits, static app dir, drain. Integration tests over real WebSockets |
| `vk-gateway` / `vibeke-gateway` | §4, §7, §8: pairing with host confirmation, device scopes, op_id cache, normalization, event ring, push triggers, RFC 8291/8292 Web Push in pure Rust (RFC test vector), SSRF guard, revocation; `examples/devclient.rs` is a CLI device for testing. End-to-end test with relay + fake server; smoke-tested against a real server |
| Server additions | `git.status` / `git.diff` (hostile-config test proves fsmonitor/external diff/textconv never run), `interaction.answer {actor}` with `answer_key`, and retried answers no longer re-deliver |
| `@vibeke/core` | Noise, channel, RPC, pairing, multi-host manager, inbox ranking/grouping |
| Desktop (§16) | `web/apps/desktop`: main-process host engine, safeStorage vault, local + relay transports, Connect to this Mac, menu-bar quick approvals, native notifications, ⌘K palette and keyboard navigation (shared with the PWA), pop-out panes, deep links, hardened IPC, packaging; Codex review fixed; unit + real-gateway Electron e2e + memory e2e in CI |
| Share + handoff (§15) | `share.create`, limit enforcement on every call and event, handoff export/transfer/import with Claude/Codex resume; end-to-end handoff test (thin bundle, patch, untracked files, secret skipped, transcript rewritten + redacted, resume args); app screens for share, hand off and receiving |
| `@vibeke/ui`, `@vibeke/pwa` | Baseline screens + inbox/quick actions/batches/push built; 136 tests; headless-Chrome smoke against real server + relay + gateway. Not yet verified on real iOS/Android push or device voice input. Transcript turns are `{role, text, ts}` only, so tool calls are not separated yet (server addition needed). |

## 13. Work in existing code (coordinated with the TUI/server session)

The gateway, relay and web apps live in new crates and `web/`. What they still need from the existing server, CLI and TUI is listed here and owned by the session working on those crates. Hosted runners and Slack/Teams stay out of scope.

| # | Where | Work | Why |
|---|---|---|---|
| — | status | X1–X5 landed on main (d3323b7, `vk-server/src/gateway_api.rs`) and are consumed by the gateway; X6 and the TUI confirm overlay are in progress in the TUI session; X7 waits until the new crates are committed; X8 exists (`attention.list`) |
| — | workspace views | Built on the `workspace-ui` branch: transcript timing (`ts`, `duration_ms`, `tool_count`, `subagent_count`), `git.log`, `git.diff {base|range}`, `fs.list`, `fs.read` (`vk-server/src/fs_api.rs`), and the §7.4 passthroughs for attention, review, checks, previews, worktrees and tabs with share-limit checks |
| X1 | server | `pane.read {source: "styled"}` → rows of `{text, runs: [{start, len, fg, bg, bold, italic, underline, inverse}]}` from the VT engine (or ANSI SGR text) | Colour terminal mirror in the apps |
| X2 | server | `agent.transcript` turns carry `kind: text|thinking|tool_call|tool_result`, `tool`, `summary`; native paging with `before` | History screen separates tool calls; no gateway-side slicing |
| X3 | server | `client.list` reports per-client `last_input_ms` and focus (TUI attached, active in the last N s) | Presence-aware push: no phone pushes while the user is typing in the TUI (spec 12 attention inbox) |
| X4 | server | `client.hello {kind: "gateway"}` recognised in 09 §3.2 with full capabilities, and events/audit attributed `gateway:<device>` for all mutating calls (an optional `actor` param on `pane.send_*`, `agent.prompt`, `agent.interrupt`, like `interaction.answer`) | Audit trail shows which phone did what |
| X5 | server + TUI | A generic out-of-band confirmation: `client.confirm {title, body, options, timeout_ms}` shown as a TUI overlay (not in a PTY), answered by the user at the terminal | The gateway's pairing fingerprint confirmation (§4.3) and future share invitations without a second terminal running `vibeke-gateway pair` |
| X6 | TUI | Interaction overlay, inbox and sidebar show `answered_by` (e.g. "answered on the maintainer's iPhone"); a small indicator of connected devices | Visible remote activity |
| X7 | CLI | `vibeke gateway …` and `vibeke relay …` wired to the new crates (thin wrappers), `vibeke doctor` gateway section (relay reachable, devices, push) | One binary (§ intro) |
| X8 | server | `attention.list` (spec 15) consumable by the gateway; the app's inbox ranking switches to it when present, falling back to client ranking | One ranking across TUI and phone |

The gateway side of X3, X5 and X8 (calling the new methods) is done by the gateway owner once the server methods exist.

## 14. Resolutions of the Codex reviews

### 14.1 Implementation review (design review)

| Finding | Resolution |
|---|---|
| P1 confirmation could authorize a different claimant | Each claim gets a `claim_id` and the full device key; the operator's answer is written with `confirmed_claim` and only counts for that claim; reservation and consumption are atomic under the registry lock |
| P1 concurrent updates could undo revocation | Cross-process `flock` on `registry.lock` around every read-modify-write (gateway, `pair`, `revoke`); unique temp files (`create_new`) |
| P1 revocation didn't cancel queued work | Every side-effecting server call re-reads the registry and re-checks the device; a revoked connection aborts its in-flight tasks |
| P1 `op_id` raced | Atomic reservation of `(device, op_id)` bound to method + params; concurrent duplicates wait for the original result |
| P1 git clean/process filters ran | Every configured filter driver is overridden with empty commands (reading config runs nothing); test with clean + process filters |
| P1 untracked read symlink race | `openat` walk with `O_NOFOLLOW` per component, leaf checked with `fstat` |
| P1 pathspec magic | `git --literal-pathspecs`; rename sources checked for secrets; test with a file named `*` beside `.env` |
| P1 relay trusted as app publisher | Pairing and share links require an explicit `--app-url`, or `--app-from-relay` as a deliberate self-hosting choice |
| P1 batch fingerprints merged commands | Exact command bytes (gateway and app); collision test |
| P2 optional/non-atomic revision | `decision_rev` required; the server compares `expected_decision_rev` under its decision lock; batch items re-checked for eligibility at execution |
| P2 view devices could set DND | Host-wide preferences need full scope |
| P2 gateway limits | Capacity reserved before dialing an accept; accept sockets bounded to 128 KiB messages with a 15 s connect deadline; liveness counts authenticated traffic only; 30 s reassembly deadline in `vk-e2e` |
| P2 relay accounting | The accept socket's admission slot lives as long as the splice; pending connections count against global and per-host caps (test) |
| P2 event epoch | The gateway resumes with the full server cursor (machine, session, epoch, seq) |
| P2 stale dashboard after reconnect | App tracks a dirty marker and refetches on resume |
| P2 push throttling | 120 sends/hour/device, `Retry-After` cooldown, 8 concurrent sends, five consecutive failures clear the subscription |
| P2 concurrent key creation | Atomic get-or-create (IndexedDB transaction + Web Locks) |
| P2 devclient key file | Created `0600` with `create_new` |
| P2 markdown recursion | Nesting and work bounded; error boundary |
| P3 legacy idempotency fallback | Only for records without `answer_key` |

### 14.2 Share/handoff review (design review)

| Finding | Resolution |
|---|---|
| P1 handoff files escaping the worktree | Untracked files created with `create_new` + `O_NOFOLLOW` after a no-symlink component walk; export opens with `O_NOFOLLOW` and checks the file type |
| P1 sender-controlled launch args / cwd | Resume args rebuilt locally from harness + validated session id; cwd must canonicalize inside the worktree |
| P1 transcript writes outside the session namespace | Claude: `<session>.jsonl` in the computed project dir; Codex: only `sessions/…/*.jsonl` containing the session id; content must be JSON lines; exclusive no-follow create |
| P1 extra selectors bypassing share limits | Every selector present (`pane`, `target`, `interaction`, batch items) is authorized independently |
| P1 handoff devices receiving events | Connection-level methods (`events.subscribe`, `hello`, `client.visibility`) go through the same kind/scope rules |
| P1 share pushes ignoring scope/expiry | Items carry their pane; share devices only get items inside their limit; expired and handoff devices get none; `push_to` re-checks the registry |
| P1 decompression bomb | Whole decoded stream budgeted before tar parsing, effective entry sizes, entry count, path length and decoder window capped |
| P1 git hardening gaps | Handoff uses the same filter overrides and literal pathspecs; submodule recursion and submodule status disabled (server and gateway) |
| P1 TUI approval overriding a terminal rejection | Both prompts record through one locked, claim-bound, first-answer-wins write; consumption requires that recorded approval |
| P2 notification.read on shares | Only a notification attached to a shared pane; never `all` |
| P2 dashboard metadata leak | Allowlisted response (no previews, tasks or global counts); pane-only shares get only their tab with a single-pane layout |
| P2 workspace shares missing agent events | Event scope resolves pane/run/interaction subjects through a snapshot index refreshed on unknown ids; unresolved subjects stay hidden |
| P2 cancellation re-enabling duplicates | Interrupted operations leave an `outcome_unknown` tombstone instead of freeing the `op_id` |
| P2 unlocked reload | Registry reload reads and swaps the cache under the registry lock |
| P2 reservation gaps | Relay converts a pending reservation into an active-splice slot in one step under the pending lock; the gateway reserves its dial slot before spawning |
| P2 manifest mismatch | The reviewed manifest must equal the bundled one; `head` must be a full commit id and match the bundle's advertised HEAD |
| P2 origin normalization | Remotes parsed as URL / scp / local path; host case-insensitive, path exact; ambiguous forms rejected |

### 14.3 Spec review

| Finding | Resolution |
|---|---|
| P1-1 relay-served JS | §1.3, §9.4: transport vs app-publisher trust; hosted app on a separate origin; relay-served app only for self-hosters |
| P1-2 hashed psk | §3: psk stored recoverably, short-lived; §4.2: authorization only after the first transport message |
| P1-3 photographed QR | §4.3: host-side fingerprint confirmation; atomic consume; failures don't burn; explicit bearer mode |
| P1-4 relay state machine | §6.2–§6.3: private-key signatures over socket-bound nonce + canonical origin; generations; atomic pending→spliced; fenced cleanup |
| P1-5 approve scope too strong | §7.2: free-text prompts and raw input require `full` |
| P1-6 UI-only safeguards | §7.4, §7.6, §8.3: gateway checks `decision_rev`, batch eligibility incl. repo root and risk; deep links only pre-select |
| P1-7 retries and attribution | §7.3: `op_id` with param binding, no automatic mutation retries; §7.7: `actor` separate from `idempotency_key` |
| P1-8 per-host VAPID | §8.1: device-owned VAPID key shared with each host |
| P1-9 SSRF | §8.2 |
| P1-10 git safety | §7.7 execution and filesystem rules |
| P2-11 limits | §6.4 incl. gateway-side limits |
| P2-12 event fan-out | §7.1 dedicated event connection; §7.5 ring, reset, barrier, overflow |
| P2-13 event names/enums | §7.8 uses implemented events; §7.5 normalization |
| P2-14 API gaps | §7.4 full method/scope matrix; `agent.start` via `tab.create` → `root_pane` |
| P2-15 channel lifecycle | §5: message types, flags, deadlines, liveness, rekey rules; §3 rotation key selection |
| P2-16 silent push / latency | §7.8: visible only, foreground reconciliation, DND suppresses, measured objective |
| P2-17 platform boundaries | §9.3 injection, sanitization, Electron IPC/navigation/safeStorage rules |
| P2-18 speech privacy | §8.4 consent, bounded local command, no auto-send |
| P2-19 acceptance | §11 adversarial and race cases |
| P3-20 consistency | intro states additive server changes; socket precedence; `jsonrpc` envelope; per-host tags; DND host-wide vs device prefs; G5–G8 moved to Appendix A |

---

## 15. Share and handoff (G6, build now)

Both reuse pairing and the app API; no relay or server changes are needed.

### 15.1 Live share

A share is a **scoped, expiring pairing invitation** for someone else (or another device of yours):

- `share.create {kind: "share", scope: view|approve, ttl_s, workspace?, pane?, name?, op_id}` (full scope) → `{link, pid, expires_at}`; CLI `vibeke-gateway share [--scope view|approve] [--ttl 2h] [--workspace W | --pane P]`.
- The link is a pairing link (§4.1) with `share: {scope, until, label}` in its payload so the app can say "the maintainer shared *samplehub* with you, view-only, until 16:00". It is a **bearer invitation** (no fingerprint confirmation; the owner created it deliberately), single use, and must be opened within 15 min.
- The resulting device record carries `kind: "share"`, `expires_at` and `limit {workspace?, pane?}`. The gateway enforces the limit on every call: `dashboard.get`, `interaction.list`, `notification.list`, `attention.list` and `preview.list` are filtered; any method naming a pane, run, interaction, task (by its workspace; pane-only shares see no tasks), check run (by its task), tab or preview (by its pane) outside the limit returns `forbidden`; a `machine` selector is refused (handles resolve on the local machine only); `worktree.list` (repository-wide: sibling checkouts) and `task.check.run`, `attention.update`, `preview.status` and `tab.rename/close/focus` are never available to share devices; events are forwarded only when their subject's pane/workspace is inside it; `tab.create`/`agent.start` only inside the limited workspace; `devices.*`, `share.*` and `handoff.*` are never available to share devices. Expired devices are refused at the handshake and disconnected within 5 s.
- Shares are listed and revoked like devices (`devices.list` shows `kind`, `expires_at`, `limit`).

### 15.2 Handoff

Moves an agent's work to another host at a **turn boundary**. The source gateway delivers straight to the destination gateway (peers, §15.3), so no app has to stay open: an app, the TUI or the CLI only starts a `handoff.send` job on the source host and follows it. There is no app-side bundle transfer.

1. **Export** (source gateway, when a `handoff.send` job runs; it is not an API method of its own). The agent must be idle (or the job's `interrupt: true` interrupts and waits ≤ 30 s). The gateway builds a bundle (zstd-compressed tar, ≤ 200 MiB):
   - `manifest.json`: source host, repo name, `origin` URL, branch, `HEAD`, base commit, harness, session id, resume args (from the run's `resume_argv`), cwd relative to the repo root, last agent message, skipped files, redaction count;
   - `repo.bundle`: `git bundle` of `HEAD` excluding commits on any remote-tracking ref (thin); full history when the repo has no remotes or `full: true`;
   - `changes.patch`: `git diff --binary HEAD` (staged + unstaged);
   - `untracked/…`: untracked, non-ignored files, skipping symlinks, files > 5 MiB and secret-looking files (`.env*`, keys, credential files — listed in the manifest so the recipient brings their own);
   - `transcript.jsonl`: the harness transcript with each line passed through `vk-redact` (count reported).
   The manifest (skipped secrets, redactions) travels with the offer and is what the receiver reviews.
2. **Transfer**: gateway to gateway (below): `handoff.offer`, binary `handoff.write` chunks, `handoff.commit`. The destination verifies size and sha256 at the commit. Bundles live in the gateways' state dirs (0700) while in flight.
3. **Delivery** (destination): at `handoff.commit` the gateway verifies size and sha256 and hands the bundle to its server with `handoff.incoming.add {path, manifest, sha256, from: {host, owner: self|teammate, device}}` (gateway clients only; `owner` is `teammate` for a teammate's host, `self` for the owner's own; `device` is the authenticated peer device that carried it). The server links or copies the bundle to `<server state dir>/handoffs/in/<id>.tar.zst` (directory 0700), checks the checksum and that the manifest is the one inside, and keeps an **incoming handoff** record; the gateway then deletes its copy. Result `{incoming: <id>, state, result}`. Delivering the same bundle again returns the same record, and so does a bundle whose manifest names the same `source_job` from the same sender (the sending gateway's job exported again after a restart), even when that record was declined or imported (it is returned as is, so a declined handoff never comes back). The server limits bundles kept waiting (pending, failed or importing): `[handoff] max_waiting_per_sender` (default 5, counted per authenticated sending device, else host name) and `max_waiting_bytes` (default 1 GiB across all senders); beyond either, `handoff.incoming.add` is `rate_limited {reason: quota, limit: per_sender|bytes}` and the gateway fails the delivery with "the recipient has too many waiting handoffs". Delivery never places the work: the handoff waits for the receiver (step 4), or the server's automatic import policy decides.
4. **Incoming handoffs** (server, full scope; exposed by the gateway to full-scope devices): records `{id, from, manifest summary, size, bundle_path, state: pending|importing|imported|failed|declined, error, result, created_at_ms, updated_at_ms, expires_at_ms}` persist in the server store for **7 days**; a sweep at start and every hour removes expired records with their bundles, the bundles of imported and declined records, and files no record refers to. `handoff.incoming.list`, `handoff.incoming.get {id}` (adds `suggested: {repos, repo, worktree_path, branch}`: matching clones, the remembered or default worktree path `<parent>/<repo>-handoff-<branch>`, `handoff/<branch>`), events `handoff.incoming` (new), `handoff.updated` (state or phase `cloning|importing|starting`) and `handoff.expired`.
   - **Automatic import** happens only when all hold: `from.owner == self`; a clone of the origin is known (the repository of an open workspace, or one a handoff was imported into before, any remote matching); a placement is remembered for that origin (`prefs` `handoff.placement`: origin → repository and worktree parent); and `handoff.always_ask` is off (`handoff.prefs {always_ask?}`). It imports at `<remembered parent>/<repo>-handoff-<branch>` and starts the agent. Otherwise the record stays `pending` and the server notifies "Incoming handoff from <host>: <branch>".
5. **Accept** (receiver): `handoff.accept {id, repo: {path} | {clone_to}, worktree_path?, branch?, start_agent?: true, trust?: [mise, direnv]}` →
   - `{path}`: a git repository (a linked worktree resolves to its main checkout) with **any** remote matching the manifest `origin`, else `conflict` `repo_mismatch` with details `{origin, remotes}`; `{clone_to}`: an absolute path that does not exist (or an empty directory) whose parent exists, cloned from `origin` with the receiver's own credentials (long timeout);
   - the transactional import (unpack, verify, fetch the bundle — fetching `origin` first if the thin bundle's prerequisites are missing — `worktree add` on a new branch, apply `changes.patch`, write untracked files; rolled back on failure; untracked files that could not be written are reported in `not_written`); defaults: worktree `<repo>-handoff-<branch>` next to the repository and branch `handoff/<branch>` (`-2`, `-3`, … when taken), chosen ones must not exist;
   - transcript: rewrite the source cwd to the new worktree path and install it where the harness resumes from (Claude: `$CLAUDE_CONFIG_DIR|~/.claude/projects/<cwd with non-alphanumerics as '-'>/<session>.jsonl`; Codex: `$CODEX_HOME|~/.codex/sessions/YYYY/MM/DD/<original file name>`); other harnesses get a fresh agent with a handoff prompt;
   - `trust`: `mise trust <dir>` / `direnv allow <dir>` in the worktree (and the agent's cwd) only when the tool's files exist there and the binary is on `PATH`, outcomes in `result.trust`;
   - remembers the repository and the worktree's parent for the origin, opens a workspace on the agent's cwd and, with `start_agent` and a known harness, `agent.start {pane, harness, args: resume args rebuilt on this host, prompt: "This session was handed off from <host>…"}`.
   The record becomes `imported` with `result {repo, cloned, worktree, branch, cwd, resumed, resume_args, not_written, skipped, trust, workspace, pane, run?, agent_error?, workspace_error?}` and its bundle is deleted; a failure makes it `failed` with the error and keeps the bundle, so accepting again retries. Accepting an imported record returns it unchanged; one being imported answers `conflict`. `handoff.decline {id}` drops the bundle (`declined`); `handoff.resume {id}` runs `agent.start` again in the imported pane with the stored resume arguments (`{incoming, run?, agent_error?}`).
6. The source keeps its pane and branch untouched; nothing is deleted.

**Gateway to gateway** (peers of §15.3). The source gateway delivers straight to the destination gateway:

- **Jobs (source server):** `handoff.send {pane, peer, interrupt?: false}` (full scope, never from a pane) → `{job}` records `{id, pane, peer, peer_name, interrupt, state: queued|exporting|sending|delivered|failed|cancelled, sent, total, incoming?, incoming_state?, error?, created_at, updated_at}` and emits `handoff.job` (data: the record) on every change. `handoff.jobs` lists them (finished ones for 7 days), `handoff.cancel {id}` stops one. Only the gateway reports (`handoff.job.update {id, state?, sent?, total?, incoming?, incoming_state?, error?}`, checked transitions; a finished or cancelled job answers `conflict`, which stops the gateway). The server can't read `peers.json`, so the gateway publishes its peers without keys or addresses (`handoff.peers.set`, at start, on change and every 30 s); `handoff.peers` lists them for clients. Apps reach all four through the gateway (full-scope devices); CLI `vibeke handoff send [--pane P] [--interrupt] <peer>` (default pane: the one it runs in), `handoff jobs`, `handoff cancel <id>`, `handoff peers`.
- **Worker (source gateway):** on `handoff.job` (and at start, and every 30 s for jobs left in flight by a restart) it exports as in step 1, connects to the peer with backoff and calls on the destination:
  1. `handoff.offer {manifest, size, sha256}` → `{id, received, size}`, idempotent per (device, sha256): the same bundle gets the same id and the bytes already there; a bundle committed in the last 24 h answers `{committed: true, result}`. The worker puts its job id in the manifest (`source_job`), so a job exported again after a restart (another checksum) is recognised: a committed one answers `{committed: true, result}`, an unfinished upload of it is replaced. Limits: 2 unfinished uploads per device, 8 in all, 200 MiB.
  2. `handoff.write {id, offset}` with **1 MiB binary chunks** (≤ 4 MiB): the bytes follow the JSON request in the same encrypted message after one NUL byte (JSON text never contains a raw NUL), so nothing is base64-encoded in the channel; only `handoff.write` may carry a payload, and `data_b64` is still accepted. `offset` must be ≤ `received`; a lower offset truncates and rewrites.
  3. `handoff.commit {id}`: the destination checks size and sha256 (off the async threads), then calls its server's `handoff.incoming.add {path, manifest, sha256, from: {host, owner: self|teammate, user?, device}}` (owner and user from the peer device's pairing) and removes its own copy. Result: `{incoming: <id>, state, result}` from the server's record; committing again returns the same. The commit runs on its own task, so a request dropped mid-way (its connection closed) still completes and `handoff.status` reports it.
  - A dropped connection is redialed with backoff; `handoff.status {id}` → `{received, size, state: receiving|committing|committed, result?}` says where to continue (`not_found` after a destination restart: offer again). The job fails after 10 min without progress; cancelling discards the upload on the peer. Progress goes to the server at most 4×/s.
  - Uploads live in the destination gateway's memory and expire 24 h after their last write.
- **Sending from a pane** (09 §3.2 "Approved calls"): `handoff.send`, `handoff.cancel` and the gateway's `peer.redeem` are never callable with a pane token, so an agent can't move code to another host without the user knowing. A pane *asks* instead: `auth.approve {method: "handoff.send", params: {pane?, peer, interrupt?}, reason?}` (or `handoff.cancel {id}` for its own jobs, or `gateway.call {method: "peer.redeem", params: {link, share_user?}}`). The server validates the params as `handoff.send` would, freezes them, records the repository root, branch and HEAD, and shows the user its own summary ("Send pane %3 (repo vibeke, branch main, 12 changed files, agent: none) to marvin (your host)"; the caller's reason separately, unverified). The user approves once, always (for that pane, target pane and peer until the pane's process restarts) or denies, outside the pane: TUI chrome, app, or `vibeke auth approval <request> approve|always|deny`. Approval runs the frozen `handoff.send` once as the user; the job records `by: "pane:<id>"` and `expect {repo_root, branch, head, request, requested_by, approved_by}`, and the worker fails the job with `repo_moved` when the export's manifest (`source_root`, `branch`, `head`) differs, so nothing the user didn't approve is sent. CLI: inside a pane (a pane token, no elevated token) `vibeke handoff send <peer>`, `cancel <job>` and `redeem <link>` ask through `auth.approve`, print `Waiting for approval in Vibeke (prefix+shift+e, or the notice in the tab bar)…` and print the method's result once approved; Ctrl-C withdraws the request (the connection closes), `--no-wait` prints the request id, a denial exits non-zero with "denied". `send` defaults to the CLI's own pane; when that pane has no agent and its workspace has exactly one agent pane, it asks on the terminal (`Send the agent pane %2 instead? [Y/n]`, only when stdin is a terminal); `--pane` picks one. A shell pane without a run sends its repository with no conversation. Outside panes `vibeke handoff redeem <link>` is `vibeke gateway peer add <link>`.

**To a teammate:** the teammate creates a handoff invitation on *their* host (`share.create {kind: "handoff", ttl_s}`) and sends you the link. You redeem it on one of your hosts (`peer.redeem`, or `vibeke gateway peer add <link>`), which becomes a `teammate` peer of their host. An app cannot claim a handoff invitation: a `pair.claim` that is not a host introducing itself (`platform: "host"` with a `peer` object) is refused with `forbidden` ("open this invitation on one of your hosts") and the invitation stays open. Credentials, harness logins and subscriptions never travel. A teammate's handoff is **never imported or started by the sender**: it arrives as a pending incoming handoff (`owner: teammate`, never imported automatically) and the receiver accepts it, choosing the repository (a clone, a path, or a fresh clone), worktree and branch; starting the agent then is the receiver's decision.

**Import is hardened against a hostile bundle:** resume arguments are rebuilt locally from the harness and a validated session id (`[A-Za-z0-9_-]{1,128}`), never taken from the manifest; Claude transcripts are written as `<session>.jsonl` under the computed project dir, Codex transcripts only under `sessions/…/*.jsonl`; a session id that already exists on the receiver gets a fresh one, rewritten in the transcript (Claude `sessionId`, Codex `session_meta` `payload.id` and the rollout's file name) and in the resume arguments; the worktree directory and branch are claimed atomically (`mkdir`, `git branch`) so concurrent imports never take or roll back each other's; the manifest cwd must be a relative path inside the worktree; untracked files are created with `create_new` and no symlink in any path component (the patch may have created symlinks); tar entries must be regular files with safe relative paths and bounded total size; manifest text shown to people or agents is stripped of control characters and truncated.

**Move to self:** the same with two of your own hosts paired as peers (`peer.invite` / `peer.redeem`); `handoff.peers` lists the destinations.

### 15.3 Peers: host-to-host trust

A gateway can pair with another gateway as a client (`vk-gateway/src/peer_client.rs`): the same `hello {mode: "pair"}` + Noise IKpsk2 + `pair.claim` as an app, then authenticated IK connections and JSON-RPC calls over `/v1/connect` (or the gateway's local socket when the link's relay is `local:<path>`). The relay needs no changes. This is the transport for gateway-to-gateway handoff delivery.

- **Destination side:** a new device kind `peer` (another host). It may call only `hello`, `ping` and `handoff.offer/status/write/commit/discard`. The device record carries `peer {owner: self|teammate, host_name, user?: {name, email}}`; the identity comes from the `peer` object of the host's `pair.claim {name, platform: "host", peer: {host_name, user?}}`.
  - `peer.invite {ttl_s?}` (full scope, own devices only) → `{link, pid, open_by}`: a bearer pairing with `share.kind = "peer"`, open for 15 min (`ttl_s` 60–3600). The resulting device has owner `self` and **no expiry**.
  - A teammate's handoff invitation (`share.create {kind: "handoff"}`) redeemed by a host makes a `peer` device with owner `teammate` and the invitation's expiry. The server enforces what a teammate may do from the incoming record's `from.owner` (never imported automatically, never started by the sender); the gateway offers a peer no placement parameters at all. The retired `handoff` device kind (an app that had redeemed such an invitation, when apps carried bundles) is removed from `devices.json` when the gateway loads it, with an audit entry `device.pruned` (`reason: legacy_handoff_device`).
- **Source side:** `peers.json` in the gateway state dir (0600, atomic) holds `{id, name, relay, host, host_key, device_key (private), device_id, owner, added_at, expires_at?}`.
  - `peer.redeem {link, share_user?}` (full scope) pairs this host with the link's host and saves the record (replacing an earlier one for the same host). Only `peer` and handoff invitations are accepted; `share_user` sends `git config --global user.name/email`. Result `{peer}` without the key.
  - `peer.list` → `{peers}` (no keys); `peer.remove {id}` (id or name).
  - CLI: `vibeke gateway peer invite [--ttl 15]` (destination), `peer add <link> [--share-user]`, `peer list`, `peer remove <id|name>`.

### 15.4 Managing invitations

- `share.list` (full scope) → `{invitations: [{id (pid), kind, scope, label, limit, created, link_expires_at, device_expires_at}], devices: [{id, kind, name, scope, owner, sender, expires_at, limit}]}`: unused pairing links still open, and the share and peer devices invitations produced.
- `share.revoke {id}`: cancels a pending invitation (audit `invitation.cancelled`) or revokes an invited device (audit `device.revoked`, live connections closed). It never touches the owner's own `device`s (`devices.revoke` does).
- CLI: `vibeke gateway invites` lists both with kind, expiry, limit and owner; `vibeke gateway revoke <id>` cancels an invitation or revokes a device and writes the same audit entry; `vibeke gateway devices` shows kind, expiry and limit columns.
- Expired devices are removed from `devices.json` at startup and by the 5 s sweep (audit `device.expired`).
- The app pairs each share/handoff invitation with **its own key** (keystore `invite_static:<pid>`, kept on the host record), so the host never confuses it with the device's own pairing (re-pairing the same key still replaces the old record). The app keeps one record per host, so it does not offer to accept an invitation for a host it already has full access to.

### 15.5 The server bridge (`gateway.call`)

`peer.*` and `share.*` live in the host's gateway, which a TUI attached to a remote machine cannot reach (the CLI reads the gateway state directory itself). One generic bridge through the server carries them:

- `gateway.call {method, params?, timeout_ms? = 30000}` (full scope, never from a pane) runs `method` in the gateway and returns its result. Allowed: `peer.invite`, `peer.redeem`, `peer.list`, `peer.remove`, `share.create` (kinds `handoff` and `peer` only), `share.list`, `share.revoke`; anything else is `invalid_params`. The gateway checks the list again.
- The server stores a pending request and emits the transient event `gateway.request {id, method, params, client}` (seq 0, never stored or replayed), delivered only to the event stream of the one gateway it is addressed to, since the params can hold an invitation link. The gateway runs the request through the same code as its app API, as the host owner (`by: "tui"` in `audit.log`), and answers with `gateway.reply {id, result | error: {kind, message, details?}}`. `gateway.reply` is accepted only from gateway clients, like `handoff.job.update`. Error kinds the server has keep their kind (`forbidden` becomes `permission_denied`).
- No gateway connected, no answer within `timeout_ms`, or the gateway's stream ending first: `remote_unavailable`, "the gateway isn't running: start it with `vibeke gateway run`". Pending entries are removed on each of these.
- `gateway.status {}` returns `{connected: bool}`: a gateway is connected while its event stream is open.
- Server code that needs the gateway (the pairing step of `auth.approve` calls `peer.redeem`) uses `vk_server::gateway_bridge::call(server, method, params, timeout)`, which does the same allow-list check and round trip.

## 16. Desktop app (G4, build now)

`web/apps/desktop` (`@vibeke/desktop`): an Electron shell over `@vibeke/core` + `@vibeke/ui`. It must feel native on macOS first (Linux and Windows build and run; polish follows), and it adds what a phone can't do well.

### 16.1 Shell and platform

- **Main / preload / renderer split.** The renderer is the shared UI with a `Platform` implemented over a narrow preload bridge (`contextBridge`). The main process owns keys, sockets, notifications, tray, shortcuts, deep links, windows and updates. `contextIsolation`, `sandbox`, no `nodeIntegration`, strict CSP, all navigation and `window.open` denied except the bundled app, every IPC handler validates `event.senderFrame` against the app's own origin and validates arguments.
- **Keys:** device keys and host records encrypted with `safeStorage` in the app's user-data dir (0600); on Linux the `basic_text` backend is refused (the app asks the user to set up a keyring). Atomic get-or-create (§14.1).
- **Transports:** `RelayTransport` (WebSocket from the main process, so connections survive a closed window) and `LocalTransport` (WebSocket over the gateway's Unix socket `<gateway state dir>/gateway.sock`, same Noise channel, §9.3). "Connect to this Mac" runs `vibeke gateway pair --local` (or the state dir's socket when the CLI isn't on `PATH`) and pairs over the local socket in one click.
- **Background:** closing the window keeps the app (and its connections) running in the menu bar / tray; quitting is explicit. Optional start at login.

### 16.2 Desktop-native features

- **Menu-bar quick approvals:** a tray/menu-bar icon with the open-interaction count (template icon on macOS, dock badge too) opens a compact popover window: the inbox cards with Allow / Deny / Allow always, batch groups, keyboard navigation, without opening the main window.
- **Native notifications** from the main process for new interactions and finished agents (privacy level as on the phone, §7.8), grouped per host. Clicking opens the card; on macOS, actions *Approve…* / *Open* (approve still shows a confirm for high/unknown risk; low/medium approve in place with the app's confirm sheet in the popover). Suppressed while the main window is focused. DND respected.
- **Global shortcut** (default `⌥⌘V`, configurable) toggles the quick-approval popover; in-app **command palette** (`⌘K`): jump to any host/workspace/pane, run actions (new agent, share, hand off, pair, settings).
- **Keyboard-first:** `j`/`k` move through inbox cards and pane lists, `a` allow, `d` deny, `A` allow always, `Enter` open, `Esc` back, `⌘1–4` switch tabs, `/` find; shown in a `?` cheat sheet.
- **Multi-window:** pop a pane out into its own window (terminal mirror + composer + keys); window positions persisted.
- **Deep links:** `vibeke://pair?d=…` (also `https://<app origin>/#/pair?d=…` opened from the OS) routes to the pairing screen; single-instance lock forwards links to the running app.
- **Native menus:** standard app/edit/view/window menus with accelerators; macOS vibrancy sidebar, traffic-light inset title bar, system accent colour and dark/light following the OS.
- **Updates:** `electron-updater` wired to a release feed, off unless a feed is configured; code signing / notarization configuration in `electron-builder` with secrets from the environment.

### 16.3 Quality bar

- Cold start to interactive < 1 s on Apple silicon (measured 2.2–3.4 s including Playwright attach; not yet met); memory with three hosts connected < 250 MB **physical footprint** (Activity Monitor's figure; measured ~170 MB with the main window, ~200 MB with the popover too, 80 MB with only the menu-bar item) — working set counts shared framework pages per process and sits at 300–400 MB for any Electron app; no work while hidden beyond open sockets and the event stream (display timers pause; hidden popovers are destroyed after 60 s and the main renderer after 10 min closed).
- Accessibility: full keyboard reachability, focus rings, VoiceOver labels on all controls, reduced-motion respected.
- Tests: unit tests for the platform layer (IPC validation, key storage, transports), a Playwright-for-Electron smoke test (launch → pair over the local socket against a real gateway → inbox → approve) and `electron-builder --dir` packaging in CI.

## Appendix A — Design notes (non-binding)

### A.1 Accounts and the SaaS (G5)

- **Separation:** account auth decides who may use a relay and how much; end-to-end device keys decide which device may talk to which host. The relay never sees the second.
- **Flow:** `vibeke login` (OAuth device-code; refresh token in the OS keychain) → the gateway registers its relay public key under the account → short-lived host tokens for `/v1/host`. Devices need no account: at pairing the host issues a **device ticket** `{host, device_pub, scope, exp}` signed by the host relay key; the relay verifies it on `/v1/connect` and bills the host's account. Revocation: tickets expire in 24 h and the host pushes a revocation list.
- **Metering:** per account hosts, devices, concurrent connections, bytes/s and GB/month, native pushes/day; counters batched to a control plane (Postgres + Stripe), never on the frame path.
- **Tiers (sketch):** Free (self-hosted or tight hosted limits) · Pro (limits, native push, preview links, sync) · Team (shared inbox, SSO, audit) · usage add-ons.

### A.2 Native push proxy

Native apps receive pushes only via the vendor's APNs/FCM credentials, so the service runs a proxy: the host posts `{device push handle, padded ciphertext}`; the app's Notification Service Extension decrypts with the device key. The proxy sees "wake device X" and a padded length.

### A.3 Share and handoff

Specified and built in §15. Later: offline delivery through an encrypted relay mailbox (HPKE to the recipient host key) when the destination is not online.

### A.4 Zero-knowledge services (G7)

- Key hierarchy: user key wrapped to each device; teams as MLS groups (RFC 9420, `openmls`); key directory with fingerprint verification or admin-signed member keys; recovery via printed key or passkey-PRF wrapping.
- Sync/history: client-side encrypted, keyed-hash-addressed blobs; search and dashboards on clients.
- Team inbox: MLS-encrypted interaction events; host is the authority on the first valid signed answer.
- Audit: device-signed hash chain stored opaquely.
- Preview links: `https://p.<domain>/<id>#k=<key>` with a service worker tunnelling requests end-to-end to the host; the fragment never reaches the server.
- Exceptions are labelled and opt-in.

### A.5 Direct paths (G8)

LAN (mDNS for Electron, remembered LAN URL for the PWA) and hole punching: both sides learn their public address from the relay, exchange candidates over it, and send simultaneously so each NAT treats the other's packet as a reply; symmetric NATs fall back to the relay. Browsers via WebRTC data channels, native via QUIC (iroh). The same Noise channel runs over every path.

### A.6 Not built here

- **Integrations (Slack/Teams/GitHub):** would run from the host with the user's own tokens; inbound buttons carry a host-MAC'd token so the service cannot forge an approval; the third party sees what is sent to it.
- **Hosted runners:** BYO cloud account, confidential VMs with attestation, or an explicit "runner sees your code" label.
