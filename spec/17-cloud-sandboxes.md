# 17 — Cloud sandboxes

**Status: in progress (branch `cloud-sandboxes`).** This spec builds the "cloud runners" that [13](13-sandboxes-and-vms.md) §4/§13 and [12](12-phase-2-outlook.md) reserve: a new execution level, `cloud`, backed by hosted sandbox providers. Fly.io Sprites ships first and E2B second. A local `fake` provider backs the tests.

## 1. Goals

- Run a task's panes in a hosted sandbox. Every pane of a task (agent, shell, dev server) shares the task's one box.
- **Send** a running agent from the host to a box at a turn boundary, and **bring it back** to this host or to a paired host (spec 16 §15.2).
- Sign in to a provider from any client. The TUI, PWA, desktop app and CLI all render the same server-provided auth methods.
- See and clean up boxes from every client. Vibeke never touches boxes it did not create, and it never destroys unsynced work without an explicit `force`.
- New providers need one file in `vk-cloud` and no client code.

Non-goals for v1: a remote holder (the holder stays on the host), network policy per profile (the provider's network is used), GPU sizing.

## 2. Architecture

```
host                                                         provider (Sprites / E2B)
┌──────────────┐  unix  ┌──────────┐ PTY ┌───────────────────────┐  wss / Connect  ┌──────────────┐
│ vk-server    │◀──────▶│ vk-hold  │◀───▶│ vibeke cloud exec -it │◀───────────────▶│ box: bash -l │
│  TaskBox     │        └──────────┘     └───────────────────────┘  (detachable)   │  claude …    │
│  (cloud)     │  link: vibeke cloud exec -i <box> -- /vibeke/bin/vibeke sandbox   │              │
│              │◀────── bridge (boxlink mux: broker:<pane>, listen, tcp:<port>) ──▶│ /workspace   │
│              │  git:  vibeke cloud exec -i <box> -- git upload-pack|receive-pack │ (task repo)  │
└──────────────┘                                                                   └──────────────┘
```

The cloud level reuses the `container` level's shape (13 §4, `sandbox_container.rs`). Every in-box operation is an exec argv:
- pane shells;
- the bridge link, which carries the per-pane broker sockets, so hooks and `VIBEKE_SOCKET` work in the box;
- git services for `task.sync`;
- setup scripts.

For containers that argv is `<runtime> exec`. For cloud boxes it is `vibeke cloud exec` (§3.2), which speaks the provider protocol. The holder, the render stream and the holder protocol do not change.

## 3. `vk-cloud`

### 3.1 Provider trait

`crates/vk-cloud/src/lib.rs` defines `Provider`:
- create, get, list, destroy;
- suspend, resume, checkpoint;
- exec, attach, sessions;
- write_file, port_url;
- verify, import, auth_methods.

It also defines `Caps`, `RemoteBox`, `BoxState`, `ExecReq`, `Session` (an `In`/`Out` channel pair) and `CloudError`.

A `CloudError` has a kind: `needs_auth`, `account`, `not_found`, `conflict`, `unsupported`, `rate_limited`, `invalid_params`, `unavailable` or `internal`. Messages never contain a secret.

`providers(cfg)` lists the providers: `sprites`, `e2b`, and `fake` when `VIBEKE_CLOUD_FAKE_DIR` is set.

### 3.2 `vibeke cloud exec`

```
vibeke cloud exec [-i] [-t] [-w DIR] [-e K=V]... [--session-file PATH] <provider>/<box-id> [--] CMD [ARG]...
```

- `-t` runs the command on a terminal.
  - Local stdin goes into raw mode when it is a TTY.
  - The size comes from the local terminal, with 80x24 as the fallback, and `SIGWINCH` sends a resize.
  - Remote output is written to stdout unchanged.
  - The session is created `detachable`.
  - With `--session-file`:
    1. If the file names a session that `sessions()` reports as active, the command attaches to it instead of starting `CMD`.
    2. Otherwise it starts `CMD` and writes the new id to the file (0600).

    `vk-hold` restarts the same argv after a host or server restart, so the pane reattaches.
  - When the connection is lost (`Out::Lost`), the command attaches again with backoff (0.5 s doubling to 10 s, without limit). While it waits, it writes one dim status line: `\r\n\x1b[2m[vibeke: reconnecting to <box>…]\x1b[0m\r\n`.
- `-i` without `-t` runs pipe mode:
  - stdin is sent as data, followed by EOF;
  - stdout and stderr are written separately;
  - the process exits with the remote exit code.

  The bridge link and the git services use pipe mode.
- The command resolves the credential itself with `auth::require` (config, then keychain item, then env). It does not take a secret in argv or env.
- Exit codes:
  - the remote command's own code;
  - 125 when the command cannot run: no credential (stderr `vibeke: needs_auth <provider>`), unknown box, or a provider error;
  - 255 when the connection is lost in pipe mode.

### 3.3 Providers

**Sprites** (`sprites.rs`). The base URL is `https://api.sprites.dev/v1`, and requests use `Authorization: Bearer <org>/<id>/<secret>`.
- Boxes:
  - create is `POST /sprites {name, url_settings:{auth}}`;
  - get, list and destroy use `GET/DELETE /sprites/{name}`;
  - list is `GET /sprites?prefix=vk-` with continuation tokens.
- State is mapped from `status`: `cold`, `warm`, or `running`.
- Exec is a WebSocket on `/sprites/{name}/exec?cmd=..&cmd=..&path=CMD0&tty=&stdin=true&cols=&rows=&dir=&env=K=V&detachable=true`. Attach uses `/sprites/{name}/exec/{session_id}`, and listing sessions is `GET /sprites/{name}/exec`.
  - **TTY mode:** binary frames carry raw bytes. Text frames carry JSON: `resize {cols,rows}`, `signal {signal}`, `session_info`, `exit {exit_code}`, `port_opened`.
  - **Pipe mode:** each binary frame starts with a stream byte: `0x00` stdin, `0x01` stdout, `0x02` stderr, `0x03` exit (code byte follows), `0x04` stdin EOF.
- Files are written with `PUT /sprites/{name}/fs/write?path=&mode=&mkdirParents=true` (octet-stream; the parameter names of the official Go SDK). A checkpoint is `POST /sprites/{name}/checkpoint`.
- Suspend is implicit, because Sprites sleep when idle. Resume is a no-op.
- Auth methods:
  - a pasted token, from `https://sprites.dev/account`;
  - import `fly`, which mints a token from `fly auth token` through `POST /organizations/{org}/tokens` with `Authorization: FlyV1 <macaroon>`;
  - import `sprite-cli`, which reads `~/.sprites/sprites.json`;
  - the env var `SPRITES_TOKEN`.
- `verify` calls `GET /sprites?max_results=1` and labels the account with the response's `org.name`.
- Caps: resize, reattach, checkpoints, port_urls.

**E2B** (`e2b.rs`). The base URL is `https://api.e2b.app`, and requests send `X-API-Key`.
- Boxes:
  - create is `POST /v2/sandboxes {templateID, timeout, autoPause, metadata:{vibeke_host, vibeke_key}, envVars}`;
  - list is `GET /v2/sandboxes?metadata=...`;
  - get and destroy use `GET/DELETE /sandboxes/{id}`;
  - pause is `POST /sandboxes/{id}/pause`;
  - resume is `POST /v2/sandboxes/{id}/connect`.
- Exec goes to envd over Connect RPC with JSON, at `https://49983-{id}.e2b.app`. Requests carry `X-Access-Token: <envdAccessToken>`, which is returned by create and connect.
  - `process.Process/Start` is a server stream. With `pty:{size}` it runs a terminal; without it, it runs a pipe.
  - `SendInput`, `Update` (resize), `SendSignal` and `CloseStdin` are unary calls.
  - `Connect {process:{pid}}` attaches, and `List` lists processes.
  - The session id is the pid.
- Files are written with `POST /files?path=` on envd.
- The box name is the tag name (§6.1). Tags live in metadata.
- Auth methods:
  - a pasted key, from `https://e2b.dev/dashboard?tab=keys`;
  - import `e2b-cli`, which reads `~/.e2b/config.json`;
  - the env var `E2B_API_KEY`.
- `checkpoint` uses `POST /sandboxes/{id}/snapshots`. A snapshot is a template for new sandboxes, not an in-place restore.
- Caps: resize, reattach, explicit_suspend, keeps_memory, checkpoints, port_urls, max_runtime_s 86400.

**Fake** (`fake.rs`, enabled by `VIBEKE_CLOUD_FAKE_DIR`). A box is a directory `<dir>/boxes/<id>/`, and processes run on the host with that directory as the working directory and `HOME`. Terminal sessions use a real PTY, and they are kept by a tiny per-session daemon so `attach` works. The valid credential is `fake-token`. It exists only for tests: it is not an isolation boundary.

**Box root.** `Provider::box_root(id)` is the prefix of every absolute in-box path. It is empty for real providers. For the fake provider it is the box directory, so `/workspace` and `/vibeke` stay inside the box. The server builds every in-box path from it.

## 4. The `cloud` level in the server

- `IsolationLevel::Cloud` (`"cloud"`) is appended to the enum. `IsoRequest` gains `provider: Option<String>` and `cloud_box: Option<String>`, both `#[serde(default)]`. `task.create --isolate cloud --provider sprites` and `agent.spawn {isolate:"cloud", provider}` work through `IsoRequest::from_params`.
- `BoxRunner::Cloud(Box<cloud::CloudCtx>)` lives in `crates/vk-server/src/sandbox_cloud.rs`, as the module `sandbox::cloud`.
- `CloudCtx` holds:
  - `provider`, `box_id` and `name`;
  - `workdir` (`/workspace`);
  - the `bin` path in the box (`/vibeke/bin/vibeke`);
  - `run_dir`, the host directory of the broker sockets;
  - the clone info (host repo, host worktree, branch, base);
  - the link.
- Code isolation is always `clone`. Creating the box takes these steps:
  1. `provider.create`.
  2. Detect the architecture with `uname -m`.
  3. Upload the Linux `vibeke`. It comes from `VIBEKE_ARTIFACT_DIR`, the release cache `~/.cache/vibeke/releases/<version>/vibeke-linux-<arch>`, or `container::linux_vibeke`. If none of these has it, `cloud_bin::fetch_linux_vibeke` downloads the release asset. The download is verified against the signed release manifest (minisign, version, SHA-256) and cached atomically. A box that is a host directory (the fake provider) gets the host binary. The binary goes to `/vibeke/bin/vibeke` (0755).
  4. `git init` `/workspace` with `receive.denyCurrentBranch=updateInstead`.
  5. Push the task branch from the host with `--receive-pack "<vibeke> cloud exec -i <box> -- git receive-pack"`, then check it out.
  6. Set the git identity.
  7. Upload projected credentials (13 §8) to `/vibeke/creds` and `/vibeke/home`.
- `task.sync` pull and push work through `BoxRemote` with the cloud exec git services.
- Each pane gets a `PreparedSpawn` with this argv: `vibeke cloud exec -i -t -w <workdir> -e TERM=… -e VIBEKE_SOCKET=/tmp/vibeke-brokers/<short>.sock … --session-file <ctl>/cloud-session <provider>/<id> -- <box shell or cmd>`. The broker socket is served through the link, the same way as for containers.
- `Isolation {level: cloud, provider: "sprites", network: "provider", scope}`.
- **The host id** is a ULID created once and kept in kv `cloud/host_id`. Owner tags come from it (§6.1).
- **Persistence.** The kv `sandbox/<key>` record, as for other levels, restores the context. A kv `cloud_box/<provider>/<id>` record holds a `BoxRecord`:
  - `{provider, id, name, key, task, tags, created_at, last_activity_at, state, ownership, panes, unsynced, workdir}`;
  - `unsynced` is the last unsynced summary.

  Credentials are never stored here.

Functions that other modules use from `sandbox::cloud`:
```rust
pub async fn ensure_for_task(server: &Arc<Server>, task: &str, provider: Option<&str>) -> Result<Arc<TaskBox>, RpcError>;
pub fn ctx(tb: &TaskBox) -> Option<&CloudCtx>;
pub async fn exec_capture(server: &Arc<Server>, c: &CloudCtx, argv: &[String], stdin: Vec<u8>, timeout: Duration) -> Result<Captured, RpcError>; // {stdout, stderr, code}
pub async fn upload(server: &Arc<Server>, c: &CloudCtx, path: &str, data: Vec<u8>, mode: u32) -> Result<(), RpcError>;
pub async fn unsynced(server: &Arc<Server>, c: &CloudCtx) -> Result<Unsynced, RpcError>; // {commits, dirty, untracked, summary}
pub async fn release_task(server: &Arc<Server>, task: &str, after: &str /* keep|suspend|destroy */, force: bool) -> Result<Value, RpcError>;
pub fn credential(server: &Server, provider: &str) -> Result<(Arc<dyn vk_cloud::Provider>, vk_cloud::Secret), RpcError>; // needs_auth error
```

## 5. Auth API

Methods are in `crates/vk-server/src/cloud_api.rs`.

| Method | Params | Result |
|---|---|---|
| `cloud.providers` | `{verify?: bool}` | `{providers: [{id, label, caps, default: bool, auth: {state: missing\|ok\|invalid, source?: config\|keychain\|env:<VAR>, account?}, methods: [AuthMethod]}]}` |
| `cloud.auth.set` (mutating) | `{provider, token}` | `{provider, account}`. It verifies the token first. A rejected token gives `needs_auth`. |
| `cloud.auth.import` (mutating) | `{provider, source}` | `{provider, account}`. `not_found` when the source has nothing. |
| `cloud.auth.clear` (mutating) | `{provider}` | `{provider, cleared, boxes_running}` |

`AuthMethod` is one of:
- `{kind:"paste_token", label, help_url, hint}`
- `{kind:"import", source, label}`
- `{kind:"env", var}`

**`needs_auth` error.** Any cloud method without a usable credential fails with `permission_denied` and `details: {reason: "needs_auth", provider, methods: [AuthMethod]}`. Clients then:
1. show the sign-in prompt;
2. call `cloud.auth.set` or `cloud.auth.import`;
3. retry the original call once.

The token param of `cloud.auth.set` is redacted in the request log and the audit log, and it never appears in events. Each change emits `cloud.auth.changed {provider} => {state, account}`.

## 6. Box lifecycle

### 6.1 Ownership tags

The name is `vk-<host8>-<key10>`. The two parts are hex blake3 prefixes of the host id and of the box key (`vk_cloud::naming`). E2B also puts both tags in metadata. `list()` returns only boxes with valid tags.

### 6.2 Ownership states (reconciler)

| `ownership` | Meaning |
|---|---|
| `attached` | The host tag is ours, a record exists, and the task exists with live panes or sessions. |
| `idle` | Attached, but with no live session for `idle_suspend_after`. |
| `orphaned` | The host tag is ours, but the task or record is gone. |
| `foreign` | Another host's tag. |
| `missing` | A local record exists, but the provider does not list the box. |

Only live terminal sessions count as activity; Vibeke's own processes (the bridge link, git services, scripts) do not, and the bridge link disconnects when the task's last box pane closes. The reconciler also refreshes `unsynced` for running boxes that Vibeke owns. Commits on other branches or tags of the box that no host ref reaches count as unsynced commits, and submodules with changes count as dirty. It skips sleeping boxes, so the check does not wake them. A box whose repository cannot be inspected reports `unsynced.unknown = true`, and the destroy guard treats that as unsynced.

The reconciler is `cloud_reconcile.rs`.
- It runs at server start, every 10 minutes, and on `cloud.box.list {refresh:true}`.
- It lists each signed-in provider, merges the result with the records, commits changes and emits `cloud.box.changed`.
- It applies the policies:
  - `idle_suspend_after` suspends;
  - `idle_destroy_after` destroys an idle box only when `unsynced` is clean, and never with force.

### 6.3 Box API

| Method | Params | Result |
|---|---|---|
| `cloud.box.list` | `{provider?, ownership?, refresh?}` | `{boxes: [BoxView], errors: [{provider, kind, message}]}` |
| `cloud.box.suspend` / `cloud.box.resume` (mutating) | `{box}` | `BoxView` |
| `cloud.box.checkpoint` (mutating) | `{box, note?}` | `{box, checkpoint}` |
| `cloud.box.destroy` (mutating) | `{box, force?}` | `{box, destroyed: true}`. With unsynced work and no `force`, it fails with `conflict` and `details {reason:"unsynced_changes", unsynced}`. Its panes close first. |
| `cloud.box.adopt` (mutating) | `{box, repo?, title?, root?}` | `{box, task, panes, sessions, repo, branch, worktree}`. It works on orphaned and foreign boxes: it pulls the box's branch into `repo` (default: the recorded repository, when it is on this host; otherwise `invalid_params`, reason `repo_required`), creates a host task with a worktree on it (a new `<branch>-adopted` branch when the host has that branch checked out or with other history), records the box for the task (`adopted`: the name keeps the old host tag, ownership follows the record), attaches the box and opens a pane for each live terminal session (attached to it), or one new pane. |
| `cloud.box.forget` (mutating) | `{box}` | `{box}`. It drops a `missing` record. |
| `cloud.prune` (mutating) | `{provider?, ownership?: ["orphaned","idle"], boxes?: [box], dry_run?, force?}`. `boxes` limits the prune to those boxes; a named box that is no longer a candidate is reported in `skipped`. | `{candidates: [BoxView], destroyed: [box], skipped: [{box, reason}]}` |

`box` is `"<provider>/<id>"`. A task can name only its own recorded box: `task.create {box}` refuses a box that belongs to another task or host (`conflict`, reason `box_not_ours`; use `cloud.box.adopt`), and panes can't pass `box` at all. Destroying a box closes panes and detaches the task context only when it is the task's current box.

`BoxView` has these fields:
- `box`, `provider`, `id`, `name`;
- `state` (`BoxState`) and `ownership`;
- `key`, `task?`, `workspace?`, `panes: [pane]`, `sessions`;
- `created_at` and `last_activity_at`;
- `url?`;
- `unsynced: {commits, dirty, untracked, summary} | null`;
- `caps`;
- `host_tag`.

Event: `cloud.box.changed {box} => BoxView`. A destroyed box sends `state:"destroyed"`.

When a task closes, `on_task_close` decides what happens: `ask` opens the existing task-close interaction with these choices: bring back / keep / suspend / destroy.

## 7. Moves

`cloud.move` runs as a server job. It is in `crates/vk-server/src/cloud_move.rs`.

| Method | Params | Result |
|---|---|---|
| `cloud.move` (mutating) | `{pane? \| run? \| box?, to: {kind:"cloud", provider?, box?} \| {kind:"local"} \| {kind:"peer", peer}, interrupt?, source_after?: keep\|suspend\|destroy}` | `Job` |
| `cloud.jobs` | `{}` | `{jobs: [Job]}` |
| `cloud.cancel` (mutating) | `{id}` | `Job`. A job that reached `resuming` (the commit point) can no longer be cancelled: `conflict`, reason `too_late`. |

`Job` has these fields:
- `id`;
- `direction`: `send` or `bring_back`;
- `pane?`, `run?`, `box?`;
- `from` and `to`;
- `state`: `queued`, `waiting_turn`, `creating`, `bootstrapping`, `exporting`, `uploading`, `importing`, `resuming`, `done`, `failed` or `cancelled`;
- `progress? {done, total}`;
- `error?` (`{kind, message, details}`);
- `result? {pane?, task?, box?, peer_job?}`;
- `created_at` and `updated_at`.

Event: `cloud.job {job} => Job`. Jobs are kept in kv `cloud_job/<id>` and listed for 7 days.

**Send** (host to cloud):
1. Wait for the turn boundary, as handoff does. `interrupt` interrupts the turn instead.
2. `ensure_for_task`. A pane without a task first gets a task on its checkout's current branch. The task's earlier box, if one is on record (for example a box suspended after a bring-back), is reused and woken.
3. Export the bundle on the host with `vk_handoff::export`. This is the gateway's `export_bundle` core, moved into `vk-handoff`.
4. Upload it to `/vibeke/in/<job>.tar.zst`.
5. Import it in the box with `vibeke sandbox import-bundle --bundle <file> --workspace /workspace`, which prints `{cwd, resume_argv}`.
6. Open a box pane in the task and type the resume command.
7. Close the source run's pane.

If any step fails, the source keeps running, and the send is rolled back: the task's box context is detached, the task gets its earlier isolation back, and a box the send created is destroyed when nothing was imported into it (the job result's `rollback`). A retry then works. When the source runs an agent, its pane closes only after the agent's run in the box is confirmed (it left `starting` and has not exited).

**Bring back** (cloud to this host or a peer):
1. Wait for the turn boundary in the box pane.
2. Run `vibeke sandbox export-bundle --cwd <cwd> [--harness H --session S --transcript P]` in the box. It streams the bundle on stdout.
3. Then, depending on the target:
   - **local:** `task.sync` pulls, then the bundle is imported with the server's handoff import (`handoff::import_local`) into the task's host worktree. A clean worktree at an ancestor of the bundle's head gets the import in place; otherwise the import goes into a new worktree, using handoff's placement. The agent resumes in a new host pane, and the task's isolation becomes `host`.
   - **peer:** the bundle is written to the gateway handoffs directory, and a handoff job is created with `bundle` set. The gateway's `handoff_send` skips the export when `bundle` is set and transfers the file.
4. Mark the box's working tree as synced. This happens only for a local bring-back where the export skipped nothing and the import wrote every file.
   - Before the export, the job takes a fingerprint of the tree (`TREE_FINGERPRINT`: HEAD, the diff, and the names and contents of untracked files).
   - After the import, it writes that fingerprint to `.git/vibeke-synced-tree` in the box.
   - The unsynced report counts the tree's changes as synced only while the fingerprint still matches, so a change made during or after the export counts again.
   - A tree with submodule changes is never marked: a bundle does not carry them.
   - A peer bring-back never marks the tree, because the peer's import is not confirmed. That box needs `force` to be destroyed.
   - A later send clears a marked tree before its import, but only while the fingerprint matches.
5. Apply `source_after` (default `[cloud] after_bring_back`) to the box with `release_task`. `release_task` checks for unsynced work before it suspends, so the check does not wake the box again.

## 8. Clients

- **TUI:**
  - `cloud.rs` handles the send flow, with these stages: provider, auth, box, sending.
  - `sandboxes.rs` is the overview: groups by provider, row actions, and Clean up with a dry run.
  - The auth stage renders `methods`. Its token field is masked and accepts bracketed paste.
  - Actions: `cloud_send` ("Send to cloud…"), `cloud_bring_back` ("Bring back…") and `sandboxes` ("Sandboxes"). They are unbound and listed in the palette and the pane menu.
  - `push::TYPES` gains `cloud.job`, `cloud.box.changed` and `cloud.auth.changed`.
  - The tab bar shows job progress as `☁ <provider> <state>`.
- **Web** (`web/packages/ui`, shared by the PWA and desktop):
  - `screens/cloud-send.tsx` is the send/bring-back sheet;
  - `components/cloud-auth.tsx` renders the auth methods (password field, `openExternal(help_url)`);
  - `screens/sandboxes.tsx` is the `#/sandboxes` route;
  - `app/cloud-stores.ts` holds the stores.
  - Commands live next to handoff, with Full scope only.
  - The gateway gives every `cloud.*` method `Full` scope and forwards it with a params allowlist.
- **CLI:**
  - `vibeke cloud providers`
  - `vibeke cloud login <provider> [--import <source>]` reads the token with echo off, or from stdin when stdin is not a TTY.
  - `vibeke cloud logout <provider>`
  - `vibeke cloud ls [--ownership X] [--refresh]`
  - `vibeke cloud send [--pane P] [--provider X] [--box B]`
  - `vibeke cloud bring-back <pane|box> [--to local|<peer>]`
  - `vibeke cloud rm <box> [--force]`
  - `vibeke cloud prune [--dry-run] [--force]`
  - `vibeke cloud adopt <box>`
  - `vibeke cloud suspend|resume <box>`
  - `vibeke cloud exec …` (§3.2)
  - `vibeke task create --isolate cloud --provider sprites`

## 9. Config

`[cloud]` is described in `vk_cloud::config` (`default_provider`, `idle_suspend_after`, `idle_destroy_after`, `on_task_close`, `after_bring_back`). Each provider also has its own table, `[cloud.<provider>]`, with `credential`, `api_url`, `template`, `timeout`, `auto_pause` and `url_auth`.

## 10. Security

- A box is a different machine with its own network. A cloud box sees only the task clone, the projected harness credentials and the broker of its own panes.
- The host's provider credential never enters the box.
- Every `cloud.*` method is forbidden from pane scope. An agent can ask for `cloud.move` only through `auth.approve`.
- Destroying a box with unsynced work needs an explicit `force`. Policy-driven destroys never use force.
- Tokens are kept in the keychain. They are redacted in logs, and they never appear in argv, env, events or the box.
