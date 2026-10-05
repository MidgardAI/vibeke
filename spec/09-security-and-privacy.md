# 09 — Security and privacy

Vibeke is, by design, a remote-control surface for shells running with the user's full privileges. Anything that can talk to the control socket can run arbitrary commands as the user. The security model therefore focuses on: (1) keeping the socket and remote links reachable only by the user, (2) **not letting the things Vibeke hosts — agents, repos, plugins — escalate through Vibeke**, and (3) not leaking secrets into logs, events, previews or bug reports.

Milestone tags as in 07: [M1]…[M6].

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
| T3 | Prompt-injected / misbehaving agent | runs commands as the user *inside a pane*, can call the `vibeke` CLI | yes — **main novel threat** | §5 per-pane capability tokens, no self-answering, no policy edits, rate limits, audit |
| T4 | Malicious or compromised plugin | runs code as the user; holds a token | partially (code runs as user — cannot be fully contained before M6 sandboxing) | §6 capability consent, scoped tokens, audit, install-time review, M6 OS sandboxing |
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

### 3.2 Client identities and tokens

Every connection carries an identity, determined at `client.hello`:

| Client kind | How identified | Default capabilities |
|---|---|---|
| `tui` | launched by the user from a terminal not managed by Vibeke (no `VIBEKE_PANE_TOKEN` in env) | `*` (full) |
| `cli` (outside any pane) | same as above | `*` |
| `agent` / `cli` **inside a pane** | presents `VIBEKE_PANE_TOKEN` (injected into the pane env) | **pane scope** — §5.2 |
| `plugin` | presents `VIBEKE_PLUGIN_TOKEN` | the plugin's approved capabilities — §6 |
| `adapter` | hook shim / extension inside a pane, presents the pane token with `kind: adapter` | pane scope + `adapter.*` for *its own pane only* |
| `remote` | the local server's link to a bridge | §7 |
| anonymous | no hello | read-only `server.status`, `api.*` (so `vibeke doctor` works) |

**How do we know a connection comes from inside a pane if it doesn't present a token?** The peer PID is resolved and walked up its process ancestry; if any ancestor is a pane child (holder's child PID is known), the connection is treated as *in-pane* and gets pane scope **regardless of what it claims** — an agent cannot escape its scope by unsetting `VIBEKE_PANE_TOKEN`. (Ancestry check is best-effort on macOS where `getpeereid` gives no PID: use `LOCAL_PEERPID`; if unavailable, connections lacking a valid full-scope proof are pane-scoped when any live pane belongs to the same TTY session.) Users who deliberately want a full-scope CLI inside a pane run `vibeke auth elevate` which requires confirmation in the *TUI* (a popup on the attached client the agent cannot type into because it's out-of-band of the pane's PTY), granting a time-boxed (default 10 min) elevated token for that pane.

Tokens: 256-bit random, held only in server memory (hashed), rotated on server restart; pane tokens re-injected via holder env is impossible after spawn, so on server restart a pane's old token is re-validated through the persisted *hash* (stored in `state.db` 0600) — tokens remain valid across restarts for the life of the pane.

---

## 4. Untrusted repository content (T2) [M2/M3]

Repo-local files Vibeke reads: `.vibeke/config.toml` (layouts, task setup, port hints), `.vibeke/policy.toml`, `.vibeke/harnesses/*.toml`, `.vibeke/setup.sh` / `[task.setup] script`, `.vibeke/previews.toml`.

Rules:
1. **Nothing executable or permission-relevant is used until trusted.** `vibeke policy trust <path>` (or a TUI prompt on first `task new` in that repo) records `(canonical path, blake3 digest of the .vibeke/ tree)`. Any change to the tree invalidates trust → re-prompt showing a diff.
2. **Repo policy can only tighten.** Repo `policy.toml` may add `deny`/`ask` rules; `allow` rules from repo files are ignored unless the user has explicitly trusted that repo *with* `--allow-policy-grants` (shown in red). Global/user policy always wins over repo policy for `allow`.
3. **Setup scripts** run only after trust, with the script content displayed at trust time, in the task's worktree, in a pane (visible, interruptible), never silently.
4. Repo harness manifests may not override built-in harness ids; they get a `repo:` namespace and their `launch`/`resume` argv are shown at trust time.
5. Agents themselves editing `.vibeke/` invalidates trust (digest change) — an agent cannot grant itself permissions by writing policy files.
6. `.env` templates copied into worktrees are never logged and never put into events (only their path).
7. Untrusted repos still work as plain terminals and with built-in harness adapters; only the repo-provided automation is disabled.

---

## 5. Agents as adversaries (T3) — the core of the model [M2]

Agents run as the user and can type anything into their own shell, including `vibeke …` commands. Hook shims and extensions run *in the agent's process tree*. Prompt injection (from repo text, web pages, issue comments, tool output) can steer an agent to abuse the Vibeke API. The goal is that **an agent cannot use Vibeke to obtain more authority than its harness already gave it**.

### 5.1 Invariants

1. **No self-answering.** `interaction.answer`/`interaction.cancel`, and `adapter.interaction.await` responses, are rejected (`permission_denied:self_answer_forbidden`) when the caller's pane is the interaction's pane, or a pane whose AgentRun has a `parent_run_id` chain leading to the interaction's run, or a pane *created by* that run (`created_by_ref`). Lineage is tracked transitively through `agent.spawn`, `pane.split` by an agent, and `task.create` by an agent.
2. **No cross-agent answering by default.** Pane-scoped tokens cannot answer *any* interaction. A user can grant a specific supervisor agent the `interactions_answer` capability for named runs (`vibeke agent grant <supervisor> answer --for <run…>`), recorded and visible; this is how Phase 2 "supervisor agents" will be enabled, deliberately.
3. **No policy changes from panes.** `policy.add/remove/trust`, `integration.install/uninstall`, `plugin.*` install/enable, `config.set --persist`, `machine.add/install`, `server.*` lifecycle: full scope only.
4. **Keystrokes into other panes are not a bypass.** Pane scope cannot `pane.send_text`/`send_keys`/`run`/`agent.prompt` into panes it did not create (its own pane and its descendants only) — otherwise an agent could type "1⏎" into a neighbour's approval dialog. Reading other panes (`pane.read`, `agent.transcript`, `search.query`) is allowed by default within the same workspace and configurable (`security.pane_scope.read = "workspace" | "session" | "self"`), because cross-pane reading is a common legitimate workflow (reviewer agents).
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
| write (text/keys/run/prompt) to own + descendant panes | ✔ | |
| write to other panes | ✘ | grant per pane |
| `agent.start/spawn` | ✔ (rate-limited, depth-limited) | |
| `task.create`, `worktree.create` | ✔ | worktree removal of non-descendant tasks ✘ |
| `interaction.list/get` | ✔ (workspace) | answering ✘ (§5.1) |
| `preview.*`, `browser.screenshot/console/navigate` | ✔ | `browser.eval` ✘ unless granted |
| `image.show`, `notification.send` | ✔ | notifications from agents are labelled with the agent name, rate-limited 6/min |
| `events.subscribe` | ✔ (workspace subjects) | |
| `policy.*`, `plugin.*`, `integration.*`, `machine.*`, `server.*`, `config.set --persist` | ✘ | |

### 5.3 Hook shims and extensions

- Hook shims (`vibeke hook claude PermissionRequest`) run in the agent's tree. They hold the pane token, open interactions and await answers — they can *open* interactions (harmless) and *deliver* answers to the harness, but they receive answers only from the server, which applies §5.1.
- A malicious agent could bypass its own hook (e.g. edit its settings to remove the hook). That doesn't escalate through Vibeke: it's the harness's own permission model being disabled, which the agent could do without Vibeke. Vibeke detects integration tampering (`integration.doctor` on agent start; `integration_tampered` notification when hook config files change while a run is live).

---

## 6. Plugins (T4) [M5, sandboxing M6]

- Install shows: source repo + commit, requested capabilities with risk levels (`interactions_answer`, `panes_write`, `agents_control`, `browser.script`, `network: *` in red), build commands. Consent recorded with manifest digest.
- Updates that widen capabilities require re-consent; narrowing is silent.
- Each plugin process gets its own token limited to its approved capabilities; argv actions get a short-lived (60 s) token.
- Plugin UI contributions are data rendered by Vibeke; plugins cannot inject escape sequences into chrome (all strings sanitized: C0/C1 controls stripped, bidi overrides neutralized).
- Audit: every plugin API call that mutates state is an event with `actor.kind = plugin`.
- M6: OS-level sandbox for process plugins — Linux Landlock (fs) + seccomp (no ptrace) + network namespace allowlist via proxy; macOS `sandbox-exec` profile generated from declared capabilities; plugins can opt-in early in M5 via `sandbox = true`.
- The marketplace index is metadata only; Vibeke never auto-installs or auto-updates plugins without the user running a command (opt-in `plugin.auto_update = "patch"` allowed for non-widening updates).

---

## 7. Remote machines (T5, T6) [M3, QUIC M5]

- **Transport auth**: M3 uses the user's SSH (`ssh -T host vibeke bridge`), inheriting their keys, known_hosts and agent config — no new credential system. M5 QUIC: keys exchanged over the SSH bootstrap (pinned Ed25519 per machine, stored 0600), TLS 1.3 with raw public keys, no CA, no listening port reachable without the key; QUIC listener binds to configured interfaces only (default: the address the SSH bootstrap used).
- **The remote is untrusted input to the local side.** Things a remote server sends that could affect the local machine are gated:
  - Clipboard writes (OSC 52 from remote panes): default `ask-once-per-machine`, configurable `allow|deny|ask`.
  - Open-URL requests (`preview.open`, clicked links): only `http(s)` to forwarded loopback ports or user-clicked links; `file:`, `javascript:`, custom schemes refused.
  - Notifications: labelled with the machine name; rate-limited.
  - Image/blob transfers: size-capped (default 32 MiB each, 512 MiB/hour), MIME-sniffed, images decoded in a separate thread with limits (dimension caps against decompression bombs).
  - Render stream: decoded with strict bounds (max cols/rows, grapheme table size, image count); fuzzed (10 §6).
  - The local server never executes a command on the local machine on behalf of a remote request. (Local → remote is the only direction of command flow.)
- **Port forwarding** binds `127.0.0.1` (and `::1`) on the client machine only, never `0.0.0.0`; ports allocated from a configurable range.
- Remote binary install (`machine.install`): downloads on the remote from the official release URL, verifies signature there (§10); never pushes an unsigned local binary unless `--from-local-binary` is given explicitly.
- A remote session's interactions can be answered from the local TUI — the answer travels to the remote server, which re-applies §5 rules with the local client's (full) identity.

---

## 8. Preview fabric and browser (T6, T7) [M4]

- Each preview gets its own origin: `http://v4.<session>.vibeke.localhost:<port>/` (RFC 6761 `.localhost` resolves to loopback in browsers without DNS) so cookies/localStorage of different dev servers don't collide and a malicious preview page can't read another preview's cookies.
- The proxy validates `Host` (must match the preview's allocated hostname — defeats DNS rebinding) and refuses requests whose `Origin` is a *different* preview host for state-changing methods.
- The proxy never forwards to arbitrary remote hosts: only `(machine, host, port)` tuples registered as previews, and `host` must be loopback or an explicitly declared LAN address on that machine.
- **CDP is never exposed.** The headless browser runs with `--remote-debugging-pipe` (not a TCP port), owned by the server; agents get high-level `browser.*` methods, not raw CDP. `browser.eval` requires an explicit capability.
- Browser profiles are ephemeral per browser session (fresh temp user-data-dir), deleted on close; no access to the user's real browser profile, cookies or password manager. Downloads disabled. File chooser dialogs auto-cancelled. Permissions (camera, mic, geolocation, notifications) denied.
- Screenshots and console logs are stored as blobs (0600) and subject to retention (§9.3).

---

## 9. Secrets and privacy [M1–M3]

### 9.1 What is never stored or logged

- Environment variable *values* (pane env, task env, harness env) — only keys are logged.
- Contents of `.env` files, `auth.json`-style credential files, SSH keys.
- Tokens (pane, plugin, elevate) — only hashes; never in events, logs, debug bundles.
- Keystrokes/input bytes are not logged (input is not an event). Answers to interactions are stored (they're the audit trail), but free-text answers are subject to redaction (below).

### 9.2 Redaction

A shared `vk-redact` module scrubs strings before they enter logs, events, debug bundles, notifications and search snippets sent to remote clients:
- Pattern set: AWS keys, GitHub/GitLab tokens (`ghp_`, `github_pat_`, `glpat-`), OpenAI/Anthropic keys (`sk-…`, `sk-ant-…`), Slack tokens, JWTs, PEM blocks, generic `password=…`/`secret=…`/`token=…` assignments, URLs with userinfo, Bearer headers.
- High-entropy heuristic for 32+ char base64/hex strings adjacent to secret-ish keys.
- User-extensible (`[security.redact] patterns = [...]`).
- Redaction is **not** applied to the live pane view or local scrollback (the user must see their own terminal), but is applied to the FTS index *optionally* (`security.redact_scrollback_index = true` default) so `search` results and Phase 2 remote clients don't surface raw secrets.

### 9.3 Retention

| Data | Default retention | Config |
|---|---|---|
| Event log | 30 days / 2 M rows | `retention.events` |
| `agent.item` detail | 7 days, then compacted | `retention.items` |
| Scrollback archive | 14 days or 2 GiB per session (oldest first) | `retention.scrollback` |
| Screenshots/blobs | 7 days unless referenced by a live object | `retention.blobs` |
| Audit log | 90 days | `retention.audit` |
| Logs | 7 days, 100 MiB | `logging.*` |

`vibeke forget --pane p | --workspace w | --before date` purges archives, events and blobs for scope (events replaced by tombstones to keep `seq` gapless).

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

## 11. Audit log [M2]

- Separate append-only file `~/.local/state/vibeke/<session>/audit.jsonl` (0600) plus `audit.*` events in the event log, for security-relevant actions: interaction answers (who, channel, decision, scope), policy changes, trust grants/revocations, auto-approvals by policy, elevate grants, capability violations, plugin installs/updates, integration installs/tamper detections, remote machine adds/connections, clipboard-from-remote decisions, rate-limit trips, self-answer attempts.
- Entries are hash-chained (`prev_hash`) so truncation/tampering by a non-root process is detectable by `vibeke doctor --audit` (best-effort; same-UID malware can rewrite the whole chain — T9 out of scope).
- `vibeke audit tail|search` CLI.

---

## 12. Security testing

- Unit tests for every §5.1 invariant (self-answer via direct pane, via spawned child agent, via split pane, via task; cross-pane keystroke writes; token-less in-pane connection gets pane scope).
- Integration test: a "red-team agent" harness (scripted) that attempts a catalog of escalations from inside a pane; CI asserts all are denied and audited.
- Fuzzing of all decoders that parse untrusted bytes: render frames from remote, holder protocol, compat socket JSON, hook payloads, harness transcripts (10 §6).
- Preview proxy tests for DNS-rebinding (`Host` mismatch), cross-preview cookie isolation, `Origin` checks.
- Pre-1.0 external security review (M6) focusing on §3, §5, §7, §8.
