# Herdr compatibility inventory (generated)

Baseline: **Herdr v0.9.3** at commit `7b116c05bfda646af39d2524c54e70c751f57ee8` (spec 07 §8.0). Generated from `crates/vk-compat/src/herdr/inventory.rs`; regenerate with `VIBEKE_UPDATE_INVENTORY=1 cargo test -p vk-compat inventory`.

**Status: partial.** This inventory lists every surface the spec's §7.7/§8 text and tables name, plus surfaces the 99 real plugins in `tests/compat/herdr/0.9.3/manifests/` use. It is not yet the exhaustive inventory §8.0 requires, which must be derived from the pinned binary's `herdr api schema --json`, CLI help and manifest schema. No Herdr binary was run to build it. *Implemented* means mapped and tested against Vibeke using the spec's shapes; no entry is certified until the differential suite (07 §8.4) passes. *Source* `corpus` marks entries the plugins use but the spec does not name.

| Area | Implemented | Partial | Missing | Total |
|---|---|---|---|---|
| Socket wire protocol and endpoints | 12 | 4 | 0 | 16 |
| Socket methods | 28 | 42 | 4 | 74 |
| Events (subscriptions and `[[events]]` hooks) | 15 | 8 | 2 | 25 |
| CLI commands | 7 | 19 | 5 | 31 |
| Plugin manifest fields | 9 | 5 | 0 | 14 |
| Plugin invocation environment | 8 | 5 | 2 | 15 |
| Plugin lifecycle, registry and trust | 12 | 5 | 4 | 21 |
| **All** | **91** | **88** | **17** | **196** |

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
| socket path `<herdr_root>/herdr.sock` | implemented | spec | `<herdr_root>` = `$RUNTIME/herdr-compat` (shared by sessions): default session `herdr.sock`, named sessions `sessions/<name>/herdr.sock`; never under ~/.config/herdr; `compat.herdr_socket_path` not built |
| socket removed on clean stop | implemented | spec | on SIGTERM/SIGINT and `server.stop`; a stale socket after a crash is replaced at the next start |
| caller identity from peer credentials (pane scope) | implemented | spec | same ancestry rule as the native socket |
| private broker endpoint per plugin invocation | implemented | spec | 0600 socket in a 0700 dir, bound server-side to plugin id + grant digest |
| broker re-checks the grant on every request | implemented | spec | revoked/disabled/stale grants get `permission_denied` |
| broker bindings survive server recovery | implemented | spec | persisted in `brokers.json`, re-issued at the same path for live invocations whose grant still matches; output is tailed from files so it survives; pid reuse is not detected; exit status after a restart is unknown |
| broker authority for long-lived children after the action exits | implemented | spec | only for long-running entrypoints: `[[startup]]` brokers follow the process group, plugin-pane brokers the pane; action and hook brokers close when their process exits |
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
| `pane.list` | partial | spec | `terminal_id` = pane id, `foreground_cwd` = cwd, `scroll` always 0 |
| `pane.get` | implemented | spec |  |
| `pane.current` | implemented | spec |  |
| `pane.read` | partial | spec | `format: ansi` returns text; baseline wrapping rules unverified |
| `pane.send_text` | implemented | spec | raw bytes, no bracketed paste |
| `pane.send_keys` | partial | spec | Vibeke key grammar, not restricted to Herdr's accepted set |
| `pane.send_input` | partial | spec | text then keys |
| `agent.send` | implemented | spec | literal text, no Enter |
| `pane.focus` | implemented | spec |  |
| `pane.rename` | implemented | spec | null clears |
| `pane.close` | implemented | spec |  |
| `pane.split` | partial | spec | direction/cwd/focus; size params ignored |
| `pane.wait_for_output` | partial | spec | `match`/`regex`, `timeout_ms`; emits `pane.output_matched` |
| `pane.report_agent` | implemented | spec | Herdr self-report path (selfreport.rs) |
| `pane.report_agent_session` | implemented | spec |  |
| `agent.list` | partial | spec | status projection unverified |
| `agent.get` | partial | spec |  |
| `agent.read` | partial | spec | reads the agent's pane |
| `agent.start` | partial | spec | `agent` → harness; `pane_id` starts in that pane, else splits the target/focused pane; result `agent_started` unverified |
| `agent.prompt` | partial | spec | text + Enter (native `agent.prompt`), `wait`, `timeout_ms` |
| `agent.wait` | partial | spec | `status`/`until` in Herdr statuses mapped to Vibeke wait conditions; `timeout` |
| `agent.rename` | partial | spec | result `agent_info` unverified |
| `worktree.list` | partial | spec | native shape under `worktree_list` |
| `worktree.repo_root` | partial | spec |  |
| `worktree.create` | partial | spec | `git worktree add` under `[tasks] root`, opens a workspace (`open` defaults to true); no `path` param; emits `worktree.created` |
| `worktree.open` | partial | spec | reuses the workspace rooted at the worktree or creates one; emits `worktree.opened` |
| `events.subscribe` | partial | spec | see events |
| `events.wait` | partial | spec | first projected event matching the subscription |
| `plugin.list` | implemented | spec | registry entries with status |
| `plugin.link` | implemented | spec | registers in place; never builds or trusts; refused from panes |
| `plugin.unlink` | implemented | spec | files preserved; refused from panes |
| `plugin.enable` | implemented | spec | refused from panes |
| `plugin.disable` | implemented | spec | refused from panes; cuts live brokers |
| `plugin.action.list` | implemented | spec |  |
| `plugin.action.invoke` | implemented | spec | returns the running log record immediately |
| `plugin.log.list` | implemented | spec | status, timestamps, exit code, separate stdout/stderr |
| `plugin.pane.open` | partial | spec | `split`, `tab`, `zoomed`, `overlay` (a zoomed pane that restores focus when it ends, not a real overlay); `popup` needs the TUI; width/height ignored; one broker per pane |
| `plugin.pane.focus` | partial | spec | by `plugin_id` + entrypoint or `pane_id` |
| `plugin.pane.close` | partial | spec | by `plugin_id` + entrypoint or `pane_id` |
| `popup.close` | missing | spec | needs the TUI popup layer |
| `layout.export` | partial | spec | binary split tree (`split_id`, `direction`, `ratio`, `first`, `second` / `pane_id`); PaneLayoutSnapshot shape unverified |
| `layout.apply` | partial | spec | rearranges the tab's own panes from a snapshot; every pane exactly once |
| `layout.set_split_ratio` | partial | spec | `split_id` (from the snapshot) or `pane_id`; ratio of the first side |
| `pane.process_info` | partial | spec | pid, foreground argv/command, cwd; field names unverified |
| `pane.move` | partial | spec | to `tab_id`, or next to `target_pane_id` in `direction`, within a workspace; cross-workspace refused; emits `pane.moved` |
| `pane.swap` | partial | spec | within a workspace; emits `pane.moved` for both panes |
| `pane.resize` | partial | spec | direction + percent |
| `pane.zoom` | partial | spec |  |
| `client.window_title.set` | partial | spec | stored and evented (`client.window_title_changed`); the TUI does not apply it yet |
| `client.window_title.clear` | partial | spec | see `client.window_title.set` |
| `agent.view.set` | missing | spec | semantics unknown until the baseline schema is captured; TUI |
| `agent.view.clear` | missing | spec | see `agent.view.set` |
| `pane.report_metadata` | partial | spec | merged per pane (`metadata`, `key`/`value` or flat params), shown as `metadata` in pane records; in memory |
| `workspace.report_metadata` | partial | spec | as `pane.report_metadata`, per workspace |
| `ping` | partial | spec | reports the emulated baseline |
| `api.schema` | partial | spec | lists inventory methods, not the baseline JSON schema |
| `server.reload_config` | partial | corpus | acknowledged; Vibeke reloads config itself |
| `server.stop` | missing | corpus | refused on the compat endpoint |
| `workspace.close` | partial | corpus | inferred from CLI usage |
| `workspace.get` | partial | corpus | inferred |
| `notification.show` | partial | corpus | `herdr notification show`; maps to a Vibeke notification |
| `pane.run` | partial | corpus | `herdr pane run`; types the command + Enter |

## Events (subscriptions and `[[events]]` hooks)

| Entry | Status | Source | Notes |
|---|---|---|---|
| `workspace.created` | implemented | spec |  |
| `workspace.updated` | partial | spec | emitted on rename only |
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
| `pane.moved` | implemented | spec | from `pane.move`/`pane.swap`; `from_tab_id`/`to_tab_id` payload unverified |
| `pane.exited` | implemented | spec |  |
| `pane.agent_detected` | partial | spec | no 250 ms debounce |
| `pane.agent_status_changed` | partial | spec | fires on mapped-status change only; status enumeration unverified |
| `pane.output_matched` | partial | spec | fires when a compat `pane.wait_for_output` matcher matches |
| `pane.scroll_changed` | missing | spec |  |
| `layout.updated` | partial | spec | payload carries `layout` (snapshot shape unverified) |
| `worktree.created` | implemented | spec | from `worktree.create` and `task.create` worktree checkouts; used by 8 corpus plugins |
| `worktree.opened` | implemented | spec | from `worktree.open`; used by 5 corpus plugins |
| `worktree.removed` | partial | spec | payload `worktree {path}` |
| `workspace.reordered` | missing | corpus | declared by one plugin; not named by the spec (may not exist in the baseline) |

## CLI commands

| Entry | Status | Source | Notes |
|---|---|---|---|
| --version | implemented | spec | reports the emulated baseline |
| global session selection (`--session`, HERDR_SESSION) | partial | spec | `--session NAME` and `HERDR_SESSION`; explicit sessions never spawn; panes cannot switch sessions; plugins keep their identity (grant re-checked on the destination) |
| plugin install <path> | partial | corpus | local directories and manifest paths; build runs after trust |
| plugin install owner/repo[/subdir] [--ref] | missing | spec | git sources need network; not built |
| plugin install --yes | implemented | spec | accepts the displayed legacy trust terms (operator only) |
| plugin link | implemented | corpus | no build |
| plugin unlink | implemented | corpus | files preserved |
| plugin uninstall | implemented | corpus | managed checkout removed, config/state kept |
| plugin enable / disable | implemented | corpus |  |
| plugin list | partial | corpus | JSON output shape unverified |
| plugin config-dir | implemented | corpus |  |
| plugin action list / invoke | partial | corpus | argument grammar unverified |
| plugin log list | partial | corpus |  |
| plugin pane open|focus|close | partial | corpus | `--plugin --entrypoint --placement --direction --cwd --focus`; popup refused |
| plugin update | missing | corpus |  |
| pane list|get|current|read | partial | corpus | flag grammar unverified |
| pane send-text|send-keys|run|focus|split|close|rename|wait-output | partial | corpus |  |
| pane report-metadata|process-info|move | partial | corpus | flag grammar unverified |
| workspace list|create|rename|focus|move|close | partial | corpus |  |
| workspace report-metadata | partial | corpus | flag grammar unverified |
| tab list|create|rename|focus|move|close | partial | corpus |  |
| agent list|get|send|read | partial | corpus |  |
| agent start|prompt|wait|explain | partial | corpus | `explain` missing |
| worktree list|repo-root | partial | corpus |  |
| worktree create|open | partial | corpus | flag grammar unverified |
| notification show | partial | corpus |  |
| server reload-config | partial | corpus |  |
| server stop | missing | corpus | refused |
| api schema | partial | corpus |  |
| integration install|status | missing | corpus | refused by design: never installs into Herdr; use `vibeke integration` |
| config check, completion, update/upgrade, web ui | missing | corpus | refused; Vibeke equivalents exist for some |

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
| [[actions]] contexts | partial | spec | validated; default when omitted (`global`) unverified |
| [[events]] on, command, platforms, id | implemented | spec | unknown event names warn |
| [[panes]] id, title, description, placement, command, width, height, platforms | partial | spec | parsed and validated; opened by `plugin.pane.open` (split, tab, zoomed, overlay); default placement unverified |
| [[link_handlers]] id, title, pattern, action, platforms | partial | spec | regex compiled, action resolved; not wired to clicks |
| [[keys.command]] key, type, command, description | partial | spec | parsed, action resolution warns; bindings not installed |
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
| HERDR_PLUGIN_CONTEXT_JSON | partial | spec | source, correlation id, workspace/tab/pane ids and labels, cwd; not the full PluginInvocationContext |
| HERDR_WORKSPACE_ID, HERDR_TAB_ID, HERDR_PANE_ID | partial | spec | from the invocation context; upstream presence rules unverified |
| HERDR_PLUGIN_ACTION_ID | implemented | spec |  |
| HERDR_PLUGIN_EVENT, HERDR_PLUGIN_EVENT_JSON | partial | spec | payload shape unverified |
| HERDR_PLUGIN_ENTRYPOINT_ID | partial | spec | set for actions/hooks/startup |
| HERDR_PLUGIN_CLICKED_URL, HERDR_PLUGIN_LINK_HANDLER_ID | missing | spec | link handlers not wired |
| stale context variables cleared | implemented | spec |  |
| build steps without socket/context/authority | implemented | spec |  |
| HERDR_* in ordinary panes (`compat.herdr_env`) | missing | spec | still stripped |
| HERDR_SESSION | partial | corpus | selects the session in the shim; semantics otherwise unverified |

## Plugin lifecycle, registry and trust

| Entry | Status | Source | Notes |
|---|---|---|---|
| per-user registry shared across sessions (`plugins.json`, atomic) | implemented | spec | works with no server running |
| explicit `herdr_legacy` trust grant, shown with entrypoints | implemented | spec | `vibeke plugin trust <id> --legacy` |
| nothing runs before trust | implemented | spec | actions, hooks, startup, build |
| grant bound to manifest digest + root; change requires re-review | implemented | spec |  |
| disable/unlink/uninstall/revoke stop future execution and brokers | implemented | spec |  |
| no escalation from pane scope | implemented | spec | pane-scoped callers cannot run, trust or install legacy plugins |
| build runs for installs only, after trust | partial | spec | abort-on-manifest-mutation during build missing |
| link never builds | implemented | spec |  |
| async action invocation with log records | implemented | spec | 100 records/session, 64 KiB per stream |
| [[startup]] once per server activation | partial | spec | runs at server start for active plugins, in its own process group (its broker follows the group); takeover semantics unverified |
| [[events]] dispatch with baseline names | partial | spec | projected subset; concurrency/log limits unverified |
| actions in the command palette | missing | spec | API `plugin.action.list/run` ready for the TUI |
| plugin panes and popups (all placements) | partial | spec | server side: split/tab/zoomed/overlay as Vibeke panes with their own broker and `HERDR_*` env; popups and real overlays need the TUI |
| link handlers | missing | spec |  |
| `[[keys.command]] type = plugin_action` bindings | missing | spec |  |
| migration of Herdr plugin registry/config/state (copy, conflict report, rollback) | partial | spec | `vibeke plugin migrate --from <dir>`: copy only, conflicts left alone, `--rollback`; registry entries reported, linked with `--link`; Herdr's per-plugin dir layout unverified |
| `herdr` launcher symlink installed only on request into a Vibeke bin dir | implemented | spec | `vibeke compat install-shim` |
| compat CLI never reaches a live Herdr server | implemented | spec | HERDR_SOCKET_PATH honored only for Vibeke brokers |
| audit events for plugin invocations and mutating callbacks | implemented | spec | metadata only: `plugin.invocation_started/finished`, `plugin.api_call` (method, outcome) and `plugin.pane_opened` with `actor.kind = plugin` |
| credentials redacted in plugin logs | implemented | spec | stdout/stderr tails pass through `vk-redact`; the transient output files are removed once read |
| differential suite against pinned Herdr 0.9.3 | missing | spec | harness gated behind VIBEKE_HERDR_DIFF=1 + VIBEKE_HERDR_BIN; not run |

