# Herdr compatibility inventory (generated)

Baseline: **Herdr v0.9.3** at commit `7b116c05bfda646af39d2524c54e70c751f57ee8` (spec 07 §8.0). Generated from `crates/vk-compat/src/herdr/inventory.rs`; regenerate with `VIBEKE_UPDATE_INVENTORY=1 cargo test -p vk-compat inventory`.

**Status: partial.** This inventory lists every surface the spec's §7.7/§8 text and tables name, plus surfaces the 99 real plugins in `tests/compat/herdr/0.9.3/manifests/` use. It is not yet the exhaustive inventory §8.0 requires, which must be derived from the pinned binary's `herdr api schema --json`, CLI help and manifest schema. No Herdr binary was run to build it. *Implemented* means mapped and tested against Vibeke using the spec's shapes; no entry is certified until the differential suite (07 §8.4) passes. *Source* `corpus` marks entries the plugins use but the spec does not name.

Real-plugin results: see the latest run of the [Herdr plugins workflow](https://github.com/MidgardAI/vibeke/actions/workflows/herdr-plugins.yml) for the current matrix. Runtime results stay out of this file.

| Area | Implemented | Partial | Missing | Total |
|---|---|---|---|---|
| Socket wire protocol and endpoints | 11 | 6 | 0 | 17 |
| Socket methods | 27 | 49 | 9 | 85 |
| Events (subscriptions and `[[events]]` hooks) | 15 | 12 | 1 | 28 |
| CLI commands | 18 | 27 | 8 | 53 |
| Plugin manifest fields | 9 | 5 | 0 | 14 |
| Plugin invocation environment | 12 | 3 | 0 | 15 |
| Plugin lifecycle, registry and trust | 14 | 6 | 1 | 21 |
| **All** | **106** | **108** | **19** | **233** |

## Socket wire protocol and endpoints

| Entry | Status | Source | Notes |
|---|---|---|---|
| newline-delimited JSON request `{id, method, params}` | implemented | spec | no JSON-RPC envelope, no handshake |
| string `id` required; integer id rejected | implemented | spec | `invalid_request`, empty id echoed |
| success `{id, result: {type, …}}` | implemented | spec | result types from the 07 §8.3 table |
| error `{id, error: {code, message}}` | implemented | spec | string id echoed when recoverable; code set unverified |
| one request per connection, closed after the response | implemented | spec |  |
| `events.subscribe` streams `{event, data}` lines | partial | spec | projected subset of events; loss/reconnect behavior not reproduced |
| invalid UTF-8 / invalid JSON handling | partial | spec | `parse_error`; exact baseline codes unverified |
| line limit | partial | spec | 16 MiB; baseline limit unverified |
| socket path `<herdr_root>/herdr.sock` | implemented | spec | `<herdr_root>` = `$RUNTIME/herdr-compat` (shared by sessions): default session `herdr.sock`, named sessions `sessions/<name>/herdr.sock`; never under ~/.config/herdr unless `compat.herdr_socket_path` names it (then named sessions sit next to it); a live socket at the path is never replaced |
| socket removed on clean stop | implemented | spec | on SIGTERM/SIGINT and `server.stop`; a stale socket after a crash is replaced at the next start |
| caller identity from peer credentials (pane scope) | implemented | spec | same ancestry rule as the native socket |
| private broker endpoint per plugin invocation | implemented | spec | 0600 socket in a 0700 dir, bound server-side to plugin id, grant digest and grant id |
| broker re-checks the grant on every request | implemented | spec | every request re-checks the exact grant (digest + grant id, so revoke then re-grant does not revive it) and that the broker is open and its invocation alive; `events.wait` re-checks after waking; otherwise `permission_denied` |
| broker bindings survive server recovery | implemented | spec | persisted in `brokers.json`, re-issued at the same path for live invocations whose grant still matches; output is written by a capture helper (bounded per invocation) to files that are tailed, so it survives; pid reuse is not detected; exit status after a restart is unknown |
| broker authority for long-lived children after the action exits | partial | spec | deliberate difference: Herdr gives every invocation the real server socket, so a daemon an action starts keeps API access; Vibeke keeps it only for long-running entrypoints (`[[startup]]` brokers follow the process group, plugin-pane brokers the pane). Action and hook brokers close when the action or hook process exits, and closing a broker drops every connection it accepted (requests in flight, subscriptions, waits), so a background process an action starts loses the socket (for example a web server that an action starts); run such a process from `[[startup]]` or a plugin pane |
| plugin identity across sessions (`herdr --session`) | partial | spec | single-use ticket from the invocation's own broker, verified with the issuing session (broker open, invocation alive, same grant), grant re-checked on the destination; no identity from environment values; `events.subscribe`/`events.wait` across sessions are refused for plugins |
| Herdr-style ids from a persisted mapping table | partial | spec | Vibeke handles (`w<n>`, `w<n>:t<n>`, `w<n>:p<n>`) are persisted and use the same grammar; baseline allocation/move semantics unverified |

## Socket methods

| Entry | Status | Source | Notes |
|---|---|---|---|
| `session.snapshot` | partial | spec | `layouts` are per-tab layout snapshots (shape unverified); version/protocol report the emulated baseline |
| `workspace.list` | implemented | spec |  |
| `workspace.create` | implemented | spec | `workspace_created {workspace, tab, root_pane}` |
| `workspace.rename` | implemented | spec |  |
| `workspace.move` | implemented | spec | `insert_index` |
| `workspace.focus` | implemented | spec |  |
| `tab.list` | implemented | spec |  |
| `tab.create` | implemented | spec |  |
| `tab.rename` | implemented | spec |  |
| `tab.move` | implemented | spec | `insert_index` |
| `tab.focus` | implemented | spec |  |
| `tab.close` | implemented | spec | closes all panes |
| `pane.list` | partial | spec | Herdr `PaneInfo` (absent optional fields left out, `agent_status` `unknown` without an agent, `scroll` as `{offset_from_bottom, max_offset_from_bottom, viewport_rows}`, `title`/`display_agent`/`state_labels`/`tokens` from `pane.report_metadata`, `terminal_title` from the terminal); `terminal_id` = pane id, `foreground_cwd` = cwd; no `restore_error` or `terminal_title_stripped`; focus is the most recent client's, else the last focus any client had |
| `pane.get` | partial | spec | Herdr `PaneInfo`, see `pane.list` |
| `pane.current` | implemented | spec |  |
| `pane.read` | partial | spec | `format: ansi` returns text; baseline wrapping rules unverified |
| `pane.send_text` | implemented | spec | raw bytes, no bracketed paste |
| `pane.send_keys` | partial | spec | Vibeke key grammar, not restricted to Herdr's accepted set |
| `pane.send_input` | partial | spec | text then keys |
| `agent.send` | implemented | spec | literal text, no Enter |
| `pane.focus` | implemented | spec |  |
| `pane.rename` | implemented | spec | null clears |
| `pane.close` | implemented | spec |  |
| `pane.split` | partial | spec | splits `target_pane_id` (baseline), else `pane_id`, the invocation's pane or the focused pane; direction/cwd/focus; size params ignored |
| `pane.wait_for_output` | partial | spec | `match`/`regex`, `timeout_ms`; emits `pane.output_matched` |
| `pane.report_agent` | implemented | spec | Herdr self-report path (selfreport.rs) |
| `pane.report_agent_session` | implemented | spec |  |
| `agent.list` | partial | spec | status projection unverified |
| `agent.get` | partial | spec |  |
| `agent.read` | partial | spec | reads the agent's pane |
| `agent.start` | partial | spec | Herdr's `kind` (also `agent`/`harness`) → harness; `pane_id` starts in that pane, else splits the target/focused pane; `agent_started {agent, argv}`, where `argv` is the harness and `args` (not the resolved command line) |
| `agent.prompt` | partial | spec | text + Enter (native `agent.prompt`), `wait`, `timeout_ms`; `agent_prompted {agent}` |
| `agent.wait` | partial | spec | `status`/`until` in Herdr statuses mapped to Vibeke wait conditions; `timeout` |
| `agent.rename` | partial | spec | `agent_info {agent}` |
| `agent.explain` | partial | corpus | `agent explain`: result `agent_explain` {agent, agent_status, reason, execution, blocked, unseen_turn, turns_completed, last_tool, last_message, source}; Vibeke's own shape, baseline unverified |
| `worktree.list` | partial | spec | native shape under `worktree_list` |
| `worktree.repo_root` | partial | spec |  |
| `worktree.create` | partial | spec | `git worktree add` under `[tasks] root`, opens a workspace (`open` defaults to true); no `path` param; emits `worktree.created` |
| `worktree.open` | partial | spec | reuses the workspace rooted at the worktree or creates one; `worktree_opened {workspace, tab, root_pane, worktree, already_open}` (Vibeke extra `created`); only `path` is accepted; emits `worktree.opened` |
| `events.subscribe` | partial | spec | see events |
| `events.wait` | partial | spec | first projected event matching the subscription |
| `plugin.list` | implemented | spec | registry entries with status |
| `plugin.link` | implemented | spec | registers in place; never builds or trusts; refused from panes; `plugin_linked {plugin}` |
| `plugin.unlink` | implemented | spec | files preserved; refused from panes; `plugin_unlinked {plugin_id, removed}` (`removed: false` for an unknown plugin) |
| `plugin.enable` | implemented | spec | refused from panes; `plugin_enabled {plugin}` |
| `plugin.disable` | implemented | spec | refused from panes; cuts live brokers; `plugin_disabled {plugin}` |
| `plugin.action.list` | implemented | spec |  |
| `plugin.action.invoke` | partial | spec | qualified `action_id` (`<plugin>.<action>`) with optional `plugin_id`; returns Herdr's `plugin_action_invoked {action, context, log}` at once (the log is still running); the context is Herdr's `merge_plugin_context` (active workspace and focused pane, then the request's `context` field by field); `correlation_id` defaults to the log id (Herdr: the request id) |
| `plugin.log.list` | partial | spec | records carry `command` (argv), `started_unix_ms`/`finished_unix_ms`, status `running`/`succeeded`/`failed`, exit code and separate stdout/stderr; remaining baseline fields unverified |
| `plugin.pane.open` | partial | spec | all placements: `split`, `tab`, `zoomed`; `overlay` = a full-area layer over the tab, `popup` = a session-modal floating window sized by `width`/`height` (cells or `%`, default 80%) with no pane id; both close when the command exits and restore the prior focus; one popup at a time (`ui_busy`); Herdr's `invalid_params` rules and messages for `target_pane_id`/`workspace_id`/`direction`/`width`/`height` per placement; `env` applied after Herdr's checks (`invalid_env`), protected keys dropped; `target_pane_id` (Vibeke also accepts `pane_id`); context and env as Herdr's `panes.rs`; one broker per pane; result `plugin_pane_opened {plugin_pane: {plugin_id, entrypoint, pane}}` (Vibeke extra `placement`), a popup answers `ok` as in Herdr |
| `plugin.pane.focus` | partial | spec | `pane_id` (Vibeke also: `plugin_id` + entrypoint); `plugin_pane_focused {plugin_pane}`; `plugin_pane_not_found` for a pane that is not a plugin pane |
| `plugin.pane.close` | partial | spec | `pane_id` (Vibeke also: `plugin_id` + entrypoint); `plugin_pane_closed {pane_id}`; `plugin_pane_not_found` for a pane that is not a plugin pane |
| `popup.close` | partial | spec | closes the open popup (a plugin only its own) and restores focus; `ok`, or `popup_not_open` when none |
| `layout.export` | partial | spec | `layout_export {layout: LayoutDescription}`: binary split tree (`direction` `right`/`down`, `ratio`, `first`, `second`; panes with `pane_id`, `label`, `cwd`, never `command`); `tab_id` or `pane_id`, else the focused tab; Vibeke extension: `workspace_id` alone lists every tab under `layouts` |
| `layout.apply` | partial | spec | `root` (with `tab_id`), or a whole description under `layout`; rearranges the tab's own panes, every pane exactly once (Herdr can also create panes with `command`/`env` and a new tab; Vibeke cannot yet); `layout_apply {layout}` |
| `layout.set_split_ratio` | partial | spec | `path` (booleans from the root, `true` = second side), a split `id` from `pane.layout` as `split_id`, or `pane_id`; ratio of the first side; `layout_split_ratio_set {layout}`; `split_not_found` |
| `pane.process_info` | partial | spec | `pane_process_info {process_info: {pane_id, shell_pid, foreground_process_group_id, foreground_processes: [{pid, name, argv0?, argv, cmdline, cwd}]}}` read live like Herdr (Linux `/proc`: the group's members below the leader and the shell, sorted by pid, `comm` as name; macOS libproc: the group's members with `argv0`); `tty` is never set (as in Herdr) |
| `pane.layout` | partial | spec | `pane_layout {layout: PaneLayoutSnapshot}` for `pane_id` or the focused pane: pane and split rects, split ids `split_<n>_<path>`, `right`/`down`, Herdr's rounding (pane rects include borders); the area is rebuilt from the panes' current sizes at origin 0,0 (Herdr reports its terminal area, offset by its sidebar), 80x24 per pane never sized by a client |
| `pane.move` | partial | spec | Herdr's `destination {type: tab, tab_id, split, target_pane_id?, ratio?}` within a workspace, or Vibeke's flat `tab_id`/`target_pane_id`/`direction`; `pane_move {move_result: PaneMoveResult}` with `changed: false` + `same_tab`/`zoomed_tab`; `new_tab`/`new_workspace` destinations and cross-workspace moves unsupported; emits `pane.moved` |
| `pane.swap` | partial | spec | Herdr's `source_pane_id` + `target_pane_id` or `pane_id` + `direction`; `pane_swap {swap: PaneSwapResult}` with `changed: false` + `not_found`/`same_pane`/`cross_tab`/`no_neighbor`; Vibeke's older `pane_id` + `other_pane_id` also swaps across tabs of a workspace; emits `pane.moved` for both panes |
| `pane.resize` | partial | spec | direction + percent |
| `pane.zoom` | partial | spec |  |
| `client.window_title.set` | implemented | spec | evented (`client.window_title_changed`); the TUI shows it as the outer terminal title (OSC 2) with `ui.title_sync`, else in the tab bar; sanitized; `client_window_title {changed, reason}` (`no_foreground_client` without an attached client; the title is kept for the next client) |
| `client.window_title.clear` | implemented | spec | back to `ui.title_format`; `client_window_title {changed, reason}`; see `client.window_title.set` |
| `agent.view.set` | partial | spec | Herdr's `{source, label?, filter?, sort[]}`: validated like Herdr (`invalid_agent_view`), one active view per session, `agent_view {active, source, label}`; stored and announced, but Vibeke's sidebar does not apply the filter and sort yet. Vibeke extension: a plugin invocation's run status line (`target`, `text` <= 80 chars, `detail`, `tone`), <= 16 per plugin, shown in the sidebar and peek, gone when the plugin is disabled or untrusted |
| `agent.view.clear` | partial | spec | `{source?}` clears the active agent-list view when the source matches (`agent_view`); without `source` a plugin's run status lines go too; Vibeke's `target` clears one run's status line |
| `pane.report_metadata` | partial | spec | merged per pane (`metadata`, `key`/`value` or flat params), shown as `metadata` in pane records and as Herdr's `title`, `display_agent`, `state_labels` and `tokens` (merged per key; `clear_*` flags honoured); emits `pane.updated`; `seq`, `ttl_ms`, `source` precedence not applied; in memory |
| `workspace.report_metadata` | partial | spec | as `pane.report_metadata`, per workspace; `tokens` shown in `WorkspaceInfo`; emits `workspace.metadata_updated` |
| `ping` | partial | spec | reports the emulated baseline |
| `api.schema` | partial | spec | lists inventory methods, not the baseline JSON schema |
| `server.reload_config` | partial | corpus | acknowledged; Vibeke reloads config itself |
| `server.stop` | partial | corpus | stops this Vibeke session when `[compat.herdr] allow_server_stop = true` (default off: `unsupported`); never from a plugin, a sandboxed invocation or a pane; never reaches a Herdr server |
| `workspace.close` | partial | corpus | inferred from CLI usage |
| `workspace.get` | partial | corpus | inferred |
| `notification.show` | partial | corpus | `herdr notification show`; maps to a Vibeke notification |
| `pane.run` | partial | corpus | types the command + Enter; Herdr's `herdr pane run` (and the shim) send `pane.send_input` instead |
| `pane.neighbor` | missing | corpus | `herdr pane neighbor`; not served yet |
| `pane.edges` | missing | corpus | `herdr pane edges`; not served yet |
| `pane.focus_direction` | missing | corpus | `herdr pane focus --direction`; not served yet |
| `pane.input.set` | missing | corpus | `herdr pane input --right-click`; not served yet |
| `pane.release_agent` | missing | corpus | `herdr pane release-agent`; not served yet |
| `tab.get` | missing | corpus | `herdr tab get`; not served yet |
| `agent.focus` | missing | corpus | `herdr agent focus`; not served yet |
| `agent.send_keys` | missing | corpus | `herdr agent send-keys`; not served yet |
| `worktree.remove` | missing | corpus | `herdr worktree remove`; not served yet |

## Events (subscriptions and `[[events]]` hooks)

| Entry | Status | Source | Notes |
|---|---|---|---|
| `workspace.created` | implemented | spec |  |
| `workspace.updated` | partial | spec | emitted on rename only |
| `workspace.metadata_updated` | partial | spec | from `workspace.report_metadata`; subscriptions only (Herdr runs no hooks for it) |
| `workspace.renamed` | implemented | spec |  |
| `workspace.closed` | implemented | spec |  |
| `workspace.focused` | partial | spec | derived from pane focus of any client |
| `workspace.moved` | implemented | spec |  |
| `tab.created` | implemented | spec |  |
| `tab.closed` | implemented | spec |  |
| `tab.focused` | partial | spec | derived from pane focus |
| `tab.renamed` | implemented | spec |  |
| `tab.moved` | implemented | spec | native `tab.moved` from `tab.move` |
| `pane.created` | implemented | spec |  |
| `pane.closed` | implemented | spec |  |
| `pane.focused` | implemented | spec |  |
| `pane.updated` | partial | spec | on a pane rename, cwd change or `pane.report_metadata`; not on terminal title changes; subscriptions only (Herdr runs no hooks for it) |
| `pane.output_changed` | missing | spec | a Herdr `EventKind` without a subscription type; Vibeke accepts it in `events.subscribe` but never emits it |
| `pane.moved` | implemented | spec | from `pane.move`/`pane.swap`; `previous_pane_id` equals the pane id (Vibeke ids are stable across moves) |
| `pane.exited` | implemented | spec |  |
| `pane.agent_detected` | partial | spec | 250 ms debounce per pane; `released`/`final_status` never set |
| `pane.agent_status_changed` | partial | spec | fires on mapped-status change only (Herdr also fires on title/label changes); hooks get the `EventEnvelope`, pane-scoped subscriptions Herdr's dotted `SubscriptionEventEnvelope` |
| `pane.output_matched` | partial | spec | fires when a compat `pane.wait_for_output` matcher matches (Herdr matches in the subscription itself); `{pane_id, matched_line, read}` |
| `pane.scroll_changed` | partial | spec | from the TUI's copy-mode viewport (`ClientFrame::ScrollView`, coalesced to 150 ms); `{pane_id, workspace_id, scroll: PaneScrollInfo}`, also in `pane.list` |
| `layout.updated` | partial | spec | payload carries `layout` as Herdr's `PaneLayoutSnapshot` (see `pane.layout`; also the `layouts` of `session.snapshot`) |
| `worktree.created` | implemented | spec | from `worktree.create` and `task.create` worktree checkouts; `{workspace: WorkspaceInfo with worktree, worktree: WorktreeInfo}`; only when a workspace is open at the worktree; used by 8 corpus plugins |
| `worktree.opened` | implemented | spec | from `worktree.open`; `{workspace, worktree, already_open}`; used by 5 corpus plugins |
| `worktree.removed` | partial | spec | `{workspace_id, workspace?, worktree, forced}`; `forced` is always false |
| `workspace.reordered` | partial | corpus | declared by one plugin, not named by the spec; emitted after every `workspace.move` (the only reordering Vibeke has); `{workspace_ids, workspaces}` without `before_workspace_id` |

## CLI commands

| Entry | Status | Source | Notes |
|---|---|---|---|
| output of socket-backed commands | implemented | spec | the whole response line `{"id":"cli:…","result":…}` on stdout (exit 0), error lines on stderr (exit 1, `permission_denied` too); keys sorted as Herdr prints them; request ids as Herdr's CLI sends them (`cli:pane:get`, `cli:plugin`, `cli:request`, …); `pane send-text|send-keys|run|report-*` and `workspace report-metadata` print nothing on success; `pane read` and `agent read` print the text; the shim's own errors use Herdr's `{id, error}` shape (`server_not_running` when nothing answers) |
| argument errors | implemented | spec | Herdr's per-command parsers: `unknown option: …`, `missing value for …`, usage lines and command-group help on stderr with exit 2 (`<group> help`: exit 0); `unknown command: …` for commands Herdr does not have (`ping`, `events`, `layout`, `popup`, `session snapshot`, …: use the socket methods); values Herdr rejects in `main` print `Error: Custom { … }`, exit 1 |
| --version | partial | spec | prints `herdr 0.9.3 (Vibeke … compatibility layer, partial)`; Herdr prints `herdr 0.9.3` |
| --help, <command> --help | implemented | corpus | `<command> --help` prints the captured 0.9.3 help page; `herdr --help` prints the shim's own summary |
| --default-config | implemented | corpus | prints the captured 0.9.3 default configuration |
| launch and attach (`herdr`, `client`, `--remote`, `--machine`, `--skill`) | missing | corpus | refused: the shim never launches or attaches a Herdr client; run `vibeke` |
| global session selection (`--session`, HERDR_SESSION) | partial | spec | `--session NAME` and `HERDR_SESSION`; explicit sessions never spawn (`server_not_running`, exit 1); panes cannot switch sessions; plugins keep their identity (grant re-checked on the destination) |
| plugin install <path> | partial | corpus | a Vibeke addition (Herdr installs GitHub sources only): local directories and manifest paths; build runs after trust |
| plugin install owner/repo[/subdir] [--ref] | partial | spec | Herdr's grammar `[--ref REF] [--yes|-y] <source>`; shallow, hardened clone (no hooks/fsmonitor/filters, https+file only) of `https://github.com/owner/repo` at a tag, branch or sha (`@ref` or `--ref`); the resolved commit is recorded and pinned by the trust grant; prints Vibeke's registry JSON, not Herdr's install messages |
| plugin install --yes | implemented | spec | accepts the displayed legacy trust terms (operator only) |
| plugin link | partial | corpus | `<path> [--disabled|--enabled]`; no build; prints Vibeke's registry JSON, not Herdr's `cli:plugin` response |
| plugin unlink | partial | corpus | files preserved; prints Vibeke's registry JSON, not Herdr's `cli:plugin` response |
| plugin uninstall | partial | corpus | plugin id only (not `owner/repo`); managed checkout removed, config/state kept; prints Vibeke's registry JSON, not `Uninstalled <id>.` |
| plugin enable / disable | partial | corpus | prints Vibeke's registry JSON, not Herdr's `cli:plugin` response |
| plugin list | partial | corpus | `[--plugin ID] [--json]` accepted; always prints Vibeke's registry JSON (no filter, no table) |
| plugin config-dir | partial | corpus | prints a JSON object; Herdr prints the bare path |
| plugin action list / invoke | implemented | corpus | `list [--plugin ID]`; `invoke <action_id> [--plugin ID]` with `context.invocation_source = cli`; results: see the methods |
| plugin log list | implemented | corpus | `[list] [--plugin ID] [--limit N]` (`logs` alias) |
| plugin pane open | implemented | corpus | `--plugin --entrypoint --placement --width --height --workspace --target-pane --direction --cwd`, repeated `--env K=V` into `env`, `--focus/--no-focus` (focus defaults to true), `fullscreen` = `zoomed`; placement behaviour: see `plugin.pane.open` |
| plugin pane focus|close <PANE_ID> | implemented | corpus | the positional is the pane id (`pane_id`) |
| plugin update | partial | corpus | a Vibeke addition: re-fetches the recorded repository and ref; a new commit or manifest leaves the plugin inactive until reviewed again; a link or local install is refused |
| pane list|current|get|rename|close|send-text|send-keys|wait-output | implemented | corpus | `list --workspace` → `workspace_id`; `current [--pane|--current]` → `caller_pane_id` (default `HERDR_PANE_ID`); `rename <id> <label…>|--clear`; `wait-output (--match|--regex) [--source --lines --timeout --raw]` |
| pane read | implemented | corpus | `[--source --lines --format --ansi --raw]`; prints the text |
| pane run | implemented | corpus | sends `pane.send_input {text, keys: ["Enter"]}` like Herdr (not `pane.run`) |
| pane split | implemented | corpus | `[PANE_ID|--pane|--current] --direction [--ratio --cwd --env… --right-click --focus/--no-focus]` → `target_pane_id` (default `HERDR_PANE_ID`), `env`, `focus` (default false), `right_click` (default herdr) |
| pane layout|process-info | partial | corpus | `[--pane ID | --current]` (`--current` = `HERDR_PANE_ID`); results: see `pane.layout`, `pane.process_info` |
| pane neighbor|edges|focus|input | missing | corpus | Herdr's grammar (`focus` is directional: `pane.focus_direction`); the server does not serve `pane.neighbor`, `pane.edges`, `pane.focus_direction` or `pane.input.set` yet (`unsupported`) |
| pane resize|zoom|swap|move | partial | corpus | Herdr's parameters: `resize {direction, amount}`, `zoom {mode}`, `swap {source_pane_id, target_pane_id}` or `{pane_id, direction}`, `move {pane_id, destination: {type, …}, focus}`; the server still reads Vibeke's names for `pane.swap` (`other_pane_id`) and `pane.move` (`tab_id`, `direction`), so those two fail from the CLI |
| pane report-agent|report-agent-session|release-agent|report-metadata | partial | corpus | Herdr's flags (`--source` required, `-- <resume argv>`, `--token`/`--clear-token`, `--state-label`, …) and parameters; `pane.release_agent` is not served yet; metadata handling: see `pane.report_metadata` |
| workspace list|create|get|focus|rename|close | implemented | corpus | `create [--cwd --label --env… --focus/--no-focus]`; `close <id> [--group]` → `close_group` |
| workspace report-metadata | partial | corpus | `<id> --source ID (--token K=V | --clear-token K)… [--seq --ttl-ms]` → `tokens`; metadata handling: see `workspace.report_metadata` |
| tab list|create|get|focus|rename|close | partial | corpus | `list|create --workspace` → `workspace_id`; `tab.get` is not served yet |
| worktree list|create|open|remove | partial | corpus | Herdr's flags; paths made absolute like Herdr (`~` and the working directory); `open` takes `--path` or `--branch`, never a positional; `worktree.remove` is not served yet |
| agent list|get|read|send-keys|prompt|rename|focus|wait | partial | corpus | Herdr's grammar (`wait --until STATUS… --timeout MS`, `prompt <target> <text> [--wait --until --timeout]`); `agent.focus` and `agent.send_keys` are not served yet |
| agent start | partial | corpus | `<name> --kind --pane [--timeout] [-- args]` → one `agent.start`; Herdr's CLI also waits for the agent to become ready and retries a busy pane; the kind is checked by the server |
| agent explain | partial | corpus | `<target> [--json|--format text|json] [--verbose]` with Herdr's text summary; `--file` is refused (it needs Herdr's detection manifests) |
| agent attach, terminal attach|session | missing | corpus | refused: attaching a terminal needs Herdr's client |
| terminal title set|clear | implemented | corpus | `client.window_title.set|clear` |
| notification show | implemented | corpus | `<title> [--body --position --sound]` |
| server reload-config | partial | corpus | see `server.reload_config` |
| server stop | partial | corpus | forwarded as `server.stop` (see there), nothing printed on success; a plugin invocation is refused by the shim |
| server (headless), live-handoff, agent-manifests, update-agent-manifests, reload-agent-manifests | missing | corpus | refused: Vibeke runs its own server and agent detection |
| api schema | implemented | corpus | local: the summary, `--json` or `--output PATH` of the captured 0.9.3 schema; it describes Herdr, not what Vibeke serves (this inventory does) |
| api snapshot | partial | corpus | sends `session.snapshot` |
| status [server|client] [--json] | partial | corpus | Herdr's text and JSON; the client is the emulated 0.9.3 (protocol 22); the server is whatever answers `ping` on the shim's route (never spawned); `ping` carries no protocol or capabilities yet, so compatibility shows unknown and `restart_needed` true |
| session list [--json] | implemented | corpus | Vibeke's sessions (state directories, `default` first) as `{sessions: [{name, default, running, socket_path, session_dir}]}` or Herdr's table; `socket_path` is the session's compat listener path |
| session attach|stop|delete | missing | corpus | refused: sessions are Vibeke's (`vibeke --session NAME …`) |
| config check | partial | corpus | local, also inside plugin invocations; reads `HERDR_CONFIG_PATH`, else `$XDG_CONFIG_HOME/herdr/config.toml` or `~/.config/herdr/config.toml`; prints `config: ok` (exit 0) or `config: issues found` and the diagnostics (exit 1); checks TOML syntax, unknown sections and keys, `[keys]` types and keybinding syntax, but not Herdr's keybinding conflicts or the values of other sections |
| config reset-keys | missing | corpus | refused: the shim never rewrites Herdr's config.toml |
| channel show|set, machine … | missing | corpus | refused: update channels and saved SSH machines are Vibeke's (`vibeke update`, `vibeke remote`) |
| integration install|status | partial | corpus | served by the vibeke binary on Vibeke's own harness integrations (`status`, `doctor`, `list`; `install`/`uninstall` only with `--yes` or `--dry-run`); never touches Herdr's hooks; an unconfirmed install is refused |
| completion | partial | corpus | bash, zsh and fish scripts generated from the shim grammar; elvish and powershell refused |
| update | missing | corpus | refused: Vibeke has `vibeke update`; the shim never upgrades Herdr |

## Plugin manifest fields

| Entry | Status | Source | Notes |
|---|---|---|---|
| id | implemented | spec | identifier rule unverified |
| name, version, description | implemented | spec |  |
| min_herdr_version | implemented | spec | checked against 0.9.3, never Vibeke's version |
| platforms (plugin level) | implemented | spec | linux, macos, windows |
| [[build]] command, platforms | implemented | spec | entry platforms override the plugin's |
| [[startup]] command, platforms | implemented | spec |  |
| [[actions]] id, title, description, command, platforms | implemented | spec | per-platform twins with one id |
| [[actions]] contexts | partial | spec | validated; default when omitted (`global`) unverified; the TUI palette shows only actions whose contexts apply (workspace/tab/pane need that focus, selection needs a selection) |
| [[events]] on, command, platforms, id | implemented | spec | unknown event names warn |
| [[panes]] id, title, description, placement, command, width, height, platforms | partial | spec | parsed and validated; opened by `plugin.pane.open` in every placement, `width`/`height` size popups; default placement unverified |
| [[link_handlers]] id, title, pattern, action, platforms | partial | spec | offered by the TUI on hint labels and Ctrl/Alt+click; matching order (registry, then manifest) and modifier unverified |
| [[keys.command]] key, type, command, description | partial | spec | manifest defaults are installed while the plugin is trusted and enabled, never over the user's keys (conflicts reported in `plugin.list`, skipped), and removed on disable/unlink; the user's `[[keys.command]] type = plugin_action` bindings work |
| qualified action resolution (`<plugin>.<action>`) | implemented | spec |  |
| unknown-key warnings | partial | spec | warning text differs from upstream |

## Plugin invocation environment

| Entry | Status | Source | Notes |
|---|---|---|---|
| HERDR_ENV | implemented | spec |  |
| HERDR_SOCKET_PATH | implemented | spec | the invocation's private broker |
| HERDR_BIN_PATH + private PATH launcher dir | implemented | spec | `herdr` → Vibeke shim; never the user's Herdr |
| HERDR_PLUGIN_ID, HERDR_PLUGIN_ROOT | implemented | spec |  |
| HERDR_PLUGIN_CONFIG_DIR, HERDR_PLUGIN_STATE_DIR | implemented | spec | Vibeke-owned, outside the checkout |
| HERDR_PLUGIN_CONTEXT_JSON | partial | spec | Herdr's `PluginInvocationContext` field names and values from the server's model (`workspace_id`, `workspace_label`, `workspace_cwd`, `worktree`, `tab_id`, `tab_label`, `focused_pane_*`, `invocation_source`, `correlation_id`, `clicked_url`, `link_handler_id`) for actions, hooks (Herdr's per-event context), startup and plugin panes; the active workspace and focused pane are kept server-side when no client is attached; `selected_text` only when the caller provides it; Vibeke-only keys (`source`, `plugin_id`, `plugin_version`, `action_id`, `event`, `entrypoint_id`, `placement`) are added |
| HERDR_WORKSPACE_ID, HERDR_TAB_ID, HERDR_PANE_ID | implemented | spec | actions, hooks and startup: the context's `workspace_id`, `tab_id` and `focused_pane_id`, as Herdr; plugin panes: the pane's own ids (popups: no `HERDR_PANE_ID`) |
| HERDR_PLUGIN_ACTION_ID | implemented | spec |  |
| HERDR_PLUGIN_EVENT, HERDR_PLUGIN_EVENT_JSON | implemented | spec | hooks: the dotted event name and Herdr's whole `EventEnvelope` (`{event, data: {type, …}}`, schema `EventData` fields); startup: `HERDR_PLUGIN_EVENT=startup` without JSON |
| HERDR_PLUGIN_ENTRYPOINT_ID | implemented | spec | plugin panes only, as Herdr (never for actions, hooks or startup) |
| HERDR_PLUGIN_CLICKED_URL, HERDR_PLUGIN_LINK_HANDLER_ID | implemented | spec | set for link-handler invocations (`plugin.link.open`), also `clicked_url`/`link_handler_id` in the context JSON |
| stale context variables cleared | implemented | spec |  |
| build steps without socket/context/authority | implemented | spec |  |
| HERDR_* in ordinary panes (`compat.herdr_env`) | partial | spec | with `compat.herdr_env` and the compat listener enabled every pane gets HERDR_ENV, HERDR_SOCKET_PATH (the listener), HERDR_PANE_ID, HERDR_WORKSPACE_ID, HERDR_TAB_ID and HERDR_BIN_PATH (the private launcher); inherited HERDR_* are still stripped first; off while the listener is off |
| HERDR_SESSION | partial | corpus | selects the session in the shim; semantics otherwise unverified |

## Plugin lifecycle, registry and trust

| Entry | Status | Source | Notes |
|---|---|---|---|
| per-user registry shared across sessions (`plugins.json`, atomic) | implemented | spec | works with no server running; every change is a read-modify-write under an exclusive lock, so concurrent revocations are never lost |
| explicit `herdr_legacy` trust grant, shown with entrypoints | implemented | spec | `vibeke plugin trust <id> --legacy` |
| nothing runs before trust | implemented | spec | actions, hooks, startup, build |
| grant bound to the reviewed content and source; change requires re-review | partial | spec | manifest digest, root, source path, whole-tree digest (checked on reinstall: changed content or source drops the grant; a plugin with [[build]] always needs review + build) and a digest of the files the commands reference (checked on every status read and launch); edits to other files of an installed tree are only caught on reinstall |
| disable/unlink/uninstall/revoke stop future execution and brokers | implemented | spec | brokers close within 1 s and drop their connections; every request checks the exact grant; each launch (actions, hooks, startup) re-verifies trust from disk, so the hook cache cannot run changed code |
| no escalation from pane scope | implemented | spec | pane-scoped callers cannot run, trust or install legacy plugins; a process inside a pane of any session (by ancestry, with or without its token) cannot switch sessions |
| build runs for installs only, after trust | implemented | spec | the manifest digest is re-checked before every step and after the last; a change aborts the build and the registration; a build is recorded only for the grant it ran for |
| link never builds | implemented | spec |  |
| async action invocation with log records | implemented | spec | 100 records/session, 64 KiB per stream |
| [[startup]] once per server activation | partial | spec | runs at server start for active plugins, in its own process group (its broker follows the group); takeover semantics unverified |
| [[events]] dispatch with baseline names | partial | spec | projected subset; per-plugin concurrency (default 4, excess hooks queue up to 16, excess action runs get `busy`) and log ring caps with truncation markers are Vibeke's own, upstream limits unverified |
| actions in the command palette | implemented | spec | per machine; untrusted/stale/disabled plugins listed but disabled with the fixing command; actions are filtered by their contexts |
| plugin panes and popups (all placements) | partial | spec | popups: session-modal TUI window, keys to the popup, hidden from compat pane lists/events, no `HERDR_PANE_ID`, callbacks act on the pane underneath; overlays: full-area layer; Herdr's parameter rules and `ui_busy` |
| link handlers | partial | spec | TUI chooser lists matching handlers (untrusted ones disabled) next to the default action; baseline runs the handler without a chooser (unverified) |
| `[[keys.command]] type = plugin_action` bindings | implemented | spec | `command = "<plugin>.<action>"`, `description` shown in the palette |
| migration of Herdr plugin registry/config/state (copy, conflict report, rollback) | partial | spec | `vibeke plugin migrate --from <dir>`: copy only, conflicts left alone, `--rollback`; canonical paths, never writes or deletes through a symlink, source registry dirs confined to `--from`; created paths journaled so a failed migration rolls back and a retry resumes it; registry entries reported, linked with `--link`; Herdr's per-plugin dir layout unverified |
| `herdr` launcher symlink installed only on request into a Vibeke bin dir | implemented | spec | `vibeke compat install-shim` |
| compat CLI never reaches a live Herdr server | implemented | spec | HERDR_SOCKET_PATH is used only when it resolves (plain components, no symlinks) to a socket listed in its session's broker registry; `server.*` lifecycle methods are never forwarded |
| audit events for plugin invocations and mutating callbacks | implemented | spec | metadata only: `plugin.invocation_started/finished`, `plugin.api_call` (method, outcome) and `plugin.pane_opened` with `actor.kind = plugin` |
| credentials redacted in plugin logs | implemented | spec | stdout/stderr tails pass through `vk-redact`; the transient output files are removed once read |
| differential suite against pinned Herdr 0.9.3 | missing | spec | harness gated behind VIBEKE_HERDR_DIFF=1 + VIBEKE_HERDR_BIN; compares normalized values (id bijection, error codes, statuses, array length/order; malformed output fails), comparator self-tested; event streams and unmodified plugins not compared yet; not run |

