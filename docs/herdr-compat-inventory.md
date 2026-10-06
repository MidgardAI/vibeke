# Herdr compatibility inventory (generated)

Baseline: **Herdr v0.9.3** at commit `7b116c05bfda646af39d2524c54e70c751f57ee8` (spec 07 §8.0). Generated from `crates/vk-compat/src/herdr/inventory.rs`; regenerate with `VIBEKE_UPDATE_INVENTORY=1 cargo test -p vk-compat inventory`.

**Status: partial.** This inventory lists every surface the spec's §7.7/§8 text and tables name, plus surfaces the 99 real plugins in `tests/compat/herdr/0.9.3/manifests/` use. It is not yet the exhaustive inventory §8.0 requires, which must be derived from the pinned binary's `herdr api schema --json`, CLI help and manifest schema. No Herdr binary was run to build it. *Implemented* means mapped and tested against Vibeke using the spec's shapes; no entry is certified until the differential suite (07 §8.4) passes. *Source* `corpus` marks entries the plugins use but the spec does not name.

| Area | Implemented | Partial | Missing | Total |
|---|---|---|---|---|
| Socket wire protocol and endpoints | 8 | 6 | 2 | 16 |
| Socket methods | 24 | 23 | 27 | 74 |
| Events (subscriptions and `[[events]]` hooks) | 11 | 7 | 7 | 25 |
| CLI commands | 7 | 13 | 11 | 31 |
| Plugin manifest fields | 9 | 5 | 0 | 14 |
| Plugin invocation environment | 8 | 4 | 3 | 15 |
| Plugin lifecycle, registry and trust | 10 | 3 | 6 | 19 |
| **All** | **77** | **61** | **56** | **194** |

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
| socket path `<herdr_root>/herdr.sock` | partial | spec | `$RUNTIME/<session>/herdr-compat/herdr.sock`; named-session layout `sessions/<name>/` missing; never under ~/.config/herdr |
| socket removed on clean stop | partial | spec | removed on SIGTERM/SIGINT stop path |
| caller identity from peer credentials (pane scope) | implemented | spec | same ancestry rule as the native socket |
| private broker endpoint per plugin invocation | implemented | spec | 0600 socket in a 0700 dir, bound server-side to plugin id + grant digest |
| broker re-checks the grant on every request | implemented | spec | revoked/disabled/stale grants get `permission_denied` |
| broker bindings survive server recovery | missing | spec | brokers die with the server |
| broker authority for long-lived children after the action exits | missing | spec | broker closes when the invocation's process exits |
| Herdr-style ids from a persisted mapping table | partial | spec | Vibeke handles (`w<n>`, `w<n>:t<n>`, `w<n>:p<n>`) are persisted and use the same grammar; baseline allocation/move semantics unverified |

## Socket methods

| Entry | Status | Source | Notes |
|---|---|---|---|
| `session.snapshot` | partial | spec | `layouts` empty; version/protocol report the emulated baseline |
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
| `pane.wait_for_output` | partial | spec | `match`/`regex`, `timeout_ms`; no output_matched event |
| `pane.report_agent` | implemented | spec | Herdr self-report path (selfreport.rs) |
| `pane.report_agent_session` | implemented | spec |  |
| `agent.list` | partial | spec | status projection unverified |
| `agent.get` | partial | spec |  |
| `agent.read` | partial | spec | reads the agent's pane |
| `agent.start` | missing | spec |  |
| `agent.prompt` | missing | spec |  |
| `agent.wait` | missing | spec |  |
| `agent.rename` | missing | spec |  |
| `worktree.list` | partial | spec | native shape under `worktree_list` |
| `worktree.repo_root` | partial | spec |  |
| `worktree.create` | missing | spec |  |
| `worktree.open` | missing | spec |  |
| `events.subscribe` | partial | spec | see events |
| `events.wait` | partial | spec | first projected event matching the subscription |
| `plugin.list` | implemented | spec | registry entries with status |
| `plugin.link` | missing | spec | CLI-local only (registry file); not over the socket |
| `plugin.unlink` | missing | spec | CLI-local only |
| `plugin.enable` | missing | spec | CLI-local only |
| `plugin.disable` | missing | spec | CLI-local only |
| `plugin.action.list` | implemented | spec |  |
| `plugin.action.invoke` | implemented | spec | returns the running log record immediately |
| `plugin.log.list` | implemented | spec | status, timestamps, exit code, separate stdout/stderr |
| `plugin.pane.open` | missing | spec |  |
| `plugin.pane.focus` | missing | spec |  |
| `plugin.pane.close` | missing | spec |  |
| `popup.close` | missing | spec |  |
| `layout.export` | missing | spec | PaneLayoutSnapshot shape unknown |
| `layout.apply` | missing | spec |  |
| `layout.set_split_ratio` | missing | spec |  |
| `pane.process_info` | missing | spec |  |
| `pane.move` | missing | spec |  |
| `pane.swap` | missing | spec |  |
| `pane.resize` | partial | spec | direction + percent |
| `pane.zoom` | partial | spec |  |
| `client.window_title.set` | missing | spec |  |
| `client.window_title.clear` | missing | spec |  |
| `agent.view.set` | missing | spec |  |
| `agent.view.clear` | missing | spec |  |
| `pane.report_metadata` | missing | spec | named as "metadata reporting"; also in corpus |
| `workspace.report_metadata` | missing | spec |  |
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
| `tab.moved` | missing | spec | Vibeke emits no tab move event |
| `pane.created` | implemented | spec |  |
| `pane.closed` | implemented | spec |  |
| `pane.focused` | implemented | spec |  |
| `pane.moved` | missing | spec |  |
| `pane.exited` | implemented | spec |  |
| `pane.agent_detected` | partial | spec | no 250 ms debounce |
| `pane.agent_status_changed` | partial | spec | fires on mapped-status change only; status enumeration unverified |
| `pane.output_matched` | missing | spec |  |
| `pane.scroll_changed` | missing | spec |  |
| `layout.updated` | partial | spec | payload is not a PaneLayoutSnapshot |
| `worktree.created` | missing | spec | used by 9 corpus plugins |
| `worktree.opened` | missing | spec | used by 6 corpus plugins |
| `worktree.removed` | partial | spec |  |
| `workspace.reordered` | missing | corpus | declared by one plugin; not named by the spec (may not exist in the baseline) |

## CLI commands

| Entry | Status | Source | Notes |
|---|---|---|---|
| --version | implemented | spec | reports the emulated baseline |
| global session selection (`--session`, HERDR_SESSION) | missing | spec | shim uses VIBEKE_SESSION or the broker |
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
| plugin pane open|focus|close | missing | corpus |  |
| plugin update | missing | corpus |  |
| pane list|get|current|read | partial | corpus | flag grammar unverified |
| pane send-text|send-keys|run|focus|split|close|rename|wait-output | partial | corpus |  |
| pane report-metadata|process-info|move | missing | corpus |  |
| workspace list|create|rename|focus|move|close | partial | corpus |  |
| workspace report-metadata | missing | corpus |  |
| tab list|create|rename|focus|move|close | partial | corpus |  |
| agent list|get|send|read | partial | corpus |  |
| agent start|prompt|wait|explain | missing | corpus |  |
| worktree list|repo-root | partial | corpus |  |
| worktree create|open | missing | corpus |  |
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
| [[panes]] id, title, description, placement, command, width, height, platforms | partial | spec | parsed and validated; default placement unverified; panes cannot open yet |
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
| HERDR_SESSION | missing | corpus | named in 7 corpus files; semantics unverified |

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
| [[startup]] once per server activation | partial | spec | runs at server start for active plugins; takeover/restart semantics unverified |
| [[events]] dispatch with baseline names | partial | spec | projected subset; concurrency/log limits unverified |
| actions in the command palette | missing | spec | API `plugin.action.list/run` ready for the TUI |
| plugin panes and popups (all placements) | missing | spec |  |
| link handlers | missing | spec |  |
| `[[keys.command]] type = plugin_action` bindings | missing | spec |  |
| migration of Herdr plugin registry/config/state (copy, conflict report, rollback) | missing | spec |  |
| `herdr` launcher symlink installed only on request into a Vibeke bin dir | implemented | spec | `vibeke compat install-shim` |
| compat CLI never reaches a live Herdr server | implemented | spec | HERDR_SOCKET_PATH honored only for Vibeke brokers |
| differential suite against pinned Herdr 0.9.3 | missing | spec | harness gated behind VIBEKE_HERDR_DIFF=1 + VIBEKE_HERDR_BIN; not run |

