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
        "`<herdr_root>` = `$RUNTIME/herdr-compat` (shared by sessions): default session `herdr.sock`, named sessions `sessions/<name>/herdr.sock`; never under ~/.config/herdr; `compat.herdr_socket_path` not built",
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
        "persisted in `brokers.json`, re-issued at the same path for live invocations whose grant still matches; output is tailed from files so it survives; pid reuse is not detected; exit status after a restart is unknown",
    ),
    e(
        Wire,
        "broker authority for long-lived children after the action exits",
        Implemented,
        "spec",
        "only for long-running entrypoints: `[[startup]]` brokers follow the process group, plugin-pane brokers the pane; action and hook brokers close when their process exits, and closing a broker drops every connection it accepted (requests in flight, subscriptions, waits)",
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
        "`layouts` are per-tab layout snapshots (shape unverified); version/protocol report the emulated baseline",
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
        "`terminal_id` = pane id, `foreground_cwd` = cwd, `scroll` always 0",
    ),
    e(Method, "pane.get", Implemented, "spec", ""),
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
        "`agent` → harness; `pane_id` starts in that pane, else splits the target/focused pane; result `agent_started` unverified",
    ),
    e(
        Method,
        "agent.prompt",
        Partial,
        "spec",
        "text + Enter (native `agent.prompt`), `wait`, `timeout_ms`",
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
        "result `agent_info` unverified",
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
        "reuses the workspace rooted at the worktree or creates one; emits `worktree.opened`",
    ),
    e(Method, "events.subscribe", Partial, "spec", "see events"),
    e(
        Method,
        "events.wait",
        Partial,
        "spec",
        "first projected event matching the subscription",
    ),
    // ---- methods: 07 §8.3 required-coverage list -------------------------------------------
    e(
        Method,
        "plugin.list",
        Implemented,
        "spec",
        "registry entries with status",
    ),
    e(
        Method,
        "plugin.link",
        Implemented,
        "spec",
        "registers in place; never builds or trusts; refused from panes",
    ),
    e(
        Method,
        "plugin.unlink",
        Implemented,
        "spec",
        "files preserved; refused from panes",
    ),
    e(
        Method,
        "plugin.enable",
        Implemented,
        "spec",
        "refused from panes",
    ),
    e(
        Method,
        "plugin.disable",
        Implemented,
        "spec",
        "refused from panes; cuts live brokers",
    ),
    e(Method, "plugin.action.list", Implemented, "spec", ""),
    e(
        Method,
        "plugin.action.invoke",
        Partial,
        "spec",
        "qualified `action_id` (`<plugin>.<action>`) with optional `plugin_id`; returns the running log record immediately; request shape per the pinned schema as reviewed, full response shape unverified",
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
        "`split`, `tab`, `zoomed`, `overlay` (a zoomed pane that restores focus when it ends, not a real overlay); `popup` needs the TUI; width/height ignored; one broker per pane",
    ),
    e(
        Method,
        "plugin.pane.focus",
        Partial,
        "spec",
        "by `plugin_id` + entrypoint or `pane_id`",
    ),
    e(
        Method,
        "plugin.pane.close",
        Partial,
        "spec",
        "by `plugin_id` + entrypoint or `pane_id`",
    ),
    e(
        Method,
        "popup.close",
        Missing,
        "spec",
        "needs the TUI popup layer",
    ),
    e(
        Method,
        "layout.export",
        Partial,
        "spec",
        "binary split tree (`split_id`, `direction`, `ratio`, `first`, `second` / `pane_id`); PaneLayoutSnapshot shape unverified",
    ),
    e(
        Method,
        "layout.apply",
        Partial,
        "spec",
        "baseline `root` (with `tab_id`), or a whole snapshot under `layout`; rearranges the tab's own panes, every pane exactly once; node shape unverified",
    ),
    e(
        Method,
        "layout.set_split_ratio",
        Partial,
        "spec",
        "baseline `path` (booleans from the root, `true` = second side; inferred), `split_id` (from the snapshot) or `pane_id`; ratio of the first side",
    ),
    e(
        Method,
        "pane.process_info",
        Partial,
        "spec",
        "pid, foreground argv/command, cwd; field names unverified",
    ),
    e(
        Method,
        "pane.move",
        Partial,
        "spec",
        "to `tab_id`, or next to `target_pane_id` in `direction`, within a workspace; cross-workspace refused; emits `pane.moved`",
    ),
    e(
        Method,
        "pane.swap",
        Partial,
        "spec",
        "within a workspace; emits `pane.moved` for both panes",
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
        Partial,
        "spec",
        "stored and evented (`client.window_title_changed`); the TUI does not apply it yet",
    ),
    e(
        Method,
        "client.window_title.clear",
        Partial,
        "spec",
        "see `client.window_title.set`",
    ),
    e(
        Method,
        "agent.view.set",
        Missing,
        "spec",
        "semantics unknown until the baseline schema is captured; TUI",
    ),
    e(
        Method,
        "agent.view.clear",
        Missing,
        "spec",
        "see `agent.view.set`",
    ),
    e(
        Method,
        "pane.report_metadata",
        Partial,
        "spec",
        "merged per pane (`metadata`, `key`/`value` or flat params), shown as `metadata` in pane records; in memory",
    ),
    e(
        Method,
        "workspace.report_metadata",
        Partial,
        "spec",
        "as `pane.report_metadata`, per workspace",
    ),
    e(
        Method,
        "ping",
        Partial,
        "spec",
        "reports the emulated baseline",
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
        Missing,
        "corpus",
        "refused on the compat endpoint",
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
        "`herdr pane run`; types the command + Enter",
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
        "pane.moved",
        Implemented,
        "spec",
        "from `pane.move`/`pane.swap`; `from_tab_id`/`to_tab_id` payload unverified",
    ),
    e(Event, "pane.exited", Implemented, "spec", ""),
    e(
        Event,
        "pane.agent_detected",
        Partial,
        "spec",
        "no 250 ms debounce",
    ),
    e(
        Event,
        "pane.agent_status_changed",
        Partial,
        "spec",
        "fires on mapped-status change only; status enumeration unverified",
    ),
    e(
        Event,
        "pane.output_matched",
        Partial,
        "spec",
        "fires when a compat `pane.wait_for_output` matcher matches",
    ),
    e(Event, "pane.scroll_changed", Missing, "spec", ""),
    e(
        Event,
        "layout.updated",
        Partial,
        "spec",
        "payload carries `layout` (snapshot shape unverified)",
    ),
    e(
        Event,
        "worktree.created",
        Implemented,
        "spec",
        "from `worktree.create` and `task.create` worktree checkouts; used by 8 corpus plugins",
    ),
    e(
        Event,
        "worktree.opened",
        Implemented,
        "spec",
        "from `worktree.open`; used by 5 corpus plugins",
    ),
    e(
        Event,
        "worktree.removed",
        Partial,
        "spec",
        "payload `worktree {path}`",
    ),
    e(
        Event,
        "workspace.reordered",
        Missing,
        "corpus",
        "declared by one plugin; not named by the spec (may not exist in the baseline)",
    ),
    // ---- CLI --------------------------------------------------------------------------------
    e(
        Cli,
        "--version",
        Implemented,
        "spec",
        "reports the emulated baseline",
    ),
    e(
        Cli,
        "global session selection (`--session`, HERDR_SESSION)",
        Partial,
        "spec",
        "`--session NAME` and `HERDR_SESSION`; explicit sessions never spawn; panes cannot switch sessions; plugins keep their identity (grant re-checked on the destination)",
    ),
    e(
        Cli,
        "plugin install <path>",
        Partial,
        "corpus",
        "local directories and manifest paths; build runs after trust",
    ),
    e(
        Cli,
        "plugin install owner/repo[/subdir] [--ref]",
        Missing,
        "spec",
        "git sources need network; not built",
    ),
    e(
        Cli,
        "plugin install --yes",
        Implemented,
        "spec",
        "accepts the displayed legacy trust terms (operator only)",
    ),
    e(Cli, "plugin link", Implemented, "corpus", "no build"),
    e(
        Cli,
        "plugin unlink",
        Implemented,
        "corpus",
        "files preserved",
    ),
    e(
        Cli,
        "plugin uninstall",
        Implemented,
        "corpus",
        "managed checkout removed, config/state kept",
    ),
    e(Cli, "plugin enable / disable", Implemented, "corpus", ""),
    e(
        Cli,
        "plugin list",
        Partial,
        "corpus",
        "JSON output shape unverified",
    ),
    e(Cli, "plugin config-dir", Implemented, "corpus", ""),
    e(
        Cli,
        "plugin action list / invoke",
        Partial,
        "corpus",
        "argument grammar unverified",
    ),
    e(Cli, "plugin log list", Partial, "corpus", ""),
    e(
        Cli,
        "plugin pane open|focus|close",
        Partial,
        "corpus",
        "`--plugin --entrypoint --placement --direction --cwd --focus`; popup refused",
    ),
    e(Cli, "plugin update", Missing, "corpus", ""),
    e(
        Cli,
        "pane list|get|current|read",
        Partial,
        "corpus",
        "flag grammar unverified",
    ),
    e(
        Cli,
        "pane send-text|send-keys|run|focus|split|close|rename|wait-output",
        Partial,
        "corpus",
        "",
    ),
    e(
        Cli,
        "pane report-metadata|process-info|move",
        Partial,
        "corpus",
        "flag grammar unverified",
    ),
    e(
        Cli,
        "workspace list|create|rename|focus|move|close",
        Partial,
        "corpus",
        "",
    ),
    e(
        Cli,
        "workspace report-metadata",
        Partial,
        "corpus",
        "flag grammar unverified",
    ),
    e(
        Cli,
        "tab list|create|rename|focus|move|close",
        Partial,
        "corpus",
        "",
    ),
    e(Cli, "agent list|get|send|read", Partial, "corpus", ""),
    e(
        Cli,
        "agent start|prompt|wait|explain",
        Partial,
        "corpus",
        "`explain` missing",
    ),
    e(Cli, "worktree list|repo-root", Partial, "corpus", ""),
    e(
        Cli,
        "worktree create|open",
        Partial,
        "corpus",
        "flag grammar unverified",
    ),
    e(Cli, "notification show", Partial, "corpus", ""),
    e(Cli, "server reload-config", Partial, "corpus", ""),
    e(Cli, "server stop", Missing, "corpus", "refused"),
    e(Cli, "api schema", Partial, "corpus", ""),
    e(
        Cli,
        "integration install|status",
        Missing,
        "corpus",
        "refused by design: never installs into Herdr; use `vibeke integration`",
    ),
    e(
        Cli,
        "config check, completion, update/upgrade, web ui",
        Missing,
        "corpus",
        "refused; Vibeke equivalents exist for some",
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
        "validated; default when omitted (`global`) unverified",
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
        "parsed and validated; opened by `plugin.pane.open` (split, tab, zoomed, overlay); default placement unverified",
    ),
    e(
        Manifest,
        "[[link_handlers]] id, title, pattern, action, platforms",
        Partial,
        "spec",
        "regex compiled, action resolved; not wired to clicks",
    ),
    e(
        Manifest,
        "[[keys.command]] key, type, command, description",
        Partial,
        "spec",
        "parsed, action resolution warns; bindings not installed",
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
        "source, correlation id, workspace/tab/pane ids and labels, cwd; not the full PluginInvocationContext",
    ),
    e(
        Env,
        "HERDR_WORKSPACE_ID, HERDR_TAB_ID, HERDR_PANE_ID",
        Partial,
        "spec",
        "from the invocation context; upstream presence rules unverified",
    ),
    e(Env, "HERDR_PLUGIN_ACTION_ID", Implemented, "spec", ""),
    e(
        Env,
        "HERDR_PLUGIN_EVENT, HERDR_PLUGIN_EVENT_JSON",
        Partial,
        "spec",
        "payload shape unverified",
    ),
    e(
        Env,
        "HERDR_PLUGIN_ENTRYPOINT_ID",
        Partial,
        "spec",
        "set for actions/hooks/startup",
    ),
    e(
        Env,
        "HERDR_PLUGIN_CLICKED_URL, HERDR_PLUGIN_LINK_HANDLER_ID",
        Missing,
        "spec",
        "link handlers not wired",
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
        Missing,
        "spec",
        "still stripped",
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
        "projected subset; concurrency/log limits unverified",
    ),
    e(
        Lifecycle,
        "actions in the command palette",
        Missing,
        "spec",
        "API `plugin.action.list/run` ready for the TUI",
    ),
    e(
        Lifecycle,
        "plugin panes and popups (all placements)",
        Partial,
        "spec",
        "server side: split/tab/zoomed/overlay as Vibeke panes with their own broker and `HERDR_*` env; popups and real overlays need the TUI",
    ),
    e(Lifecycle, "link handlers", Missing, "spec", ""),
    e(
        Lifecycle,
        "`[[keys.command]] type = plugin_action` bindings",
        Missing,
        "spec",
        "",
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
}
