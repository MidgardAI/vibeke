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

**Implementation note (Goal 01):** the first build puts the per-machine SSH link in the local *client* (TUI/CLI) instead of the local server: the client owns one bridge mux per machine and opens raw channels to each remote server, over which the unchanged control and render protocols run. The unified sidebar, `--machine` forwarding (no local fallback) and reconnection work this way; a local-server proxy can be added when a non-terminal surface (Phase 2 gateway) needs it.

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
remote server: path = <pane inbox>/<blake3-12>/clipboard-<ts>.png   (pane inbox: see A11.4)
   ├─ structured harness (pi/omp RPC, Codex app-server, ACP): send as image content in the next prompt
   │     (pi: prompt{images:[{type:"image",data,mimeType}]}; codex: localImage input item)
   └─ TUI harness (claude, codex TUI): bracketed-paste the file path (Claude/Codex accept image paths)
emit agent.item{kind:user_message, attachments:[blob]}
```
- Keybinding `remote_image_paste = "ctrl+v"` is active only when the clipboard holds an image and the focused pane cannot see local files (remote, container, VM, or a sandbox that denies the source); otherwise ctrl+v passes through. Also available as `vibeke attach-file <path> --pane devbox/w3:p5`, which accepts any file type and uploads it. Both use the A11 pipeline.

**Remote → local (show an image produced remotely):**
- **Kitty graphics** emitted by a remote program are parsed by the remote VT engine (libghostty-vt parses kitty graphics natively, 03 §2). Images are stored as blobs and placements are sent in render frames by hash. The local client fetches each blob once (cached) and re-emits kitty graphics to the host terminal, or falls back to a `[image 1280×720 — prefix+i to open]` placeholder.
- Screenshots and other agent artifacts use the same blob path (B6).

### A11. Dropped and pasted paths: filesystem namespace translation (M3)

**Problem.** Dragging `~/Desktop/Screenshot 2026-10-05 at 20.49.03.png` onto iTerm2/Ghostty/Kitty/WezTerm inserts the *local* path as text (shell-escaped, usually as a bracketed paste). An agent in a pane on devbox — or in a container, a VM, or a sandbox that denies `~/Desktop` — receives a path that doesn't exist in its filesystem.

**Principle.** Every pane has an *execution namespace* (02/13: `host`, `ssh{machine}`, `sandbox`, `container`, `vm`). The Vibeke TUI client always runs on the machine the user is sitting at, so it sees every paste **before** it reaches the pane. When pasted text names local files the target namespace can't see, the client copies them into that namespace's inbox and rewrites the paste. This is one mechanism for remote machines, local containers/VMs and local sandboxes; it is not tied to any terminal emulator's file-transfer feature.

#### A11.1 Detection (client-side, `vk-tui` input pipeline)
1. Trigger: a **paste event** from the host terminal (bracketed paste; drag-and-drop arrives this way in iTerm2, Ghostty, Kitty, WezTerm — **[verify M0]** per terminal, incl. whether each uses backslash escapes, quotes or `file://` URLs). Typed keystrokes are never rewritten.
2. Parse the paste with POSIX shell-word rules (backslash escapes, single/double quotes) plus `file://` URLs (percent-decoded). Newline- or space-separated lists of several dropped files are supported.
3. Rewrite only if **all** of:
   - every token is an absolute or `~/` path that exists locally (`stat`, regular file or directory);
   - the paste consists only of path tokens (default). With `paste.translate = "embedded"`, path tokens inside mixed text are also translated; other text is left byte-for-byte;
   - the focused pane's namespace cannot see the path: pane is on another machine, or in a container/VM, or in a sandbox whose allowlist doesn't include it (the client asks the server `pane.can_see_paths {pane, paths}`, which evaluates the pane's runner and sandbox profile).
4. Otherwise the paste goes through untouched (local host panes are never affected).

#### A11.2 Transfer
- Files are hashed (blake3) locally and sent with `blob_put` over the link's **bulk channel** (separate flow-controlled stream; render and input never wait behind it, A4). Content-addressed: re-dropping the same screenshot costs nothing. Chunked and resumable across reconnects.
- Size policy: ≤ `paste.max_auto_bytes` (default 50 MiB total) uploads immediately; larger, or any directory, asks first in the status line (`↵ upload 312 MB · esc paste original`). Directories are sent as a tar stream and unpacked on arrival (symlinks not followed; special files skipped).
- Non-ASCII and spaces in names are preserved.

#### A11.3 Rewrite and delivery
- The paste is held (status line: `⇡ uploading Screenshot…png 2.1 MB`) and delivered as one bracketed paste once all files have arrived; `esc` cancels and sends the original text instead. Small files feel instant.
- Each path is replaced by its in-namespace path, **re-escaped in the same style as the original** (backslash-escaped stays backslash-escaped, quoted stays quoted), keeping the original basename so the agent sees a meaningful name:
  `/Users/demo/Desktop/Screenshot\ 2026-10-05\ at\ 20.49.03.png` → `/home/demo/.local/state/vibeke/inbox/3f9a1c0b2e7d/Screenshot\ 2026-10-05\ at\ 20.49.03.png`
- TUI harnesses (Claude Code, Codex, pi, omp) recognise image paths in pasted text and attach the image. For headless runs (pi/omp RPC, Codex app-server, ACP), images are additionally offered as native image content in the next prompt (A10).
- Event: `paste.translated {pane, files:[{blob, bytes, local_name}], target_namespace}` (no local paths in the event — only basenames; 09 §9).

#### A11.4 Pane inbox (where files land)
| Namespace | Inbox path seen by the agent | Notes |
|---|---|---|
| `ssh{machine}` | `$XDG_STATE_HOME/vibeke/inbox/<blake3-12>/<basename>` on that machine | |
| `container` / `vm` | `/vibeke/inbox/<blake3-12>/<basename>` (read-only mount of a host-side inbox dir) | 13 §5 |
| `sandbox` (local) | `$XDG_STATE_HOME/vibeke/inbox/<blake3-12>/<basename>` on the host, which is on every sandbox profile's read-only allowlist | a copy, not a widening of the allowlist to `~/Desktop` |
| `host` (local) | — (no translation) | |

- Never inside the repo/worktree (no accidental commits). Retention: `paste.inbox_retention = "14d"`, plus cleanup when the task is archived.
- Files are written 0600 in a 0700 directory.

#### A11.5 Security
- Only the local client initiates transfers, and only for content the user explicitly pasted/dropped. There is **no** API by which a remote server, pane, plugin or agent can request a local file (no reverse fetch, no mount). Bridge/broker reject `blob_get` toward the client for anything the client didn't push.
- Directory drops and large files always confirm. `paste.translate = "off"` disables the feature; `"ask"` confirms every translation.

#### A11.6 Topology requirement and fallbacks
- Works whenever the Vibeke client runs locally and attaches to the remote (`vibeke --machine devbox`, or the unified multi-machine view). If the user instead runs `ssh devbox` and starts `vibeke` *on the remote*, no local process sees the drop. Fallbacks: `vibeke ssh devbox` (a thin wrapper that runs the local client against the remote server, recommended in docs and `doctor`), or terminal-specific features (iTerm2 shell-integration scp upload) which Vibeke does not depend on.

#### A11.7 Alternative considered: mounting local files on the remote
A reverse mount (`/vibeke/local/Users/demo/...` via FUSE/9p, fetched lazily) keeps paths nearly unchanged and handles big folders, but breaks when disconnected, needs FUSE on the remote, and gives the remote a live window into the laptop. Rejected as default; possible post-1.0 opt-in for directories only.

---

## Part B — Preview fabric

### B1. Goals, and what we learned from cmux

cmux already ships a scriptable browser pane, per-workspace listening ports, and SSH workspaces whose browser traffic is routed through the remote network so `localhost` just works. The idea is not novel; the differentiation is that Vibeke does it **terminal-agnostically, on headless Linux devboxes, inside sandboxes/VMs (13), and with screenshots tied to tasks and commits** (evidence groundwork for Phase 2). The design copies cmux's best decision: **route the browser through the remote network instead of rewriting HTTP.**

Goals:
- Reach a dev server running on any machine (or inside a container/VM) from the laptop, with `http://localhost:<port>` meaning *the remote's* localhost — no Host/Origin rewriting, no cookie games, HMR and OAuth callbacks unchanged.
- **The browser lives in the terminal.** The default human view of a preview is a live, interactive **browser pane** in the Vibeke layout (B3.2), drawn with kitty graphics: split it next to the agent, zoom it, switch tabs, detach and reattach, like any pane. A normal browser window (B3.3) is one keystroke away for DevTools, extensions or a big screen.
- **Render near the eyes, network near the server.** The human's browser runs on the machine of the viewing client (the laptop) and only its HTTP traffic crosses the link. Pixels never cross SSH in the recommended topology, so scrolling and typing feel local and HMR costs one RTT.
- Agents and humans share one browser service: agents drive a headless browser next to the server (navigate, click, type, eval, screenshot, console, network); humans see the same screenshots and can **watch an agent's browser session live** in a pane (B7).
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

### B3. Primary: a Vibeke-managed browser routed over SOCKS5, in a pane or a window (M3)

One route, two ways to look at it. The route (B3.1) makes `localhost` mean the remote's localhost. The **browser pane** (B3.2) is the default view; the **external window** (B3.3) uses the same profile and route.

#### B3.1 The route

```
Vibeke-managed Chromium on the laptop (pane: headless · window: headful)
   profile: ~/.local/state/vibeke/browser-profiles/devbox
   --proxy-server=socks5://127.0.0.1:<socks_port>  --proxy-bypass-list="<-loopback>"
        │  CONNECT localhost:5173   (SOCKS5, hostname resolved proxy-side)
        ▼
local server SOCKS5 listener (127.0.0.1, peer-checked) ──► route by profile's machine/runner
        │  destination is loopback/localhost? ──► tcp_forward{127.0.0.1,5173} over the link ──► devbox vite
        │  otherwise ──► route policy: "direct" (laptop network, default) | "remote" (egress via devbox)
```

#### B3.2 The browser pane (default view)

A **browser pane** is a non-PTY pane kind whose content is a live Chromium viewport. `prefix+o` on a pane with a preview (or clicking a preview chip, or a `localhost` URL printed in a remote pane) opens it as a split next to that pane; `vibeke preview open v4 [--split right|down|tab|float]` does the same from the CLI.

```
┌ claude · devbox w2:p1 ─────────────────┬ ◉ web :5173 · devbox ──────────────────┐
│ ● Editing src/routes/login.tsx         │ ← → ⟳  localhost:5173/login          ▢ │
│ ...                                    │ ┌────────────────────────────────────┐ │
│ ✓ pnpm test (12 passed)                │ │                                    │ │
│                                        │ │     (live page, kitty graphics)    │ │
│ > _                                    │ │                                    │ │
│                                        │ └────────────────────────────────────┘ │
└────────────────────────────────────────┴ laptop chromium → devbox loopback ─────┘
```

- **Where it runs.** A headless Chromium (`--headless=new`) owned by the server **on the viewing client's machine** (the laptop server in the recommended topology), using the machine's or task's persistent profile and the B3.1 route. One browser process per profile; one CDP target (over `--remote-debugging-pipe`) per browser pane. In the plain-SSH topology (no local client) it runs on the remote server instead and frames cross the link under the A7 budgets (adaptive quality and fps, paused when not visible); `vibeke doctor` explains the difference.
- **Frames.** CDP screencast (`Page.startScreencast`, with per-frame acks for backpressure) produces frames at the pane's pixel size (JPEG q80, ack on receipt, `everyNthFrame` 1; Stage 0 showed `everyNthFrame` > 1 delays isolated changes by 500+ ms and `captureScreenshot` polling manages 5–7 fps at ~30 % CPU, so neither is used). The server decodes them, diffs against the previous frame in cell-aligned tiles (exact compare: 0.7 ms per 1600×1280 frame; hashing every tile cost 11 ms), and publishes only changed tiles on a separate **media channel** of the render stream: latest-wins per pane and lower priority than cell frames, so a busy page never delays text panes. A browser pane not visible on any client gets no frames and its screencast stops.
- **Drawing on the host.** The client draws tiles with kitty graphics, placed with unicode placeholders so clipping in splits, popups over the pane and chrome stay correct. When the client and the host terminal are on the same machine (always, for the laptop), image data goes via shared memory (`t=s`) or temp files (`t=t`) instead of base64 through the PTY; otherwise chunked, zlib-compressed `t=d`. Each tile is its own image id with a virtual placement (`a=T,U=1`), re-sent under the same id when it changes; placeholder cells are written once per layout. Stage 0 (prototype measurement): while scrolling a 1600×1280 pane, shm puts 0.4–0.6 MB/s on the PTY versus 14–20 MB/s for changed tiles as RGBA+zlib `t=d` and 27–37 MB/s for full frames; temp files cost 27 ms/frame of writes. A scroll frame creates ~260 shm objects; if that proves costly in Ghostty, one persistent `t=f` file per pane addressed with `O=`/`S=` is the alternative `[verify M3: host]`. The browser pane bypasses the VT engine: the server already has the pixels. (Inbound kitty graphics from programs in normal panes are parsed by libghostty-vt, 03 §2.)
- **Crisp sizing.** The viewport is the pane's cell rect × the host's cell pixel size (`CSI 16 t`, 03 §6.1) at the host's device pixel ratio, applied with `Emulation.setDeviceMetricsOverride` **and** `--force-device-scale-factor=<DPR>` at launch (verified in Stage 0: without the flag, screencast frames come out at CSS px even though screenshots honour the override; so one browser process per host DPR), so text is sharp on Retina screens and CSS breakpoints match the pane width. Resizing or zooming the pane resizes the viewport (debounced 100 ms). `--viewport 390x844` / `--device iphone-15` pins a device size and letterboxes it.
- **Input.** When the browser pane is focused, keys and mouse go to the page; the prefix key still goes to Vibeke.
  - Mouse: pixel-precise with SGR-pixels (DECSET 1016) where the host supports it, else cell centres; clicks, drags, hover and wheel → `Input.dispatchMouseEvent` (wheel in pixel deltas for smooth scrolling).
  - Keys: the kitty keyboard protocol gives full key events (press/repeat/release, modifiers, base layout key) → `Input.dispatchKeyEvent` with correct `key`/`code`, so page shortcuts work on any layout; text → `Input.insertText`; bracketed paste → `insertText`; image paste and file drops reuse A10/A11 as a file-chooser/drop target. Verified in Stage 0 against Chromium 153: US and Norwegian letters (`ø`/`æ`/`å` with `code` from the base-layout key), macOS Option/AltGr text (`@`, `[`) sent as text with the Alt/Ctrl bits dropped (`altKey: false` in the page), dead-key compositions via `insertText`, Cmd editing shortcuts via Chromium `commands`; `nativeVirtualKeyCode` must not carry a Windows key code (full Chrome then reports `Unidentified`). Still `[verify M3: host]`: the same through a real kitty-keyboard host, trackpad scrolling, and IME composition.
  - Clipboard: page copy → OSC 52 / client clipboard; page clipboard reads follow the OSC 52 read rules (03 §8).
- **Browser chrome in one row.** The pane's top row shows back/forward/reload, the URL (editable), a loading indicator and an environment label (`laptop chromium → devbox loopback`). Browser actions live in the prefix table (defaults in 08): address bar, back/forward, reload, hard reload, screenshot (B6, `environment = LocalPane`), console/network split, **open in window** (B3.3), close.
- **Console and network in the terminal.** The console split is a text pane following the page's console and failed requests (`vibeke browser console --follow`, same capture as B5), so errors sit next to the code and the agent. Full DevTools are in the window.
- **Lifecycle.** Browser panes are persisted like other panes (URL, profile, history), re-created after a server restart or reattach, and closed with their tab. Chromium is not holder-owned, so the page reloads at its last URL after a server restart rather than surviving it.
- **Fallbacks.** Hosts with full kitty graphics (Ghostty, Kitty, WezTerm): the browser pane is the default. iTerm2: kitty graphics support is partial `[verify M3: host — still open; the probe batch and reply parser exist (vk-browser::probe), iTerm2's answers need the maintainer]`; if it fails the capability probe, the pane uses iTerm2 inline images (OSC 1337) at a lower frame rate, or falls back to the window. Hosts without graphics (Terminal.app): `prefix+o` opens the window.
- **Performance targets**, checked by a spike before the full UI is built: input→pixel ≤ 50 ms p95 and ≥ 30 fps while scrolling, laptop browser with a remote dev server over a 50 ms link; an idle page costs no frames and < 1% CPU. If CDP screencast misses them, the numbers and the tuning tried (frame size, format, quality, ack pacing) are recorded and the targets or design are revisited in this spec.
  - **Stage 0 result (M1 Pro, Chromium 153 headless shell, 1600×1280 at DPR 2, laptop-local page):** idle page 0 frames and 0–0.4 % CPU — met. Key→pixel p50 17–20 ms, p95 21–40 ms — met. Wheel→scrolled pixels p50 26–31 ms, p95 35–46 ms with `--disable-smooth-scrolling` (62–120 ms with smooth scrolling) — met for the compositor scroll; paint that waits for a page `wheel` handler took p95 100–150 ms. Scrolling fps **19–28, missed**: the ceiling held across JPEG/PNG, quality 60/80, device vs half resolution, ack pacing, GPU flags, headless shell vs `--headless=new`, while Chromium stayed below ~70 % CPU (pacing inside the capture path). `HeadlessExperimental.beginFrame` is unavailable on macOS. **Target revised:** ≥ 24 fps while scrolling (30 desired), ≤ 50 ms p95 for key input and compositor scroll. Before Stage 2: re-measure on an idle machine and with the remote dev server over the 50 ms link, and try lower resolution during motion with a crisp keyframe when motion stops.

#### B3.3 The external window

- **Open in window** (from the browser pane, `vibeke preview open v4 --window`, or `preview.mode = "window"`) hands the same profile to a headful browser: the server closes the headless instance (a profile can be open in only one Chromium process) and opens the window at the same URL with logins intact. Closing the window, or "back to pane", hands it back. Use it for DevTools, extensions, password managers or a second screen.
#### B3.4 Profiles, routing and authentication (both views)

- Both views use a **Vibeke-managed browser profile** for the preview's machine (or task, `preview.profile_scope = "machine" | "task"`) and open `http://localhost:5173/<path>`: exactly the URL the dev server printed.
- Browser support (the browser pane always uses Chromium; the window can use any of these):
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
- **Local-machine previews** need no proxy: the browser pane or window uses the profile without a proxy (or the default browser if `preview.local_browser = "default"`).
- Profiles persist (logins survive), live under Vibeke state, and never touch the user's real browser profile. `vibeke preview profile reset devbox` wipes one.
- **Opening from the TUI**: a preview chip in the pane frame (`◉ web :5173`) and the sidebar Previews section; click or `prefix+o` opens a browser pane. Clicking a `http://localhost:<port>` URL (plain or OSC 8) printed in a **remote** pane opens it in a browser pane on that machine's profile; `open_url` requests from remote panes are routed to the focused attached client (09 §7 open-URL rules apply).
- `vibeke preview list [--machine m] [--task k7] [--all]` (suggestions included with `--all`), `vibeke preview open [--split right|down|tab|float | --window]|url|forget|mirror|unmirror`, `vibeke preview profile list|reset`.

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
enum BrowserEnvKind { RemoteHeadless, LocalPane, LocalProfile, LocalProxy }   // LocalPane: the human's browser pane (B3.2)
struct BrowserEnv { kind: BrowserEnvKind, machine: MachineId, runner: RunnerKind,
                    browser: String /* "chrome-headless-shell 141.0…" */, color_scheme: Light|Dark,
                    device: Option<String>, fresh_context: bool }
struct CodeState { task: TaskId, repo_root: PathBuf, head_sha: String,
                   dirty_digest: Option<Blake3>,  // hash of `git diff --binary HEAD` + untracked file hashes; None = clean
                   captured_at: Ms }
```

- The UI labels screenshots by environment ("devbox · headless · fresh context" vs "your browser pane · profile devbox"): a remote headless screenshot with a fresh context is **not** proof of what the human sees in their logged-in local profile, and the label says so.
- `code` is captured at screenshot time from the preview's task checkout (cheap: `git rev-parse HEAD`, `git status --porcelain=v2 -z`, hashing changed files; cached per fs-watch generation).
- Phase 2's `EvidenceRecord` will reference screenshots, check runs `{command, env digest, exit_status, log blob}` and `CodeState`; Phase 1 only guarantees screenshots carry `CodeState`.
- Retention: last 200 per task + anything referenced by an Interaction.
- **Visual diff**: pixelmatch-style YIQ diff → diff PNG + `{changed_ratio, regions}`; `--baseline task-start` compares against the first screenshot of the same path/viewport/environment in the task. Diffs across different `environment.kind` are refused unless `--force`.

### B7. Agents use the same browser: `vibeke mcp`

- `vibeke mcp` is a stdio MCP server (installed via `vibeke integration install <harness> --mcp`), scoped by `VIBEKE_PANE_TOKEN` to the pane's task and machine (09 §3).
- Tools: `preview_list`, `preview_declare {port, path?, label?}`, `browser_open`, `browser_navigate`, `browser_click`, `browser_type`, `browser_press`, `browser_wait`, `browser_eval` (only with capability `browser.script`), `browser_screenshot` (returns MCP image content + `{blob, environment, code}`), `browser_console`, `browser_network`, `browser_dom`, `browser_diff`, `browser_close`.
- Harnesses without MCP use `vibeke browser … --json`; pi/omp get equivalent tools from `@vibeke/pi-extension` returning `ImageContent`.
- **The human sees what the agent saw**: agent screenshots show as a 📷 counter on the pane frame, `prefix+i` opens the gallery, optional inline thumbnail in the sidebar row; remote blobs are fetched once over the blob channel.
- **Watch the agent's browser live**: while an agent has a browser session open, the pane frame shows `◉ browsing`; `vibeke browser watch <session>` (or the action on that chip) opens a browser pane showing the agent's remote headless session via screencast over the link (A7 budgets apply; frames stop when not visible). It is **read-only** by default. **Take over** gives the human input; while taken over, the agent's `browser_*` calls fail with a clear `human_control` error until the human releases it. Screenshots from a watched session keep `environment = RemoteHeadless`.

### B8. Displaying screenshots in the TUI

- Kitty graphics–capable terminals: `prefix+i` / `vibeke preview show v4` popup (←/→ history, `d` diff, `o` open the live preview in a browser pane, `c` copy image). A screenshot pane can be split into a tab and refreshes on new screenshots.
- No graphics: metadata + `[o] open image locally` (temp file + `open`/`xdg-open`). Sixel M6.
- Transmission: kitty `t=s`/`t=t` (shared memory / temp file) when the client is local to the terminal, else chunked `t=d`; cached per terminal session. Same code path as the browser pane's tiles (B3.2).

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
mode            = "pane"       # pane (B3.2; default when the host has kitty graphics) | window (B3.3) | proxy (B4)
pane_split      = "right"      # right | down | tab | float
pane_fps        = 60           # cap for local clients; remote-rendered panes follow A7 budgets
pane_location   = "client"     # client (render on the viewing machine; recommended) | server
profile_browser = "auto"       # window only: auto | chrome | chromium | edge | brave | firefox (the pane always uses Chromium)
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
- Path drop (A11): dragging `~/Desktop/Screenshot 2026-10-05 at 20.49.03.png` from Finder into iTerm2, Ghostty, Kitty and WezTerm with a remote Claude pane focused delivers a paste of an existing devbox inbox path with the same basename and escaping style; Claude attaches the image. Same drop into a local host pane is untouched; into a local container/VM pane it yields `/vibeke/inbox/...`; into a local sandbox pane it yields the host inbox path and the agent can read it while `~/Desktop` stays denied.
- Dropping 3 files at once translates all three in one paste; re-dropping the same file re-uses the blob (no second transfer); a 300 MB drop asks first; `esc` during upload pastes the original text.
- No API call from a remote server, pane token or plugin can cause the client to send a file it wasn't given by a user paste (negative test).
- OSC 52 copy in a remote Neovim lands in the laptop clipboard after the one-time `ask_once` approval for that machine (and without a prompt when `clipboard.remote_write = "allow"`).

**M3 (preview fabric):**
- Start `pnpm dev` (Vite) in a remote task pane. A suggestion appears in < 3 s; declared previews appear immediately. `vibeke preview open` loads the app on the laptop. Editing a file on devbox triggers HMR in the laptop browser in < 500 ms (+RTT).
- `prefix+o` on a remote Vite pane opens a browser pane next to it in Ghostty within 1 s, showing `http://localhost:<port>` from the laptop's Chromium over the bridge; clicking, typing (incl. shift/ctrl shortcuts and a Norwegian layout) and smooth scrolling work; input→pixel ≤ 50 ms p95 and ≥ 30 fps scrolling on a 50 ms link; no pixels cross the link (bridge byte counters).
- The browser pane survives split/zoom/resize with a sharp, correctly sized viewport; it reloads at its last URL after a server restart; an off-screen browser pane produces zero frames.
- "Open in window" moves the page to a headful window with the same logins, and back.
- In iTerm2 the browser pane works via kitty graphics or the OSC 1337 fallback; in Terminal.app `prefix+o` opens the window.
- `vibeke browser watch` shows an agent's remote session live; take-over makes the agent's next `browser_click` fail with `human_control` until released.
- `vibeke preview open v4 --window` for a remote Vite app opens `http://localhost:<port>` in the Vibeke Chrome profile; HMR works with a project that hard-codes `server.hmr.clientPort`; a `localhost` OAuth callback URL completes; no local port is bound.
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
