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

**Implementation note (Goal 03 Stage 1, partial reversal):** the preview route needs a link owned by the *local server* (the SOCKS listener and the managed browser live there, not in a short-lived CLI). `Link` moved from the `vibeke` binary into `vk-remote::link` (reconnecting, with an injectable connector) and the local server now holds **one ControlMaster-backed link per machine, opened on demand** for `tcp:`/`egress:` channels and for preview lookups (`preview.get` on the remote server over a `socket` channel). The TUI/CLI still own their own links for control and render streams; with ControlMaster the extra link costs no new SSH handshake. Federating clients through the local server remains future work.

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
  - `tcp_forward{host, port}`: preview forwarding, opened by the local side. *(Built as kind string `tcp:<host>:<port>`; the bridge refuses any host that is not `localhost`/`*.localhost`, `127.0.0.0/8` or `::1` before connecting. `egress:<host>:<port>` is the separate kind for `profile_route = "remote"`, refused when the remote's `preview.allow_remote_egress = false`.)*
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

**Implementation notes (Goal 03 Stage 1)** — `vk-preview` (primitives) + `vk-server::preview` (wiring):
- **One preview per port per machine.** A listener, an output URL and a declaration for the same port merge into one record (banner adds `label`/`path`, the listener scan adds `pid`/`pane`; declaring a suggested port promotes it and sets `source = declared`).
- **Which panes are scanned:** a pane whose foreground is not a shell, *or* whose shell has child processes (one `proc_listchildpids`/procfs call). Deviation: the foreground report lags for programs that print nothing (e.g. a server stuck in `getfqdn()`), and `pnpm dev &` is common. A shell with no children costs nothing else. Scans run every 2 s and on `pane.process_changed` (debounced 300 ms), in a blocking task: process tree (depth 8) → libproc `PROC_PIDLISTFDS` + `PROC_PIDFDSOCKETINFO` (macOS) / `/proc/<pid>/fd` ⨝ `/proc/net/tcp{,6}` (Linux) → LISTEN sockets bound to loopback or wildcard.
- **Classification:** new ports get a 1 s `HEAD / HTTP/1.0` probe (any `HTTP/` reply = HTTP; others hidden). No TLS probe yet: `https` output URLs count as up on TCP connect.
- **Output URLs:** the pane feed loop only splits new bytes into lines and keeps those containing `://` (bounded tail, 4 KiB lines, 256-line queue, dropped when full); escape stripping (CSI/OSC, with OSC 8 targets kept), the URL regex and the Vite `➜  Local:` / Next `- Local:` banner match run in a separate task. The `<hostname>` alternative of the regex is not implemented. A URL creates a suggestion only if its port answers within ~5 s (10 probes); `0.0.0.0` becomes `localhost` in the stored URL. Replayed output (recovery) is never scanned.
- **Presence:** a listener found in the scan, else (pane not scanned, declared, or output-sourced) a TCP connect to `127.0.0.1`/`::1`. `last_seen_ms` updates stay in memory; only status/pid/pane/label/path/url changes are committed (with events), so a running preview costs no writes.
- **Gone:** after 60 s absent (no extra "process change" condition); declared previews stay `down` until forgotten; previews of a closed pane retire unless declared. `preview.forget` remembers `(port, pid)` so the same listener isn't re-suggested; a new process on the port is.
- Bound: 64 previews per server. Task `[previews]` port-lease declarations are not built yet; MCP `preview_declare` exists since Stage 3 (B7).

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

**Implementation notes (Goal 03 Stage 1, window only):**
- `vibeke preview open <v4 | devbox/v4 | url> --window [--machine devbox]`. `--machine` is *not* forwarded for `preview.open`/`url`/`profile`/`status`: the CLI sends them to the local server with `machine` set, because the browser runs on the viewing machine. The local server resolves a remote preview with `preview.get`/`preview.promote` on the remote server over its own link.
- Browser discovery: `[preview] browser` (or an absolute `profile_browser` path), `$VIBEKE_BROWSER`, newest Playwright `chromium-<rev>` build, then `/Applications` (and `~/Applications`) Chromium, Chrome, Brave, Edge bundles (Linux: `chromium`, `google-chrome`, `brave-browser`, `microsoft-edge` on `PATH`). Firefox is not supported yet.
- Arguments: `--user-data-dir=<state>/browser-profiles/<profile>` (directory created 0700 and checked to be under that root), `--no-first-run --no-default-browser-check --disable-sync --password-store=basic --use-mock-keychain`, and for remote machines `--proxy-server=socks5://127.0.0.1:<port> --proxy-bypass-list=<-loopback>`. Local-machine previews get no proxy (`profile` named `local`), or the system browser with `preview.local_browser = "default"`.
- One browser process per profile: the root pid (+ start time) is tracked and persisted; a second open runs the same command line, which hands the URL to the running instance (new tab) and exits. A profile already open for another machine/route is a `conflict`. After a server restart, still-running browsers on our profile dirs are re-adopted (pid, start time and `--user-data-dir` argv must match) and the previous SOCKS port is re-bound so their proxy setting keeps working.
- Pane mode returns `unsupported {fallback: "window"}` until Stage 2.
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

**Implementation notes (Goal 03 Stage 1):**
- One listener per server on `127.0.0.1`, started lazily on the first remote-profile open (nothing listens until then), ephemeral port persisted and preferred on restart; shown as `server.status.preview.socks_port` and `preview.status`. SOCKS5 no-auth `CONNECT` with IPv4, domain and IPv6 address types; other commands get `0x07`, unknown address types `0x08`.
- Peer check after the handshake, in a blocking task: for each live managed browser (root pid + start time), its process tree (depth 6) is searched for a TCP socket whose local port is the client's port and whose remote port is the listener's (libproc on macOS; `/proc/net/tcp{,6}` inode → `/proc/<pid>/fd` on Linux). Only the browser's tree is enumerated, never the whole system. The owning tree determines the profile, and with it the machine and route. Failure → reply `0x02`, logged and counted (`preview.status.rejected`).
- Routing: `localhost`, `*.localhost`, `127/8`, `::1` (incl. v4-mapped) and `0.0.0.0`/`::` → the profile's machine (local: direct connect to 127.0.0.1/::1; remote: `tcp:` channel; a closed port gives `0x05`). Otherwise `loopback` route = direct from this machine, **except** that a name resolving to a loopback address is still sent to the profile's machine (never this machine's loopback); `remote` route = `egress:` channel, refused with `0x02` when `preview.allow_remote_egress = false`.
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

**As built (Goal 03 Stage 3; `vk-browser::{policy, proxy, headless, install, snapshot}` + `vk-server::agent_browser`):**
- **Process.** One headless Chromium per server (= per machine and session), started by the first `browser.open`, on `<state>/<session>/agent-browser/profile` (wiped at each launch; sessions are contexts, not profiles), stderr to `logs/agent-browser.log`. Binary order (deviation: an installed build first, Playwright before the system browser): `preview.browser_path` → `$VIBEKE_HEADLESS_BROWSER` → a `vibeke browser install` build (`~/.local/share/vibeke/browsers`) → Playwright `chromium_headless_shell-*` (newest) → Playwright `chromium-*` → system Chromium/Chrome (binary only; `--headless=new` unless it is a headless shell). No binary → `unsupported {fallback: "vibeke browser install"}`. The browser stops after `browser_idle` without sessions; a session closes when its owner pane disappears, its run ends, it is unused for `browser_idle` (and neither watched nor taken over), or the browser exits.
- **Sessions** are `Target.createBrowserContext` contexts with one page (`Target.createTarget`, flattened attach), handles `b1`, `b2`, … (plus a ULID `session_id`), owned by the calling pane and its live run (or by the user for full-scope callers). Per context: downloads denied (`Browser.setDownloadBehavior`), JS dialogs dismissed and logged, file choosers intercepted (cancelled), viewport `preview.default_viewport` unless given. Sessions don't survive a server restart.
- **Destination filtering: two layers, one `Policy`.**
  1. *Per-session filtering proxy* (the boundary). Each context gets `proxyServer = http://127.0.0.1:<its own ephemeral port>` and `proxyBypassList = "<-loopback>"` (Chromium otherwise bypasses proxies for loopback; verified on Chrome 153). The proxy accepts only connections owned by the headless browser's process tree (the SOCKS listener's libproc/procfs peer lookup), handles absolute-form HTTP (forwarded in origin form with `Connection: close` on both legs, so one proxy connection never carries a second request to another host) and `CONNECT` (which Chromium uses for `https:`, `ws:` and `wss:`; verified), resolves the name **once** (IP literals and `localhost`/`*.localhost` without DNS), requires **every** resolved address to pass, and connects only to those addresses. Denials get `403` + `X-Vibeke-Denied: <reason>`.
  2. *CDP `Fetch` layer.* `Fetch.enable {urlPattern: "*"}` on the page and on every auto-attached iframe/worker target of the same context (`waitForDebuggerOnStart`, so nothing runs before interception is on). It fails (`BlockedByClient`) non-network schemes (`file:`, `chrome:`, …; Fetch sees `file:` navigations, which never reach a proxy — verified), IP-literal and localhost destinations the policy refuses, and top-level navigations to public addresses when `browser_external = "subresources"`. Host names are left to the proxy, which does the pinned resolution. WebSockets are invisible to Fetch; the proxy covers them.
  - The **default context** points at a dead proxy (`--proxy-server=http://127.0.0.1:1`), and `--host-resolver-rules="MAP * ~NOTFOUND , EXCLUDE 127.0.0.1"`, `--disable-quic`, `--force-webrtc-ip-handling-policy --webrtc-ip-handling-policy=disable_non_proxied_udp` and `--dns-prefetch-disable` keep the browser from resolving or connecting on its own.
  - Classes (`vk-browser::policy`): **loopback** incl. `0.0.0.0`/`::` and IPv4-mapped/compatible/NAT64 forms — allowed only on ports of this machine's previews with status declared/up/down (suggestions don't count; `browser.open v3` on a suggestion promotes it, as `preview.open` does); **metadata** (`169.254.169.254`, `169.254.170.2`, `fd00:ec2::254`, `100.100.100.200`) — only an allow rule naming the address unlocks it, never a host rule; **link-local**; **private** (RFC 1918, CGNAT `100.64/10` — which includes Tailscale addresses —, `198.18/15`, ULA, site-local); **reserved** (multicast, broadcast, `240/4`, `0/8`, documentation ranges) always denied; **public** per `browser_external`. `browser_allow_private` entries are CIDRs or host names, each with an optional `:port`.
  - Reasons (stable strings): `loopback_port_not_a_preview`, `metadata_address`, `link_local_address`, `private_address`, `reserved_address`, `external_denied`, `external_navigation`, `scheme_not_allowed`, `unresolvable`, `bad_target`. Every denial lands in the session's network ring (`blocked_by_policy`, `layer: proxy|fetch|api`), as a `browser.request_denied {session, url, reason, layer, resource_type}` event (≤ 20 per session per 10 s), and — for `browser.open`/`browser.navigate`, pre-checked before navigating and re-checked for denied redirect hops afterwards — as a `destination_denied` error.
- **Capture.** `Runtime.consoleAPICalled`, `Runtime.exceptionThrown`, `Log.entryAdded` and dialogs → console ring; `Network.requestWillBeSent/responseReceived/loadingFinished/loadingFailed` plus policy denials → network ring; 500 entries each. The sampled `preview.console_error` event is not built.
- **Install.** `vibeke browser install` plans a pinned Chrome-for-Testing `chrome-headless-shell` (153.0.8010.12) download, shows it and asks (`--yes` without a terminal), downloads with `curl --proto =https`, and verifies SHA-256 **before** unpacking (staging dir, then rename). Chrome for Testing publishes no checksums and none is recorded yet, so today `--sha256 <hex>` (verified out of band) is required; recorded hashes go into `vk_browser::install::PINNED_SHA256`. Tests mock the download.
- Not built: `--device` presets, one-shot screenshots without a session, `preview.console_error` events, limiting a session to *its own* previews (all of the machine's declared previews are reachable), dedicated service-worker target handling beyond auto-attach (the proxy still covers their traffic).

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

**As built (Goal 03 Stage 4; `vk-server::screenshots`, `vk-review::screenshot`, `vk-browser::diff`):**
- **Record.** Every screenshot is a `screenshot` entity (`ScreenshotMeta`, full shape in 07 §2.11 `screenshot.*`) next to its content-addressed PNG blob (`<state>/blobs/<h2>/<blake3>.png`, 0600, JSON sidecar): id, handle `s<N>`, blob hash, `created_at_ms`, `environment {kind, machine, runner, browser, browser_version, viewport, dpr, color_scheme, device, fresh_context, profile}`, `label`, url + `final_url` (`location.href`) + title, preview handle/id, browser session, requester (`taken_by`, pane, run), `task`, `workspace`, `code`, `runtime`, `binding` + `binding_reason`. `environment.kind` is `remote_headless | local_pane | window | local_proxy` (deviation: `LocalProfile` is called `window`; `headless`/`local_profile` are accepted aliases). Labels: `devbox · headless · fresh context`, `your browser pane · profile devbox`.
- **One capture path.** `browser.screenshot` and the Stage 2 browser pane both call `screenshots::record_screenshot(server, png, ShotInputs) -> ScreenshotMeta` (the pane passes `kind: local_pane`, its profile, and may pass `checkout`/`runtime`). It resolves preview → task/pane, the requesting pane's workspace and live run, and the task (preview's task, else the run's task, else the workspace's task).
- **CodeState** `{repo, origin_url, head_sha, dirty_digest, dirty_state: clean|dirty|unknown, captured_at_ms, warnings}` is captured at screenshot time from the first git checkout among: explicit `checkout`, the task's worktree/repo, the preview pane's cwd, the requesting pane's cwd, the workspace root — with the review observation-baseline digest (`git status --porcelain=v2 -z`, `git diff --binary HEAD`, untracked contents), so a screenshot's code state compares directly with review subjects. `dirty_digest` is `null` when clean (and when capture failed: then `dirty_state: unknown`, never clean). Runs in `spawn_blocking`, never under the core lock. Deviation: no per-fs-watch-generation cache yet (each screenshot runs the git plumbing; ≈ tens of ms on small repos). `task` in `code` is not duplicated: the record's `task` field carries it. No checkout → `code: null` + `code_note`.
- **Running-build identity** (15 §6.4) is separate from checkout identity: `runtime {status: known|unknown, source: probe|header|caller|none, build_id, head_sha, dirty_digest, dirty_state, started_at_ms, fixture, observed_at_ms, detail}`. The server probes `GET /__vibeke_build` on the page's origin — only loopback http on a port of this machine's declared/up/down previews, 1.5 s budget, 64 KiB — and accepts a JSON body (the output of `vibeke screenshot code-state --json`, or `{build_id?, head_sha?, dirty_digest?, …}`) or an `X-Vibeke-Build: <head_sha>[+<dirty_digest>][; build=<id>]` header. Vite/Next expose nothing usable by default, so without that endpoint the identity is `unknown` ("Build not verified"). `[screenshots] probe_build = false` disables the probe.
- **Binding:** `bound` only when the running build reports exactly the captured checkout state (same head SHA; same dirty digest when dirty, none when clean; capture complete); a build id without a SHA, a different SHA (the server still runs an old build) or different uncommitted changes → `illustrative`, with the reason.
- **Retention** (`[screenshots]`, Part C): unreferenced screenshots older than `keep_days` (30) go; at most `max_per_task` (200, the spec's "last 200 per task") per task, oldest unreferenced first; screenshots **referenced by a review acceptance** (same task, showing the accepted head, taken before the acceptance) are kept up to `referenced_keep_days` (365; 15 §11: references pin blobs only within the declared policy). Blobs are deleted with their last record. Runs hourly (and right after a capture, for that task's cap) off the state actor; diff images expire with `keep_days`. "Referenced by an Interaction" is not implemented (no interaction references screenshots yet).
- **Visual diff** (`browser.diff`, `vibeke browser diff a b --out`, MCP `browser_diff`): per-pixel RGBA comparison with a per-channel threshold (`threshold × 255`, default 0.1) instead of YIQ (deviation; simple and deterministic), union canvas for different sizes, changed regions as bounding boxes of 8-connected 16 px cells (≤ 50, largest first), diff PNG (after-image faded grey, changes red) as a blob. Different `environment.kind` refused unless `force`. Not built: `--baseline task-start`, WebP thumbnails.
- **Evidence:** task review packages list the task's screenshots (by task, or taken by a run bound to the task) as `browser` evidence (15 §6.4 note): see 15 §6.4.

### B7. Agents use the same browser: `vibeke mcp`

- `vibeke mcp` is a stdio MCP server (installed via `vibeke integration install <harness> --mcp`), scoped by `VIBEKE_PANE_TOKEN` to the pane's task and machine (09 §3).
- Tools: `preview_list`, `preview_declare {port, path?, label?}`, `browser_open`, `browser_navigate`, `browser_click`, `browser_type`, `browser_press`, `browser_wait`, `browser_eval` (only with capability `browser.script`), `browser_screenshot` (returns MCP image content + `{blob, environment, code}`), `browser_console`, `browser_network`, `browser_dom`, `browser_diff`, `browser_close`.
- Harnesses without MCP use `vibeke browser … --json`; pi/omp get equivalent tools from `@vibeke/pi-extension` returning `ImageContent`.
- **The human sees what the agent saw**: agent screenshots show as a 📷 counter on the pane frame, `prefix+i` opens the gallery, optional inline thumbnail in the sidebar row; remote blobs are fetched once over the blob channel.
- **Watch the agent's browser live**: while an agent has a browser session open, the pane frame shows `◉ browsing`; `vibeke browser watch <session>` (or the action on that chip) opens a browser pane showing the agent's remote headless session via screencast over the link (A7 budgets apply; frames stop when not visible). It is **read-only** by default. **Take over** gives the human input; while taken over, the agent's `browser_*` calls fail with a clear `human_control` error until the human releases it. Screenshots from a watched session keep `environment = RemoteHeadless`.

**As built (Goal 03 Stage 3):**
- `vibeke mcp` (`vk-cli::mcp`): MCP stdio transport (JSON-RPC 2.0, one message per line), protocol versions `2025-11-25` (preferred), `2025-06-18`, `2025-03-26`, `2024-11-05` (a supported requested version is echoed), methods `initialize`, `ping`, `tools/list`, `tools/call` (plus empty `resources/list`/`prompts/list`), batches accepted. Tools: `preview_declare`, `preview_list`, `browser_open`, `browser_navigate`, `browser_click`, `browser_type`, `browser_press`, `browser_wait`, `browser_eval`, `browser_screenshot` (image content + metadata text), `browser_snapshot`, `browser_console`, `browser_network`, `browser_close`; `browser_dom` is `browser_snapshot`; `browser_diff` was added in Stage 4 (diff image as image content + stats). Each call is the matching API method on the pane's server (pane scope by `VIBEKE_PANE_TOKEN` or by process ancestry); API errors become `isError: true` results whose text starts with the Vibeke error kind (`destination_denied: …`, `human_control: …`). It reconnects once if the server restarts.
- `vibeke integration install|uninstall|status <claude|codex|all> --mcp` (`vk-agents::mcp`): Claude Code user scope `mcpServers.vibeke = {type: "stdio", command: <stable vibeke>, args: ["mcp"], env: {}}` in `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json` when redirected; `settings.json` holds no server definitions); Codex `[mcp_servers.vibeke]` in `$CODEX_HOME/config.toml` with `env_vars` for the `VIBEKE_*` variables (Codex filters MCP server environments) and `tool_timeout_sec = 120`, edited with `toml_edit` so comments survive. Same rules as the hook installers (04 §11): merge-only, idempotent, one backup, atomic write with change detection, dry run unless `--yes` (or a redirected config dir); a `vibeke` entry that isn't ours is never overwritten (error) and never removed.
- **Watch/take-over groundwork.** `browser.take_over` / `browser.release` (full scope only) set `human_control`; while set, every pane-scoped call on that session fails with `human_control` (retryable; the user's own calls work). The watch view itself needs the Stage 2 browser pane and isn't built here. What it attaches to: `AgentBrowsers::attach_screencast(session) → ScreencastSub`, a `tokio::sync::watch` receiver of latest-wins JPEG `Frame {seq, data, width, height, received_ms}` (`Page.startScreencast` runs while ≥ 1 subscriber exists, frames are acked on receipt, dropping the last subscriber stops it), and `AgentBrowsers::human_input(session, &[CdpInput])` for the taken-over pane's keys and mouse. Over JSON-RPC (full scope only): `browser.attach_screencast`, `browser.detach_screencast`, `browser.screencast_frame {session, after_seq?}` (latest frame, base64). Watching a remote session from the laptop needs the media channel over the link (Stage 2); not built.

### B8. Displaying screenshots in the TUI

- Kitty graphics–capable terminals: `prefix+i` / `vibeke preview show v4` popup (←/→ history, `d` diff, `o` open the live preview in a browser pane, `c` copy image). A screenshot pane can be split into a tab and refreshes on new screenshots.
- No graphics: metadata + `[o] open image locally` (temp file + `open`/`xdg-open`). Sixel M6.
- Transmission: kitty `t=s`/`t=t` (shared memory / temp file) when the client is local to the terminal, else chunked `t=d`; cached per terminal session. Same code path as the browser pane's tiles (B3.2).

**As built (Goal 03 Stage 4, server/CLI side only):** the data the TUI needs exists — `screenshot.list {task?, preview?, run?, since?}` / `screenshot.get` (path, environment label, binding, code state), `screenshot.captured {id, task, binding}` events for 📷 counters, `browser.diff` for `d`, and the no-graphics fallback as `vibeke screenshot open <sN>` (temp file + `open`/`xdg-open`). Not built (TUI): the `prefix+i` gallery popup and `vibeke preview show`, the screenshot pane, 📷 badges on pane frames, sidebar thumbnails, the screenshot action in the browser pane (which calls `record_screenshot` with `local_pane`), and kitty transmission of screenshot images.

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
allow_remote_egress = true     # remote-routed profiles may egress via the machine (local side opens egress: channels; the remote's bridge accepts them)
browser         = ""           # window browser binary (Chromium family); empty = discover (Stage 1)
local_browser   = "profile"    # profile | default
proxy_port      = 47800
tls_origin      = false
browser_path    = ""           # headless browser binary (auto)
browser_idle    = "10m"
browser_external = "subresources"   # deny | subresources | allow
browser_allow_private = []     # CIDRs/hosts the headless browser may reach (`10.0.0.0/8`, `db.internal:5432`, `[::1]:9229`)
browser_script  = false        # grants browser.eval to pane-scoped callers (capability browser.script)
default_viewport = "1440x900"
screenshot_format = "png"
inline_thumbnails = true

[screenshots]                  # Stage 4 (B6)
keep_days            = 30      # unreferenced screenshots older than this are deleted (0 = never)
max_per_task         = 200     # per-task cap, oldest unreferenced first (0 = none)
referenced_keep_days = 365     # screenshots referenced by a review acceptance
probe_build          = true    # probe /__vibeke_build on preview ports for the running-build identity

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
  - *Stage 3:* take-over/`human_control` is built and tested (in-process with a fake CDP browser); `vibeke browser watch` waits for the Stage 2 browser pane.
- `vibeke preview open v4 --window` for a remote Vite app opens `http://localhost:<port>` in the Vibeke Chrome profile; HMR works with a project that hard-codes `server.hmr.clientPort`; a `localhost` OAuth callback URL completes; no local port is bound.
- A non-browser process connecting to the SOCKS listener is rejected (peer check).
- Proxy mode (B4): two tasks of the same Next.js app open on different `*.vibeke.localhost` hosts with independent login cookies; an upstream `Set-Cookie: x=1; Domain=vibeke.localhost` arrives host-only; the app never sees `vk_token` or `__Host-vk_preview`; `curl` with a forged Host or no credential gets 403/401.
- Mirror mode is never active unless explicitly enabled per preview.
- Headless browser: a page that redirects to `http://127.0.0.1:5432`, loads an `<img src=http://169.254.169.254/>`, or opens a WebSocket to an undeclared loopback port gets all three blocked and logged.
  - *Automated (Stage 3, `crates/vibeke/tests/browser.rs`, `VIBEKE_BROWSER_TESTS=1`):* fetch + WebSocket to an undeclared loopback port, the metadata `<img>`, a redirect hop to a forbidden port and a `file:` navigation are denied and logged (network ring, `browser.request_denied` events, `destination_denied` errors) and the forbidden service sees no connection, while the declared preview loads. Unit tests cover the proxy (redirect hop, WebSocket `CONNECT`, rebinding: resolve once and pin, mixed answers).
- Every screenshot carries `environment` and (for task previews) `code.head_sha` + `dirty_digest`; the TUI labels headless vs profile screenshots.
  - *Automated (Stage 4):* `crates/vibeke/tests/browser.rs` (`VIBEKE_BROWSER_TESTS=1`) takes real-Chromium screenshots of a declared preview in a git checkout and asserts environment, `code.head_sha`, `dirty_state`, running-build probe, `bound` → `illustrative` after a new commit, list/get/open and `browser diff --out`; in-process tests cover clean/dirty code state, binding, retention, diff and pane scope. The TUI labels are not built.
- An agent on devbox opens a session, clicks a button and calls MCP `browser_screenshot` → gets an image. The laptop TUI shows a 📷 badge within 1 s, and `prefix+i` displays it via kitty graphics in Ghostty, with the open-locally fallback working in Terminal.app.
- `browser logs v4 --level error` shows a thrown exception from the page. `browser diff` between before/after screenshots outputs a changed ratio and a diff image.
  - *Stage 4:* `vibeke browser diff s1 s2 --out diff.png` outputs `changed_ratio`, regions and the diff PNG (real-Chromium test above).
- Clicking `http://localhost:5173` printed in a remote pane opens the forwarded preview URL.

**Post-1.0 (QUIC):**
- Switching Wi-Fi networks mid-session keeps the remote session attached with no visible reconnect (< 1 s stall).
- Predictive echo makes typing in a remote shell at 200 ms RTT feel local, with mispredictions rolled back.
- Blocked UDP falls back to SSH automatically.
