# 13 — Sandboxes and VMs: isolated execution as an alternative

**Status: Phase 1 scope (yolo detection M1, `sandbox` + `container` M2, `vm` M4 — see [11](11-milestones.md)). Implementation status in §15: the sandbox level is built on macOS; the Linux backend is generated but unverified; `container` is groundwork; `vm` is not started.** This supersedes the "container/microVM designed now, implemented after Phase 1" notes in [05](05-tasks-isolation-and-worktrees.md) §4/§14 and the cloud-sandbox non-goal in [00](00-vision-and-scope.md). Cloud-hosted runners remain Phase 2; **local** sandboxes, containers and VMs are Phase 1.

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

### 2.2 Guardrails vs containment (the security line)

Vibeke is explicit about which levels **enforce** anything against a misbehaving or prompt-injected agent (09 §2):

| Level | Classification | Why |
|---|---|---|
| `host` (with or without yolo) | **Cooperative guardrails only** | The agent runs as your UID. It can read the pane token from its environment, edit harness hook configs, call the harness's own CLI with other flags, connect to Vibeke sockets, or type into its own TTY. Policy `deny` rules, hook gates, and the "can't answer your own interaction" rule stop *mistakes and cooperative agents*, not an adversary. The UI never calls a host run "contained". |
| `sandbox` | **Enforced containment** (filesystem + network + Vibeke API) | The process tree runs under a kernel-enforced profile: no access outside the allowlist, egress only via the proxy, and **no access to Vibeke's privileged sockets or state** (§4.1). |
| `container` / `vm` | **Enforced containment**, stronger | Separate userland (container) or kernel (vm); only the broker channel crosses the boundary. |

Rules that depend on enforcement (boundary actions, egress approvals, "agent cannot answer its own interactions" as a *security* property) are only claimed for `sandbox`/`container`/`vm` runs.

## 3. User experience

```bash
vibeke task new "upgrade next to 16" --agent claude --yolo            # yolo ⇒ default yolo isolation (config; default "vm" if available, else "container", else "sandbox")
vibeke task new "fix flaky test" --agent codex --isolate sandbox      # explicit level, approvals still on
vibeke task new "try 3 approaches" --agents claude:2,codex:1 --isolate vm   # best-of-N, one VM each (forked from one warm snapshot)
vibeke agent start --kind pi --pane w3:p2 --isolate container         # also for ad-hoc panes, not only tasks
```

- **`--yolo`** = launch the harness with its approval-bypass flags (from the harness manifest, 04) **and** apply `isolation.yolo_default`. Running yolo on `host` requires `--yolo --isolate host` plus a one-time confirmation per workspace, and the pane gets a permanent red `YOLO·HOST` badge.
- **User-typed yolo is supported, never blocked.** Vibeke is a terminal: if you type `codex -a never -s danger-full-access`, `codex --yolo`, `claude --dangerously-skip-permissions`, or run pi, it runs exactly as typed — no interception, no confirmation. The adapter detects the effective mode from (a) the process argv (manifest `[yolo] detect_args`), (b) hook payloads (`permission_mode`, Codex approval/sandbox policy from `SessionStart`/thread config), and (c) the harness's own status line as a fallback, and marks the run `yolo: true`, `execution: host` → red `YOLO·HOST` badge. All state facets (04 §2.4: liveness, execution state, pending interactions, adapter health) are still tracked; with `-a never` there simply are no approval interactions. For Codex, the PATH shim (04 §6.2) still adds only `--disable daemon_auto_start` so the run is deterministically bound to its pane; all user arguments pass through untouched. The confirmation prompt applies only when *Vibeke* launches a yolo run on the host (`--yolo --isolate host`), and is skippable per workspace with `isolation.confirm_host_yolo = false`.
- Optional nudge (off by default, `isolation.suggest_sandbox_for_yolo = true`): a one-line hint in the sidebar "relaunch this in a sandbox?" that restarts the run with the harness's resume argv (e.g. `codex resume <id>`) inside the chosen isolation level. Only offered when the run has the `resume` capability (04 §2.2).
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

### 4.1 The broker: the only Vibeke API inside a box

Contained agents still need to talk to Vibeke (hook shims, the pi/omp extension, `vibeke preview declare`, `vibeke mcp`). They never get the main control socket:

- Inside `sandbox`/`container`/`vm`, `VIBEKE_SOCKET` points to a **per-pane broker socket** (sandbox: an allowlisted path under the pane's private runtime dir; container: bind-mounted; vm: vsock port). The broker exposes only pane scope (09 §5.2): `adapter.*` for its own pane, `preview.declare`, `browser.*` against its own task's previews, read-only `agent.get` for its own run.
- The broker **cannot** reach: `vibeke.sock`, holder sockets, `state.db`, other panes, `interaction.answer` (for any pane), `pane.send_keys`/`send_text` (for any pane, including its own), plugin APIs, or elevation (`vibeke auth elevate` is unavailable inside a box).
- The sandbox profile/mount table denies the runtime dir (`$XDG_RUNTIME_DIR/vibeke/`), `~/.local/state/vibeke/`, and `~/.config/vibeke/` to the contained process tree.
- Self-answering: a contained agent cannot inject approval keystrokes into its own dialog through Vibeke (no input methods on the broker). It *can* still write to its own TTY directly — that only affects its own harness's dialog, and is why approvals *inside* a yolo box are not a security boundary; the boundary is egress/push/credentials (§7–8, §10).

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

**Dropped/pasted local files** (06 A11): the box sees a read-only `/vibeke/inbox` (container/VM) or the host inbox dir on the sandbox allowlist. When you drag a file from `~/Desktop` onto a sandboxed pane, the client copies it into the inbox and rewrites the pasted path — the allowlist is never widened to the file's original location.

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
- Boundary approvals are Vibeke-only enforcement and therefore **fail closed** (04 §2.7): if the server is down or the proxy loses its policy, connections are denied, never passed through. Decisions go through the same delivery transaction as other interactions (04 §7.3); the proxy holds the connection attempt (≤ 30 s) or refuses it and lets the agent retry after approval.
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

| Level | Approvals inside | Vibeke policy engine (02 §4) | Classification (§2.2) |
|---|---|---|---|
| `host` (non-yolo) | harness asks; Vibeke uses `gate`/`answer_native` where the capability matrix allows (04 §2.3) | allow/deny/ask rules | cooperative guardrail |
| `host` + yolo | none (harness doesn't ask) | `deny` rules still applied for harnesses with pre-tool hooks (Claude `PreToolUse`, Codex `PreToolUse` for Bash) — "yolo with a seatbelt". **Not for pi/omp**: Vibeke has no gate there (04 §6.3); pi/omp yolo on the host gets the badge only — use `--isolate` for real protection. Fail-closed per 04 §2.7, but bypassable by the agent itself | cooperative guardrail |
| `sandbox`/`container`/`vm` + yolo | none inside | **boundary policy**: egress, push, credential use, port exposure, copying artifacts out — fail-closed | enforced |
| `sandbox`/`container`/`vm` non-yolo | as host | in-box rules (guardrail) **plus** boundary policy (enforced) | enforced at the boundary |

## 11. Resource and lifecycle management

- Limits per box: cpus, memory, disk, pids; defaults from config; shown in the sidebar on hover; `sandbox.resource_pressure` events.
- Lifecycle tied to the task: `task park` stops (container) or suspends (VM save state) the box; `task resume` restores; `task archive` tears down after syncing the branch. Idle boxes auto-suspend after `isolation.idle_suspend = "30m"` when no run's execution state is `working` and no interaction is pending.
- Crash handling: if the box dies, its panes' liveness becomes `exited{reason: runner_lost}` and open interactions are cancelled (04 §2.5 rule 1); runs with the `resume` capability can be resumed in a fresh box from the template + synced branch.
- `vibeke sandbox list|shell <task>|logs|prune` for debugging; `vibeke doctor` checks providers (Apple `container` version, OrbStack/Docker socket, Lima, KVM access, Seatbelt availability, bwrap/Landlock kernel support).

## 12. Events and data model additions (02)

- Task gains `execution: { level: host|sandbox|container|vm, provider?, profile, network, yolo: bool, runner_id? }`.
- Pane gains `execution` (inherited from task or set ad-hoc).
- Events: `sandbox.created`, `sandbox.started`, `sandbox.suspended`, `sandbox.resumed`, `sandbox.destroyed`, `sandbox.egress_denied {host, port}`, `sandbox.egress_allowed {rule}`, `sandbox.resource_pressure`, `sandbox.boundary_action {kind: push|copy_out|credential_use, interaction, outcome}` (outcome follows the delivery states of 04 §7.3), `task.synced {commits}`.
- Run gains `containment: guardrail|enforced` (derived from level, §2.2), exposed with `run.capabilities`.

## 13. Milestones

| Milestone | Scope |
|---|---|
| **M1** | Harness manifests gain `[yolo]` flags/`detect_args`, `[sandbox]` fs/network needs and `[auth]` projection (data only). `deny` rules applied for yolo-on-host via pre-tool hooks (cooperative guardrail). Red `YOLO·HOST` badge; user-typed yolo detection; Codex PATH shim. |
| **M2** | `sandbox` level (Seatbelt on macOS, bubblewrap+Landlock+seccomp on Linux) + per-pane broker (§4.1) + egress proxy + network profiles + fail-closed egress Interactions. `container` level with Apple `container`, OrbStack/Docker, Podman providers; `clone` code isolation + `task sync`; devcontainer support; credential projection for Claude, Codex, pi, omp. Brings the local bridge transport (06) forward from M3 for containers; SSH machines stay in M3. |
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
8. From inside a `sandbox`, `container` or `vm` run: connecting to `vibeke.sock` or any holder socket fails; reading `state.db` fails; the broker rejects `interaction.answer` and `pane.send_keys` for every pane; `vibeke auth elevate` is unavailable.
9. With the Vibeke server stopped, a contained yolo agent's new egress to a non-allowlisted host is denied (fail-closed), while already-allowlisted provider traffic continues.
10. `codex -a never -s danger-full-access` typed in a host pane runs with exactly those arguments (plus the shim's daemon flag), shows `YOLO·HOST`, and its run is bound to the pane (hooks carry the pane token).

## 15. Implementation status (M2, 2026-10-06)

**Built: the `sandbox` level on macOS (tested with real `sandbox-exec`), the egress proxy with network profiles and egress Interactions, the per-pane broker, credential projection, `--yolo` / `--isolate` / `--network`, and paste translation for local sandboxed panes.** The Linux backend exists as generated, unit-tested configuration that has **never run on a Linux host**. `container` is groundwork only. `vm` is a placeholder.

Code: `crates/vk-sandbox` (runner abstraction, policy, Seatbelt and Linux generators, proxy, credentials, env, the `vibeke sandbox exec` helper, the `[isolation]` config) and `crates/vk-server/src/sandbox.rs` (per-task contexts, spawn wrapping, broker, egress Interactions, restore, `sandbox.*` API).

| Area | Status |
|---|---|
| Runner (05 §14) | `vk_sandbox::Runner`: `HostRunner`, `SandboxRunner`, `ContainerRunner` (groundwork), `VmRunner` (placeholder). It produces the argv wrapper, scrubbed env, cwd and mounts. Holders keep owning the PTY on the host. The holder's child is `sandbox-exec -f <profile> $SHELL -l`. |
| Seatbelt profile | `(deny default)`, layered last-match-wins. Reads are allowed outside the deny roots: `$HOME`, `/Volumes`, `/Users/Shared`, `/tmp`, `/var/tmp`. Hidden even under a readable root: the Vibeke runtime, state and config dirs, and the user's `$TMPDIR`. Re-allowed: the home allowlist (git config, toolchains, read-only caches), the checkout, the git dirs, the inbox, the pane's private dir, the `vibeke` binary and the shims. Never readable: registry, cloud and SSH credentials next to toolchains (`~/.cargo/credentials*`, `~/.npmrc`, `~/.ssh`, `~/.aws`, …). Writes are allowed only to the checkout, the private dir and `/dev` ttys. Git (§6): the common dir and the worktree dir are read-only except `objects/`, `logs/`, `refs/heads/<task-branch>` and the worktree's own dir. Never writable: `hooks/`, `config`, `info/`, `config.worktree`, `commondir`, `gitdir`, the checkout's `.git` link file, the checkout root itself (so the box can't rename it away) and projected credential files. Mach lookups are a short allowlist: no LaunchServices (`open` would escape), pasteboard, securityd/Keychain, DNS or user preferences. Network is either nothing or `localhost:<proxy>`, plus the task's leased ports and the broker's unix socket. Dev servers may listen on localhost. |
| Linux | Generated from the same policy: bubblewrap arguments (user/pid/ipc/uts/net namespaces; `/` read-only; tmpfs over the deny and hidden roots; binds that mirror the layers); Landlock ABI v1 rules, applied by `vibeke sandbox inner` *after* bwrap, because Landlock forbids mount changes; and a seccomp BPF deny-list (ptrace, process_vm_*, mount/umount/pivot_root/setns/unshare, keyrings, bpf, perf, userfaultfd, modules, kexec, `open_by_handle_at`, `ioctl(TIOCSTI)`; x86_64 and aarch64, with an x32 guard). The chain is `vibeke sandbox bwrap` (seccomp on a pipe fd) → `bwrap` → `vibeke sandbox inner` (Landlock plus a 127.0.0.1:<port> → unix-socket forwarder to the host proxy) → the shell. Unit-tested on macOS; `cargo check`/`clippy` are clean for `{x86_64,aarch64}-unknown-linux-musl`. **Unverified on Linux.** |
| Egress proxy (§7) | HTTP CONNECT plus absolute-form `http://` forwarding on `127.0.0.1:<port>` (and a unix socket on Linux), one per contained task. The proxy resolves names itself, filters every resolved address, then connects to the exact address it checked. Blocked addresses: loopback (except declared ports), link-local, cloud metadata (169.254.169.254, fd00:ec2::254), RFC 1918/ULA (unless `allow_private`), CGNAT, multicast and reserved ranges. It looks through IPv4-mapped and NAT64 forms. Forwarded requests get `Connection: close`, so a second request on the same connection can't reach another host. |
| Network profiles | `none` (no proxy, no network), `harness-apis` (spec `offline`; the alias is accepted), `package-registries`, `dev` (default: harness APIs + registries) and `open` (everything except forbidden IP classes, logged). `[isolation.sandbox]` adds `allow_domains`, `deny_domains`, `local_ports` and `allow_private`. |
| Egress Interactions | A destination on no list opens an `Approval` Interaction ("Allow network access to npmjs.org:443?") on the task's agent pane. No new `InteractionKind` was needed. `allow` = this connection only; `allow_always` = for this task; `deny` = refused, and remembered for 10 min. Concurrent attempts to the same host share one Interaction. The proxy holds a connection for up to 30 s, then refuses it (fail closed); a later answer still applies. An unanswered Interaction expires after 10 min. Interactions left open across a server restart are expired, and the proxy asks again. Events: `sandbox.created`, `sandbox.destroyed`, `sandbox.egress_denied` (rate-limited per host), `sandbox.egress_allowed` (task approvals, `open`, `sandbox.allow`) and `pane.isolation_changed`. |
| Broker (§4.1) | One unix socket per pane, in the pane's private dir. It is the only socket the profile can reach, and `VIBEKE_SOCKET` points to it. Pane scope is fixed by the socket, so tokens can't change it. It serves only `client.hello`, `adapter.signal`, `adapter.gate`, `adapter.delivery_ack`, `agent.report`, `agent.get`, `pane.current` and `preview.declare`. Everything else (`interaction.answer`, `pane.send_*`, layout, tasks, events, `sandbox.allow`, …) returns `permission_denied`. |
| Credentials (§8) | Each harness's config dir is relocated into an ephemeral home under the task's sandbox dir (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`). It is seeded with copies of non-secret config (settings with Vibeke's hooks, instructions, agents/commands/skills, extensions) and a minimal `.claude.json`: no MCP env blocks, and the workspace trust pre-accepted only for yolo launches. Credential files (`~/.claude/.credentials.json`, `~/.codex/auth.json`, `~/.pi/agent/auth.json`) are copied read-only (0400 plus a profile write-deny). Env credentials: `CLAUDE_CODE_OAUTH_TOKEN` (server env, or `<state>/credentials/claude-oauth-token` from `claude setup-token`, which must be 0600), `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, and provider keys for pi/omp. The rest of the env is an allowlist: no cloud keys, `GITHUB_TOKEN` or `SSH_AUTH_SOCK`. Secret values never appear in events, kv records, profiles or `sandbox.list`; only names do (`env:CLAUDE_CODE_OAUTH_TOKEN`, `file:.codex/auth.json`). Values reach the holder through its 0600, read-once spec file, or the 0600 read-once `vibeke sandbox exec --spec` file for launches typed into a pane, so they never show on screen, in scrollback or in shell history. |
| `--yolo` / `--isolate` / `--network` | Supported on `task new`, `agent start` and `agent spawn`. `--yolo` adds the harness's bypass flags and applies `isolation.yolo_default` (default `sandbox`: container and vm are not wired for agents). Explicit `--yolo --isolate host` runs on the host with the red `YOLO·HOST` badge; the same run in a sandbox shows a yellow `YOLO`. A sandboxed task's panes are all contained, including splits and respawns. An isolated agent launched into a host pane gets a run-scoped context: only the agent command is wrapped, the pane shows `sb` while the run lives, and the context is torn down ~15 s after the run ends. |
| Paste/drop (§5, 06 A11.4) | Panes carry `isolation.visible_roots`. For a local sandboxed pane, the TUI translates only paths outside those roots: it copies them into the shared host inbox, which every sandbox can read, and pastes the inbox path. `pane.can_see_paths {pane}` evaluates the pane's profile. |
| Sidebar/doctor | Agent rows and shell-pane rows show the isolation glyph (`[ui.sidebar.isolation_glyphs]`, plus `·<network>` when the profile is not `dev`). `vibeke doctor` has an `isolation` section. `vibeke sandbox status|list|allow`. |
| Model | `Isolation {level, provider, network, yolo, scope, visible_roots}` on `Pane` and `Task`, with `IsolationLevel` appended to the model. The field is named `isolation`, not `execution`, because `AgentRun.execution` is the state facet. Runs derive containment from their pane; `AgentRun` has no new field. |
| `container` | Provider detection (Apple `container`, OrbStack, Docker, Podman) without starting anything. `ContainerRunner` wraps the pane command in `<runtime> run --rm -it`: checkout bind-mounted, `--cap-drop ALL`, `no-new-privileges`, a pids limit, the inbox mounted read-only at `/vibeke/inbox`, and secrets passed by name only (`--env KEY`). It is wired into `task new --isolate container --image X`, but **only for network `none` or `open`**: a bridge network has a default route, so proxy profiles are refused until egress is enforced. The holder stays on the host. The real-run test is gated by `VIBEKE_CONTAINER_TESTS=1`. |
| Not done | The `vm` level; the in-box bridge/holder for containers (§4); `clone` code isolation + `vibeke task sync`; devcontainers, images, templates and warm pools (§9); resource limits and the lifecycle (§11: park/suspend, idle suspend); `vibeke sandbox shell|logs|prune`; repo `.vibeke/sandbox.toml`; SOCKS5; a persistent "always" egress allow; the host-yolo confirmation (`confirm_host_yolo`); a CLI to store the Claude setup-token; `sandbox.boundary_action` events (push/copy-out). |

**Deviations and caveats.**
- `sandbox-exec`/SBPL is deprecated by Apple but still ships. Nested Seatbelt is impossible (`sandbox_apply: Operation not permitted`). Vibeke therefore launches Codex in a sandbox with `--sandbox danger-full-access`, with approvals unchanged: Vibeke's profile replaces Codex's own. Claude Code's optional bash sandbox can't nest either.
- The proxy runs inside the server process. With the server stopped, **all** egress fails, including already-allowlisted provider traffic, so acceptance 9 is only half met. Fail-closed holds. After a restart, task contexts are rebuilt from a kv record (no secrets), the proxy rebinds its old port when free, and brokers rebind at the same paths. Contained processes survive in their holders, but they lose network if the port moved.
- There is no SNI/Host cross-check: domain fronting within an allowed CDN is possible. The egress Interaction's `allow_always` means "for this task", not globally.
- Stat metadata is readable everywhere (`file-read-metadata`, needed for realpath/getcwd): names and sizes of files outside the allowlist are observable, contents are not. The translated-drop inbox is shared, so every sandbox can read every drop (as specified in 06 A11.4).
- Ephemeral harness homes: transcripts of sandboxed runs live in the task's sandbox dir, are deleted on `task finish`, and host-side `claude --resume`/`codex resume` can't see them. Codex refreshes tokens against a read-only `auth.json`: the refresh can't be persisted, and if OpenAI rotates the refresh token, the host's copy may need a re-login. On macOS, Claude needs an env token or the stored setup-token, because the Keychain is never exposed.
- Shell rc files are not readable inside: they often export secrets. `ZDOTDIR` points to a stub that sets a `[sbx]` prompt, and `PATH` comes from the server's env. Caches are private per pane (`XDG_CACHE_HOME`, `npm_config_cache`, `PIP_CACHE_DIR`). `~/.cargo/registry` is read-only unless `[isolation.sandbox] write = ["~/.cargo/registry"]`. `/tmp` and `$TMPDIR` are hidden; `TMPDIR` points into the private dir.
- The setup script still runs on the host, gated by repo trust (09 §4), not in the sandbox. omp's yolo flag (`--approval off`) and config layout are unverified.
- Host-side git commands on a sandboxed worktree do not yet add `-c core.hooksPath=/dev/null -c core.fsmonitor=false`. The protections above (no writes to the `.git` link, git config or hooks) close the known injection paths.

**Acceptance (§14) today:** 1, 2, 5 and 6 are not applicable yet (container/vm). 3: partial; the sandbox denies `.git/hooks`/`config` writes (tested), and there is no container sync yet. 4: macOS yes (real `sandbox-exec` tests); Linux unverified. 7: yes (`vibeke doctor` → isolation, with hints). 8: partial for `sandbox`. Tested: unix sockets other than the broker are unreachable, `state.db` is unreadable, and the broker rejects `interaction.answer`/`pane.send_*`. Elevation does not exist on the broker. 9: partial (fail-closed yes; allowlisted traffic does not continue). 10: unchanged from M1.
