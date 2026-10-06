# 09 — Security and privacy

Vibeke is, by design, a remote-control surface for shells running with the user's full privileges. Anything that can talk to the control socket can run arbitrary commands as the user. The security model therefore focuses on: (1) keeping the socket and remote links reachable only by the user, (2) **not letting the things Vibeke hosts — agents, repos, plugins — escalate through Vibeke**, and (3) keeping secrets out of logs, telemetry and bug reports, and being honest about what is stored locally.

## 0. The explicit choice: guardrails on the host, containment in boxes

Two different promises, and the spec never mixes them up:

| Execution level (13) | Promise | What it means |
|---|---|---|
| `host` | **Cooperative guardrails** | Agents run as the user. Tokens, scopes, the self-answer rules and rate limits stop *accidental* misuse and *casual* prompt-injected misuse that goes through the `vibeke` API. They **do not** stop code running as the same UID from bypassing Vibeke: it can read `state.db` and scrollback files, connect to holder sockets, ptrace or signal processes, edit hook configs, or just do the dangerous thing directly. Vibeke never describes host mode as contained. |
| `sandbox`, `container`, `vm` | **Enforced containment** | The agent's process tree cannot reach Vibeke's privileged sockets or files: the control socket, holder sockets, `state.db`, blobs, other panes' runtime dirs and the user's home are outside the sandbox profile / not mounted (isolation is refused for a checkout that is or contains `$HOME`, `/` or Vibeke's own state). Inside, the only Vibeke endpoint is a **brokered, pane-scoped socket** that implements exactly the §5.2 capability set, only for the broker's own pane/run/task/preview targets, and cannot be upgraded. Spawns in a contained task whose context is unavailable fail instead of falling back to the host. Boundary actions (egress, push, credentials) are gated per 13 §7–§10. |

Every place below that says "denied" means: denied by the API on host (guardrail), and unreachable or denied by the broker in a box (containment).

Milestone tags as in 07: [M1]…[M6], per the plan in [11](11-milestones.md).

---

## 1. Assets

| Asset | Why it matters |
|---|---|
| Control socket / API | = arbitrary command execution as the user |
| Interactions (approvals) | the human-in-the-loop gate that agent harnesses rely on; answering one = granting a permission |
| Pane contents, scrollback archive, transcripts | source code, secrets printed by tools, tokens in env dumps |
| `state.db`, blobs, screenshots | same, persisted |
| Remote links | extend the above to other machines |
| Preview fabric (forwarded ports, CDP browser) | exposes dev servers and a scriptable browser with cookies/session state |
| Plugin tokens | scoped API access handed to third-party code |
| Update channel | code that runs everywhere Vibeke runs |

## 2. Threat model

| # | Adversary | Capability | In scope? | Primary defenses |
|---|---|---|---|---|
| T1 | Other local user on a shared host | can connect to world-accessible sockets, read world-readable files | yes | §3.1 socket perms + peer credential check; 0700 dirs; state files 0600 |
| T2 | Malicious repository content | `.vibeke/` config, policy, harness manifests, task setup scripts, `.env` templates, dev server code, prompt-injection text read by agents | yes | §4 trust-on-first-use with content digests; repo files never loosen policy; setup scripts shown before first run |
| T3 | Prompt-injected / misbehaving agent | runs commands as the user *inside a pane*, can call the `vibeke` CLI | yes — **main novel threat** | host: §5 guardrails (pane tokens, no self-answering incl. via keystrokes, no policy edits, rate limits, audit); boxes: enforced containment (§0, 13) |
| T4 | Malicious or compromised plugin | runs code as the user; holds a scoped token or an approved Herdr legacy grant | partially (host execution is not contained; full legacy compatibility deliberately grants user-level host access) | §6 capability/legacy trust consent, identity-bound brokers, audit, install-time review; M6 sandboxing for scoped/restricted execution |
| T5 | Compromised remote machine | controls the remote `vibeke bridge` and everything it sends | yes | §7 remote is untrusted *input*: no local command execution on behalf of remote, clipboard/open-URL/notification gating, render stream sanitization |
| T6 | Network attacker | on the path between machines, or on LAN reaching forwarded ports | yes | §7 SSH/QUIC authenticated encryption; forwarded ports bind loopback only; Host/Origin checks |
| T7 | Web content in previews | malicious JS on a dev server page (e.g. dependency compromise) driving the CDP browser or attacking the proxy | yes | §8 isolated browser profiles, CDP never exposed, proxy origin isolation |
| T8 | Supply chain | compromised dependency, release artifact, update server | yes | §10 signed releases, reproducible builds, dependency policy |
| T9 | Root / same-UID malware not running through Vibeke | full control of user account | **no** — cannot defend; we avoid making things *worse* (no plaintext credential stores, no new persistence beyond the server) |

Non-goal: multi-tenant use (several humans sharing one Vibeke session). Phase 2's gateway adds per-device identity; Phase 1 is single-user.

---

## 3. Local access control [M1]

### 3.1 Sockets and files

- Runtime dir `$XDG_RUNTIME_DIR/vibeke/<session>/` (Linux) or `$TMPDIR/vibeke-$UID/<session>/` (macOS): created 0700, ownership verified on every start (refuse to start if owned by another UID or group/world-writable — prevents socket-squatting in shared `/tmp`).
- `vibeke.sock`, `holders/*.sock`, `herdr-compat/**/herdr.sock`: 0600.
- **Peer credential check** on accept: `SO_PEERCRED` (Linux), `getpeereid`/`LOCAL_PEERCRED` (macOS), `GetNamedPipeClientProcessId` + token SID (Windows, M6). Connections from a different UID are closed before reading a byte. Same for holder sockets.
- State dir `~/.local/state/vibeke/` 0700; `state.db`, blobs, scrollback segments, logs 0600. `umask 077` in server and holder.
- Env files passed to holders are 0600 temp files unlinked immediately after the holder reads them (never secrets in argv — argv is visible in `ps`).
- **Holder sockets** additionally require a per-holder **server key** (32 bytes, generated at holder spawn, known only to the spawning server and persisted 0600 in `state.db`): `Hello` must carry an HMAC over a holder-issued nonce. Without it, a connection can't attach, write input, resize or kill — only the peer-UID check applies otherwise. Processes inside a pane never see this key (it is not in the pane env). On host this stops *accidental* and *API-level* misuse; same-UID code that reads `state.db` can still obtain it (§0).
- **Render and compat connections** enforce the same identity/scopes as control connections (§3.2). Native render uses `client.hello`; a pane token cannot write outside its scope. Herdr clients retain their original wire format with no added hello/token fields: the public compat listener uses peer identity and pane guardrails; private plugin brokers carry a server-bound approved identity (§6, 07 §8.3). Choosing the compat protocol does not elevate a caller.

### 3.2 Client identities and tokens

Every connection carries an identity, determined at native `client.hello` or by the compatibility listener/broker before dispatch:

| Client kind | How identified | Default capabilities |
|---|---|---|
| `tui` | launched by the user from a terminal not managed by Vibeke (no `VIBEKE_PANE_TOKEN` in env, ancestry not in a pane) | `*` (full) |
| `cli` (outside any pane) | same as above | `*` |
| `agent` / `cli` **inside a pane** | presents `VIBEKE_PANE_TOKEN` | **pane scope** — §5.2 |
| `adapter` | hook shim / extension inside a pane: the same `VIBEKE_PANE_TOKEN` with `kind: adapter` | pane scope + `adapter.*` for its own pane |
| `plugin` | native token or compat broker bound to its approved grant | approved native capabilities or explicit `herdr_legacy` host authority — §6 |
| `remote` | the local server's link to a bridge | §7 |
| anonymous | no native hello and no established compat listener/broker identity | read-only `server.status`, `api.*` (so `vibeke doctor` works) |

**One pane token type.** `VIBEKE_PANE_TOKEN` is the only token injected into panes (the name `VIBEKE_RUN_TOKEN` used in earlier drafts of 04 is retired; adapters use the pane token):
- 256-bit random, generated when the pane is spawned and placed in the holder's spawn env, so the pane process tree inherits it and **it survives server restarts** (the holder and its child keep their env; the server persists only a hash in `state.db`).
- **Scope**: the pane and panes/runs it creates (descendants via `created_by_ref`, `parent_run_id`, agent-created tasks).
- **Lifetime** = the pane. Revoked when the pane closes; never rotated while the pane lives (rotation would require re-injecting env into a running process, which is impossible). A leaked token is mitigated by revoking it (`vibeke pane revoke-token <p>`), which marks the pane `token_revoked` and leaves its agent working but without API access until the pane is restarted.
- Elevation: `vibeke auth elevate` from inside a pane requires confirmation in the TUI chrome (out-of-band of the PTY) and yields a separate time-boxed (10 min) elevated token.

**Ancestry as a guardrail.** A connection without a token whose peer PID's ancestry includes a pane child is treated as in-pane and gets pane scope (so unsetting the variable doesn't escape scope). This is best-effort: a daemonized descendant that re-parented to `launchd`/`init` loses its ancestry and looks like an outside CLI. That is acceptable under the host-guardrail promise (§0) and irrelevant in boxes, where the outside socket is unreachable.

**Implementation status (Goal 01 review follow-up):** capability checks run before dispatch for every pane-scoped connection, whether scoped by token or by peer-pid ancestry; a token can't switch scope away from the caller's own pane. Enforced: own pane and panes it created (`created_by = agent:<pane>`) only for input/layout writes; no `interaction.answer/cancel` (`self_answer_forbidden`), focus changes, `server.*` control, `policy.trust`, `integration.install/uninstall`, `render.attach` or tracking mutations (15 §11); `input_locked_open_interaction` while the target pane has an open interaction; adapter delivery acks only from the interaction's own pane with the issued idempotency key. Tokens are stored as blake3 hashes (`pane_token_hashes`); re-issuing a pane's token revokes the old one. Read scope (§5.1 rule 4, M4): `pane.read`, `pane.wait_output` and `search.query` (including `pane.read {source: archive}` on closed panes, via `archive_panes`) are limited to the caller's workspace (`security.pane_scope.read = "workspace" | "session" | "self"`). `group.*` mutations, `workspace.move {group}` and `client.focus` are refused for pane scope. Not yet: spawn lineage across runs, rate limits (§5.1.7), depth limits (§5.1.8), revoking cached scope on live connections. Test: `crates/vibeke/tests/auth.rs`.

---

## 4. Untrusted repository content (T2) [M1/M2]

Repo-local files Vibeke reads: `.vibeke/config.toml` (layouts, task setup, port hints), `.vibeke/policy.toml`, `.vibeke/harnesses/*.toml`, `.vibeke/setup.sh` / `[task.setup] script`, `.vibeke/previews.toml`.

Rules:
1. **Nothing executable or permission-relevant is used until trusted.** `vibeke policy trust <path>` (or a TUI prompt on first `task new` in that repo) records `(canonical path, blake3 digest of the .vibeke/ tree)`. Any change to the tree invalidates trust → re-prompt showing a diff.
2. **Repo policy can only tighten.** Repo `policy.toml` may add `deny`/`ask` rules; `allow` rules from repo files are ignored unless the user has explicitly trusted that repo *with* `--allow-policy-grants` (shown in red). Global/user policy always wins over repo policy for `allow`.
3. **Setup scripts** run only after trust, with the script content displayed at trust time, in the task's worktree, in a pane (visible, interruptible), never silently.
4. Repo harness manifests may not override built-in harness ids; they get a `repo:` namespace and their `launch`/`resume` argv are shown at trust time. *Implemented (M2): loaded only while the `.vibeke/` digest is trusted, ids prefixed `repo:`, detection limited to the repo, `policy.trust` returns `harness_manifests` with their argv (04 §5).*
5. Agents themselves editing `.vibeke/` invalidates trust (digest change) — an agent cannot grant itself permissions by writing policy files.
6. `.env` templates copied into worktrees are never logged and never put into events (only their path).
7. Untrusted repos still work as plain terminals and with built-in harness adapters; only the repo-provided automation is disabled.

---

## 5. Agents as adversaries (T3) — the core of the model [M1; enforced containment M2]

Agents run as the user and can type anything into their own shell, including `vibeke …` commands. Hook shims and extensions run *in the agent's process tree*. Prompt injection (from repo text, web pages, issue comments, tool output) can steer an agent to abuse the Vibeke API. The goal is that **an agent cannot use Vibeke to obtain more authority than its harness already gave it**.

### 5.1 Invariants

1. **No self-authorization; retrieving is fine.** Two different operations:
   - **Retrieve a decision** — `adapter.interaction.await` / `interaction.get` by the hook shim or extension of the interaction's own pane: **allowed**. This is how a hook receives the human's answer to deliver it to the harness.
   - **Authorize** — `interaction.answer` / `interaction.cancel` / policy writes: **rejected** (`permission_denied:self_answer_forbidden`) when the caller is the interaction's pane or in its lineage (a pane whose run has a `parent_run_id` chain to the interaction's run, or a pane created by that run, tracked transitively through `agent.spawn`, agent `pane.split` and agent `task.create`).
2. **No cross-agent answering by default.** Pane-scoped tokens cannot answer *any* interaction. A user can grant a specific supervisor agent the `interactions_answer` capability for named runs (`vibeke agent grant <supervisor> answer --for <run…>`), recorded and visible; this is how Phase 2 "supervisor agents" will be enabled, deliberately.
3. **No policy changes from panes.** `policy.add/remove/trust`, `integration.install/uninstall`, `plugin.*` install/enable, `config.set --persist`, `machine.add/install`, `server.*` lifecycle: full scope only.
4. **Keystrokes are not a bypass — including into your own pane.** Pane scope cannot `pane.send_text`/`send_keys`/`run`/`agent.prompt`/render-stream input into panes it did not create (own pane and descendants only) — otherwise an agent could type "1⏎" into a neighbour's approval dialog. Additionally, **while an interaction is open on a pane, no pane-scoped token in that interaction's lineage may write input to that pane** (`permission_denied:input_locked_open_interaction`). That closes the path where a background command spawned by the agent (`sleep 2; vibeke pane send-keys --current 1 Enter`) answers the agent's own native dialog. Human input (TUI/CLI with full scope) and Vibeke's own verified-keystroke delivery are unaffected. On host this is a guardrail: same-UID code could still write to the holder directly if it extracts the holder key (§0). Reading other panes (`pane.read`, `agent.transcript`, `search.query`) is allowed by default within the same workspace and configurable (`security.pane_scope.read = "workspace" | "session" | "self"`), because cross-pane reading is a common legitimate workflow (reviewer agents).
5. **Answers from the human are authenticated by channel.** An interaction answered via the TUI requires the keypress to come from the TUI client's input stream (not from a pane PTY). The TUI's interaction popup is drawn in chrome, not inside a pane, so an agent cannot spoof it with terminal output; Vibeke never parses pane output for "answers".
6. **Screen-spoofing resistance.** Screen detectors can be fooled by an agent printing a fake "Allow? (y/n)" box. Consequences are bounded: a screen-detected interaction is marked `source: screen`, policy auto-answers **never** apply to screen-sourced interactions, and keystroke delivery only happens on explicit human action.
7. **Rate limits.** Pane tokens: 50 req/s burst, 10 req/s sustained; `agent.spawn`/`task.create` from panes: 10/min; exceeding → `rate_limited` + audit event + notification ("agent a12 is spawning agents rapidly").
8. **Spawn depth limit.** Agent-created agents carry depth; default max depth 3 and max 20 live descendant runs per root run, configurable.
9. **Everything an agent does through Vibeke is attributed** (`actor: {kind: agent, run, pane}`) and visible in the pane's timeline and audit log.

### 5.2 Pane-scope capability set (default)

| Capability | Default | Notes |
|---|---|---|
| read own pane / own run | ✔ | |
| read other panes, transcripts, search | workspace | configurable |
| `pane.split/float`, `tab.create` in own workspace | ✔ | created panes become descendants |
| write (text/keys/run/prompt) to own + descendant panes | ✔ | ✘ while an interaction in this lineage is open on the target pane (§5.1.4) |
| write to other panes | ✘ | grant per pane |
| `agent.start/spawn` | ✔ (rate-limited, depth-limited) | |
| `task.create`, `worktree.create` | ✔ | worktree removal of non-descendant tasks ✘ |
| `interaction.list/get` | ✔ (workspace) | answering ✘ (§5.1) |
| `preview.*`, `browser.open/navigate/click/type/press/wait/screenshot/snapshot/console/network/dom/close/list` | ✔ (own sessions only; previews of the pane's own task only, `[browser] session_previews = "own"` default — other previews are denied `foreign_preview`, 06 B5; `device` presets and one-shot screenshots obey the same destination policy) | `browser.eval` ✘ unless `browser.script` granted (as built: `preview.browser_script = true`); `browser.install`, `take_over/release`, screencast ✘ |
| `image.show`, `notification.send` | ✔ | notifications from agents are labelled with the agent name, rate-limited 6/min |
| `desk.search/sessions/context`, `draft.create/update/get/list/reorder/delete/combine/check`, `notes.get/set` | ✔ (own workspace only) | desk results only for sessions whose run was in the caller's workspace; opted-in transcript roots are invisible to panes; `draft.send/reconcile`, `desk.open/resume/forget/index/status` ✘ |
| `events.subscribe` | ✔ (workspace subjects) | |
| `policy.*`, `plugin.*`, `integration.*`, `machine.*`, `server.*`, `config.set --persist` | ✘ | |

### 5.3 Hook shims and extensions

- Hook shims (`vibeke hook claude PermissionRequest`) run in the agent's tree. They hold the pane token, open interactions and await answers — they can *open* interactions (harmless) and *deliver* answers to the harness, but they receive answers only from the server, which applies §5.1.
- A malicious agent could bypass its own hook (e.g. edit its settings to remove the hook). That doesn't escalate through Vibeke: it's the harness's own permission model being disabled, which the agent could do without Vibeke. Vibeke detects integration tampering (`integration.doctor` on agent start; `integration_tampered` notification when hook config files change while a run is live).

---

## 6. Plugins (T4) [M5, sandboxing M6]

- Install shows: source repo + commit, requested capabilities with risk levels (`interactions_answer`, `panes_write`, `agents_control`, `browser.script`, `network: *` in red), build commands. Consent recorded with manifest digest.
- Updates that widen capabilities require re-consent; narrowing is silent.
- Native plugin processes get tokens limited to their approved capabilities; native argv actions get a short-lived (60 s) token. Imported Herdr plugins use the separately approved legacy grant below; their normal callbacks cannot break merely because an action has run for more than 60 seconds.
- Plugin UI contributions are data rendered by Vibeke; plugins cannot inject escape sequences into chrome (all strings sanitized: C0/C1 controls stripped, bidi overrides neutralized).
- Audit: every plugin API call that mutates state is an event with `actor.kind = plugin`.
- M6: OS-level sandbox for scoped process plugins — Linux Landlock (fs) + seccomp (no ptrace) + network namespace allowlist via proxy; macOS `sandbox-exec` profile generated from declared capabilities; plugins can opt-in early in M5 via `sandbox = true`. Applying restrictions to a Herdr legacy plugin is an explicit restricted mode with compatibility diagnostics; full legacy mode remains trusted host execution.
- The marketplace index is metadata only; Vibeke never auto-installs or auto-updates plugins without the user running a command (opt-in `plugin.auto_update = "patch"` allowed for non-widening updates).

**Herdr legacy trust (`herdr_legacy`, M5).** Full compatibility (07 §7.7) requires unchanged plugins to use Herdr's public operations and inherited host environment. Installation/linking therefore presents an explicit broad trust grant, covering host execution and the complete public Herdr API, with source/commit, manifest digest and entrypoints recorded in Vibeke's per-user registry. No capability list is guessed from static analysis, and an absent Herdr capability declaration never becomes silent consent. Noninteractive activation requires an explicit accepted grant; compatibility CLI `--yes` accepts the displayed trust terms only for a caller with installation authority. No prompt response means no activation. A grant does not confer access to holder keys, other plugin identities, or native-only administrative APIs. Same-user code can still bypass API guardrails as described in §0.

Each invocation and plugin-owned pane receives a private 0600 compat broker endpoint in a 0700 runtime directory. Its binding to plugin id, session, approved grant and lifetime is server-owned; client JSON/context/env cannot select a stronger identity. Direct raw requests and the private `herdr` launcher use this same identity, without changing the Herdr wire format. Recreate the broker binding after server recovery only for still-approved live invocations; disable, unlink, uninstall and trust revocation close/reject broker access, including existing subscriptions. Check current authorization on every request. Host-side same-UID access to broker paths remains a guardrail limitation; restricted OS sandboxes must not expose another plugin's broker, privileged sockets or registry files.

Legacy authority persists for the invocation/process lifetime, including long-lived children and plugin panes; closed invocations and removed panes lose their bindings. Session routing rechecks the same grant on the destination. An agent or scoped plugin must not escape its scope by invoking a more privileged legacy action, opening its pane, installing/enabling it, or starting it through the native API: deny the elevation or require explicit operator authorization outside the pane. Full compatibility applies after an authorized operator grants legacy execution; a restricted invocation is labeled accordingly.

For managed legacy installs, source revision, entrypoint/manifest or trust-mode changes require review before new code runs, including patch updates; the native non-widening auto-update option does not bypass legacy trust review. Linked development sources are covered by explicit trust in that local directory, revoked when unlinked. Build processes receive no runtime broker/token authority. Preserve inherited host environment for approved legacy runtime commands, while replacing runtime identity/context values with the current invocation's values. Logs and audit records redact credentials. Audit compat mutations with the plugin identity just like native mutations; a copied manifest cannot acquire a grant belonging to another installation.

*As built (M5 first slice, 2026-10-06).* The grant is stored in `plugins.json` (0600, atomic replace) as `{mode: herdr_legacy, manifest_sha256, root, granted_at_ms, entrypoints, baseline}`. Every execution path checks status first: actions, event hooks, startup and the post-grant build all require a matching digest and root and an enabled entry. Brokers are 0600 sockets in `$RUNTIME/<session>/herdr-compat/brokers/` (0700). The server binds each to `(plugin id, grant digest)` and the invocation's pane. Every request and streamed event re-checks the registry, so revocation, disable, uninstall or a manifest edit cuts off a running invocation's callbacks; tested end to end. Brokers close when the invocation's process exits (slice 1). No escalation: `plugin.action.run`/`plugin.action.invoke` refuse pane-scoped callers, and the CLI refuses registry changes and trust grants from a pane (`VIBEKE_PANE_TOKEN`) and trust grants from inside a plugin invocation. Build steps get no broker, launcher, context or pane variables. Restricted (sandboxed) legacy mode: see "As built (plugin completion)" below.

*As built (M5 slice 2, 2026-10-06).* **Recovery:** broker bindings `{path, plugin id, grant digest, default pane, entrypoint, log id, lifetime}` are persisted in `$RUNTIME/<session>/herdr-compat/brokers.json` (0600). After a server restart each binding is re-issued at the same socket path only when its process (or pane) is still alive and the registry still holds an active grant with the same digest; everything else is dropped and its socket removed. Invocation output goes to 0600 files (not pipes), so a long-running invocation survives the restart and its log keeps filling; the exit status of a re-attached process is reported as unknown, and pid reuse is not detected. **Lifetime:** authority outlives the invocation's process only for the manifest's long-running entrypoints: a `[[startup]]` process is a process-group leader and its broker stays while any process of the group is alive; a plugin pane's broker lives as long as the pane. Action and event-hook brokers close when their process exits, so children they leave behind lose authority. Inside a plugin invocation whose broker is closed, the `herdr` launcher fails with `permission_denied` rather than falling back to the user-level socket. A watcher re-checks liveness and the grant every second. **Audit:** metadata-only events with `actor = {kind: plugin, id, invocation}`: `plugin.invocation_started` (source, action/event/entrypoint, pid, long_lived), `plugin.invocation_finished` (status, exit code, duration), `plugin.api_call` for every mutating method called with a plugin's identity (method, ok, error code; never parameters or text), `plugin.pane_opened`. **Redaction:** stdout/stderr tails in log records pass through `vk-redact` (keys, tokens, bearer and URL credentials); the transient output files are removed once read. **Sessions:** a pane-scoped caller cannot select another session with `herdr --session`; a plugin that does keeps its plugin identity on the destination, which re-checks the grant (`compat.herdr.call {as_plugin}` is refused from panes). **Migration:** `vibeke plugin migrate` is refused from panes and plugin invocations and reads only the directory the user names.

*As built (M5 review fixes, 2026-10-06; `reviews/2026-10-06-codex-m5-compat-review.md`).* **Content-bound trust:** a grant also records a unique `grant_id`, the source path, a digest of the whole reviewed tree and a digest of every file the manifest's commands reference. The referenced-file digest, source and root are checked on every status read and launch. A reinstall keeps the grant only for the same source with an identical tree and no `[[build]]`; otherwise the plugin is inactive until reviewed and rebuilt. A build is recorded only for the grant it ran for, and the manifest digest is re-checked before every build step and after the last, so a mutation aborts the build and the registration. **Registry:** every change is a read-modify-write under an exclusive lock (`plugins.json.lock`), so a revocation made while another plugin builds is never overwritten. **Brokers:** a binding carries its `grant_id`; every request checks that exact grant and that the broker is open and its invocation alive; closing a broker aborts every connection it accepted (requests, `events.subscribe`, `events.wait`), and `events.wait` re-checks after waking. Each launch (actions, hooks, startup) re-reads the registry first, so the hook dispatcher's cache cannot run changed code. **Sessions:** cross-session plugin identity is a single-use ticket from the invocation's own broker (`vibeke.invocation_ticket`), redeemed with the issuing session (`compat.invocation.verify`: broker open, invocation alive, same grant) and re-checked on the destination; environment values never carry identity. A process inside a pane of any session under the runtime root (holder ancestry) cannot reach another session's `compat.herdr.call`, with or without its token; the shim refuses first. **Shim:** a broker path is used only when it resolves, with plain components and no symlinks, to a socket listed in its session's `brokers.json`; `server.*` lifecycle methods are never forwarded. **Migration:** destination roots are canonicalized and nothing is written or deleted through a symlink below them (re-checked before each write and before rollback deletes); source registry `config_dir`/`state_dir` must resolve inside `--from`; created paths are journaled as they are made, so a failed migration can be rolled back and a retry continues the same record.

*As built (plugin completion, 2026-10-06).* **Restricted legacy mode:** `[plugins."<id>"] isolate = "sandbox"` runs a trusted legacy plugin's actions, hooks and startup commands under the `sandbox` level (13): plugin and config dirs read-only, only its own state dir writable, the home and all of Vibeke's runtime/state/config hidden, network off unless `network = true`, only its own broker socket connectable, scrubbed environment; a working sandbox is probed (a trivial command is really run under a profile) and without one the invocation is refused with a clear error, never run on the host (07 §7.7). It narrows what the plugin can do; the grant, broker binding and revocation rules are unchanged, and the mode never widens authority. **Repository installs:** the grant also pins the resolved git commit, so an update (new commit) needs a new review; the fetch runs git hardened (no hooks, fsmonitor, filters, credential helpers or user/system git configuration; https and file protocols only) and a test-only URL override exists solely under `VIBEKE_TEST_HOOKS=1`. **Plugin agent views** (`agent.view.set`) require a plugin broker identity, are sanitized and redacted, size-limited, bound to the grant and purged on disable/untrust. **Limits:** per-plugin concurrency (`busy`), a bounded hook queue and log rings (bytes, lines, records) keep one plugin from exhausting process slots or the log; the transient output files are not size-limited on disk.

---

## 7. Remote machines (T5, T6) [M3, QUIC post-1.0]

- **Transport auth**: M3 uses the user's SSH (`ssh -T host vibeke bridge`), inheriting their keys, known_hosts and agent config — no new credential system. Post-1.0 QUIC: keys exchanged over the SSH bootstrap (pinned Ed25519 per machine, stored 0600), TLS 1.3 with raw public keys, no CA, no listening port reachable without the key; QUIC listener binds to configured interfaces only (default: the address the SSH bootstrap used).
- **The remote is untrusted input to the local side.** Things a remote server sends that could affect the local machine are gated:
  - Clipboard writes (OSC 52 from remote panes): `clipboard.remote_write` default **`ask_once`** per machine (06 A9), configurable `allow|deny`. Remote clipboard reads follow `clipboard.osc52_read` (default deny).
  - Open-URL requests (`preview.open`, clicked links): only `http(s)` to forwarded loopback ports or user-clicked links; `file:`, `javascript:`, custom schemes refused.
  - Notifications: labelled with the machine name; rate-limited.
  - Image/blob transfers: size-capped (default 32 MiB each, 512 MiB/hour), MIME-sniffed, images decoded in a separate thread with limits (dimension caps against decompression bombs).
  - Render stream: decoded with strict bounds (max cols/rows, grapheme table size, image count); fuzzed (10 §6).
  - The local server never executes a command on the local machine on behalf of a remote request. (Local → remote is the only direction of command flow.)
- **Port forwarding**: previews are reached through the peer-checked SOCKS5 listener or the authenticated reverse proxy (06 B3/B4), both bound to `127.0.0.1`/`::1` only, never `0.0.0.0`. The only unauthenticated listener that can exist is an explicitly enabled mirror (06 B4), shown with a warning badge. *As built:* the mirror is still peer-checked (same user only, §8), the reverse proxy listens only after the first proxy open.
- Remote binary install (`machine.install` / bootstrap, 06 A3) has two modes with one verification boundary — the **local** binary's embedded release key:
  - `push` (default): the laptop verifies the artifact's minisign signature + sha256, streams it over SSH, and the remote re-checks the sha256 before the atomic swap.
  - `remote-download`: the laptop verifies the signed release manifest and sends the expected sha256; the remote downloads and checks the hash. The remote never runs an unverified binary in either mode.
  - Pushing a locally built, unsigned binary requires `--from-local-binary` (development only) and is marked in `machine show`.
- A remote session's interactions can be answered from the local TUI — the answer travels to the remote server, which re-applies §5 rules with the local client's (full) identity.

---

## 8. Preview fabric and browser (T6, T7) [M3]

- **Browser profile mode (primary, 06 B3; same for the browser pane and the window)**: the local SOCKS5 listener binds loopback and accepts a connection only if the client socket's owning PID is in the managed browser's process tree (peer lookup — Chromium doesn't support SOCKS auth). It only forwards to the profile's machine/runner loopback; non-loopback follows `preview.profile_route`. The profile is a dedicated Vibeke user-data-dir: no access to the user's real browser profile, cookies or password manager.
  - *As built (Goal 03 Stage 1):* rejected peers get reply `0x02` and a log line; the bridge refuses `tcp:` channels to anything but loopback before connecting; a public name that resolves to a loopback address is routed to the profile's machine, never to the laptop's own loopback; profile directories must lie under `<state>/browser-profiles`. A server started with `VIBEKE_TEST_HOOKS=1` accepts `preview.test.register_browser` (registers an arbitrary pid tree as a managed browser) — only someone who controls the server's environment can enable it.
- **Reverse proxy mode (secondary, 06 B4)**:
  - Validates `Host` against allocated `*.vibeke.localhost` names (DNS-rebinding defence) and requires the per-preview credential (one-time `vk_token` → `__Host-vk_preview` cookie).
  - **Strips its own credentials before forwarding** (the `vk_token` query parameter and the `__Host-vk_preview` cookie) and **drops upstream `Set-Cookie` for reserved `vk_` names**, so an app can neither read nor overwrite the proxy credential.
  - **Strips `Domain=` from upstream `Set-Cookie`** so cookies are host-only. Cookies are not isolated by port or origin (RFC 6265) and `SameSite` is not an isolation boundary; isolation between previews relies on distinct hostnames + host-only cookies, and is tested.
  - Forwards only to registered `(machine, host, port)` preview tuples on loopback (or an explicitly declared LAN address on that machine).
  - Mirror mode (raw local port, unauthenticated) exists only when explicitly enabled per preview.
  - *As built (preview fabric completion; `vk_preview::proxy`, `vk-server::preview_fabric`):* listener `127.0.0.1:<proxy_port>` (+ `[::1]`), started lazily, accepted peers must be loopback, 30 s header timeout. Unknown host / wrong port / IP literal → `421`. One-time tokens (256-bit, 60 s, consumed on first use, ≤ 16 per origin) are exchanged by the proxy itself for a per-origin session (256-bit) in `__Host-vk_preview` (HttpOnly, SameSite=Strict, Secure, host-only, `Path=/`) — no non-`Secure` copy (cookies ignore ports: another local listener serving the same hostname could read it); hostnames carry a random per-session tag, so two sessions never share an origin or its cookies, and the configured proxy port is machine-wide (busy on `127.0.0.1` or `[::1]` → refused, no fallback); the redirect (absolute, on the preview's own origin) removes the token from the address bar (`Referrer-Policy: no-referrer`, `no-store`). Only SHA-256 digests are kept, in memory; a restart revokes all. Every request incl. WebSocket upgrades needs a session **of that origin** (a session or token of another preview is refused). Because all previews are same-site (`vibeke.localhost`), cross-preview requests are refused at the proxy: every `Sec-Fetch-Site: same-site` request, `cross-site` unless a top-level document navigation (`Sec-Fetch-Mode: navigate` + `Sec-Fetch-Dest: document`), and a foreign `Origin` on upgrades and non-GET/HEAD (CSRF, cross-site WebSocket hijacking); proxy-generated responses are unframeable (`X-Frame-Options: DENY`, `frame-ancestors 'none'`). Each forwarded request re-checks that the preview still exists on the route's port (locally, or `preview.get` on its machine); forgotten, retired or moved previews lose their route and sessions. Preview paths are absolute-path references (no `//`, `\`, scheme). Upstream never sees `vk_token` or any reserved `vk_*` cookie (also `__Host-`/`__Secure-` prefixed); upstream `Set-Cookie` for reserved names is dropped and `Domain=` stripped. The tokenized URL is returned only to full-scope callers and never written to events or `preview.url`; it is passed to the platform opener as an argument (briefly visible in the process list — the token is single-use and expires in 60 s). Upstream TLS (https previews) skips certificate verification for loopback dev servers only (local loopback or a bridge `tcp:` channel, which is loopback-only), still verifying the handshake signature.
  - *Mirror as built:* explicit `preview.mirror`, full scope only, remote previews only, `conflict` when the port is busy, in memory only. Stricter than "unauthenticated": each accepted connection must be owned by a process of **this user** (complete 4-tuple; macOS libproc over the user's processes, Linux socket uid from `/proc/net/tcp{,6}`); other local users are refused and counted. Same-user processes are not distinguished (09 §2: same-UID malware is out of scope). Listed in `preview.status.mirrors` with `authenticated: false`; the TUI badge is not built.
- **TLS probe (06 B2)**: TLS handshake + `GET /` to this machine's loopback only, self-signed accepted, no credentials sent (no cookies, `Authorization` or client certificate).
- **Task previews (06 B2)**: repo-local `[previews]` may only name ports inside the task's own lease (`port_env`/`offset`); absolute ports only from the caller's `task.create` params. Since previews define headless-browser reachability (B5), an untrusted repository cannot widen it beyond the task's leased block; no repo trust is required for declaring them.
- **Firefox window profiles (06 B3.4)**: a dedicated `<state>/browser-profiles/<name>-firefox` directory (checked to be under that root, 0700, `user.js` 0600) passed with `-profile` and `-new-instance` on first launch; the user's own Firefox profiles and `profiles.ini` are never read or written. Tests launch Firefox only with `VIBEKE_BROWSER_TESTS=1` and an installed Firefox.
- **Headless browser (06 B5)**:
  - **CDP is never exposed**: `--remote-debugging-pipe`, owned by the server; agents get `browser.*` methods, not raw CDP; `browser.eval` requires the `browser.script` capability.
  - **All browser traffic goes through a filtering proxy** with browser-side DNS disabled, so destination rules apply to redirects, subresources, fetch/XHR, WebSockets and service-worker requests, and are checked against the **resolved IP**: other loopback ports, link-local/cloud-metadata and private ranges are denied by default; public internet is subresource-only by default.
  - Contexts are ephemeral per browser session (fresh context, deleted on close). Downloads disabled, file choosers auto-cancelled, camera/mic/geolocation/notifications denied.
  - *As built (Goal 03 Stage 3):* the filtering proxy is **per session** (its own `127.0.0.1` port, set as that browser context's proxy with `<-loopback>` so loopback is proxied too) and peer-checked like the SOCKS listener (only the headless browser's process tree may connect). It resolves each name once, requires every resolved address to pass, and connects only to those addresses (no DNS-rebinding window). A second, CDP `Fetch` layer on every page/iframe/worker target of the session blocks non-network schemes (`file:` navigations never reach a proxy), refused IP literals/localhost and public top-level navigations; the default context points at a dead proxy and browser-side DNS is mapped to `~NOTFOUND`; QUIC off, non-proxied WebRTC UDP off. Cloud metadata addresses need an allow rule naming the address (a host rule never unlocks them); CGNAT `100.64/10` (Tailscale) counts as private. Denials are logged (network ring, `browser.request_denied` events, `destination_denied` errors). Pane-scoped callers see only their own sessions (and those of panes they created); `browser.install`, take-over/release and screencast access are full-scope only; `browser.eval` from a pane needs `preview.browser_script = true` (the `browser.script` capability as a config grant until per-agent grants exist). Screenshots are 0600 blobs under `<state>/blobs`. The server never writes a screenshot to a caller-chosen path (`--out` is written by the CLI). `vibeke browser install` asks first and verifies SHA-256 before unpacking; without a recorded checksum it requires `--sha256`. Camera/mic/geolocation/notifications are not explicitly denied yet (headless has no devices; `--deny-permission-prompts` is passed).
  - *Goal 03 review follow-up:* the WebRTC policy is now actually in force (both `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` and `--webrtc-ip-handling-policy=…`: the headless shell and full Chromium read different switches; also for remote-profile pane browsers and the window), verified with a UDP/STUN sentinel. Peer checks (SOCKS route and agent proxy) match the complete socket 4-tuple and the owner's uid. Open/navigate URLs are parsed with a WHATWG parser and the checked canonical URL is what Chromium gets; credentials in URLs are refused. IPv4-mapped DNS answers are canonicalised before loopback classification. The pane-scope machine restriction for `preview.open`/`browser.pane.create` runs at the shared authorization point. The TUI treats media frames as untrusted input (shm only from the local server, for requested panes, with pane-bound names; `fstat` before mapping; bounded geometry and inflation). Disconnected clients drop their screencast subscriptions. Pane screenshots are managed records (retention applies).
- **Browser pane page I/O (06 B3.2, as built 2026-10-06; corrected after the Codex B3 review the same day):**
  - *Page clipboard → host:* a page can set the host clipboard only through a per-page random CDP binding, and only (a) from the main world of the current top-level document (the call's `executionContextId`; iframes, isolated worlds and contexts of earlier documents are refused), (b) after Chromium's own `writeText`/`write` resolved, or for a trusted `copy`/`cut` event while the page has user activation, and (c) within 5 s of a mouse press or key press the user made in that pane — text input, drops and API calls never count, and a navigation clears it. ≤ 1 MiB, then under the `clipboard` config. The TUI accepts the write only from the machine rendering that pane, and judges it as a remote machine's write (`remote_write` ask-once, `remote_write_max_bytes`) when that machine or the pane's owner is remote. What this does not cover: the top document itself can still write during the 5 s after any click or key in the pane (as in a desktop browser). The host clipboard is never read into a page: reads see headless Chromium's own clipboard, and a clipboard image is read from the host only on `prefix+shift+v`, from the platform clipboard of the machine the TUI runs on and only when the host terminal is on that machine (OSC 5522 is not used: a late reply would become keystrokes).
  - *Files into a page:* only files the user confirmed in the drop prompt (or the explicit clipboard-image key). The TUI binds the confirmation to the file, not its name: it opens the file `O_NOFOLLOW` and requires the device, inode and size the prompt showed, then uploads exactly those bytes from that descriptor (a file that is swapped — directly or through a parent directory — or grows or shrinks is refused). The page gets only the copy in the rendering server's private drop directory (`<state>/browser-drops`, 0700, content-addressed, no links, ≤ 50 MiB, ≤ 16 per drop), removed 10 minutes after upload or delivery; a path anywhere else is refused, so nothing the server's user can read but the user didn't confirm reaches a page. Nothing is read implicitly; pasting text that merely names a directory or a missing file stays text.
  - *Console capture:* console text and request URLs are redacted (`vk-redact`) and control characters escaped (visible `\x1b`-style; the split's terminal never sees raw escape sequences) when captured, capped at 8 KiB per entry, 500 entries per ring, in the server's memory; whole responses are redacted, the page's current URL included. A pane-scoped caller may read a browser pane's console only from that pane's console split (`created_by = "browser-console:<id>"`, set only by the server) or as the agent that created the browser pane, and the same holds for the split's printed output (`pane.read`, `pane.wait_output`, `search.query`); the split is `no_archive` (no archive segments, no FTS rows), so its output lives only in the pane's scrollback (and its recovery snapshot) while the pane exists. Opening/closing the split and the relay method are full-scope only. The media host relays a remote-owned page's capture to its owner per entry, decided at capture: only entries from a loopback frame, captured while the top-level document was loopback, in the same top-level navigation (no external iframes, no external site visited between relay ticks, no request started elsewhere that finishes later); the owner accepts only known fields and redacts and escapes again.
- Screenshots and console logs are blobs (0600) with environment and code labels (06 B6) and subject to retention (§9.3).

---

## 9. Secrets and privacy [M1–M3]

### 9.1 Data classes: what is stored, and what is never logged

Vibeke cannot promise secrets are "never stored": the terminal itself shows whatever tools print, and that is the product. Two classes, with different rules:

| Class | Examples | Where | Rule |
|---|---|---|---|
| **Operational data** | live screen, VT snapshots, scrollback archive, transcripts, tool outputs and diffs (blobs), Bash commands in Interactions, screenshots | local state dir (0700/0600), on the machine where the pane runs | Stored **as-is** (secrets printed by tools included), local only, subject to retention (§9.3) and `vibeke forget`. Optional encryption at rest: `security.encrypt_state = true` encrypts blobs and scrollback segments with a key in the OS keychain (macOS Keychain, libsecret) — protects backups/disk images, not against same-UID processes. |
| **Derived conversation index, drafts, notes** (research R2/R3) | `desk.db` (FTS5 of transcript text from runs Vibeke saw, plus opted-in roots), `draft` / `workspace_notes` entities incl. the exact text of each send attempt | local state dir (0600), on the server's machine | Operational data. Never copied into events (metadata only). Transcripts outside Vibeke's own runs are read only after `[desk] roots` opts in; `[desk] exclude` keeps paths/cwds/repos out and purges existing rows; model reasoning is not indexed. |
| **Telemetry, logs, debug bundles, notifications, search snippets sent to remote/Phase 2 clients, OTel exports** | `server.log`, `audit.jsonl` free-text, crash reports, bundle | may leave the machine or be shared | **Always redacted** (§9.2). Never contain env values, credential file contents or tokens. |

Hard rules regardless of class:
- Environment variable *values* (pane, task, harness env) are not written anywhere by Vibeke — only keys.
- Contents of `.env` files, `auth.json`-style credential files and SSH keys are never copied into events, logs or blobs by Vibeke itself (they can still appear in operational data if a tool prints them).
- Tokens (pane, plugin, elevate, holder keys) — only hashes in `state.db`, never in events, logs or bundles.
- Keystrokes/input bytes are not logged (input is not an event). Interaction answers are stored as audit; free-text answers are redacted in logs.

### 9.2 Redaction

A shared `vk-redact` module scrubs strings before they enter logs, events, debug bundles, notifications and search snippets sent to remote clients:
- Pattern set: AWS keys, GitHub/GitLab tokens (`ghp_`, `github_pat_`, `glpat-`), OpenAI/Anthropic keys (`sk-…`, `sk-ant-…`), Slack tokens, JWTs, PEM blocks, generic `password=…`/`secret=…`/`token=…` assignments, URLs with userinfo, Bearer headers.
- High-entropy heuristic for 32+ char base64/hex strings adjacent to secret-ish keys.
- User-extensible (`[security.redact] patterns = [...]`).
- Redaction is **not** applied to operational data such as the live pane view or local scrollback (the user must see their own terminal), but is applied to the FTS index *optionally* (`security.redact_scrollback_index = true` default) so `search` results and Phase 2 remote clients don't surface raw secrets.

### 9.3 Retention

| Data | Default retention | Config |
|---|---|---|
| Event log | 30 days / 2 M rows | `retention.events` |
| `agent.item` detail | 7 days, then compacted | `retention.items` |
| Scrollback archive | 14 days or 2 GiB per session (oldest first) | `retention.scrollback` |
| Screenshots/blobs | 7 days unless referenced by a live object | `retention.blobs` |
| Audit log | 90 days | `retention.audit` |
| Logs | 7 days, 100 MiB | `logging.*` |
| Conversation index (`desk.db`) | 90 days per row (pruned hourly); sources removed from the selection or excluded are purged on the next pass | `desk.retention_days`, `desk.roots`, `desk.exclude` |
| Drafts and workspace notes | until deleted; delivered drafts are archived and removed 30 days later; each draft keeps its last 10 send attempts (exact sent text) | — |
| Assistant requests and generated drafts (14) | 24 h after finishing, then deleted (lazy, on the next `assistant.*` call); payloads are never stored — an unconfirmed preview lives only in server memory and is cancelled and dropped when `preview_ttl_seconds` (600 s) pass, even if no further call comes; at most `max_concurrent_requests + max_queued_requests` previews are held at once | `[assistant] result_retention_hours`, `vibeke assist purge` |

`vibeke forget --pane p | --workspace w | --before date` purges archives, events and blobs for scope (events replaced by tombstones to keep `seq` gapless).

As built: `vibeke forget --pane p | --workspace w | --before t | --all [--yes] [--dry-run]` (`scrollback.forget`, full scope only; pane tokens are refused) purges the **scrollback archive** only: segment files, `scrollback_fts` rows and `archive_panes` metadata, atomically (02 "Archive search as implemented"). Events (tombstoning), blobs, the desk index, drafts, notes and assistant records are **not** purged by it yet (use `desk.forget`, `draft.delete`, `assistant.purge`); the `scrollback.forgotten` event records scope and counts, never text. Retention for the scrollback archive (`terminal.archive_max_per_pane`, `terminal.archive_days`, defaults 200 MiB and 30 days, not the table's `retention.scrollback` figure) runs hourly and deletes the matching index rows in the same transaction. `vibeke doctor --rebuild-index` rebuilds the derived archive index offline from the segments.

As built for the session desk: `desk.forget {session | repo | workspace | before}` (`vibeke desk forget`) deletes conversation-index rows; forgotten sessions are tombstoned so the indexer never re-adds them, while source cursors stay put so already-read bytes are not read again. It does not delete the native transcript files (the harness owns them) or drafts (`draft.delete`). The general `vibeke forget` command so far covers only the scrollback archive (above); extending it to call `desk.forget`, and to delete scoped drafts/notes, events and blobs (15 §11), is open.

### 9.3a Assistance egress, consent and retention (14) — as built 2026-10-06

Optional LLM assistance is the one intentional path by which operational content can leave the machine. Rules as implemented (`crates/vk-assist`, `crates/vk-server/src/assist.rs`):

- **Off by default.** `[assistant] enabled = false`; while false, `assistant.generate/confirm` fail with `disabled`. Turning it off takes effect through two paths: every provider attempt (the first and any automatic retry) re-reads the config immediately before sending and refuses — so a request waiting in the queue is cancelled at dispatch, in any session, without anyone polling — and the next `assistant.*` call in a session cancels that session's open requests (aborting an in-flight HTTP attempt). An attempt already sent cannot be recalled; its reservation is charged. Settings are read only from the user's config (repository files cannot configure or redirect assistant traffic).
- **Per-workspace consent.** `vibeke assist consent [workspace]` records a grant in `<state root>/assistant-consent.json` (0600, user-level, shared by the user's sessions): canonical workspace path, connection ID, adapter+endpoint fingerprint, allowed context classes (`selected_text`, `structured_state`, `review_package`; `screen` only when named), optional operation list and optional per-operation `auto_send`. A changed adapter/endpoint invalidates the grant. Consent is checked on IDs before any content is read, **for every workspace a selected object belongs to**: the pane's, the run's (through its pane), the task's, and for a handoff each bound run's. An object whose workspace can't be determined is refused (`workspace_unknown`; a handoff's bound run in that state is left out and listed in the request's inputs). A selection spanning workspaces needs each workspace's consent and always previews (never `auto_send`). `vibeke assist revoke` removes the grant and cancels the receiving session's unfinished requests that depend on that workspace; other sessions re-read the consent file before every provider attempt, so their queued requests and retries are refused (an attempt another session already sent is not aborted). Revocation cannot retract content already sent.
- **Selected inputs only, previewed.** Each operation sends only the inputs the user selected (e.g. the chosen turns' prompts for Suggest task details), bounded by the profile's byte/token limits and redacted with `vk-redact` (built-in patterns plus `[security.redact] patterns`) — the source text **and** its label and identity metadata (a run name or title can carry a token), which are what the preview, the payload and the stored request record carry. `assistant.generate` returns the exact system and user text that would be sent, with a digest; only `assistant.confirm {request, preview_digest}` sends it (or `auto_send`, which must be enabled both in config and in that workspace's consent for that operation, for a single-workspace selection). The TUI preview soft-wraps every line (no clipping), shows control and invisible format characters visibly (`^[`, `<U+202E>`), states the payload bytes and enables `[y]` only once the end of the payload has been on screen; the CLI prints the full text (`--json` gives it exactly). Pattern redaction cannot guarantee removal of every secret; the preview says so.
- **Credentials.** Only an explicitly named environment variable or a user-created key file (regular file, owned by the user, mode 0600, ≤ 4 KiB; opened once without following a final symlink and validated and read through that same descriptor, so it can't be swapped between check and read). Harness and cloud CLI credential stores (`~/.claude*`, `~/.codex`, `~/.config/{claude,anthropic,openai,gcloud}`, `~/.aws`, `~/.ssh`, …) are refused; there is no fallback to ambient keys such as `ANTHROPIC_API_KEY` unless the user names that variable. Keys are read on the coordinator per request, never stored, logged, returned by the API or put in agent environments. Keychain references are accepted in config but report `unsupported_capability` until implemented.
- **Transport.** HTTPS required except for loopback endpoints; endpoints with userinfo are refused; redirects are never followed; certificate validation is never disabled; a body that fails after the headers is an error, never a shortened reply.
- **Content-free errors.** Error messages carry a category and fixed text: provider response bodies, provider-supplied values (an out-of-schema enum value, an unknown cited ID) and configured values (an inline credential string, an endpoint URL, a key-file path, a TOML line) never appear in them; config parse errors name only an unknown key or a line/column.
- **Budgets.** Every provider attempt — an automatic 429 retry included — is admitted separately: budget check against used + reserved, the per-minute rate window, and a reservation (estimated input + full output allowance + maximum cost) that is **persisted before the attempt is sent**. After a crash, a reservation whose request had been dispatched is charged in full. Reported usage replaces only the components it reports; an unreported component keeps its reservation.
- **Authority.** Pane-scoped callers have no `assistant.*` access. An operation can call only `task.review.get`, `task.intent.get` and `pane.read` while gathering context (`assistant_read_only` otherwise). Generated output is validated against a fixed schema (unknown fields such as a model-proposed `method`/`params` are dropped; invented source/target IDs reject the output) and stored as a draft labelled generated; nothing interprets it as an action. No request is started by turn ends, agent launches or timers.
- **Audit.** `assistant.*` events carry metadata only — operation, state, adapter, connection, model, endpoint host, counts of sources/redactions, payload bytes, token counts, attempts, estimated cost, error category — never prompts, source text or generated text.

### 9.4 Telemetry

None by default. Opt-in anonymous crash reports (`telemetry.crash_reports = true`) send a minidump-free, redacted panic message + version + OS. No content, paths are hashed. The update check sends only version, OS, arch, channel.

### 9.5 Debug bundles

`vibeke debug bundle` → a tarball of redacted logs, `state.db` schema + row counts (not content), config with secrets removed, `doctor` output, holder list, terminal capability report. Printed manifest of what's included; `--include-pane <p>` adds a redacted screen capture explicitly.

---

## 10. Updates and supply chain (T8) [M1 basic, M6 full]

- **Release signing**: every artifact is signed with minisign (Ed25519); the public key is compiled into the binary and published on the website and in the repo. Additionally, GitHub Actions build provenance via **Sigstore** (keyless, SLSA level 3 provenance attestations) so third parties can verify `gh attestation verify`.
- `vibeke update` verifies minisign signature + sha256 before swapping; swap is atomic (`rename` of a versioned dir + `current` symlink), previous version kept for `--rollback`. The running server is restarted via `server.restart` (holders untouched).
- Key rotation: binary embeds current + next public key; rotation announced one release ahead.
- Install script (`curl | sh`) verifies the minisign signature using an inline key, and is itself published with a signature and a checksum in the docs for people who verify first.
- **Reproducible builds** target for Linux musl artifacts (M6): pinned toolchain, `--remap-path-prefix`, `SOURCE_DATE_EPOCH`; CI double-builds and compares.
- **Dependency policy**: `cargo-deny` (licenses, advisories via RustSec, banned crates, duplicate versions), `cargo-vet` audits for new dependencies, `Cargo.lock` committed, no build-script network access, minimal dependency count in `vk-hold` (reviewed separately — it's the long-lived process). Integration packages (`@vibeke/pi-extension`) published with npm provenance, zero runtime dependencies.
- Plugins/harness manifests fetched from GitHub are pinned to commit SHAs in the lockfile `~/.config/vibeke/plugins.lock`.

---

## 11. Audit log [M1]

- Separate append-only file `~/.local/state/vibeke/<session>/audit.jsonl` (0600) plus `audit.*` events in the event log, for security-relevant actions: interaction answers (who, channel, decision, scope), policy changes, trust grants/revocations, auto-approvals by policy, elevate grants, capability violations, plugin installs/updates, integration installs/tamper detections, remote machine adds/connections, clipboard-from-remote decisions, rate-limit trips, self-answer attempts.
- Entries are hash-chained (`prev_hash`) so truncation/tampering by a non-root process is detectable by `vibeke doctor --audit` (best-effort; same-UID malware can rewrite the whole chain — T9 out of scope).
- `vibeke audit tail|search` CLI.

---

## 12. Security testing

- Unit tests for every §5.1 invariant (self-answer via direct pane, via spawned child agent, via split pane, via task; cross-pane keystroke writes; **own-pane keystrokes while an interaction is open**; token-less in-pane connection gets pane scope; holder connection without the server key rejected; compat and render connections apply the same scopes).
- Containment tests (M2 sandbox/container, M4 VM, per 13): from inside `sandbox`/`container`/`vm` panes, the control socket, holder sockets, `state.db` and `$HOME` are unreachable; only the brokered pane socket answers, and it refuses foreign run/pane/task/preview targets; host-side git in a box-writable checkout runs nothing the box could have written (hooks, fsmonitor, filters, swapped `.git`).
- Integration test: a "red-team agent" harness (scripted) that attempts a catalog of escalations from inside a pane; CI asserts all are denied and audited.
- Fuzzing of all decoders that parse untrusted bytes: render frames from remote, holder protocol, compat socket JSON, hook payloads, harness transcripts (10 §6).
- Preview tests: SOCKS peer check rejects non-browser processes; proxy DNS-rebinding (`Host` mismatch), credential stripping (`vk_token`, `__Host-vk_preview` never reach upstream; upstream can't set `vk_` cookies), `Domain=` stripping and cross-preview cookie isolation; headless filtering proxy blocks redirects/subresources/WebSockets to forbidden resolved IPs.
- Pre-1.0 external security review (M6) focusing on §3, §5, §7, §8.
