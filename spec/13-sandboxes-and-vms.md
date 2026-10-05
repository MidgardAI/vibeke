# 13 — Sandboxes and VMs: isolated execution as an alternative

**Status: Phase 1 scope (M3 + M4).** This supersedes the "container/microVM designed now, implemented after Phase 1" notes in [05](05-tasks-isolation-and-worktrees.md) §4/§14 and the cloud-sandbox non-goal in [00](00-vision-and-scope.md). Cloud-hosted runners remain Phase 2; **local** sandboxes, containers and VMs are Phase 1.

## 1. Why this is in Phase 1

Many users run agents in "yolo" mode on the machine where Vibeke runs: `claude --dangerously-skip-permissions`, Codex with `--dangerously-bypass-approvals-and-sandbox` / `--yolo`, pi (which has no approval system at all), omp in auto modes. That is the fastest way to work, and the most dangerous: one prompt-injected README can read `~/.ssh`, `~/.aws`, browser cookies, other repos, or push to production.

Vibeke's job is not to forbid yolo but to make **"yolo, but contained" a single flag**. The user chooses per task (or per workspace default) how much isolation they want; Vibeke makes every level feel identical: same panes, same agent states, same previews, same review flow.

## 2. Two orthogonal axes

| Axis | Values | Defined in |
|---|---|---|
| **Code isolation** — which checkout the agent edits | `none` (shared repo), `worktree`, `jj`, `clone` (new: private clone inside the sandbox) | 05 §4 |
| **Execution isolation** — where processes run and what they can touch | `host`, `sandbox`, `container`, `vm` (+ `ssh` = another machine, 06) | this file |

Any combination is valid except `vm`/`container` + `none` (sandboxed agents never share the host repo directly; see §6).

### 2.1 Execution levels

| Level | What it is | macOS | Linux | Start (warm) | Protects against | Doesn't protect against |
|---|---|---|---|---|---|---|
| `host` | Today's behavior. Process runs as you. | — | — | 0 | nothing | everything |
| `sandbox` | OS-level confinement of the pane's process tree: filesystem allowlist, network via egress proxy, no access to other repos/home secrets | Seatbelt profile (`sandbox-exec`; same mechanism Claude Code and Codex use for their own sandboxes) | bubblewrap + Landlock + seccomp; net namespace + proxy | < 50 ms | reading `~/.ssh`/`~/.aws`/other repos, arbitrary exfiltration (egress allowlist), writes outside the worktree | kernel exploits; anything allowed through the profile; same-UID IPC edge cases |
| `container` | OCI container with the checkout mounted | Apple `container` (one lightweight VM per container), OrbStack, Docker Desktop, Colima/Lima | Podman rootless (preferred), Docker | < 2 s | as above + separate userland/toolchain, resource limits, clean env | kernel shared (Linux) — Docker Desktop/Apple container/OrbStack on macOS are VM-backed anyway |
| `vm` | Full microVM / VM per task with its own kernel | Apple Virtualization.framework via Lima (`vz`), Tart, or Apple `container` machines | Firecracker / Cloud Hypervisor (KVM), Lima (qemu) | < 3 s warm, < 1 s from snapshot (Firecracker) | kernel-level escapes; enables snapshot/fork | — (strongest local option) |

`docker sandbox` (Docker Sandboxes, microVM-backed, with "Kits") is supported as a `container`-level provider when present.

## 3. User experience

```bash
vibeke task new "upgrade next to 16" --agent claude --yolo            # yolo ⇒ default yolo isolation (config; default "vm" if available, else "container", else "sandbox")
vibeke task new "fix flaky test" --agent codex --isolate sandbox      # explicit level, approvals still on
vibeke task new "try 3 approaches" --agents claude:2,codex:1 --isolate vm   # best-of-N, one VM each (forked from one warm snapshot)
vibeke agent start --kind pi --pane w3:p2 --isolate container         # also for ad-hoc panes, not only tasks
```

- **`--yolo`** = launch the harness with its approval-bypass flags (from the harness manifest, 04) **and** apply `isolation.yolo_default`. Running yolo on `host` requires `--yolo --isolate host` plus a one-time confirmation per workspace, and the pane gets a permanent red `YOLO·HOST` badge.
- **User-typed yolo is supported, never blocked.** Vibeke is a terminal: if you type `codex -a never -s danger-full-access`, `codex --yolo`, `claude --dangerously-skip-permissions`, or run pi, it runs exactly as typed — no interception, no confirmation. The adapter detects the effective mode from (a) the process argv (manifest `[yolo] detect_args`), (b) hook payloads (`permission_mode`, Codex approval/sandbox policy from `SessionStart`/thread config), and (c) the harness's own status line as a fallback, and marks the run `yolo: true`, `execution: host` → red `YOLO·HOST` badge, `agent.state` still fully tracked. The confirmation prompt applies only when *Vibeke* launches a yolo run on the host (`--yolo --isolate host`), and is skippable per workspace with `isolation.confirm_host_yolo = false`.
- Optional nudge (off by default, `isolation.suggest_sandbox_for_yolo = true`): a one-line hint in the sidebar "relaunch this in a sandbox?" that restarts the run with `codex resume <id>` inside the chosen isolation level.
- Sidebar shows an isolation glyph per pane/agent row: none · `sbx` · `ctr` · `vm` (configurable icons), and network profile when not default.
- Workspace and repo defaults: `.vibeke/sandbox.toml` (trusted like other repo config, 09) and `[isolation]` in user config.
- Everything else is unchanged: panes, agent states, Interactions, previews, screenshots, search, events.

## 4. Architecture: a sandbox is a machine

The key design decision: **container and VM runners reuse the remote-machine stack from 06.** Inside the container/VM runs `vibeke bridge` (and the holders); the host server connects to it like to an SSH machine, but over a local transport:

| Level | Transport host ↔ inside | Holder location |
|---|---|---|
| `sandbox` | none needed — holder runs on host, *child* is spawned under the sandbox profile (`sandbox-exec -p … ` / `bwrap …` wrapper in the holder spawn path) | host |
| `container` | unix socket bind-mounted into the container (`/run/vibeke/bridge.sock`), or `docker exec -i vibeke bridge` stdio | inside |
| `vm` | vsock (Firecracker, Virtualization.framework) or virtio-serial; fallback SSH over the VM's private network | inside |

Consequences, all for free from 06/07:
- Process durability: holders inside the box survive host server restarts.
- **Previews**: ports inside the box are discovered and forwarded exactly like a remote machine — dev servers in a VM get `*.vibeke.localhost` URLs and screenshots with no extra work.
- Clipboard, image paste, `vibeke mcp` for agents, and the event stream behave identically.
- The `vibeke` binary inside the box is the same static Linux binary used for remote bootstrap (injected read-only, never fetched from the network by the box).

`Runner` (05 §14) gains concrete Phase 1 implementations: `HostRunner`, `SandboxRunner`, `ContainerRunner{provider}`, `VmRunner{provider}`, `SshRunner`. Providers are pluggable (`vk-sandbox` crate, provider trait), so Docker Sandboxes, OrbStack machines, Tart, or later cloud providers (E2B, Daytona, Modal, Morph) slot in without touching the task model.

## 5. Filesystem

| Mount | sandbox | container / vm |
|---|---|---|
| Task checkout | rw (worktree path only) | rw via bind mount / virtiofs **or** private clone (§6) |
| Rest of `$HOME` | **denied** except an allowlist (`~/.gitconfig` ro, harness config dirs per manifest, toolchain caches ro) | not mounted |
| Toolchains | host toolchains visible ro (`/usr`, `/opt/homebrew`, `~/.cargo/bin`, `~/.bun`…) | provided by the image |
| Package caches | ro host cache + rw overlay, or shared rw cache volume (opt-in) | named cache volumes per ecosystem (npm/pnpm store, cargo registry, pip) shared across tasks |
| Temp | private `$TMPDIR` | private |

Filesystem allowlists are generated from: global profile + harness manifest `[sandbox]` section (e.g. Claude needs `~/.claude/` state dir) + repo `.vibeke/sandbox.toml` (can only *narrow* unless the user approves widening, 09).

## 6. Git safety at the boundary

A sandboxed agent must not be able to plant code that later runs **on the host**:
- **`.git` hooks/config injection**: a worktree shares the main repo's `.git` (hooks, config, `core.fsmonitor`, filters). Mounting it rw into a yolo box would let the agent write `.git/hooks/post-checkout` that runs on the host. Therefore:
  - `container`/`vm` default to **`clone` code isolation**: inside the box, `git clone --reference`-style private clone (objects shared read-only, own `.git`), branch created there. Results come back by **fetch**: `vibeke task sync` runs `git fetch <box-remote> <branch>:<task-branch>` from the host side (the host pulls; the box never writes host `.git`). Fetch is safe: it doesn't execute repo-controlled code.
  - `sandbox` with `worktree` mounts the worktree rw but the common `.git` dir **read-only except** `objects/`, `refs/heads/<task-branch>`, `logs/`, `worktrees/<slug>/` (Seatbelt/Landlock path rules). `hooks/`, `config`, `info/` are never writable.
- Host-side git operations on task branches run with `-c core.hooksPath=/dev/null -c core.fsmonitor=false` when the task was yolo.
- `git push` from inside a box is a **boundary action** (§8): disabled by default (no credentials inside), done by the host after review.

## 7. Network

- All egress from `sandbox`/`container`/`vm` goes through a host-side **egress proxy** (`vk-sandbox` HTTP CONNECT + SOCKS5, with SNI/Host-based domain allowlist; DNS resolved by the proxy). Direct egress is blocked (Seatbelt `network-outbound` rules / netns / VM with no default route).
- Profiles (per task, default from config):
  - `offline` — only the model provider endpoints required by the harness.
  - `dev` (default) — provider endpoints + package registries (npm, pnpm, yarn, PyPI, crates.io, Go proxy, RubyGems, Maven Central, GitHub release/raw for downloads) + `localhost` services the user declared.
  - `open` — everything, logged.
- Harness manifests declare their required endpoints (`[sandbox.network] allow = ["api.anthropic.com", "statsig.anthropic.com", …]`).
- Every denied connection becomes a `sandbox.egress_denied` event and (rate-limited) an `Interaction{kind: approval}` "agent wants to reach `example.com` — allow once / for task / always". This is the yolo-safe replacement for per-command approvals: **commands are free, the boundary is gated.**
- Inbound: nothing except the bridge channel; previews go through the bridge forwarder (06).

## 8. Credentials

Agents need model credentials inside the box, and should get nothing else.

| Harness | Host storage | Projection into box |
|---|---|---|
| Claude Code (subscription) | macOS Keychain | Vibeke obtains a long-lived token via `claude setup-token` (one-time, user-initiated) and injects `CLAUDE_CODE_OAUTH_TOKEN` env into the box's harness process only. API-key users: `ANTHROPIC_API_KEY`. |
| Codex | `~/.codex/auth.json` | copied into an ephemeral `CODEX_HOME` inside the box (0600), deleted on teardown |
| pi / omp | provider env vars / auth files per pi `auth` | env injection of only the providers the run uses |
| Custom | manifest `[auth]` section: `env = [...]`, `files = [...]` | as declared |

- Credential projection is declared in the harness manifest (04) and shown on first use ("Codex in VM will receive your ChatGPT auth token. Network: dev profile.").
- Git push credentials, SSH keys, cloud credentials are **never** projected. Boundary actions that need them (push, PR creation, deploy) run on the host after an Interaction.
- Exfiltration risk of the model token is accepted and mitigated by the egress allowlist (token is only useful to the provider endpoints).

## 9. Images, templates and warm pools

- `.vibeke/sandbox.toml` per repo:
  ```toml
  [sandbox]
  level = "container"            # default level for tasks in this repo
  image = "ghcr.io/acme/dev:node22"   # or build from Dockerfile / devcontainer
  devcontainer = ".devcontainer/devcontainer.json"   # reuse existing devcontainers (features, postCreate)
  setup = ".vibeke/setup.sh"     # runs once per template, cached
  network = "dev"
  cpus = 4
  memory = "8G"
  disk = "30G"
  ```
- **Template snapshot**: after image + setup, Vibeke snapshots a template (container image commit / VM disk + memory snapshot where supported). New tasks start from the template: deps already installed.
- **Warm pool**: keep N (default 1) pre-booted VMs/containers per frequently used template; claiming one is the < 1 s path.
- **Fork**: best-of-N tasks fork the same template snapshot; VMs that support memory snapshots (Firecracker, Virtualization.framework save/restore on macOS 14+) fork in < 1 s. This also lays the Phase 2 groundwork for "branch this agent at turn N" (`Runner::snapshot`/`fork`).
- Devcontainer support means most repos with a `.devcontainer/` work with zero Vibeke-specific config.

## 10. Policy interplay

| Level | Approvals | Vibeke policy engine (02 §4) |
|---|---|---|
| `host` (non-yolo) | harness asks, Vibeke can gate natively | full: allow/deny/ask rules |
| `host` + yolo | none | can't gate (harness doesn't ask); Vibeke shows red badge; `deny` rules still enforced for harnesses with pre-tool hooks (Claude PreToolUse, pi/omp `tool_call`) — "yolo with a seatbelt" |
| `sandbox`/`container`/`vm` + yolo | none inside | **boundary policy**: egress, push, credential use, port exposure, copying artifacts out |
| `sandbox`/`container`/`vm` non-yolo | as host | full, plus boundary policy |

## 11. Resource and lifecycle management

- Limits per box: cpus, memory, disk, pids; defaults from config; shown in the sidebar on hover; `sandbox.resource_pressure` events.
- Lifecycle tied to the task: `task park` stops (container) or suspends (VM save state) the box; `task resume` restores; `task archive` tears down after syncing the branch. Idle boxes auto-suspend after `isolation.idle_suspend = "30m"` when no agent is `working`.
- Crash handling: if the box dies, panes show `exited` with reason `runner_lost`; agents with resume handles can be resumed in a fresh box from the template + synced branch.
- `vibeke sandbox list|shell <task>|logs|prune` for debugging; `vibeke doctor` checks providers (Apple `container` version, OrbStack/Docker socket, Lima, KVM access, Seatbelt availability, bwrap/Landlock kernel support).

## 12. Events and data model additions (02)

- Task gains `execution: { level: host|sandbox|container|vm, provider?, profile, network, yolo: bool, runner_id? }`.
- Pane gains `execution` (inherited from task or set ad-hoc).
- Events: `sandbox.created`, `sandbox.started`, `sandbox.suspended`, `sandbox.resumed`, `sandbox.destroyed`, `sandbox.egress_denied {host, port}`, `sandbox.egress_allowed {rule}`, `sandbox.resource_pressure`, `sandbox.boundary_action {kind: push|copy_out|credential_use, approved}`, `task.synced {commits}`.

## 13. Milestones

| Milestone | Scope |
|---|---|
| **M2** | Harness manifests gain `[yolo]` flags, `[sandbox]` fs/network needs and `[auth]` projection (data only). `deny`-rules enforced for yolo-on-host via pre-tool hooks. Red `YOLO·HOST` badge. |
| **M3** | `sandbox` level (Seatbelt on macOS, bubblewrap+Landlock+seccomp on Linux) + egress proxy + network profiles + egress Interactions. `container` level with Apple `container`, OrbStack/Docker, Podman providers; `clone` code isolation + `task sync`; devcontainer support; credential projection for Claude, Codex, pi, omp. Reuses the M3 bridge. |
| **M4** | `vm` level (Lima `vz`/Tart on macOS, Firecracker/Cloud Hypervisor on Linux), template snapshots, warm pool, fork for best-of-N; previews/screenshots verified inside containers and VMs. |
| **Phase 2** | Cloud runners (E2B, Daytona, Modal, Morph, Docker Cloud Sandboxes) via the same provider trait; move a running task laptop → cloud; snapshot at turn N. |

## 14. Acceptance criteria

1. `vibeke task new "x" --agent claude --yolo` on a Mac with Apple `container` or OrbStack: agent is working inside a container in ≤ 5 s cold / ≤ 2 s warm; `cat ~/.ssh/id_ed25519` from the agent fails; `curl https://example.com` is denied and produces an Interaction; `pnpm install` works under the `dev` profile.
2. A dev server started by the agent inside the container appears as a preview with a `*.vibeke.localhost` URL and a working screenshot.
3. The agent writes `.git/hooks/pre-commit` in its checkout; after `vibeke task sync`, no hook exists or runs on the host repo.
4. `sandbox` level on macOS and Linux: reading any path outside the allowlist fails with EPERM; the pane's agent states, Interactions and previews work exactly as on `host`.
5. Killing the host server while a yolo agent works inside a VM: after restart the pane reattaches with no process loss.
6. Best-of-3 with `--isolate vm` from a warm template starts all three in ≤ 5 s on an M-series Mac.
7. `vibeke doctor` reports which levels and providers are available, with a fix hint for each missing one.
