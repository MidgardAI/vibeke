# 06 — Remote machines and the preview fabric

Two promises:
1. **Remote panes feel local.** Latency, clipboard, images and notifications behave as if the pane were on the laptop.
2. **Remote dev servers are local.** A Vite or Next server started by an agent on a devbox (or inside a sandbox/VM, 13) opens on the laptop as `http://localhost:<port>` in a Vibeke browser profile routed through the remote — no manual port forwarding, no URL rewriting. The agent on the devbox can drive a headless browser and screenshot its own work, and the human sees the same image, labeled with its environment and commit.

Crates: `vk-remote` (machines, bootstrap, bridge, transports, forwarding) and `vk-preview` (discovery, SOCKS5 + reverse proxy, filtering proxy, browser service, screenshots). Milestones (see [11](11-milestones.md)): remote **M3**, preview fabric **M3**, QUIC roaming **post-1.0**.

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
                  local SOCKS5 / proxy  ◄══════════ same link ══════════►  127.0.0.1:5173 (vite)
```

### A2. Saved machines

```
vibeke machine add devbox demo@devbox.tail1234.ts.net [--port 22] [--identity ~/.ssh/id_ed25519]
                  [--jump bastion] [--session default] [--transport ssh|quic] [--auto-connect]
vibeke machine list | show devbox | connect devbox | disconnect devbox | rm devbox
vibeke machine upgrade devbox        # install/upgrade remote vibeke to the local version
vibeke machine doctor devbox
```

- Stored in `config.toml [[remote.machine]]` (label, address, options; schema in 08 §11) and mirrored as `machine.added` events.
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
- **Two bootstrap modes**, same verification boundary — trust is anchored in the **laptop's** signature check:
  - `bootstrap = "push"` (default): the laptop downloads (or has cached) the release artifact, verifies sha256 **and** its minisign/Sigstore signature against the key embedded in the local binary (09 §10), then streams it over SSH; the remote re-checks the sha256 before the atomic `mv`. Works for air-gapped devboxes.
  - `bootstrap = "remote-download"`: the laptop verifies the signed release manifest locally, sends the expected sha256 for the remote's `(os, arch, libc)` over SSH, and the remote runs `curl` + `sha256sum -c` against that hash. The remote never needs to verify signatures itself (it has no trusted Vibeke binary yet); a hash mismatch aborts and leaves the previous version in place.
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

### A8. QUIC roaming transport and predictive echo (post-1.0)

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
- Policy: `clipboard.remote_write = "ask_once" | "allow" | "deny"`, default **`ask_once` per machine**: the first OSC 52 write from a given machine shows a one-line prompt ("devbox wants to set your clipboard — allow for this machine?"); the answer is stored per machine label. Clipboard *reads* from remote programs follow `clipboard.osc52_read` (default deny). Plugin-originated OSC 52 is subject to the plugin capability `clipboard.write`.

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

### B1. Goals, and what we learned from cmux

cmux already ships a scriptable browser pane, per-workspace listening ports, and SSH workspaces whose browser traffic is routed through the remote network so `localhost` just works. The idea is not novel; the differentiation is that Vibeke does it **terminal-agnostically, on headless Linux devboxes, inside sandboxes/VMs (13), and with screenshots tied to tasks and commits** (evidence groundwork for Phase 2). The design copies cmux's best decision: **route the browser through the remote network instead of rewriting HTTP.**

Goals:
- Reach a dev server running on any machine (or inside a container/VM) from the laptop, with `http://localhost:<port>` meaning *the remote's* localhost — no Host/Origin rewriting, no cookie games, HMR and OAuth callbacks unchanged.
- Agents and humans share one browser service: agents drive a headless browser next to the server (navigate, click, type, eval, screenshot, console, network); humans see the same screenshots.
- Every screenshot says **what environment** produced it and **which code** it shows.
- Nothing listens beyond loopback; every listener is authenticated or peer-checked.

Priorities: **declared previews first**; automatic discovery produces *suggestions* that become previews when the user (or the agent) confirms or opens them.

### B2. Previews: declared, and discovered as suggestions

`Preview` records (02) come from:

1. **Declared** (authoritative, M3 day one):
   - `vibeke preview declare --port 5173 [--path /dashboard] [--label web] [--pane $VIBEKE_PANE_ID]`, API `preview.declare`, MCP `preview_declare`, task config `[previews] web = { port_env = "PORT", path = "/" }` (05 §6 port leases make the port known before the server starts).
2. **Discovered → suggested** (status `suggested`, shown dimmed with "open?"):
   - **Process-tree listeners**: every 2 s while the pane has a non-shell foreground process, plus on `pane.process_changed`, list LISTEN sockets owned by the pane's process tree (macOS libproc `PROC_PIDLISTFDS` / `PROC_PIDFDSOCKETINFO`; Linux `/proc/<pid>/fd` socket inodes ⨝ `/proc/net/tcp{,6}`), bound to loopback or wildcard. A 1 s HTTP/TLS probe classifies HTTP vs other TCP (others hidden).
   - **Output URLs**: line-feed hook regex for `https?://(localhost|127.0.0.1|0.0.0.0|[::1]|<hostname>)(:\d+)?(/\S*)?` and OSC 8 links; known banners (Vite `➜  Local:`, Next `- Local:`) add path/label.
   - Suggestions become `up` previews automatically only when `preview.auto_discover = "promote"`; default `"suggest"`.
3. **Lifecycle**: `suggested|declared → up ⇄ down → gone` (gone after 60 s absent + process change, or task removal). Scoped to `(machine, runner, pane, task)`.

### B3. Primary: the Vibeke browser profile over SOCKS5 (M3)

```
local Chrome (profile: ~/.local/state/vibeke/browser-profiles/devbox)
   --proxy-server=socks5://127.0.0.1:<socks_port>  --proxy-bypass-list="<-loopback>"
        │  CONNECT localhost:5173   (SOCKS5, hostname resolved proxy-side)
        ▼
local server SOCKS5 listener (127.0.0.1, peer-checked) ──► route by profile's machine/runner
        │  destination is loopback/localhost? ──► tcp_forward{127.0.0.1,5173} over the link ──► devbox vite
        │  otherwise ──► route policy: "direct" (laptop network, default) | "remote" (egress via devbox)
```

- `vibeke preview open v4` (or `prefix+o`) launches — or reuses — a **Vibeke-managed browser profile** for the preview's machine (or task, `preview.profile_scope = "machine" | "task"`) and opens `http://localhost:5173/<path>` in it. The URL is exactly what the dev server printed.
- Browser support:
  - Chromium family (Chrome, Chromium, Edge, Brave, Arc): `--user-data-dir=<profile dir>`, `--proxy-server=socks5://127.0.0.1:<port>`, `--proxy-bypass-list="<-loopback>"` (removes Chrome's implicit loopback bypass so `localhost` goes through the proxy). With `socks5://`, Chrome resolves hostnames proxy-side, so remote-only names (`grafana.internal`) work when routed remote.
  - Firefox: dedicated profile with `network.proxy.type=1`, `socks=127.0.0.1:<port>`, `socks_version=5`, `socks_remote_dns=true`, `network.proxy.allow_hijacking_localhost=true`.
  - Safari has no per-profile proxy → secondary mode (B4) only.
- **Routing** (`preview.profile_route`):
  - `"loopback"` (default): `localhost`, `127.0.0.0/8`, `::1` → the profile's machine/runner via `tcp_forward`; everything else → laptop network (CDNs, OAuth providers, fonts behave normally).
  - `"remote"`: all traffic egresses from the remote (internal hostnames, VPN-only services) — the cmux behaviour.
  - Container/VM runners (13): loopback means the box's loopback; non-loopback follows the box's egress policy when `"remote"`.
- **Why this is primary:** the app sees `Host: localhost:5173` and its real origin, so Vite `allowedHosts`, hard-coded HMR `clientPort`, `localhost` OAuth redirect URIs, service workers (localhost is a secure context), cookies and CORS all behave exactly as on the remote machine. No mirror listeners and no rewriting.
- **Authentication of the SOCKS listener.** Chromium does not support SOCKS5 username/password auth, so the listener authenticates by **peer lookup**: for each accepted loopback connection, the server resolves the client socket's owning PID (macOS: libproc socket enumeration matching the 4-tuple; Linux: `/proc/net/tcp` inode → `/proc/<pid>/fd`) and accepts only if the PID belongs to the managed browser's process tree for that profile. Other processes (including other local users) get the SOCKS failure reply. This is a guardrail against other users and stray processes, not against same-UID malware (09 §2).
- **Isolation between tasks**: per-task profiles (`profile_scope = "task"`) separate cookies and storage when two tasks run the same app. Same-port collisions across machines are impossible because each profile routes to one machine.
- **Local-machine previews** need no proxy: `preview open` uses the profile without a proxy (or the default browser if `preview.local_browser = "default"`).
- Profiles persist (logins survive), live under Vibeke state, and never touch the user's real browser profile. `vibeke preview profile reset devbox` wipes one.
- **Opening from the TUI**: a preview chip in the pane frame (`◉ web :5173`) and the sidebar Previews section; click or `prefix+o`. Clicking a `http://localhost:<port>` URL (plain or OSC 8) printed in a **remote** pane opens it in that machine's profile; `open_url` requests from remote panes are routed to the focused attached client (09 §7 open-URL rules apply).
- `vibeke preview list [--machine m] [--task k7] [--all]` (suggestions included with `--all`), `vibeke preview open|url|forget|mirror|unmirror`, `vibeke preview profile list|reset`.

### B4. Secondary: authenticated reverse proxy for the user's normal browser (M3)

For Safari, a browser the user insists on, or sharing a URL with a non-Vibeke tool (e.g. `curl`, Playwright on the laptop):

```
browser ──http://v4-fix-login.vibeke.localhost:47800/──► local proxy ──► tcp_forward / direct ──► upstream 127.0.0.1:5173
```

- One listener `127.0.0.1:<preview.proxy_port>` (default 47800) and `[::1]`; the preview is selected by `Host` (`<handle>-<slug>.vibeke.localhost`; `*.localhost` resolves to loopback and is a secure context in Chrome, Firefox, Safari ≥ 17).
- **Authentication**: the first navigation carries `?vk_token=` (one-time, 60 s) which the proxy exchanges for a cookie `__Host-vk_preview` (HttpOnly, SameSite=Strict, host-only). Then:
  - the proxy **strips** `vk_token` from the URL and the `__Host-vk_preview` cookie from the `Cookie` header before forwarding upstream;
  - upstream `Set-Cookie` headers that try to set `__Host-vk_preview` (or any `vk_` name) are dropped, so an app can't overwrite or read the proxy credential;
  - unknown `Host` → 421/403 (DNS-rebinding defence); missing/invalid credential → 401 page with "open via `vibeke preview open`".
- **Request rewriting** (necessary here, which is why this mode is secondary): `Host` → `localhost:<port>`; `Origin`/`Referer` → upstream origin when they match the preview origin; response `Location` and `Access-Control-Allow-Origin` mapped back.
- **Cookies — explicit handling.** RFC 6265 cookies are scoped by host (and optional `Domain`), **not by port or origin**:
  - The proxy **strips every `Domain=` attribute** from upstream `Set-Cookie`, making all app cookies host-only for `v4-….vibeke.localhost`. This prevents an app from setting cookies on the parent `vibeke.localhost` that would leak to sibling previews.
  - Different previews have different hostnames, so host-only cookies don't cross previews. Two previews must never share a hostname with different ports (the handle in the hostname guarantees this).
  - Cookies the user's browser holds for plain `localhost` do not apply to `*.vibeke.localhost`, and vice versa — documented as a behaviour difference vs. the profile mode.
  - `SameSite` is not an isolation mechanism and is not relied on.
- **Mirror mode is off by default.** `vibeke preview mirror v4` (explicit, per preview) binds `127.0.0.1:<port>` locally and forwards it raw. It cannot carry the token, so it is **unauthenticated on loopback** (other local users can connect), only one mirror per port is possible, and it fails if the port is busy locally. The UI shows the mirror with a warning badge; `vibeke preview unmirror`. Prefer B3.
- WebSockets, SSE (no buffering) and HTTPS upstreams (TLS to loopback upstream without verification; `preview.tls_origin` for apps that require `https:`, with an explicitly trusted local CA) as before.

### B5. Remote headless browser — scriptable by agents (M3)

A headless Chromium runs **on the machine (or inside the box) where the dev server runs**, owned by the server, launched lazily, shut down after `preview.browser_idle` (default 10 min). Binary: config path → system Chrome/Chromium → Playwright cache → `vibeke browser install` (pinned Chrome-for-Testing, sha256-verified, asks first). Controlled with `--remote-debugging-pipe` (CDP is never on a TCP port).

**Browser sessions.** `browser.session_open {preview?|url?, viewport?, dpr?, color_scheme?, device?}` → `browser_session` (an isolated CDP browser context with its own cookies), owned by the caller's pane/run and closed with the run. One-shot commands (`screenshot` without a session) use a temporary context.

**Commands** (API `browser.*`, CLI `vibeke browser …`, MCP tools):

```
vibeke browser open      <preview|url> [--viewport 390x844] [--device iphone-15] [--dark]   → session id
vibeke browser navigate  <session> <url|path>
vibeke browser click     <session> <selector|text=…|role=…> 
vibeke browser type      <session> <selector> <text> [--submit]
vibeke browser press     <session> <key>
vibeke browser wait      <session> load|networkidle|selector:<css>|ms:<n>
vibeke browser eval      <session> <js>                         # capability browser.script
vibeke browser screenshot <session|preview|url> [--full-page] [--selector css] [--out f.png] [--json]
vibeke browser console   <session|preview> [--since 5m] [--level error]
vibeke browser network   <session|preview> [--failed] [--since 5m]
vibeke browser dom       <session|preview> [--selector …] [--format text|html|a11y]
vibeke browser diff      <shotA> <shotB> [--threshold 0.1]
vibeke browser close     <session>
```

**Destination restrictions** are enforced in a network layer, not by checking the initial URL: the headless browser is launched with `--proxy-server` pointing at a Vibeke filtering proxy on the same machine and `--host-resolver-rules` that disable browser-side DNS, so **every** request — top-level navigations, redirects, subresources, fetch/XHR, WebSockets, service-worker fetches — goes through the proxy, which resolves DNS itself and checks the **resolved IP**:

| Destination | Default |
|---|---|
| Loopback ports belonging to this session's previews (or the box's) | allow |
| Other loopback ports | deny (prevents probing local databases, admin UIs, the Vibeke socket proxy) |
| Link-local / cloud metadata (`169.254.0.0/16`, `fd00:ec2::254`), RFC 1918 / ULA | deny unless `preview.browser_allow_private` lists them |
| Public internet | `preview.browser_external = "subresources"` (default): allowed for subresources (CDNs, fonts), denied for top-level navigation; `"deny"`; `"allow"` |

Denials are logged into the session's network log (so agents see *why* something failed) and, for top-level navigations, returned as errors.

**Console/network capture**: per session, CDP `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, `Log.entryAdded`, `Network.loadingFailed`, responses ≥ 400 → ring of 500 entries; sampled `preview.console_error` events (≤ 1 / 10 s / preview).

### B6. Screenshots as evidence (groundwork)

Every screenshot is a blob (blake3; PNG + WebP thumbnail) with metadata that answers *what produced it* and *what code it shows*:

```rust
struct ScreenshotMeta {
    preview: Option<PreviewId>, url: String, taken_at: Ms,
    taken_by: Actor,                       // user | agent(run, turn) | plugin
    environment: BrowserEnv,               // see below
    code: Option<CodeState>,               // None if the preview isn't tied to a task/repo
    viewport: Viewport, full_page: bool, selector: Option<String>,
}
enum BrowserEnvKind { RemoteHeadless, LocalProfile, LocalProxy }
struct BrowserEnv { kind: BrowserEnvKind, machine: MachineId, runner: RunnerKind,
                    browser: String /* "chrome-headless-shell 141.0…" */, color_scheme: Light|Dark,
                    device: Option<String>, fresh_context: bool }
struct CodeState { task: TaskId, repo_root: PathBuf, head_sha: String,
                   dirty_digest: Option<Blake3>,  // hash of `git diff --binary HEAD` + untracked file hashes; None = clean
                   captured_at: Ms }
```

- The UI labels screenshots by environment ("devbox · headless · fresh context" vs "your browser profile"): a remote headless screenshot with a fresh context is **not** proof of what the human sees in their logged-in local profile, and the label says so.
- `code` is captured at screenshot time from the preview's task checkout (cheap: `git rev-parse HEAD`, `git status --porcelain=v2 -z`, hashing changed files; cached per fs-watch generation).
- Phase 2's `EvidenceRecord` will reference screenshots, check runs `{command, env digest, exit_status, log blob}` and `CodeState`; Phase 1 only guarantees screenshots carry `CodeState`.
- Retention: last 200 per task + anything referenced by an Interaction.
- **Visual diff**: pixelmatch-style YIQ diff → diff PNG + `{changed_ratio, regions}`; `--baseline task-start` compares against the first screenshot of the same path/viewport/environment in the task. Diffs across different `environment.kind` are refused unless `--force`.

### B7. Agents use the same browser: `vibeke mcp`

- `vibeke mcp` is a stdio MCP server (installed via `vibeke integration install <harness> --mcp`), scoped by `VIBEKE_PANE_TOKEN` to the pane's task and machine (09 §3).
- Tools: `preview_list`, `preview_declare {port, path?, label?}`, `browser_open`, `browser_navigate`, `browser_click`, `browser_type`, `browser_press`, `browser_wait`, `browser_eval` (only with capability `browser.script`), `browser_screenshot` (returns MCP image content + `{blob, environment, code}`), `browser_console`, `browser_network`, `browser_dom`, `browser_diff`, `browser_close`.
- Harnesses without MCP use `vibeke browser … --json`; pi/omp get equivalent tools from `@vibeke/pi-extension` returning `ImageContent`.
- **The human sees what the agent saw**: agent screenshots show as a 📷 counter on the pane frame, `prefix+i` opens the gallery, optional inline thumbnail in the sidebar row; remote blobs are fetched once over the blob channel.

### B8. Displaying screenshots in the TUI

- Kitty graphics–capable terminals: `prefix+i` / `vibeke preview show v4` popup (←/→ history, `d` diff, `o` open preview, `c` copy image). A non-PTY **preview pane** can be split into a tab and refreshes on new screenshots.
- No graphics: metadata + `[o] open image locally` (temp file + `open`/`xdg-open`). Sixel M6.
- Transmission: kitty `t=f` when the client is local to the terminal, else chunked `t=d`; cached per terminal session.

### B9. Sequence: agent on devbox verifies its UI change; human reviews from the laptop

```
claude (devbox w2:p1)        devbox server             link             laptop server          laptop TUI / browser
   │ pnpm dev (PORT=20010)       │                        │                     │                     │
   │ vibeke preview declare      │ preview.declared v4    │═══ event ══════════►│ sidebar chip ◉ web  │
   │   --port 20010 --label web  │                        │                     │                     │
   │ MCP browser_open{v4}        │ headless ctx via       │                     │                     │
   │ MCP browser_click "Save"    │ filtering proxy        │                     │                     │
   │ MCP browser_screenshot      │ blob + CodeState{head, │                     │                     │
   │◄──── image + meta ──────────│   dirty_digest}        │═══ event ══════════►│ 📷 badge on w2:p1   │
   │                             │◄═ blob_get ════════════│◄════════════════════│◄── prefix+i ────────│
   │                             │══ png ════════════════►│════════════════════►│ kitty popup, label  │
   │                             │                        │                     │ "devbox · headless" │
   │                             │                        │                     │◄── o ───────────────│
   │                             │                        │   launch profile "devbox" → http://localhost:20010/
   │                             │◄═ tcp_forward :20010 ══│◄═ SOCKS CONNECT localhost:20010 (peer-checked)
```

### B10. Sequence: HMR in the browser profile (no rewriting)

```
Chrome profile "devbox"      local SOCKS (peer-checked)     link (tcp_forward)        devbox vite :20010
  │ GET http://localhost:20010/ ──►│ CONNECT localhost:20010 ═══►│──── connect ─────────────►│
  │◄──────── index.html ───────────│◄════════════════════════════│◄──────────────────────────│
  │ WS ws://localhost:20010/ (HMR, Host/Origin untouched)        │                           │
  │──────────────────────────────►│ CONNECT ════════════════════►│──── WS handshake ────────►│
  │◄════════ HMR updates ══════════│◄════════════════════════════│◄══════════════════════════│
```

---

## Part C — Configuration

Canonical schema: [08](08-ux-config-and-keybindings.md) §11. Keys used by this section:

```toml
[[remote.machine]]
label       = "devbox"
address     = "demo@devbox.tail1234.ts.net"
transport   = "ssh"            # ssh | quic (post-1.0)
keybindings = "local"
auto_connect = true
auto_upgrade = false
bootstrap   = "push"           # push | remote-download   (A3)

[remote]
input_when_offline = "drop"    # drop | ask
predictive_echo    = "auto"    # auto | always | never (quic only)

[clipboard]
remote_write = "ask_once"      # ask_once (per machine) | allow | deny — OSC 52 from remote panes

[preview]
auto_discover   = "suggest"    # suggest | promote | off
mode            = "profile"    # profile (B3) | proxy (B4)
profile_browser = "auto"       # auto | chrome | chromium | edge | brave | firefox
profile_scope   = "machine"    # machine | task
profile_route   = "loopback"   # loopback | remote
local_browser   = "profile"    # profile | default
proxy_port      = 47800
tls_origin      = false
browser_path    = ""           # headless browser binary (auto)
browser_idle    = "10m"
browser_external = "subresources"   # deny | subresources | allow
browser_allow_private = []     # CIDRs/hosts the headless browser may reach
default_viewport = "1440x900"
screenshot_format = "png"
inline_thumbnails = true

## Part D — Acceptance criteria

**M3 (remote):**
- `vibeke machine add devbox …` followed by connect on a fresh Linux arm64 box without Vibeke: the binary is pushed and verified, the bridge comes up, and remote workspaces appear in the sidebar in < 5 s on a 50 ms link. No sudo is used, and the remote's `~` contains only `~/.local/share/vibeke`, `~/.local/state/vibeke` and `~/.config/vibeke`.
- `vibeke --machine devbox agent list` returns remote runs. With devbox unreachable it fails with `machine_offline` within 3 s and never touches local state.
- Kill the SSH connection while a remote Claude runs: the TUI shows offline/greyed out, the agent keeps working, reconnect happens automatically, events are delivered with no seq gaps, and a "while you were away" summary appears.
- Remote upgrade `vibeke machine upgrade devbox` with 5 running agents: no agent process restarts (PIDs unchanged).
- Bandwidth budgets in A7 pass in CI (netem-shaped link fixture).
- Image paste: a PNG on the laptop clipboard pasted into a remote Claude pane results in Claude receiving a valid path to the file on devbox. Pasted into a remote pi RPC run, it arrives as `images` content.
- OSC 52 copy in a remote Neovim lands in the laptop clipboard after the one-time `ask_once` approval for that machine (and without a prompt when `clipboard.remote_write = "allow"`).

**M3 (preview fabric):**
- Start `pnpm dev` (Vite) in a remote task pane. A suggestion appears in < 3 s; declared previews appear immediately. `vibeke preview open` loads the app on the laptop. Editing a file on devbox triggers HMR in the laptop browser in < 500 ms (+RTT).
- `vibeke preview open v4` for a remote Vite app opens `http://localhost:<port>` in the Vibeke Chrome profile; HMR works with a project that hard-codes `server.hmr.clientPort`; a `localhost` OAuth callback URL completes; no local port is bound.
- A non-browser process connecting to the SOCKS listener is rejected (peer check).
- Proxy mode (B4): two tasks of the same Next.js app open on different `*.vibeke.localhost` hosts with independent login cookies; an upstream `Set-Cookie: x=1; Domain=vibeke.localhost` arrives host-only; the app never sees `vk_token` or `__Host-vk_preview`; `curl` with a forged Host or no credential gets 403/401.
- Mirror mode is never active unless explicitly enabled per preview.
- Headless browser: a page that redirects to `http://127.0.0.1:5432`, loads an `<img src=http://169.254.169.254/>`, or opens a WebSocket to an undeclared loopback port gets all three blocked and logged.
- Every screenshot carries `environment` and (for task previews) `code.head_sha` + `dirty_digest`; the TUI labels headless vs profile screenshots.
- An agent on devbox opens a session, clicks a button and calls MCP `browser_screenshot` → gets an image. The laptop TUI shows a 📷 badge within 1 s, and `prefix+i` displays it via kitty graphics in Ghostty, with the open-locally fallback working in Terminal.app.
- `browser logs v4 --level error` shows a thrown exception from the page. `browser diff` between before/after screenshots outputs a changed ratio and a diff image.
- Clicking `http://localhost:5173` printed in a remote pane opens the forwarded preview URL.

**Post-1.0 (QUIC):**
- Switching Wi-Fi networks mid-session keeps the remote session attached with no visible reconnect (< 1 s stall).
- Predictive echo makes typing in a remote shell at 200 ms RTT feel local, with mispredictions rolled back.
- Blocked UDP falls back to SSH automatically.
