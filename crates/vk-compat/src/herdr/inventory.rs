//! The Herdr baseline inventory (07 §8.0): every public surface the spec and the plugin corpus
//! name, with its implementation status. `docs/herdr-compat-inventory.md` is generated from
//! this table (`VIBEKE_UPDATE_INVENTORY=1 cargo test -p vk-compat inventory`) and a test keeps
//! them in sync.
//!
//! This is **not yet the exhaustive inventory** 07 §8.0 requires: that one is derived from the
//! pinned binary's `herdr api schema --json`, CLI help and manifest schema, which have not been
//! captured (no Herdr binary may be run here). Entries come from the spec's tables and from the
//! 99 real manifests/READMEs in the plugin source record; the schema capture will add rows.
//! "implemented" means mapped and tested against Vibeke with the spec's shapes; no entry is
//! certified until the differential suite (07 §8.4) passes.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Implemented,
    Partial,
    Missing,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Implemented => "implemented",
            Status::Partial => "partial",
            Status::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Wire,
    Method,
    Event,
    Cli,
    Manifest,
    Env,
    Lifecycle,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Wire => "Socket wire protocol and endpoints",
            Kind::Method => "Socket methods",
            Kind::Event => "Events (subscriptions and `[[events]]` hooks)",
            Kind::Cli => "CLI commands",
            Kind::Manifest => "Plugin manifest fields",
            Kind::Env => "Plugin invocation environment",
            Kind::Lifecycle => "Plugin lifecycle, registry and trust",
        }
    }
    pub const ALL: [Kind; 7] = [
        Kind::Wire,
        Kind::Method,
        Kind::Event,
        Kind::Cli,
        Kind::Manifest,
        Kind::Env,
        Kind::Lifecycle,
    ];
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Entry {
    pub kind: Kind,
    pub name: &'static str,
    pub status: Status,
    /// Where the entry comes from: `spec` (07 §7.7/§8 text and tables) or `corpus` (named by
    /// the real plugins' manifests/READMEs but not by the spec).
    pub source: &'static str,
    pub note: &'static str,
}

use Kind::*;
use Status::*;

const fn e(
    kind: Kind,
    name: &'static str,
    status: Status,
    source: &'static str,
    note: &'static str,
) -> Entry {
    Entry {
        kind,
        name,
        status,
        source,
        note,
    }
}

pub const ENTRIES: &[Entry] = &[
    // ---- wire -----------------------------------------------------------------------------
    e(
        Wire,
        "newline-delimited JSON request `{id, method, params}`",
        Implemented,
        "spec",
        "no JSON-RPC envelope, no handshake",
    ),
    e(
        Wire,
        "string `id` required; integer id rejected",
        Implemented,
        "spec",
        "`invalid_request`, empty id echoed",
    ),
    e(
        Wire,
        "success `{id, result: {type, …}}`",
        Implemented,
        "spec",
        "result types from the 07 §8.3 table",
    ),
    e(
        Wire,
        "error `{id, error: {code, message}}`",
        Implemented,
        "spec",
        "string id echoed when recoverable; code set unverified",
    ),
    e(
        Wire,
        "one request per connection, closed after the response",
        Implemented,
        "spec",
        "",
    ),
    e(
        Wire,
        "`events.subscribe` streams `{event, data}` lines",
        Partial,
        "spec",
        "projected subset of events; loss/reconnect behavior not reproduced",
    ),
    e(
        Wire,
        "invalid UTF-8 / invalid JSON handling",
        Partial,
        "spec",
        "`parse_error`; exact baseline codes unverified",
    ),
    e(
        Wire,
        "line limit",
        Partial,
        "spec",
        "16 MiB; baseline limit unverified",
    ),
    e(
        Wire,
        "socket path `<herdr_root>/herdr.sock`",
        Implemented,
        "spec",
        "`<herdr_root>` = `$RUNTIME/herdr-compat` (shared by sessions): default session `herdr.sock`, named sessions `sessions/<name>/herdr.sock`; never under ~/.config/herdr unless `compat.herdr_socket_path` names it (then named sessions sit next to it); a live socket at the path is never replaced",
    ),
    e(
        Wire,
        "socket removed on clean stop",
        Implemented,
        "spec",
        "on SIGTERM/SIGINT and `server.stop`; a stale socket after a crash is replaced at the next start",
    ),
    e(
        Wire,
        "caller identity from peer credentials (pane scope)",
        Implemented,
        "spec",
        "same ancestry rule as the native socket",
    ),
    e(
        Wire,
        "private broker endpoint per plugin invocation",
        Implemented,
        "spec",
        "0600 socket in a 0700 dir, bound server-side to plugin id, grant digest and grant id",
    ),
    e(
        Wire,
        "broker re-checks the grant on every request",
        Implemented,
        "spec",
        "every request re-checks the exact grant (digest + grant id, so revoke then re-grant does not revive it) and that the broker is open and its invocation alive; `events.wait` re-checks after waking; otherwise `permission_denied`",
    ),
    e(
        Wire,
        "broker bindings survive server recovery",
        Implemented,
        "spec",
        "persisted in `brokers.json`, re-issued at the same path for live invocations whose grant still matches; output is written by a capture helper (bounded per invocation) to files that are tailed, so it survives; pid reuse is not detected; exit status after a restart is unknown",
    ),
    e(
        Wire,
        "broker authority for long-lived children after the action exits",
        Partial,
        "spec",
        "deliberate difference: Herdr gives every invocation the real server socket, so a daemon an action starts keeps API access; Vibeke keeps it only for long-running entrypoints (`[[startup]]` brokers follow the process group, plugin-pane brokers the pane). Action and hook brokers close when the action or hook process exits, and closing a broker drops every connection it accepted (requests in flight, subscriptions, waits), so a background process an action starts loses the socket (for example a web server that an action starts); run such a process from `[[startup]]` or a plugin pane",
    ),
    e(
        Wire,
        "plugin identity across sessions (`herdr --session`)",
        Partial,
        "spec",
        "single-use ticket from the invocation's own broker, verified with the issuing session (broker open, invocation alive, same grant), grant re-checked on the destination; no identity from environment values; `events.subscribe`/`events.wait` across sessions are refused for plugins",
    ),
    e(
        Wire,
        "Herdr-style ids from a persisted mapping table",
        Partial,
        "spec",
        "Vibeke handles (`w<n>`, `w<n>:t<n>`, `w<n>:p<n>`) are persisted and use the same grammar; baseline allocation/move semantics unverified",
    ),
    // ---- methods: 07 §8.3 mapping table ---------------------------------------------------
    e(
        Method,
        "session.snapshot",
        Partial,
        "spec",
        "`session_snapshot {snapshot: SessionSnapshot}`: `version`, integer `protocol` (22), focused workspace/tab/pane ids (left out when unknown), workspaces, tabs and live panes, one `PaneLayoutSnapshot` per tab in workspace order, agents; the layouts share `pane.layout`'s rebuilt area",
    ),
    e(Method, "workspace.list", Implemented, "spec", ""),
    e(
        Method,
        "workspace.create",
        Implemented,
        "spec",
        "`workspace_created {workspace, tab, root_pane}`",
    ),
    e(Method, "workspace.rename", Implemented, "spec", ""),
    e(
        Method,
        "workspace.move",
        Implemented,
        "spec",
        "`insert_index`",
    ),
    e(Method, "workspace.focus", Implemented, "spec", ""),
    e(Method, "tab.list", Implemented, "spec", ""),
    e(Method, "tab.create", Implemented, "spec", ""),
    e(Method, "tab.rename", Implemented, "spec", ""),
    e(Method, "tab.move", Implemented, "spec", "`insert_index`"),
    e(Method, "tab.focus", Implemented, "spec", ""),
    e(Method, "tab.close", Implemented, "spec", "closes all panes"),
    e(
        Method,
        "pane.list",
        Partial,
        "spec",
        "Herdr `PaneInfo` (absent optional fields left out, `agent_status` `unknown` without an agent, `scroll` as `{offset_from_bottom, max_offset_from_bottom, viewport_rows}`, `title`/`display_agent`/`state_labels`/`tokens` from `pane.report_metadata`, `terminal_title` from the terminal); `terminal_id` = pane id, `foreground_cwd` = cwd; no `restore_error` or `terminal_title_stripped`; focus is the most recent client's, else the last focus any client had",
    ),
    e(
        Method,
        "pane.get",
        Partial,
        "spec",
        "Herdr `PaneInfo`, see `pane.list`",
    ),
    e(Method, "pane.current", Implemented, "spec", ""),
    e(
        Method,
        "pane.read",
        Partial,
        "spec",
        "`format: ansi` returns text; baseline wrapping rules unverified",
    ),
    e(
        Method,
        "pane.send_text",
        Implemented,
        "spec",
        "raw bytes, no bracketed paste",
    ),
    e(
        Method,
        "pane.send_keys",
        Partial,
        "spec",
        "Vibeke key grammar, not restricted to Herdr's accepted set",
    ),
    e(Method, "pane.send_input", Partial, "spec", "text then keys"),
    e(
        Method,
        "agent.send",
        Implemented,
        "spec",
        "literal text, no Enter",
    ),
    e(Method, "pane.focus", Implemented, "spec", ""),
    e(Method, "pane.rename", Implemented, "spec", "null clears"),
    e(Method, "pane.close", Implemented, "spec", ""),
    e(
        Method,
        "pane.split",
        Partial,
        "spec",
        "splits `target_pane_id` (baseline), else `pane_id`, the invocation's pane or the focused pane; direction/cwd/focus; size params ignored",
    ),
    e(
        Method,
        "pane.wait_for_output",
        Partial,
        "spec",
        "`match`/`regex`, `timeout_ms`; emits `pane.output_matched`",
    ),
    e(
        Method,
        "pane.report_agent",
        Implemented,
        "spec",
        "Herdr self-report path (selfreport.rs)",
    ),
    e(Method, "pane.report_agent_session", Implemented, "spec", ""),
    e(
        Method,
        "agent.list",
        Partial,
        "spec",
        "status projection unverified",
    ),
    e(Method, "agent.get", Partial, "spec", ""),
    e(
        Method,
        "agent.read",
        Partial,
        "spec",
        "reads the agent's pane",
    ),
    e(
        Method,
        "agent.start",
        Partial,
        "spec",
        "Herdr's `kind` (also `agent`/`harness`) → harness; `pane_id` starts in that pane, else splits the target/focused pane; `agent_started {agent, argv}`, where `argv` is the harness and `args` (not the resolved command line)",
    ),
    e(
        Method,
        "agent.prompt",
        Partial,
        "spec",
        "text + Enter (native `agent.prompt`), `wait`, `timeout_ms`; `agent_prompted {agent}`",
    ),
    e(
        Method,
        "agent.wait",
        Partial,
        "spec",
        "`status`/`until` in Herdr statuses mapped to Vibeke wait conditions; `timeout`",
    ),
    e(
        Method,
        "agent.rename",
        Partial,
        "spec",
        "`agent_info {agent}`",
    ),
    e(
        Method,
        "agent.explain",
        Partial,
        "corpus",
        "`agent explain`: result `agent_explain` {agent, agent_status, reason, execution, blocked, unseen_turn, turns_completed, last_tool, last_message, source}; Vibeke's own shape, baseline unverified",
    ),
    e(
        Method,
        "worktree.list",
        Partial,
        "spec",
        "native shape under `worktree_list`",
    ),
    e(Method, "worktree.repo_root", Partial, "spec", ""),
    e(
        Method,
        "worktree.create",
        Partial,
        "spec",
        "`git worktree add` under `[tasks] root`, opens a workspace (`open` defaults to true); no `path` param; emits `worktree.created`",
    ),
    e(
        Method,
        "worktree.open",
        Partial,
        "spec",
        "reuses the workspace rooted at the worktree or creates one; `worktree_opened {workspace, tab, root_pane, worktree, already_open}` (Vibeke extra `created`); only `path` is accepted; emits `worktree.opened`",
    ),
    e(Method, "events.subscribe", Partial, "spec", "see events"),
    e(
        Method,
        "events.wait",
        Partial,
        "spec",
        "Herdr's `{match_event, timeout_ms?}`: only `pane_agent_status_changed {pane_id, agent_status}` (every other `EventMatch` is `unsupported_event_wait_match`, as in Herdr); `wait_matched {event: EventEnvelope}` at once when the pane already has the status, else on the first projected change; `pane_not_found`; no deadline without `timeout_ms`, else `timeout`. Vibeke's older `{subscriptions}` form still answers the first matching projected event as `event`",
    ),
    // ---- methods: 07 §8.3 required-coverage list -------------------------------------------
    e(
        Method,
        "plugin.list",
        Implemented,
        "spec",
        "Herdr's `InstalledPluginInfo` (`name`, `version`, `min_herdr_version`, `manifest_path`, `plugin_root`, `enabled`, manifest lists in Herdr's order, `source` as `PluginSourceInfo` with `local`/`github`, `warnings` incl. `manifest unavailable: …`); `plugin_id` filter; Vibeke extras `status`, `trust`, `managed`, `origin`, `config_dir`, `state_dir`, sandbox settings",
    ),
    e(
        Method,
        "plugin.link",
        Implemented,
        "spec",
        "registers in place; never builds or trusts; `enabled: false` links it disabled; refused from panes; `plugin_linked {plugin: InstalledPluginInfo}`",
    ),
    e(
        Method,
        "plugin.unlink",
        Implemented,
        "spec",
        "files preserved; refused from panes; `plugin_unlinked {plugin_id, removed}` (`removed: false` for an unknown plugin)",
    ),
    e(
        Method,
        "plugin.enable",
        Implemented,
        "spec",
        "refused from panes; `plugin_enabled {plugin}`",
    ),
    e(
        Method,
        "plugin.disable",
        Implemented,
        "spec",
        "refused from panes; cuts live brokers; `plugin_disabled {plugin}`",
    ),
    e(
        Method,
        "plugin.action.list",
        Implemented,
        "spec",
        "Herdr's `PluginActionInfo` (`plugin_id`, `action_id`, `title`, `description?`, declared `contexts`, manifest `command`, the action's or else the plugin's `platforms`) for every action of every plugin whose manifest loads, sorted by qualified id; an unknown `plugin_id` gives an empty list; Vibeke extras `qualified_id`, `available`, `status`",
    ),
    e(
        Method,
        "plugin.action.invoke",
        Partial,
        "spec",
        "qualified `action_id` (`<plugin>.<action>`) with optional `plugin_id`; returns Herdr's `plugin_action_invoked {action, context, log}` at once (the log is still running); the context is Herdr's `merge_plugin_context` (active workspace and focused pane, then the request's `context` field by field); `correlation_id` defaults to the log id (Herdr: the request id)",
    ),
    e(
        Method,
        "plugin.log.list",
        Partial,
        "spec",
        "records carry `command` (argv), `started_unix_ms`/`finished_unix_ms`, status `running`/`succeeded`/`failed`, exit code and separate stdout/stderr; remaining baseline fields unverified",
    ),
    e(
        Method,
        "plugin.pane.open",
        Partial,
        "spec",
        "all placements: `split`, `tab`, `zoomed`; `overlay` = a full-area layer over the tab, `popup` = a session-modal floating window sized by `width`/`height` (cells or `%`, default 80%) with no pane id; both close when the command exits and restore the prior focus; one popup at a time (`ui_busy`); Herdr's `invalid_params` rules and messages for `target_pane_id`/`workspace_id`/`direction`/`width`/`height` per placement; `env` applied after Herdr's checks (`invalid_env`), protected keys dropped; `target_pane_id` (Vibeke also accepts `pane_id`); context and env as Herdr's `panes.rs`; one broker per pane; result `plugin_pane_opened {plugin_pane: {plugin_id, entrypoint, pane}}` (Vibeke extra `placement`), a popup answers `ok` as in Herdr",
    ),
    e(
        Method,
        "plugin.pane.focus",
        Partial,
        "spec",
        "`pane_id` (Vibeke also: `plugin_id` + entrypoint); `plugin_pane_focused {plugin_pane}`; `plugin_pane_not_found` for a pane that is not a plugin pane",
    ),
    e(
        Method,
        "plugin.pane.close",
        Partial,
        "spec",
        "`pane_id` (Vibeke also: `plugin_id` + entrypoint); `plugin_pane_closed {pane_id}`; `plugin_pane_not_found` for a pane that is not a plugin pane",
    ),
    e(
        Method,
        "popup.close",
        Partial,
        "spec",
        "closes the open popup (a plugin only its own) and restores focus; `ok`, or `popup_not_open` when none",
    ),
    e(
        Method,
        "layout.export",
        Partial,
        "spec",
        "`layout_export {layout: LayoutDescription}`: binary split tree (`direction` `right`/`down`, `ratio`, `first`, `second`; panes with `pane_id`, `label`, `cwd`, never `command`); `tab_id` or `pane_id`, else the focused tab; Vibeke extension: `workspace_id` alone lists every tab under `layouts`",
    ),
    e(
        Method,
        "layout.apply",
        Partial,
        "spec",
        "`root` (with `tab_id`), or a whole description under `layout`; rearranges the tab's own panes, every pane exactly once (Herdr can also create panes with `command`/`env` and a new tab; Vibeke cannot yet); `layout_apply {layout}`",
    ),
    e(
        Method,
        "layout.set_split_ratio",
        Partial,
        "spec",
        "`path` (booleans from the root, `true` = second side), a split `id` from `pane.layout` as `split_id`, or `pane_id`; ratio of the first side; `layout_split_ratio_set {layout}`; `split_not_found`",
    ),
    e(
        Method,
        "pane.process_info",
        Partial,
        "spec",
        "`pane_process_info {process_info: {pane_id, shell_pid, foreground_process_group_id, foreground_processes: [{pid, name, argv0?, argv, cmdline, cwd}]}}` read live like Herdr (Linux `/proc`: the group's members below the leader and the shell, sorted by pid, `comm` as name; macOS libproc: the group's members with `argv0`); `tty` is never set (as in Herdr)",
    ),
    e(
        Method,
        "pane.layout",
        Partial,
        "spec",
        "`pane_layout {layout: PaneLayoutSnapshot}` for `pane_id` or the focused pane: pane and split rects, split ids `split_<n>_<path>`, `right`/`down`, Herdr's rounding (pane rects include borders); the area is rebuilt from the panes' current sizes at origin 0,0 (Herdr reports its terminal area, offset by its sidebar), 80x24 per pane never sized by a client",
    ),
    e(
        Method,
        "pane.move",
        Partial,
        "spec",
        "Herdr's `destination {type: tab, tab_id, split, target_pane_id?, ratio?}` within a workspace, or Vibeke's flat `tab_id`/`target_pane_id`/`direction`; `pane_move {move_result: PaneMoveResult}` with `changed: false` + `same_tab`/`zoomed_tab`; `new_tab`/`new_workspace` destinations and cross-workspace moves unsupported; emits `pane.moved`",
    ),
    e(
        Method,
        "pane.swap",
        Partial,
        "spec",
        "Herdr's `source_pane_id` + `target_pane_id` or `pane_id` + `direction`; `pane_swap {swap: PaneSwapResult}` with `changed: false` + `not_found`/`same_pane`/`cross_tab`/`no_neighbor`; Vibeke's older `pane_id` + `other_pane_id` also swaps across tabs of a workspace; emits `pane.moved` for both panes",
    ),
    e(
        Method,
        "pane.resize",
        Partial,
        "spec",
        "direction + percent",
    ),
    e(Method, "pane.zoom", Partial, "spec", ""),
    e(
        Method,
        "client.window_title.set",
        Implemented,
        "spec",
        "evented (`client.window_title_changed`); the TUI shows it as the outer terminal title (OSC 2) with `ui.title_sync`, else in the tab bar; sanitized; `client_window_title {changed, reason}` (`no_foreground_client` without an attached client; the title is kept for the next client)",
    ),
    e(
        Method,
        "client.window_title.clear",
        Implemented,
        "spec",
        "back to `ui.title_format`; `client_window_title {changed, reason}`; see `client.window_title.set`",
    ),
    e(
        Method,
        "agent.view.set",
        Partial,
        "spec",
        "Herdr's `{source, label?, filter?, sort[]}`: validated like Herdr (`invalid_agent_view`), one active view per session, `agent_view {active, source, label}`; stored and announced, but Vibeke's sidebar does not apply the filter and sort yet. Vibeke extension: a plugin invocation's run status line (`target`, `text` <= 80 chars, `detail`, `tone`), <= 16 per plugin, shown in the sidebar and peek, gone when the plugin is disabled or untrusted",
    ),
    e(
        Method,
        "agent.view.clear",
        Partial,
        "spec",
        "`{source?}` clears the active agent-list view when the source matches (`agent_view`); without `source` a plugin's run status lines go too; Vibeke's `target` clears one run's status line",
    ),
    e(
        Method,
        "pane.report_metadata",
        Partial,
        "spec",
        "merged per pane (`metadata`, `key`/`value` or flat params), shown as `metadata` in pane records and as Herdr's `title`, `display_agent`, `state_labels` and `tokens` (merged per key; `clear_*` flags honoured); emits `pane.updated`; `seq`, `ttl_ms`, `source` precedence not applied; in memory",
    ),
    e(
        Method,
        "workspace.report_metadata",
        Partial,
        "spec",
        "as `pane.report_metadata`, per workspace; `tokens` shown in `WorkspaceInfo`; emits `workspace.metadata_updated`",
    ),
    e(
        Method,
        "ping",
        Partial,
        "spec",
        "`pong {version, protocol, capabilities}` with the emulated baseline (protocol 22; every capability false: no live handoff, daemon, SSH agent, surface interest or health check); Vibeke adds `emulator`, `vibeke_version`, `compatibility`",
    ),
    e(
        Method,
        "api.schema",
        Partial,
        "spec",
        "lists inventory methods, not the baseline JSON schema",
    ),
    e(
        Method,
        "server.reload_config",
        Partial,
        "corpus",
        "acknowledged; Vibeke reloads config itself",
    ),
    e(
        Method,
        "server.stop",
        Partial,
        "corpus",
        "stops this Vibeke session when `[compat.herdr] allow_server_stop = true` (default off: `unsupported`); never from a plugin, a sandboxed invocation or a pane; never reaches a Herdr server",
    ),
    e(
        Method,
        "workspace.close",
        Partial,
        "corpus",
        "inferred from CLI usage",
    ),
    e(Method, "workspace.get", Partial, "corpus", "inferred"),
    e(
        Method,
        "notification.show",
        Partial,
        "corpus",
        "`herdr notification show`; maps to a Vibeke notification",
    ),
    e(
        Method,
        "pane.run",
        Partial,
        "corpus",
        "types the command + Enter; Herdr's `herdr pane run` (and the shim) send `pane.send_input` instead",
    ),
    e(
        Method,
        "pane.neighbor",
        Partial,
        "corpus",
        "`pane_neighbor {neighbor: {pane_id, direction, neighbor_pane_id?, layout}}` with Herdr's `find_in_direction` (edge distance, overlap, centre, layout order) over the tab's pane rects; `pane_id`, else the caller's or focused pane; the rects come from `pane.layout`'s rebuilt area",
    ),
    e(
        Method,
        "pane.edges",
        Partial,
        "corpus",
        "`pane_edges {edges: {pane_id, left, right, up, down, layout}}` against the tab's area; see `pane.neighbor` for the area",
    ),
    e(
        Method,
        "pane.focus_direction",
        Partial,
        "corpus",
        "`pane_focus_direction {focus: {changed, reason?: no_neighbor, source_pane_id, focused_pane_id?, layout}}`; the move goes through Vibeke's `pane.focus` (most recent client); see `pane.neighbor`",
    ),
    e(
        Method,
        "pane.input.set",
        Partial,
        "corpus",
        "`{pane_id, right_click: herdr|pane}` validated (`pane_not_found`, `invalid_params`) and answered `ok`, but it has no effect: Vibeke's clients have no per-pane right-click routing",
    ),
    e(
        Method,
        "pane.release_agent",
        Partial,
        "corpus",
        "`{pane_id, source, agent, seq?}`: `invalid_agent` for an empty label; a run that only self-reports ends (`released`) so detection takes over; `source`/`agent`/`seq` are not compared with the reporting hook (Herdr keeps per-source authority); a pane may release only itself",
    ),
    e(
        Method,
        "tab.get",
        Implemented,
        "corpus",
        "`tab_info {tab}`; `tab_not_found`",
    ),
    e(
        Method,
        "agent.focus",
        Implemented,
        "corpus",
        "focuses the agent's pane (its tab and workspace) on the most recent client; `agent_info {agent}`; `agent_not_found`",
    ),
    e(
        Method,
        "agent.send_keys",
        Partial,
        "corpus",
        "`{target, keys[]}` to the agent's pane; `ok`; `agent_not_ready` for an ended run; Vibeke's key grammar, not restricted to Herdr's accepted set",
    ),
    e(
        Method,
        "worktree.remove",
        Partial,
        "corpus",
        "`{workspace_id, force?}`: `not_linked_worktree` unless the workspace is a linked worktree checkout; `git worktree remove [--force]` in the main repository (`dirty_worktree_requires_force`, `worktree_remove_failed` with git's message); then the workspace closes and `worktree.removed` follows with `forced` and the workspace's last snapshot; `worktree_removed {workspace_id, path, forced}`; `trust_repository` ignored (git's own safe.directory rules apply); runs synchronously",
    ),
    e(
        Method,
        "workspace.move_block",
        Partial,
        "spec",
        "`{workspace_ids, before_workspace_id?}` with Herdr's `workspace_move_block_failed` checks; `workspace_list`; Vibeke moves the workspaces one at a time, so subscribers see `workspace.moved` per moved workspace instead of one `workspace.reordered`",
    ),
    e(
        Method,
        "pane.scroll",
        Partial,
        "spec",
        "`{pane_id, offset_from_bottom}` through Vibeke's `pane.scroll` (clamped to the scrollback); clients follow the request; `pane_info`; emits `pane.scroll_changed`",
    ),
    e(
        Method,
        "pane.clear_agent_authority",
        Partial,
        "spec",
        "`{pane_id, source?, seq?}`: `ok`; a run that only self-reports ends so detection takes over (Herdr drops the hook's authority and keeps the run)",
    ),
    e(
        Method,
        "server.ssh_agent.register",
        Partial,
        "spec",
        "answered `connection_local_only` as Herdr answers a one-request connection; Vibeke has no persistent JSON API connections on this socket",
    ),
    e(
        Method,
        "release_notes.dismiss",
        Partial,
        "spec",
        "Vibeke never shows Herdr's release notes: always `stale_release_notes`, as Herdr answers when none are current",
    ),
    e(
        Method,
        "product_announcement.dismiss",
        Partial,
        "spec",
        "Vibeke never shows Herdr's announcements: always `stale_announcement`, as Herdr answers when none is current",
    ),
    e(
        Method,
        "server.live_handoff",
        Missing,
        "spec",
        "`unsupported`: hands a running Herdr server to a new binary; Vibeke upgrades with `vibeke update` and its holders keep panes alive",
    ),
    e(
        Method,
        "server.agent_manifests",
        Missing,
        "spec",
        "`unsupported`: Herdr's agent-detection manifests; Vibeke's are listed with `vibeke agent manifests`",
    ),
    e(
        Method,
        "server.reload_agent_manifests",
        Missing,
        "spec",
        "`unsupported`: see `server.agent_manifests`; Vibeke reloads with `agent.manifests_reload`",
    ),
    e(
        Method,
        "command.invoke",
        Missing,
        "spec",
        "`unsupported`: runs a command id from Herdr's client-shell projection, which Vibeke does not publish",
    ),
    e(
        Method,
        "client_shell.surface.set",
        Missing,
        "spec",
        "`unsupported`: Herdr's client-shell presentation protocol; Vibeke clients use the render stream",
    ),
    e(
        Method,
        "pane.clear",
        Missing,
        "spec",
        "`unsupported`: Herdr clears the emulator's screen; Vibeke's terminal engine has no server-side clear that keeps the program's output consistent",
    ),
    e(
        Method,
        "pane.edit_scrollback",
        Missing,
        "spec",
        "`unsupported`: opens the focused pane's scrollback in Herdr's client editor",
    ),
    e(
        Method,
        "pane.selection.read",
        Missing,
        "spec",
        "`unsupported`: selections are client-side in Vibeke and not reported to the server",
    ),
    e(
        Method,
        "pane.copy_motion",
        Missing,
        "spec",
        "`unsupported`: Herdr's copy-mode cursor; Vibeke's copy mode runs in the client",
    ),
    e(
        Method,
        "pane.copy_search",
        Missing,
        "spec",
        "`unsupported`: see `pane.copy_motion`",
    ),
    e(
        Method,
        "pane.link.resolve",
        Missing,
        "spec",
        "`unsupported`: Herdr resolves link regions at a viewport cell of its own renderer; Vibeke's clients detect links themselves",
    ),
    e(
        Method,
        "pane.link.activate",
        Missing,
        "spec",
        "`unsupported`: see `pane.link.resolve`; plugin link handlers run through `plugin.link.open` on the native socket",
    ),
    e(
        Method,
        "integration.list",
        Missing,
        "spec",
        "`unsupported`: Herdr's agent integrations; use `vibeke integration list`",
    ),
    e(
        Method,
        "integration.install",
        Missing,
        "spec",
        "`unsupported`: installing hooks needs `vibeke integration install <harness>`, which writes Vibeke's own hooks",
    ),
    e(
        Method,
        "integration.uninstall",
        Missing,
        "spec",
        "`unsupported`: see `integration.install`",
    ),
    // ---- events ---------------------------------------------------------------------------
    e(Event, "workspace.created", Implemented, "spec", ""),
    e(
        Event,
        "workspace.updated",
        Partial,
        "spec",
        "emitted on rename only",
    ),
    e(
        Event,
        "workspace.metadata_updated",
        Partial,
        "spec",
        "from `workspace.report_metadata`; subscriptions only (Herdr runs no hooks for it)",
    ),
    e(Event, "workspace.renamed", Implemented, "spec", ""),
    e(Event, "workspace.closed", Implemented, "spec", ""),
    e(
        Event,
        "workspace.focused",
        Partial,
        "spec",
        "derived from pane focus of any client",
    ),
    e(Event, "workspace.moved", Implemented, "spec", ""),
    e(Event, "tab.created", Implemented, "spec", ""),
    e(Event, "tab.closed", Implemented, "spec", ""),
    e(
        Event,
        "tab.focused",
        Partial,
        "spec",
        "derived from pane focus",
    ),
    e(Event, "tab.renamed", Implemented, "spec", ""),
    e(
        Event,
        "tab.moved",
        Implemented,
        "spec",
        "native `tab.moved` from `tab.move`",
    ),
    e(Event, "pane.created", Implemented, "spec", ""),
    e(Event, "pane.closed", Implemented, "spec", ""),
    e(Event, "pane.focused", Implemented, "spec", ""),
    e(
        Event,
        "pane.updated",
        Partial,
        "spec",
        "on a pane rename, cwd change or `pane.report_metadata`; not on terminal title changes; subscriptions only (Herdr runs no hooks for it)",
    ),
    e(
        Event,
        "pane.output_changed",
        Missing,
        "spec",
        "a Herdr `EventKind` without a subscription type; Vibeke accepts it in `events.subscribe` but never emits it",
    ),
    e(
        Event,
        "pane.moved",
        Implemented,
        "spec",
        "from `pane.move`/`pane.swap`; `previous_pane_id` equals the pane id (Vibeke ids are stable across moves)",
    ),
    e(Event, "pane.exited", Implemented, "spec", ""),
    e(
        Event,
        "pane.agent_detected",
        Partial,
        "spec",
        "250 ms debounce per pane; `released`/`final_status` never set",
    ),
    e(
        Event,
        "pane.agent_status_changed",
        Partial,
        "spec",
        "fires on mapped-status change only (Herdr also fires on title/label changes); hooks get the `EventEnvelope`, pane-scoped subscriptions Herdr's dotted `SubscriptionEventEnvelope`",
    ),
    e(
        Event,
        "pane.output_matched",
        Partial,
        "spec",
        "fires when a compat `pane.wait_for_output` matcher matches (Herdr matches in the subscription itself); `{pane_id, matched_line, read}`",
    ),
    e(
        Event,
        "pane.scroll_changed",
        Partial,
        "spec",
        "from the TUI's copy-mode viewport (`ClientFrame::ScrollView`, coalesced to 150 ms); `{pane_id, workspace_id, scroll: PaneScrollInfo}`, also in `pane.list`",
    ),
    e(
        Event,
        "layout.updated",
        Partial,
        "spec",
        "payload carries `layout` as Herdr's `PaneLayoutSnapshot` (see `pane.layout`; also the `layouts` of `session.snapshot`)",
    ),
    e(
        Event,
        "worktree.created",
        Implemented,
        "spec",
        "from `worktree.create` and `task.create` worktree checkouts; `{workspace: WorkspaceInfo with worktree, worktree: WorktreeInfo}`; only when a workspace is open at the worktree; used by 8 corpus plugins",
    ),
    e(
        Event,
        "worktree.opened",
        Implemented,
        "spec",
        "from `worktree.open`; `{workspace, worktree, already_open}`; used by 5 corpus plugins",
    ),
    e(
        Event,
        "worktree.removed",
        Partial,
        "spec",
        "`{workspace_id, workspace?, worktree, forced}`; `forced` is always false",
    ),
    e(
        Event,
        "workspace.reordered",
        Partial,
        "corpus",
        "declared by one plugin, not named by the spec; emitted after every `workspace.move` (the only reordering Vibeke has); `{workspace_ids, workspaces}` without `before_workspace_id`",
    ),
    // ---- CLI --------------------------------------------------------------------------------
    // Each row follows the captured 0.9.3 help (`tests/compat/herdr/0.9.3/cli-help/`) and Herdr's
    // parsers (`src/cli/*.rs` at the baseline commit).
    e(
        Cli,
        "output of socket-backed commands",
        Implemented,
        "spec",
        "the whole response line `{\"id\":\"cli:…\",\"result\":…}` on stdout (exit 0), error lines on stderr (exit 1, `permission_denied` too); keys sorted as Herdr prints them; request ids as Herdr's CLI sends them (`cli:pane:get`, `cli:plugin`, `cli:request`, …); `pane send-text|send-keys|run|report-*` and `workspace report-metadata` print nothing on success; `pane read` and `agent read` print the text; the shim's own errors use Herdr's `{id, error}` shape (`server_not_running` when nothing answers)",
    ),
    e(
        Cli,
        "argument errors",
        Implemented,
        "spec",
        "Herdr's per-command parsers: `unknown option: …`, `missing value for …`, usage lines and command-group help on stderr with exit 2 (`<group> help`: exit 0); `unknown command: …` for commands Herdr does not have (`ping`, `events`, `layout`, `popup`, `session snapshot`, …: use the socket methods); values Herdr rejects in `main` print `Error: Custom { … }`, exit 1",
    ),
    e(
        Cli,
        "--version",
        Implemented,
        "spec",
        "prints `herdr 0.9.3`, as Herdr does; the socket's `ping` names Vibeke (`emulator`)",
    ),
    e(
        Cli,
        "--help, <command> --help",
        Implemented,
        "corpus",
        "`<command> --help` prints the captured 0.9.3 help page; `herdr --help` prints the shim's own summary",
    ),
    e(
        Cli,
        "--default-config",
        Implemented,
        "corpus",
        "prints the captured 0.9.3 default configuration",
    ),
    e(
        Cli,
        "launch and attach (`herdr`, `client`, `--remote`, `--machine`, `--skill`)",
        Missing,
        "corpus",
        "refused: the shim never launches or attaches a Herdr client; run `vibeke`",
    ),
    e(
        Cli,
        "global session selection (`--session`, HERDR_SESSION)",
        Partial,
        "spec",
        "`--session NAME` and `HERDR_SESSION`; explicit sessions never spawn (`server_not_running`, exit 1); panes cannot switch sessions; plugins keep their identity (grant re-checked on the destination)",
    ),
    e(
        Cli,
        "plugin install <path>",
        Partial,
        "corpus",
        "a Vibeke addition (Herdr installs GitHub sources only): local directories and manifest paths; build runs after trust",
    ),
    e(
        Cli,
        "plugin install owner/repo[/subdir] [--ref]",
        Partial,
        "spec",
        "Herdr's grammar `[--ref REF] [--yes|-y] <source>`; without `--yes` and without a terminal: Herdr's `remote plugin install requires --yes when stdin is not interactive` (exit 2); shallow, hardened clone (no hooks/fsmonitor/filters, https+file only) of `https://github.com/owner/repo` at a tag, branch or sha (`@ref` or `--ref`); the resolved commit is recorded and pinned by the trust grant; prints Herdr's `Installed <id> from <source>.` and `Config: <dir>`; no install preview or confirmation prompt; errors in Vibeke's JSON",
    ),
    e(
        Cli,
        "plugin install --yes",
        Implemented,
        "spec",
        "accepts the displayed legacy trust terms (operator only)",
    ),
    e(
        Cli,
        "plugin link",
        Implemented,
        "corpus",
        "`<path> [--disabled|--enabled]`, path made absolute as Herdr does; no build; prints Herdr's `{\"id\":\"cli:plugin\",\"result\":{\"type\":\"plugin_linked\",…}}` line (Herdr error lines on stderr, exit 1); changes the registry without a server, as Herdr's offline fallback does",
    ),
    e(
        Cli,
        "plugin unlink",
        Implemented,
        "corpus",
        "files preserved; prints Herdr's `plugin_unlinked {plugin_id, removed}` response line",
    ),
    e(
        Cli,
        "plugin uninstall",
        Implemented,
        "corpus",
        "plugin id or the `owner/repo[/subdir]` it was installed from; managed checkout removed (a linked plugin is unlinked), config/state kept; prints `Uninstalled <id>.`, or `plugin not installed: <target>` (exit 1)",
    ),
    e(
        Cli,
        "plugin enable / disable",
        Implemented,
        "corpus",
        "prints Herdr's `plugin_enabled`/`plugin_disabled` response line",
    ),
    e(
        Cli,
        "plugin list",
        Implemented,
        "corpus",
        "`[--plugin ID] [--json]`: Herdr's `N plugins installed:` summary (`- id (name) enabled [local:<root>|github:owner/repo@ref; n warning(s)]`, `config:` and `warning:` lines, or `No plugins installed.`); `--json` prints the `plugin_list` response line; `config:` is Vibeke's per-plugin config directory",
    ),
    e(
        Cli,
        "plugin config-dir",
        Partial,
        "corpus",
        "prints the bare path and creates the config and state directories, as Herdr does; the path is Vibeke's per-plugin config directory (Herdr's lies under its own config root); an id that is not a safe path component is refused (`invalid_plugin_id`), where Herdr encodes it",
    ),
    e(
        Cli,
        "plugin action list / invoke",
        Implemented,
        "corpus",
        "`list [--plugin ID]`; `invoke <action_id> [--plugin ID]` with `context.invocation_source = cli`; results: see the methods",
    ),
    e(
        Cli,
        "plugin log list",
        Implemented,
        "corpus",
        "`[list] [--plugin ID] [--limit N]` (`logs` alias)",
    ),
    e(
        Cli,
        "plugin pane open",
        Implemented,
        "corpus",
        "`--plugin --entrypoint --placement --width --height --workspace --target-pane --direction --cwd`, repeated `--env K=V` into `env`, `--focus/--no-focus` (focus defaults to true), `fullscreen` = `zoomed`; placement behaviour: see `plugin.pane.open`",
    ),
    e(
        Cli,
        "plugin pane focus|close <PANE_ID>",
        Implemented,
        "corpus",
        "the positional is the pane id (`pane_id`)",
    ),
    e(
        Cli,
        "plugin update",
        Partial,
        "corpus",
        "a Vibeke addition: re-fetches the recorded repository and ref; a new commit or manifest leaves the plugin inactive until reviewed again; a link or local install is refused",
    ),
    e(
        Cli,
        "pane list|current|get|rename|close|send-text|send-keys|wait-output",
        Implemented,
        "corpus",
        "`list --workspace` → `workspace_id`; `current [--pane|--current]` → `caller_pane_id` (default `HERDR_PANE_ID`); `rename <id> <label…>|--clear`; `wait-output (--match|--regex) [--source --lines --timeout --raw]`",
    ),
    e(
        Cli,
        "pane read",
        Implemented,
        "corpus",
        "`[--source --lines --format --ansi --raw]`; prints the text",
    ),
    e(
        Cli,
        "pane run",
        Implemented,
        "corpus",
        "sends `pane.send_input {text, keys: [\"Enter\"]}` like Herdr (not `pane.run`)",
    ),
    e(
        Cli,
        "pane split",
        Implemented,
        "corpus",
        "`[PANE_ID|--pane|--current] --direction [--ratio --cwd --env… --right-click --focus/--no-focus]` → `target_pane_id` (default `HERDR_PANE_ID`), `env`, `focus` (default false), `right_click` (default herdr)",
    ),
    e(
        Cli,
        "pane layout|process-info",
        Partial,
        "corpus",
        "`[--pane ID | --current]` (`--current` = `HERDR_PANE_ID`); results: see `pane.layout`, `pane.process_info`",
    ),
    e(
        Cli,
        "pane neighbor|edges|focus|input",
        Missing,
        "corpus",
        "Herdr's grammar (`focus` is directional: `pane.focus_direction`); the server does not serve `pane.neighbor`, `pane.edges`, `pane.focus_direction` or `pane.input.set` yet (`unsupported`)",
    ),
    e(
        Cli,
        "pane resize|zoom|swap|move",
        Partial,
        "corpus",
        "Herdr's parameters: `resize {direction, amount}`, `zoom {mode}`, `swap {source_pane_id, target_pane_id}` or `{pane_id, direction}`, `move {pane_id, destination: {type, …}, focus}`; the server still reads Vibeke's names for `pane.swap` (`other_pane_id`) and `pane.move` (`tab_id`, `direction`), so those two fail from the CLI",
    ),
    e(
        Cli,
        "pane report-agent|report-agent-session|release-agent|report-metadata",
        Partial,
        "corpus",
        "Herdr's flags (`--source` required, `-- <resume argv>`, `--token`/`--clear-token`, `--state-label`, …) and parameters; `pane.release_agent` is not served yet; metadata handling: see `pane.report_metadata`",
    ),
    e(
        Cli,
        "workspace list|create|get|focus|rename|close",
        Implemented,
        "corpus",
        "`create [--cwd --label --env… --focus/--no-focus]`; `close <id> [--group]` → `close_group`",
    ),
    e(
        Cli,
        "workspace report-metadata",
        Partial,
        "corpus",
        "`<id> --source ID (--token K=V | --clear-token K)… [--seq --ttl-ms]` → `tokens`; metadata handling: see `workspace.report_metadata`",
    ),
    e(
        Cli,
        "tab list|create|get|focus|rename|close",
        Partial,
        "corpus",
        "`list|create --workspace` → `workspace_id`; `tab.get` is not served yet",
    ),
    e(
        Cli,
        "worktree list|create|open|remove",
        Partial,
        "corpus",
        "Herdr's flags; paths made absolute like Herdr (`~` and the working directory); `open` takes `--path` or `--branch`, never a positional; `worktree.remove` is not served yet",
    ),
    e(
        Cli,
        "agent list|get|read|send-keys|prompt|rename|focus|wait",
        Partial,
        "corpus",
        "Herdr's grammar (`wait --until STATUS… --timeout MS`, `prompt <target> <text> [--wait --until --timeout]`); `agent.focus` and `agent.send_keys` are not served yet",
    ),
    e(
        Cli,
        "agent start",
        Partial,
        "corpus",
        "`<name> --kind --pane [--timeout] [-- args]` → one `agent.start`; Herdr's CLI also waits for the agent to become ready and retries a busy pane; the kind is checked by the server",
    ),
    e(
        Cli,
        "agent explain",
        Partial,
        "corpus",
        "`<target> [--json|--format text|json] [--verbose]` with Herdr's text summary; `--file` is refused (it needs Herdr's detection manifests)",
    ),
    e(
        Cli,
        "agent attach, terminal attach|session",
        Missing,
        "corpus",
        "refused: attaching a terminal needs Herdr's client",
    ),
    e(
        Cli,
        "terminal title set|clear",
        Implemented,
        "corpus",
        "`client.window_title.set|clear`",
    ),
    e(
        Cli,
        "notification show",
        Implemented,
        "corpus",
        "`<title> [--body --position --sound]`",
    ),
    e(
        Cli,
        "server reload-config",
        Partial,
        "corpus",
        "see `server.reload_config`",
    ),
    e(
        Cli,
        "server stop",
        Partial,
        "corpus",
        "forwarded as `server.stop` (see there), nothing printed on success; a plugin invocation is refused by the shim",
    ),
    e(
        Cli,
        "server (headless), live-handoff, agent-manifests, update-agent-manifests, reload-agent-manifests",
        Missing,
        "corpus",
        "refused: Vibeke runs its own server and agent detection",
    ),
    e(
        Cli,
        "api schema",
        Implemented,
        "corpus",
        "local: the summary, `--json` or `--output PATH` of the captured 0.9.3 schema; it describes Herdr, not what Vibeke serves (this inventory does)",
    ),
    e(
        Cli,
        "api snapshot",
        Partial,
        "corpus",
        "sends `session.snapshot`",
    ),
    e(
        Cli,
        "status [server|client] [--json]",
        Partial,
        "corpus",
        "Herdr's text and JSON; the client is the emulated 0.9.3 (protocol 22); the server is whatever answers `ping` on the shim's route (never spawned); `ping` carries no protocol or capabilities yet, so compatibility shows unknown and `restart_needed` true",
    ),
    e(
        Cli,
        "session list [--json]",
        Implemented,
        "corpus",
        "Vibeke's sessions (state directories, `default` first) as `{sessions: [{name, default, running, socket_path, session_dir}]}` or Herdr's table; `socket_path` is the session's compat listener path",
    ),
    e(
        Cli,
        "session attach|stop|delete",
        Missing,
        "corpus",
        "refused: sessions are Vibeke's (`vibeke --session NAME …`)",
    ),
    e(
        Cli,
        "config check",
        Partial,
        "corpus",
        "local, also inside plugin invocations; reads `HERDR_CONFIG_PATH`, else `$XDG_CONFIG_HOME/herdr/config.toml` or `~/.config/herdr/config.toml`; prints `config: ok` (exit 0) or `config: issues found` and the diagnostics (exit 1); checks TOML syntax, unknown sections and keys, `[keys]` types and keybinding syntax, but not Herdr's keybinding conflicts or the values of other sections",
    ),
    e(
        Cli,
        "config reset-keys",
        Missing,
        "corpus",
        "refused: the shim never rewrites Herdr's config.toml",
    ),
    e(
        Cli,
        "channel show|set, machine …",
        Missing,
        "corpus",
        "refused: update channels and saved SSH machines are Vibeke's (`vibeke update`, `vibeke remote`)",
    ),
    e(
        Cli,
        "integration install|status",
        Partial,
        "corpus",
        "served by the vibeke binary on Vibeke's own harness integrations (`status`, `doctor`, `list`; `install`/`uninstall` only with `--yes` or `--dry-run`); never touches Herdr's hooks; an unconfirmed install is refused",
    ),
    e(
        Cli,
        "completion",
        Partial,
        "corpus",
        "bash, zsh and fish scripts generated from the shim grammar; elvish and powershell refused",
    ),
    e(
        Cli,
        "update",
        Missing,
        "corpus",
        "refused: Vibeke has `vibeke update`; the shim never upgrades Herdr",
    ),
    // ---- manifest ---------------------------------------------------------------------------
    e(
        Manifest,
        "id",
        Implemented,
        "spec",
        "identifier rule unverified",
    ),
    e(
        Manifest,
        "name, version, description",
        Implemented,
        "spec",
        "",
    ),
    e(
        Manifest,
        "min_herdr_version",
        Implemented,
        "spec",
        "checked against 0.9.3, never Vibeke's version",
    ),
    e(
        Manifest,
        "platforms (plugin level)",
        Implemented,
        "spec",
        "linux, macos, windows",
    ),
    e(
        Manifest,
        "[[build]] command, platforms",
        Implemented,
        "spec",
        "entry platforms override the plugin's",
    ),
    e(
        Manifest,
        "[[startup]] command, platforms",
        Implemented,
        "spec",
        "",
    ),
    e(
        Manifest,
        "[[actions]] id, title, description, command, platforms",
        Implemented,
        "spec",
        "per-platform twins with one id",
    ),
    e(
        Manifest,
        "[[actions]] contexts",
        Partial,
        "spec",
        "validated; default when omitted (`global`) unverified; the TUI palette shows only actions whose contexts apply (workspace/tab/pane need that focus, selection needs a selection)",
    ),
    e(
        Manifest,
        "[[events]] on, command, platforms, id",
        Implemented,
        "spec",
        "unknown event names warn",
    ),
    e(
        Manifest,
        "[[panes]] id, title, description, placement, command, width, height, platforms",
        Partial,
        "spec",
        "parsed and validated; opened by `plugin.pane.open` in every placement, `width`/`height` size popups; default placement unverified",
    ),
    e(
        Manifest,
        "[[link_handlers]] id, title, pattern, action, platforms",
        Partial,
        "spec",
        "offered by the TUI on hint labels and Ctrl/Alt+click; matching order (registry, then manifest) and modifier unverified",
    ),
    e(
        Manifest,
        "[[keys.command]] key, type, command, description",
        Partial,
        "spec",
        "manifest defaults are installed while the plugin is trusted and enabled, never over the user's keys (conflicts reported in `plugin.list`, skipped), and removed on disable/unlink; the user's `[[keys.command]] type = plugin_action` bindings work",
    ),
    e(
        Manifest,
        "qualified action resolution (`<plugin>.<action>`)",
        Implemented,
        "spec",
        "",
    ),
    e(
        Manifest,
        "unknown-key warnings",
        Partial,
        "spec",
        "warning text differs from upstream",
    ),
    // ---- env ---------------------------------------------------------------------------------
    e(Env, "HERDR_ENV", Implemented, "spec", ""),
    e(
        Env,
        "HERDR_SOCKET_PATH",
        Implemented,
        "spec",
        "the invocation's private broker",
    ),
    e(
        Env,
        "HERDR_BIN_PATH + private PATH launcher dir",
        Implemented,
        "spec",
        "`herdr` → Vibeke shim; never the user's Herdr",
    ),
    e(
        Env,
        "HERDR_PLUGIN_ID, HERDR_PLUGIN_ROOT",
        Implemented,
        "spec",
        "",
    ),
    e(
        Env,
        "HERDR_PLUGIN_CONFIG_DIR, HERDR_PLUGIN_STATE_DIR",
        Implemented,
        "spec",
        "Vibeke-owned, outside the checkout",
    ),
    e(
        Env,
        "HERDR_PLUGIN_CONTEXT_JSON",
        Partial,
        "spec",
        "Herdr's `PluginInvocationContext` field names and values from the server's model (`workspace_id`, `workspace_label`, `workspace_cwd`, `worktree`, `tab_id`, `tab_label`, `focused_pane_*`, `invocation_source`, `correlation_id`, `clicked_url`, `link_handler_id`) for actions, hooks (Herdr's per-event context), startup and plugin panes; the active workspace and focused pane are kept server-side when no client is attached; `selected_text` only when the caller provides it; Vibeke-only keys (`source`, `plugin_id`, `plugin_version`, `action_id`, `event`, `entrypoint_id`, `placement`) are added",
    ),
    e(
        Env,
        "HERDR_WORKSPACE_ID, HERDR_TAB_ID, HERDR_PANE_ID",
        Implemented,
        "spec",
        "actions, hooks and startup: the context's `workspace_id`, `tab_id` and `focused_pane_id`, as Herdr; plugin panes: the pane's own ids (popups: no `HERDR_PANE_ID`)",
    ),
    e(Env, "HERDR_PLUGIN_ACTION_ID", Implemented, "spec", ""),
    e(
        Env,
        "HERDR_PLUGIN_EVENT, HERDR_PLUGIN_EVENT_JSON",
        Implemented,
        "spec",
        "hooks: the dotted event name and Herdr's whole `EventEnvelope` (`{event, data: {type, …}}`, schema `EventData` fields); startup: `HERDR_PLUGIN_EVENT=startup` without JSON",
    ),
    e(
        Env,
        "HERDR_PLUGIN_ENTRYPOINT_ID",
        Implemented,
        "spec",
        "plugin panes only, as Herdr (never for actions, hooks or startup)",
    ),
    e(
        Env,
        "HERDR_PLUGIN_CLICKED_URL, HERDR_PLUGIN_LINK_HANDLER_ID",
        Implemented,
        "spec",
        "set for link-handler invocations (`plugin.link.open`), also `clicked_url`/`link_handler_id` in the context JSON",
    ),
    e(
        Env,
        "stale context variables cleared",
        Implemented,
        "spec",
        "",
    ),
    e(
        Env,
        "build steps without socket/context/authority",
        Implemented,
        "spec",
        "",
    ),
    e(
        Env,
        "HERDR_* in ordinary panes (`compat.herdr_env`)",
        Partial,
        "spec",
        "with `compat.herdr_env` and the compat listener enabled every pane gets HERDR_ENV, HERDR_SOCKET_PATH (the listener), HERDR_PANE_ID, HERDR_WORKSPACE_ID, HERDR_TAB_ID and HERDR_BIN_PATH (the private launcher); inherited HERDR_* are still stripped first; off while the listener is off",
    ),
    e(
        Env,
        "HERDR_SESSION",
        Partial,
        "corpus",
        "selects the session in the shim; semantics otherwise unverified",
    ),
    // ---- lifecycle -------------------------------------------------------------------------
    e(
        Lifecycle,
        "per-user registry shared across sessions (`plugins.json`, atomic)",
        Implemented,
        "spec",
        "works with no server running; every change is a read-modify-write under an exclusive lock, so concurrent revocations are never lost",
    ),
    e(
        Lifecycle,
        "explicit `herdr_legacy` trust grant, shown with entrypoints",
        Implemented,
        "spec",
        "`vibeke plugin trust <id> --legacy`",
    ),
    e(
        Lifecycle,
        "nothing runs before trust",
        Implemented,
        "spec",
        "actions, hooks, startup, build",
    ),
    e(
        Lifecycle,
        "grant bound to the reviewed content and source; change requires re-review",
        Partial,
        "spec",
        "manifest digest, root, source path, whole-tree digest (checked on reinstall: changed content or source drops the grant; a plugin with [[build]] always needs review + build) and a digest of the files the commands reference (checked on every status read and launch); edits to other files of an installed tree are only caught on reinstall",
    ),
    e(
        Lifecycle,
        "disable/unlink/uninstall/revoke stop future execution and brokers",
        Implemented,
        "spec",
        "brokers close within 1 s and drop their connections; every request checks the exact grant; each launch (actions, hooks, startup) re-verifies trust from disk, so the hook cache cannot run changed code",
    ),
    e(
        Lifecycle,
        "no escalation from pane scope",
        Implemented,
        "spec",
        "pane-scoped callers cannot run, trust or install legacy plugins; a process inside a pane of any session (by ancestry, with or without its token) cannot switch sessions",
    ),
    e(
        Lifecycle,
        "build runs for installs only, after trust",
        Implemented,
        "spec",
        "the manifest digest is re-checked before every step and after the last; a change aborts the build and the registration; a build is recorded only for the grant it ran for",
    ),
    e(Lifecycle, "link never builds", Implemented, "spec", ""),
    e(
        Lifecycle,
        "async action invocation with log records",
        Implemented,
        "spec",
        "100 records/session, 64 KiB per stream",
    ),
    e(
        Lifecycle,
        "[[startup]] once per server activation",
        Partial,
        "spec",
        "runs at server start for active plugins, in its own process group (its broker follows the group); takeover semantics unverified",
    ),
    e(
        Lifecycle,
        "[[events]] dispatch with baseline names",
        Partial,
        "spec",
        "projected subset; per-plugin concurrency (default 4, excess hooks queue up to 16, excess action runs get `busy`) and log ring caps with truncation markers are Vibeke's own, upstream limits unverified",
    ),
    e(
        Lifecycle,
        "actions in the command palette",
        Implemented,
        "spec",
        "per machine; untrusted/stale/disabled plugins listed but disabled with the fixing command; actions are filtered by their contexts",
    ),
    e(
        Lifecycle,
        "plugin panes and popups (all placements)",
        Partial,
        "spec",
        "popups: session-modal TUI window, keys to the popup, hidden from compat pane lists/events, no `HERDR_PANE_ID`, callbacks act on the pane underneath; overlays: full-area layer; Herdr's parameter rules and `ui_busy`",
    ),
    e(
        Lifecycle,
        "link handlers",
        Partial,
        "spec",
        "TUI chooser lists matching handlers (untrusted ones disabled) next to the default action; baseline runs the handler without a chooser (unverified)",
    ),
    e(
        Lifecycle,
        "`[[keys.command]] type = plugin_action` bindings",
        Implemented,
        "spec",
        "`command = \"<plugin>.<action>\"`, `description` shown in the palette",
    ),
    e(
        Lifecycle,
        "migration of Herdr plugin registry/config/state (copy, conflict report, rollback)",
        Partial,
        "spec",
        "`vibeke plugin migrate --from <dir>`: copy only, conflicts left alone, `--rollback`; canonical paths, never writes or deletes through a symlink, source registry dirs confined to `--from`; created paths journaled so a failed migration rolls back and a retry resumes it; registry entries reported, linked with `--link`; Herdr's per-plugin dir layout unverified",
    ),
    e(
        Lifecycle,
        "`herdr` launcher symlink installed only on request into a Vibeke bin dir",
        Implemented,
        "spec",
        "`vibeke compat install-shim`",
    ),
    e(
        Lifecycle,
        "compat CLI never reaches a live Herdr server",
        Implemented,
        "spec",
        "HERDR_SOCKET_PATH is used only when it resolves (plain components, no symlinks) to a socket listed in its session's broker registry; `server.*` lifecycle methods are never forwarded",
    ),
    e(
        Lifecycle,
        "audit events for plugin invocations and mutating callbacks",
        Implemented,
        "spec",
        "metadata only: `plugin.invocation_started/finished`, `plugin.api_call` (method, outcome) and `plugin.pane_opened` with `actor.kind = plugin`",
    ),
    e(
        Lifecycle,
        "credentials redacted in plugin logs",
        Implemented,
        "spec",
        "stdout/stderr tails pass through `vk-redact`; the transient output files are removed once read",
    ),
    e(
        Lifecycle,
        "differential suite against pinned Herdr 0.9.3",
        Missing,
        "spec",
        "harness gated behind VIBEKE_HERDR_DIFF=1 + VIBEKE_HERDR_BIN; compares normalized values (id bijection, error codes, statuses, array length/order; malformed output fails), comparator self-tested; event streams and unmodified plugins not compared yet; not run",
    ),
];

/// Methods the baseline is known (by this inventory) to have. Requests for other methods get
/// `method_not_found`; known-but-missing methods get an explicit `unsupported` error.
pub fn known_methods() -> impl Iterator<Item = &'static str> {
    ENTRIES.iter().filter(|e| e.kind == Method).map(|e| e.name)
}

pub fn method_status(m: &str) -> Option<Status> {
    ENTRIES
        .iter()
        .find(|e| e.kind == Method && e.name == m)
        .map(|e| e.status)
}

/// The inventory note of a method (the reason a missing method is `unsupported`).
pub fn method_note(m: &str) -> Option<&'static str> {
    ENTRIES
        .iter()
        .find(|e| e.kind == Method && e.name == m)
        .map(|e| e.note)
}

/// `(implemented, partial, missing)` for one kind, or all kinds with `None`.
pub fn counts(kind: Option<Kind>) -> (usize, usize, usize) {
    let mut c = (0, 0, 0);
    for e in ENTRIES.iter().filter(|e| kind.is_none_or(|k| e.kind == k)) {
        match e.status {
            Implemented => c.0 += 1,
            Partial => c.1 += 1,
            Missing => c.2 += 1,
        }
    }
    c
}

/// The generated Markdown document.
pub fn render_markdown() -> String {
    let mut s = String::new();
    s.push_str("# Herdr compatibility inventory (generated)\n\n");
    s.push_str(&format!(
        "Baseline: **Herdr v{}** at commit `{}` (spec 07 §8.0). Generated from \
         `crates/vk-compat/src/herdr/inventory.rs`; regenerate with \
         `VIBEKE_UPDATE_INVENTORY=1 cargo test -p vk-compat inventory`.\n\n",
        super::BASELINE_VERSION,
        super::BASELINE_COMMIT
    ));
    s.push_str(
        "**Status: partial.** This inventory lists every surface the spec's §7.7/§8 text and \
         tables name, plus surfaces the 99 real plugins in \
         `tests/compat/herdr/0.9.3/manifests/` use. It is not yet the exhaustive inventory \
         §8.0 requires, which must be derived from the pinned binary's `herdr api schema \
         --json`, CLI help and manifest schema. No Herdr binary was run to build it. \
         *Implemented* means mapped and tested against Vibeke using the spec's shapes; no entry \
         is certified until the differential suite (07 §8.4) passes. *Source* `corpus` marks \
         entries the plugins use but the spec does not name.\n\n",
    );
    // Spec 18 §12: a fixed link, so runtime results never change this generated file.
    s.push_str(
        "Real-plugin results: see the latest run of the \
         [Herdr plugins workflow](https://github.com/MidgardAI/vibeke/actions/workflows/herdr-plugins.yml) \
         for the current matrix. Runtime results stay out of this file.\n\n",
    );
    let (i, p, m) = counts(None);
    s.push_str("| Area | Implemented | Partial | Missing | Total |\n|---|---|---|---|---|\n");
    for k in Kind::ALL {
        let (a, b, c) = counts(Some(k));
        s.push_str(&format!(
            "| {} | {a} | {b} | {c} | {} |\n",
            k.title(),
            a + b + c
        ));
    }
    s.push_str(&format!(
        "| **All** | **{i}** | **{p}** | **{m}** | **{}** |\n\n",
        i + p + m
    ));
    for k in Kind::ALL {
        s.push_str(&format!(
            "## {}\n\n| Entry | Status | Source | Notes |\n|---|---|---|---|\n",
            k.title()
        ));
        for e in ENTRIES.iter().filter(|e| e.kind == k) {
            s.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                if k == Method || k == Event {
                    format!("`{}`", e.name)
                } else {
                    e.name.to_string()
                },
                e.status.as_str(),
                e.source,
                e.note
            ));
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_doc_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/herdr-compat-inventory.md");
        let want = render_markdown();
        if std::env::var_os("VIBEKE_UPDATE_INVENTORY").is_some() {
            std::fs::write(&path, &want).unwrap();
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "docs/herdr-compat-inventory.md is stale; run VIBEKE_UPDATE_INVENTORY=1 cargo test -p vk-compat inventory"
        );
    }

    #[test]
    fn entries_are_unique_and_cover_the_grammar() {
        let mut seen = std::collections::BTreeSet::new();
        for e in ENTRIES {
            assert!(
                seen.insert((e.kind as u8, e.name)),
                "duplicate {:?} {}",
                e.kind,
                e.name
            );
        }
        // Every method the CLI shim can send is in the inventory.
        for m in super::super::cli::shim_methods() {
            assert!(
                method_status(m).is_some(),
                "shim method {m} missing from inventory"
            );
        }
        // Every baseline event has an entry.
        for ev in super::super::events::BASELINE_EVENTS {
            assert!(
                ENTRIES.iter().any(|e| e.kind == Event && e.name == *ev),
                "event {ev}"
            );
        }
        // Event statuses agree with what the projector can emit.
        for e in ENTRIES.iter().filter(|e| e.kind == Event) {
            let projected = super::super::events::PROJECTED.contains(&e.name);
            assert_eq!(projected, e.status != Missing, "event {} status", e.name);
        }
        let (i, p, m) = counts(None);
        assert_eq!(i + p + m, ENTRIES.len());
    }

    #[test]
    fn every_baseline_request_method_is_inventoried() {
        let schema: serde_json::Value =
            serde_json::from_str(super::super::cli::API_SCHEMA_JSON).unwrap();
        let methods: Vec<&str> = schema["schemas"]["request"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["properties"]["method"]["const"].as_str())
            .collect();
        assert!(methods.len() > 100, "{}", methods.len());
        for m in methods {
            assert!(
                method_status(m).is_some(),
                "baseline method {m} missing from the inventory"
            );
            if method_status(m) == Some(Missing) {
                assert!(
                    method_note(m).is_some_and(|n| n.contains("unsupported")),
                    "{m}: a missing method names why it is unsupported"
                );
            }
        }
    }
}
