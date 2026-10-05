# 06 — Remote machines and the preview fabric

Two promises:
1. **Remote panes feel local.** Latency, clipboard, images and notifications behave as if the pane were on the laptop.
2. **Remote dev servers are local.** A Vite or Next server started by an agent on a devbox can be opened, screenshotted and inspected from the laptop with zero manual port forwarding. The agent on the devbox can screenshot its own work and the human sees the same image.

Crates: `vk-remote` (machines, bootstrap, bridge, transports, forwarding) and `vk-preview` (discovery, proxy, browser service, screenshots). Milestones: remote **M3**, preview fabric **M4**, QUIC roaming **M5**.

---

## Part A — Remote machines

### A1. Model

- A **Machine** (see [02](02-data-model-and-event-log.md)) is a host running its own Vibeke server.
- The **local server** is the hub: it connects out to remote servers and **federates** them. That means projections, event subscriptions, render streams and API calls, all addressed by machine.
- Remote servers are full peers. They keep running, and their panes keep living, when the laptop disconnects.
- Local clients (TUI, CLI) talk only to the local server. The local server proxies to remotes.
- `vibeke attach --machine devbox` without a local server is also allowed: the CLI spawns an ad-hoc bridge directly. This is for one-off use on a borrowed machine.

```
 laptop                                                 devbox
┌──────────┐   unix   ┌──────────────┐  ssh -T (stdio)  ┌─────────────┐  unix  ┌──────────────┐
│ TUI/CLI  │◄────────►│ local server │◄════════════════►│vibeke bridge│◄──────►│ remote server│──► holders
└──────────┘          └──────────────┘   mux frames     └─────────────┘        └──────────────┘
                              │  ▲                                                   │
                              ▼  │ forwarded TCP streams (previews)                 ▼
                       local proxy :47xx  ◄══════════ same link ══════════►  127.0.0.1:5173 (vite)
```

### A2. Saved machines

```
vibeke machine add devbox demo@devbox.tail1234.ts.net [--port 22] [--identity ~/.ssh/id_ed25519]
                  [--jump bastion] [--session default] [--transport ssh|quic] [--auto-connect]
vibeke machine list | show devbox | connect devbox | disconnect devbox | rm devbox
vibeke machine upgrade devbox        # install/upgrade remote vibeke to the local version
vibeke machine doctor devbox
```

- Stored in `config.toml [[machines]]` (label, ssh target, options) and mirrored as `machine.added` events.
- SSH options come from the user's `~/.ssh/config`, because we invoke the system `ssh` binary. That keeps ProxyJump, ControlMaster, agent forwarding, 1Password/Secretive agents and Tailscale SSH all working with no reimplementation.
- We pass `-o ServerAliveInterval=15 -o ServerAliveCountMax=3`, plus `-o ControlMaster=auto -o ControlPersist=60 -o ControlPath=$RUNTIME/ssh-%C` unless the user disables it.

### A3. Bootstrap (no sudo)

```
local server                                         remote (via ssh)
     │ ssh devbox 'sh -s' < probe.sh ───────────────────►│ uname -sm; echo $HOME; command -v vibeke;
     │◄──────────────── {os, arch, home, vibeke_path, vibeke_version, libc}
     │ if missing or version != local (major.minor):
     │   ensure release artifact for (os,arch,libc) in local cache (~/.cache/vibeke/releases)
     │   (download from release CDN, verify sha256 + minisign signature)
     │ ssh devbox 'mkdir -p ~/.local/share/vibeke/versions/<v> && cat > …/vibeke.tmp' < artifact
     │ ssh devbox 'cd … && echo <sha256>  vibeke.tmp | sha256sum -c && mv vibeke.tmp vibeke
     │             && ln -sfn versions/<v> ~/.local/share/vibeke/current'
     │ ssh -T devbox '~/.local/share/vibeke/current/vibeke bridge --session default --proto 1'
     │◄═══════════════ mux link up (Hello{version, caps}) ═══════════════►
```

- The remote binary path is `~/.local/share/vibeke/current/vibeke`, which `vibeke` on PATH links to if the user installed it. **Never sudo, never touch system paths.**
- The artifact is pushed from the laptop, so an air-gapped devbox works. With `machines.<m>.download = "remote"` the remote fetches it itself instead.
- Version policy:
  - The bridge protocol is compatible within a major.
  - With a minor mismatch, Vibeke offers to upgrade and only does it automatically if `auto_upgrade = true`.
  - Upgrading a remote never kills remote panes, because holders survive the server restart ([01](01-architecture.md) §1.2).
  - Old versions are pruned, keeping the last 2.
- If the remote server isn't running, `vibeke bridge` spawns it (daemonized). The bridge is just a stdio↔unix-socket multiplexer and holds no state.

### A4. Bridge link protocol

One SSH stdio stream carries many logical channels.

- **Frame**: `u32 len | u8 type | u32 channel_id | payload`, in postcard encoding. Types are `Open{kind, params}`, `Data`, `Close{reason}`, `WindowUpdate{bytes}`, `Ping/Pong{ts}`.
- **Channel kinds**:
  - `control`: JSON-RPC, exactly as on the local socket.
  - `render`: the render stream for one client.
  - `events`: an event subscription.
  - `tcp_forward{host, port}`: preview forwarding, opened by the local side.
  - `blob_put` / `blob_get`: images, screenshots, uploads.
  - `holder_direct`: reserved.
- **Flow control**: per-channel credit windows (default 256 KiB), so a bulk screenshot or blob transfer can't starve keystrokes.
- **Priorities**: control/input > render > events > forwards > blobs. Implemented as a weighted scheduler on the writer task.
- **Keepalive**: Ping every 5 s. RTT is measured continuously and feeds adaptive frame pacing (A7) and the status bar latency indicator.
- **Compression**: zstd per frame for `render` and `blob` channels when the link is not loopback, with a dictionary trained on terminal frames.

### A5. Unified multi-machine view

- **Sidebar**: machines appear as top-level collapsible sections (`● laptop`, `● devbox 23ms`, `○ gpu-box offline`). Workspaces nest under them, and groups nest under machines.
- **Keyboard navigation**: spans machines in sidebar order. Switching to a remote workspace is instant from cached projections, and the render stream attaches in the background with a "connecting…" overlay.
- **Combined agent list** (`prefix+a` popup / `vibeke agent list --all-machines`): every run across machines, sorted by attention: needs_approval > needs_answer > error > done > working > idle.
- **Notifications** from remote agents are delivered on the laptop with a machine badge.
- **Handles**: fully qualified `devbox/w3:p5`. The short form `w3:p5` resolves against the focused machine in the TUI and against `--machine` or the local machine in the CLI.

### A6. CLI forwarding

- `vibeke --machine devbox <any command>` sends the JSON-RPC call to devbox through the local server's link. If no local server is running, it uses an ad-hoc bridge.
- Errors are returned verbatim with `machine` added.
- **No fallback to local on failure**, because silently acting on the wrong machine is the worst outcome.
- Agents running *on* devbox use their local `vibeke` normally. Cross-machine orchestration from an agent requires `--machine`, and the plugin/agent capability `remote.control`, which is granted by default to user-started panes and denied to plugin panes.

### A7. Reconnection, offline, bandwidth

**Link states**: `connected → degraded (RTT > 400 ms or loss) → reconnecting → offline`.

- **Reconnect**: exponential backoff from 0.5 s to a 30 s cap, with jitter. Re-establishing is cheap: resubscribe events with `after_seq` (no loss), reattach render with `from_frame=0` (a full-state frame), and reopen forwards.
- **While offline**: remote workspaces stay in the sidebar with their last-known state, greyed out with "last seen 4m ago".
  - Input to an offline pane is **not buffered** by default, since a stale buffered Enter is dangerous. The user sees "offline — input not sent".
  - API calls to the machine fail fast with `machine_offline`.
- **The remote keeps working**: agents continue, events accumulate in its log, and notifications are replayed on reconnect, collapsed into one per pane: "While you were away: 2 agents finished, 1 needs approval".
- **Adaptive frame pacing**:
  - Target refresh rate is `min(client_hz, 1000 / (RTT/2 + 8ms))`.
  - Unfocused remote panes update at ≤ 4 Hz. Spinner-only damage (a single-cell change matching the harness spinner manifest) is rate-limited to 1 Hz on unfocused panes and 8 Hz on focused ones.
  - The scroll region and erase operations are encoded as ops (`ScrollUp{n}`, `Clear{rect}`), not raw cell rewrites.
- **Bandwidth budgets** (CI-enforced with a fixture of 10 panes running, one being an agent with a spinner):

| Scenario | Budget |
|---|---|
| Idle, 30 panes, 1 spinner | < 2 KB/s |
| Agent streaming text in focused pane | < 30 KB/s |
| `cat` of a 10 MB log in focused pane | frames dropped to state-sync, < 2 MB total |
| 1 hour with one animating unfocused pane | < 20 MB |

### A8. QUIC roaming transport and predictive echo (M5)

Users ask for mosh. We provide mosh-like behaviour without giving up the rest of the protocol:

- `transport = "quic"`: SSH is still used to bootstrap and authenticate. It runs `vibeke bridge --quic-listen` on the remote, which picks a UDP port, generates an ephemeral keypair and prints `{port, cert_fingerprint, token}` over the SSH channel. The laptop then dials QUIC directly (`quinn`), pins the fingerprint, presents the token, and closes the SSH session.
- **Connection migration**: if the laptop changes network (Wi-Fi → tether), QUIC migrates and the session continues. There's no reconnect, no lost frames, and the render stream is state-sync so nothing has to replay.
- **Predictive local echo** (mosh SSP-style), only in panes whose foreground process is a shell or line editor, or an agent prompt input when the harness manifest marks it `echo_predictable`:
  - Printable keys are drawn immediately, underlined, at the predicted cursor position.
  - When the server's frame confirms them, the underline is removed. A misprediction is rolled back within one frame.
  - Disabled when RTT < 30 ms, and inside alternate-screen apps unless allowlisted.
- Fallback: if UDP is blocked, Vibeke silently falls back to SSH stdio and shows `ssh` in the status bar.

### A9. Clipboard

- **Copy** in a remote pane: OSC 52 from a remote program goes into the remote VT engine. The remote server sends a `clipboard.set` render event over the link. The local TUI client writes OSC 52 to the host terminal, or uses `pbcopy`/`wl-copy` when the terminal doesn't support OSC 52 (capability probe at attach).
- Copy mode / copy-on-select of remote pane content is handled locally by the TUI, which already has the cells.
- **Paste**: bracketed paste is forwarded as an input event. Large pastes (> 64 KiB) are chunked via the blob channel and injected by the remote server, so the control channel stays responsive.
- Policy: `clipboard.allow_remote_set = "ask"|"always"|"never"`, default `always` for user machines. Plugin-originated OSC 52 is subject to the plugin capability `clipboard.write`.

### A10. Images: local → remote (paste/attach) and remote → local (display)

**Local → remote (paste screenshot into a remote agent):**
```
user ctrl+v (image on local clipboard)
TUI: read clipboard image (NSPasteboard / wl-paste / xclip) ─► blob_put over link ─► remote blob store
remote server: path = ~/.local/state/vibeke/<s>/uploads/<hash>.png
   ├─ structured harness (pi/omp RPC, Codex app-server, ACP): send as image content in the next prompt
   │     (pi: prompt{images:[{type:"image",data,mimeType}]}; codex: localImage input item)
   └─ TUI harness (claude, codex TUI): bracketed-paste the file path (Claude/Codex accept image paths)
emit agent.item{kind:user_message, attachments:[blob]}
```
- Keybinding `remote_image_paste = "ctrl+v"` is active only when the clipboard holds an image and the focused pane is remote; otherwise ctrl+v passes through. Also available as `vibeke attach-file <path> --pane devbox/w3:p5`, which accepts any file type and uploads it.

**Remote → local (show an image produced remotely):**
- **Kitty graphics** emitted by a remote program are parsed by the remote VT engine (when the engine supports it, an M0 criterion). Images are stored as blobs and placements are sent in render frames by hash. The local client fetches each blob once (cached) and re-emits kitty graphics to the host terminal, or falls back to a `[image 1280×720 — prefix+i to open]` placeholder.
- Screenshots and other agent artifacts use the same blob path (B6).

---

## Part B — Preview fabric

### B1. Goals

- Discover every HTTP service an agent or user starts, on any machine, with no configuration.
- Reach it from the laptop browser in one keystroke, with an origin that is unique per preview, so cookies and localStorage of task A and task B don't mix.
- HMR, WebSockets, SSE and HTTPS dev servers just work.
- Screenshots, console logs, network errors and DOM snapshots are available to the **human** (TUI, CLI) and to **agents** (CLI + MCP), captured **next to the server**, so no bandwidth is spent streaming a browser.
- Everything stays on loopback. No LAN exposure, and authenticated even on loopback.

### B2. Discovery

Three sources, merged into `Preview` records (02):

1. **Process-tree listeners** (authoritative for "is it up"):
   - Every 2 s while the pane has a non-shell foreground process, plus immediately on `pane.process_changed`, list listening TCP sockets owned by any process in the pane's process tree (holder child pgid/session).
     - macOS: `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` via libproc.
     - Linux: `/proc/<pid>/fd` → socket inodes joined with `/proc/net/tcp{,6}` (state `0A` LISTEN).
   - Ports are filtered to `127.0.0.1`, `::1`, `0.0.0.0` or `::`.
   - Each new port gets an HTTP probe (`HEAD /` with a 1 s timeout, then `GET /` if needed). It is recorded as a Preview if it answers HTTP or HTTPS (TLS ClientHello probe). Non-HTTP ports such as Postgres are recorded as `kind=tcp` and hidden by default.
   - Process cmdline gives a label: `vite`, `next dev`, `storybook`, `rails s`, `uvicorn`.
2. **Output URL detection**:
   - The VT engine's line feed hook scans new lines for `https?://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\]|<hostname>)(:\d+)?(/\S*)?`, and also OSC 8 hyperlinks.
   - Known banners raise confidence and supply the path, e.g. `➜  Local:   http://localhost:5173/` and `- Local: http://localhost:3000`.
   - A URL whose port is also in the listener set merges into that Preview, adding `path` and label. A URL whose port isn't listening yet becomes a `pending` Preview, confirmed when the listener appears.
3. **Declared**:
   - `vibeke preview declare --port 5173 [--path /dashboard] [--label web] [--pane $VIBEKE_PANE_ID]`, the API `preview.declare`, and task config `[previews] web = { port_env = "PORT", path = "/" }`.
   - Agents can declare too. The MCP tool `preview_declare` lets an agent say "the admin UI is on :8080/admin".

**Lifecycle**: `discovered/declared → up ⇄ down → gone`. A preview goes `gone` when the listener has been absent for 60 s and its pane process has changed, or when its task is removed. Previews are scoped to `(machine, pane, task)`, so the same port on two machines gives two distinct previews.

### B3. Forwarding and the local reverse proxy

```
browser ──http://v4-fix-login.vibeke.localhost:47800/──► local preview proxy (127.0.0.1:47800)
   proxy: Host → preview v4 → machine devbox, target 127.0.0.1:5173
   ├─ local machine: direct TCP connect
   └─ remote: open tcp_forward{127.0.0.1, 5173} channel on the link ─► remote bridge connects locally
   rewrite request: Host: localhost:5173, Origin/Referer → http://localhost:5173 (if same-origin)
   stream response; rewrite Location / Set-Cookie Domain / absolute self-URLs in redirects
```

- **One proxy listener** per local server, `127.0.0.1:<preview.port>` (default 47800, falling back to the next free port) and `[::1]`. The preview is selected by Host header.
- **Per-preview origin**: `http://<handle>-<slug>.vibeke.localhost:47800`, e.g. `v4-fix-login.vibeke.localhost`.
  - Browsers (Chrome, Firefox, Safari ≥ 17) resolve `*.localhost` to loopback without DNS, and treat `http://*.localhost` as a **secure context**, so service workers, `crypto.subtle` and clipboard APIs work.
  - Each preview is its own origin, which isolates cookies, localStorage and service workers between tasks.
- **Stable URLs**: the same preview keeps its URL across server restarts and reconnects, since the handle is stable. `vibeke preview url v4` prints it.
- **Request rewriting** (`preview.rewrite = "auto"`):
  - `Host` becomes upstream `localhost:<port>`, because Vite and Next check `Host` (Vite `server.allowedHosts`).
  - `Origin` and `Referer` are rewritten to the upstream origin when they match the preview origin.
  - In responses, these are mapped from upstream origin back to preview origin:
    - `Location` headers;
    - `Set-Cookie` (strip `Domain=localhost`);
    - `Access-Control-Allow-Origin`.
  - Bodies are never rewritten by default; `rewrite_html = true` is opt-in for apps that hard-code absolute `http://localhost:3000` URLs.
- **WebSocket / HMR**: `Upgrade` requests are proxied with the same Host/Origin rewrite. Two quirks:
  - **Vite** HMR connects to `location.host` by default, which works through the proxy. If the project hard-codes `server.hmr.clientPort`/`host`, the WS goes to `localhost:<port>` on the laptop, so Vibeke **also binds a local mirror listener** on the same port number when it is free locally. That makes `http://localhost:5173` work as a plain fallback alias too: "mirror mode", on by default for remote previews.
  - **Next.js** `/_next/webpack-hmr` and Turbopack HMR WS go over the same origin and work as-is.
  - Webpack-dev-server `client.webSocketURL` with an explicit port is covered by mirror mode.
- **SSE / streaming**: no buffering, with `Transfer-Encoding: chunked` and `text/event-stream` passed through. Idle timeouts are disabled for streaming responses.
- **HTTPS dev servers** (Vite `--https`, `next dev --experimental-https`): the proxy connects upstream over TLS with verification **disabled for loopback upstreams only** and presents plain http on the `*.localhost` origin (a secure context anyway). If an app requires `https:` in `location.protocol`, `preview.tls_origin = true` serves `https://…vibeke.localhost:47843` with a locally generated CA (installed with an explicit `vibeke preview trust-ca`, never silently).
- **Auth on loopback**: other local users and malicious web pages (DNS rebinding, `fetch('http://127.0.0.1:47800')`) must not be able to reach previews.
  - The proxy requires the `Host` to be a known `*.vibeke.localhost` name. This defeats rebinding, since an attacker's page can't set that Host.
  - The first navigation must carry a `?vk_token=` (one-time, 60 s), which the proxy exchanges for a `HttpOnly; SameSite=Strict` cookie scoped to that preview origin. `vibeke preview open` builds the URL with the token. Mirror mode (bare `localhost:<port>`) has no token and is therefore **off for local-machine previews** (pointless there) and only binds 127.0.0.1.
- **Never bound to non-loopback interfaces.** A setting to do so does not exist in Phase 1.

### B4. Opening previews

- `vibeke preview list [--machine m] [--task k7]` shows handle, label, machine, pane, port, status, URL.
- `vibeke preview open [v4|--pane …|--task …]` opens the token URL in the **client machine's** browser (`open`/`xdg-open`), even when the command is run inside a remote pane. The remote server routes an `open_url` request to the attached client that is focused on that pane, or the most recent client.
- **TUI**:
  - A preview chip appears in the pane frame/status bar (`◉ web :5173`). Click it or press `prefix+o` to open.
  - The Previews section in the sidebar under each task has open, screenshot and copy-URL actions.
  - OSC 8 / plain URLs to `localhost:<port>` printed in a **remote** pane are rewritten **at click time** to the preview URL, so clicking `http://localhost:5173` in a devbox pane opens the forwarded preview.

### B5. Browser service and screenshots

A headless browser runs **on the machine where the dev server runs**. That gives lowest latency and correct `localhost` semantics for the app, and full screenshots never need forwarding during capture.

- `vk-preview` manages one Chromium per server, launched lazily and shut down after 10 min idle. Binary resolution order:
  1. config `browser.path`;
  2. a system Chrome or Chromium;
  3. an existing Playwright cache (`~/.cache/ms-playwright`);
  4. `vibeke browser install`, which downloads a pinned Chrome-for-Testing build into `~/.cache/vibeke/browser` (with sha256 verification) on explicit request or first use, after asking.
- Driven over CDP, with one isolated **browser context per preview** (separate cookies and storage).
- **Commands** (CLI, API `browser.*`, MCP tools):

```
vibeke browser screenshot <preview|url> [--path /dashboard] [--viewport 390x844|1440x900] [--dpr 2]
                          [--full-page] [--wait load|networkidle|selector:<css>|ms:<n>]
                          [--dark] [--selector <css>] [--out file.png] [--json]
vibeke browser logs <preview> [--since 5m] [--level error]          # console + page errors
vibeke browser network <preview> [--failed] [--since 5m]            # 4xx/5xx, failed requests
vibeke browser dom <preview> [--path …] [--selector …] [--format text|html|a11y]
vibeke browser click/type/eval  <preview> …                         # minimal scripted interaction (M4 stretch)
vibeke browser diff <shotA> <shotB> [--threshold 0.1]               # visual diff
```

- **Console/network capture**: when a preview is first opened by the browser service, a CDP session subscribes to `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, `Log.entryAdded` and `Network.loadingFailed`/`responseReceived` (status ≥ 400). These go into a per-preview ring (last 500 entries). Errors emit sampled `preview.console_error` events (at most 1 per 10 s per preview).
- **Screenshots** are stored as blobs (`blake3`, PNG plus a WebP thumbnail) with metadata `{preview, url, viewport, dpr, full_page, taken_at, taken_by: user|agent(run)|plugin, run_id?, turn_id?}` and emit `preview.screenshot_captured {blob}`.
  - When taken by an agent, the screenshot is linked to the agent's current turn. This is Phase 2's evidence-bundle input.
  - Retention: last 200 per task, plus all screenshots referenced by an Interaction or evidence bundle.
- **Visual diff**: pixel diff using `pixelmatch` semantics (per-pixel YIQ distance plus an anti-aliasing heuristic), output as a diff PNG blob with `{changed_ratio, regions:[bbox]}`. `--baseline task-start` diffs against the first screenshot of the same path and viewport in this task. That's the "before/after" view.

### B6. Agents use the same browser: `vibeke mcp`

- `vibeke mcp` is a stdio MCP server, installed into harness configs by `vibeke integration install <harness> --mcp`. It's available to every agent in a Vibeke pane and scoped to that pane's task and machine via `VIBEKE_PANE_ID`.
- **Tools**:
  - `preview_list`;
  - `preview_declare {port, path?, label?}`;
  - `browser_screenshot {preview?|url?, path?, viewport?, full_page?, wait?}`. Returns an **MCP image content** (for multimodal models) plus `{blob, preview_url}` text;
  - `browser_logs {preview, since?, level?}`;
  - `browser_network {preview, failed_only?}`;
  - `browser_dom {preview, selector?, format?}`;
  - `browser_diff {a, b}`.
- Harnesses without MCP (or users who prefer CLI) can call `vibeke browser screenshot --json` and read the PNG path. pi/omp can use a small extension tool shipped in `@vibeke/pi-extension` that wraps the same API and returns `ImageContent`.
- **The human sees what the agent saw**:
  - Every agent screenshot appears in the TUI. The pane frame shows `📷 2` with a counter, `prefix+i` opens the gallery, and the latest screenshot can pop as an inline thumbnail in the agent's sidebar row (configurable).
  - These are the same blobs, and remote screenshots are fetched once over the blob channel.
- **Capability**: the `browser.*` tools only target previews on the same machine and task by default. Arbitrary external URLs need `browser.allow_external = true`. This prevents the browser being used as a general web fetcher that sidesteps the harness's own permission model.

### B7. Displaying screenshots in the TUI

- **Kitty graphics–capable host terminals** (Ghostty, Kitty, WezTerm, Konsole; iTerm2 via its own protocol): a preview popup, `prefix+i` or `vibeke preview show v4`, renders the image scaled to the popup with `←/→` to page through history, `d` for diff vs the previous one, `o` to open the preview in the browser, and `c` to copy the image to the clipboard. A **preview pane** type (a non-PTY pane) can be split into a tab and auto-refreshes when a new screenshot arrives for its preview: a live-ish view of the remote UI without a browser.
- **No graphics support**: the popup shows metadata plus `[o] open image locally`, which writes the blob to a temp file and runs `open`/`xdg-open`. Sixel is supported where the engine can emit it (M6).
- Image transmission to the host terminal uses kitty's `t=f` (file path) when the client is local to the terminal, else `t=d` chunked base64. It is cached by image id per terminal session.

### B8. Sequence: agent on devbox verifies its UI change; human reviews from laptop

```
claude (devbox w2:p1)        devbox server            link            laptop server          laptop TUI
   │ pnpm dev (PORT=20010)       │                       │                    │                     │
   │─────────── listener :20010 ►│ preview.discovered v4 │═══ event ═════════►│ sidebar chip ◉ web  │
   │ MCP browser_screenshot{v4}  │                       │                    │                     │
   │────────────────────────────►│ CDP: new ctx, goto,   │                    │                     │
   │                             │ wait networkidle, shot│                    │                     │
   │◄──── image + blob ref ──────│ blob stored;          │                    │                     │
   │                             │ preview.screenshot_   │═══ event ═════════►│ 📷 badge on w2:p1   │
   │                             │   captured(run,turn)  │                    │                     │
   │                             │                       │                    │◄── prefix+i ────────│
   │                             │◄═ blob_get(hash) ═════│◄═══════════════════│                     │
   │                             │══ png bytes ═════════►│═══════════════════►│ kitty graphics popup│
   │                             │                       │                    │◄── o (open) ────────│
   │                             │◄═ tcp_forward :20010 ═│◄── browser GET http://v4-fix-login.vibeke.localhost:47800
```

### B9. Sequence: remote preview HMR through the proxy

```
browser            local proxy :47800           link (tcp_forward)        devbox vite :20010
  │ GET / (Host v4-…)    │                               │                          │
  │─────────────────────►│ token→cookie; rewrite Host    │                          │
  │                      │══════ open fwd ══════════════►│─────── connect ─────────►│
  │◄──── index.html ─────│◄══════════════════════════════│◄─────────────────────────│
  │ WS /?token=… (vite)  │ Upgrade; rewrite Origin       │                          │
  │─────────────────────►│══════ open fwd ══════════════►│──── WS handshake ───────►│
  │◄═══ HMR updates ═════│◄══════════════════════════════│◄═════════════════════════│
```

---

## Part C — Configuration

```toml
[[machines]]
label = "devbox"
ssh = "demo@devbox.tail1234.ts.net"
transport = "ssh"          # ssh | quic (M5)
auto_connect = true
auto_upgrade = false

[remote]
input_when_offline = "drop"   # drop | ask
predictive_echo = "auto"      # auto | always | never (quic only)

[preview]
enabled = true
port = 47800
discover = { listeners = true, output_urls = true, interval = "2s" }
mirror_ports = "remote"       # remote | never | always
rewrite = "auto"
rewrite_html = false
tls_origin = false

[browser]
path = ""                     # auto
idle_shutdown = "10m"
default_viewport = "1440x900"
allow_external = false
inline_thumbnails = true

[clipboard]
allow_remote_set = "always"
```

## Part D — Acceptance criteria

**M3 (remote):**
- `vibeke machine add devbox …` followed by connect on a fresh Linux arm64 box without Vibeke: the binary is pushed and verified, the bridge comes up, and remote workspaces appear in the sidebar in < 5 s on a 50 ms link. No sudo is used, and the remote's `~` contains only `~/.local/share/vibeke`, `~/.local/state/vibeke` and `~/.config/vibeke`.
- `vibeke --machine devbox agent list` returns remote runs. With devbox unreachable it fails with `machine_offline` within 3 s and never touches local state.
- Kill the SSH connection while a remote Claude runs: the TUI shows offline/greyed out, the agent keeps working, reconnect happens automatically, events are delivered with no seq gaps, and a "while you were away" summary appears.
- Remote upgrade `vibeke machine upgrade devbox` with 5 running agents: no agent process restarts (PIDs unchanged).
- Bandwidth budgets in A7 pass in CI (netem-shaped link fixture).
- Image paste: a PNG on the laptop clipboard pasted into a remote Claude pane results in Claude receiving a valid path to the file on devbox. Pasted into a remote pi RPC run, it arrives as `images` content.
- OSC 52 copy in a remote Neovim lands in the laptop clipboard.

**M4 (preview fabric):**
- Start `pnpm dev` (Vite) in a remote task pane. A preview appears in < 3 s. `vibeke preview open` loads the app on the laptop. Editing a file on devbox triggers HMR in the laptop browser in < 500 ms (+RTT).
- Two tasks of the same Next.js app (different ports) open on different `*.vibeke.localhost` origins with independent login cookies.
- A Next dev server with `--experimental-https` previews correctly via http `*.localhost`.
- `curl http://127.0.0.1:47800/` with a forged Host and no token gets 403. A page served from another preview origin can't read responses from a second preview (verified with a cross-origin fetch test).
- An agent on devbox calls MCP `browser_screenshot` → gets an image. The laptop TUI shows a 📷 badge within 1 s, and `prefix+i` displays it via kitty graphics in Ghostty, with the open-locally fallback working in Terminal.app.
- `browser logs v4 --level error` shows a thrown exception from the page. `browser diff` between before/after screenshots outputs a changed ratio and a diff image.
- Clicking `http://localhost:5173` printed in a remote pane opens the forwarded preview URL.

**M5 (QUIC):**
- Switching Wi-Fi networks mid-session keeps the remote session attached with no visible reconnect (< 1 s stall).
- Predictive echo makes typing in a remote shell at 200 ms RTT feel local, with mispredictions rolled back.
- Blocked UDP falls back to SSH automatically.
