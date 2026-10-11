//! Grammar of the `herdr`-compatible CLI shim (`vibeke compat herdr …`, or the private `herdr`
//! launcher, 07 §8.2).
//!
//! Every command follows the Herdr 0.9.3 parser for that command (`src/cli.rs` and
//! `src/cli/*.rs` at the baseline commit) and the captured help in
//! `tests/compat/herdr/0.9.3/cli-help/`: the same flags, the same socket method and parameter
//! names (absent options are left out, as Herdr's `skip_serializing_if` does), the same request
//! `id` (`cli:pane:get`, …), the same usage messages and the same exit codes (2 for bad
//! arguments). `<command> --help` prints the captured help page. Commands the shim does not
//! perform (terminal attach, saved machines, update channels, …) are refused with a reason; the
//! inventory lists them.

use serde_json::{Map, Value, json};
use std::path::PathBuf;

/// How the shim prints the answer to a [`Parsed::Call`] (Herdr's printers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `print_response`: the whole response line on stdout (exit 0), an error line on stderr
    /// (exit 1).
    Response,
    /// `send_ok_request`: nothing on success; an error line on stderr (exit 1).
    Quiet,
    /// `pane read` and `agent read`: `result.read.text` exactly as returned.
    ReadText,
    /// `agent explain`: `result.explain` as JSON (`--json`) or as Herdr's text summary.
    Explain { json: bool, verbose: bool },
}

/// `herdr status [server|client]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusScope {
    Full,
    Server,
    Client,
}

/// `herdr api schema [--json | --output PATH]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiSchema {
    Summary,
    Json,
    Output(String),
}

/// What one shim invocation does.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// Send one request (with Herdr's request `id`) and print the answer with `output`.
    Call {
        id: String,
        method: String,
        params: Value,
        output: Output,
    },
    /// Operate on the per-user plugin registry (works without a server).
    Local(Local),
    Version,
    /// Text for stdout, printed as is (exit 0): help pages, `--default-config`, completion.
    Help(String),
    /// Text for stderr with Herdr's exit code: 2 for bad arguments, 0 for `<group> help`.
    Usage {
        text: String,
        code: i32,
    },
    /// An error Herdr's `main` returns (`Error: Custom { kind: Other, error: "…" }`, exit 1).
    Failed(String),
    /// A baseline command Vibeke deliberately does not perform (exit 1 with the reason).
    Refused {
        command: String,
        reason: String,
    },
    /// `herdr config check`: checks Herdr's `config.toml` locally (no server).
    ConfigCheck,
    /// `herdr session list [--json]`.
    SessionList {
        json: bool,
    },
    /// `herdr status [server|client] [--json]`.
    Status {
        scope: StatusScope,
        json: bool,
    },
    /// `herdr api schema …`: the captured baseline schema, printed locally.
    ApiSchema(ApiSchema),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Local {
    PluginInstall {
        source: String,
        git_ref: Option<String>,
        yes: bool,
    },
    PluginLink {
        path: String,
        /// Accept the legacy trust terms (`vibeke plugin link --yes`; not in Herdr's grammar).
        yes: bool,
        /// `false` with Herdr's `--disabled`: link the plugin without enabling it.
        enabled: bool,
    },
    PluginUnlink {
        id: String,
    },
    PluginUninstall {
        id: String,
    },
    PluginEnable {
        id: String,
    },
    PluginDisable {
        id: String,
    },
    /// `plugin list [--plugin ID] [--json]`: Herdr prints the response line with `--json`,
    /// a summary otherwise.
    PluginList {
        plugin: Option<String>,
        json: bool,
    },
    PluginConfigDir {
        id: String,
    },
    /// `plugin update <id>` (a Vibeke addition): re-fetch the recorded repository and ref.
    PluginUpdate {
        id: String,
        yes: bool,
    },
}

/// The process facts Herdr's parsers read: `--current` is the caller's pane (`HERDR_PANE_ID`),
/// and worktree paths are made absolute against the working directory (`~` against `HOME`).
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub pane_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub home: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        Env {
            pane_id: std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            cwd: std::env::current_dir().ok(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

/// Herdr's private protocol version and endpoint generation at the baseline (`status`).
pub const PROTOCOL_VERSION: u32 = 22;
pub const ENDPOINT_PROTOCOL_GENERATION: u32 = 1;

/// `herdr api schema --json` of the baseline, as captured.
pub const API_SCHEMA_JSON: &str =
    include_str!("../../../../tests/compat/herdr/0.9.3/api-schema.json");

const HELP: &str = "herdr (Vibeke compatibility shim, emulating Herdr 0.9.3 — partial)

usage: herdr <command> <subcommand> [args] [--flag value]
commands: workspace worktree tab pane agent notification plugin session status config api
          server terminal integration completion
`herdr <command> <subcommand> --help` prints Herdr's help for that command.
This shim talks to Vibeke, never to a running Herdr.
";

type R = Result<Parsed, Parsed>;

fn usage<S: Into<String>>(text: S) -> Parsed {
    Parsed::Usage {
        text: text.into(),
        code: 2,
    }
}

fn group(text: &str, code: i32) -> Parsed {
    Parsed::Usage {
        text: text.to_string(),
        code,
    }
}

fn unknown(option: &str) -> Parsed {
    usage(format!("unknown option: {option}"))
}

fn refused(command: &str, reason: &str) -> Parsed {
    Parsed::Refused {
        command: command.to_string(),
        reason: reason.to_string(),
    }
}

fn call(id: &str, method: &str, params: Value) -> Parsed {
    Parsed::Call {
        id: id.to_string(),
        method: method.to_string(),
        params,
        output: Output::Response,
    }
}

/// Herdr's `send_ok_request`: id `cli:request`, nothing printed on success.
fn quiet(method: &str, params: Value) -> Parsed {
    Parsed::Call {
        id: "cli:request".to_string(),
        method: method.to_string(),
        params,
        output: Output::Quiet,
    }
}

/// The value after the flag at `i`, or Herdr's `missing value for <flag>`.
fn value(a: &[String], i: usize, flag: &str) -> Result<String, Parsed> {
    a.get(i + 1)
        .cloned()
        .ok_or_else(|| usage(format!("missing value for {flag}")))
}

/// Exactly one argument, else the usage line.
fn one_arg(a: &[String], usage_line: &str) -> Result<String, Parsed> {
    match a {
        [x] => Ok(x.clone()),
        _ => Err(usage(usage_line)),
    }
}

/// Herdr's `expand_equals_args`: split `--flag=value` for the listed value options only.
fn expand_equals(a: &[String], value_options: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(a.len());
    for arg in a {
        match arg.split_once('=') {
            Some((flag, v)) if value_options.contains(&flag) => {
                out.push(flag.to_string());
                out.push(v.to_string());
            }
            _ => out.push(arg.clone()),
        }
    }
    out
}

fn parse_env_assignment(raw: &str) -> Result<(String, String), String> {
    let Some((key, v)) = raw.split_once('=') else {
        return Err("env must use KEY=VALUE".into());
    };
    if key.is_empty() {
        return Err("env key must not be empty".into());
    }
    if key.contains('\0') || v.contains('\0') {
        return Err("env must not contain NUL bytes".into());
    }
    Ok((key.to_string(), v.to_string()))
}

fn parse_token_assignment(raw: &str) -> Result<(String, Option<String>), String> {
    let Some((key, v)) = raw.split_once('=') else {
        return Err("token must use NAME=VALUE".into());
    };
    if key.is_empty() {
        return Err("token name must not be empty".into());
    }
    Ok((key.to_string(), Some(v.to_string())))
}

/// A JSON object built like Herdr's serde structs: `None` and empty maps are left out.
struct Obj(Map<String, Value>);

impl Obj {
    fn new() -> Self {
        Obj(Map::new())
    }
    fn put(mut self, k: &str, v: impl Into<Value>) -> Self {
        self.0.insert(k.to_string(), v.into());
        self
    }
    fn opt<T: Into<Value>>(self, k: &str, v: Option<T>) -> Self {
        match v {
            Some(v) => self.put(k, v),
            None => self,
        }
    }
    fn map(self, k: &str, m: Map<String, Value>) -> Self {
        if m.is_empty() {
            self
        } else {
            self.put(k, Value::Object(m))
        }
    }
    fn flag(self, k: &str, b: bool) -> Self {
        if b { self.put(k, true) } else { self }
    }
    fn done(self) -> Value {
        Value::Object(self.0)
    }
}

/// A float flag value Herdr parses as a finite `f32` (sent as the decimal the user wrote).
fn finite_f32(v: &str) -> Option<f64> {
    v.parse::<f32>()
        .ok()
        .filter(|f| f.is_finite())
        .and_then(|_| v.parse::<f64>().ok())
}

fn parse_u32_flag(flag: &str, v: &str) -> Result<u32, String> {
    v.parse::<u32>()
        .map_err(|_| format!("invalid value for {flag}: {v}"))
}

fn parse_u64_flag(flag: &str, v: &str) -> Result<u64, String> {
    v.parse::<u64>()
        .map_err(|_| format!("invalid value for {flag}: {v}"))
}

fn parse_read_source(v: &str) -> Result<&'static str, String> {
    match v {
        "visible" => Ok("visible"),
        "recent" => Ok("recent"),
        "recent-unwrapped" | "recent_unwrapped" => Ok("recent_unwrapped"),
        "detection" => Ok("detection"),
        _ => Err(format!("invalid read source: {v}")),
    }
}

fn parse_read_format(v: &str) -> Result<&'static str, String> {
    match v {
        "text" => Ok("text"),
        "ansi" => Ok("ansi"),
        _ => Err(format!("invalid read format: {v}")),
    }
}

fn parse_pane_direction(v: &str) -> Result<&'static str, String> {
    match v {
        "left" => Ok("left"),
        "right" => Ok("right"),
        "up" => Ok("up"),
        "down" => Ok("down"),
        _ => Err(format!(
            "invalid pane direction: {v} (expected left, right, up, or down)"
        )),
    }
}

fn parse_right_click(v: &str) -> Result<&'static str, String> {
    match v {
        "herdr" => Ok("herdr"),
        "pane" => Ok("pane"),
        _ => Err(format!("invalid right-click target: {v}")),
    }
}

fn parse_agent_status(v: &str) -> Result<&'static str, String> {
    match v {
        "idle" => Ok("idle"),
        "working" => Ok("working"),
        "blocked" => Ok("blocked"),
        "done" => Ok("done"),
        "unknown" => Ok("unknown"),
        _ => Err(format!(
            "invalid agent status: {v} (expected idle, working, blocked, done, or unknown)"
        )),
    }
}

fn parse_pane_agent_state(v: &str) -> Result<&'static str, String> {
    match v {
        "idle" => Ok("idle"),
        "working" => Ok("working"),
        "blocked" => Ok("blocked"),
        "unknown" => Ok("unknown"),
        _ => Err(format!(
            "invalid pane agent state: {v} (expected idle, working, blocked, or unknown)"
        )),
    }
}

/// Herdr's `PopupSize::parse_cli`: cells (`u16`) or a percentage `1%..=100%`.
fn parse_popup_size(v: &str) -> Result<Value, String> {
    if let Some(p) = v.strip_suffix('%') {
        let p = p
            .parse::<u8>()
            .map_err(|_| "must be a number of cells or a percentage like 80%".to_string())?;
        if !(1..=100).contains(&p) {
            return Err("percentage must be between 1% and 100%".into());
        }
        return Ok(Value::String(format!("{p}%")));
    }
    v.parse::<u16>()
        .map(Value::from)
        .map_err(|_| "must be a number of cells or a percentage like 80%".to_string())
}

// ---- help pages -----------------------------------------------------------------------------

/// A captured help page: `(command path, cli-help/<file>.txt)`.
macro_rules! page {
    ($path:literal, $file:literal) => {
        (
            $path,
            include_str!(concat!(
                "../../../../tests/compat/herdr/0.9.3/cli-help/",
                $file,
                ".txt"
            )),
        )
    };
}

/// `herdr <path> --help` for every command path of the baseline (captured help, verbatim).
const HELP_PAGES: &[(&str, &str)] = &[
    page!("completion", "completion"),
    page!("update", "update"),
    page!("status", "status"),
    page!("status server", "status_server"),
    page!("status client", "status_client"),
    page!("config", "config"),
    page!("config check", "config_check"),
    page!("config reset-keys", "config_reset-keys"),
    page!("channel", "channel"),
    page!("channel show", "channel_show"),
    page!("channel set", "channel_set"),
    page!("machine", "machine"),
    page!("machine list", "machine_list"),
    page!("machine status", "machine_status"),
    page!("machine reconnect", "machine_reconnect"),
    page!("machine add", "machine_add"),
    page!("machine rename", "machine_rename"),
    page!("machine remove", "machine_remove"),
    page!("machine enable", "machine_enable"),
    page!("machine disable", "machine_disable"),
    page!("server", "server"),
    page!("server stop", "server_stop"),
    page!("server reload-config", "server_reload-config"),
    page!("server agent-manifests", "server_agent-manifests"),
    page!(
        "server update-agent-manifests",
        "server_update-agent-manifests"
    ),
    page!(
        "server reload-agent-manifests",
        "server_reload-agent-manifests"
    ),
    page!("api", "api"),
    page!("api snapshot", "api_snapshot"),
    page!("api schema", "api_schema"),
    page!("workspace", "workspace"),
    page!("workspace list", "workspace_list"),
    page!("workspace create", "workspace_create"),
    page!("workspace get", "workspace_get"),
    page!("workspace focus", "workspace_focus"),
    page!("workspace rename", "workspace_rename"),
    page!("workspace report-metadata", "workspace_report-metadata"),
    page!("workspace close", "workspace_close"),
    page!("worktree", "worktree"),
    page!("worktree list", "worktree_list"),
    page!("worktree create", "worktree_create"),
    page!("worktree open", "worktree_open"),
    page!("worktree remove", "worktree_remove"),
    page!("tab", "tab"),
    page!("tab list", "tab_list"),
    page!("tab create", "tab_create"),
    page!("tab get", "tab_get"),
    page!("tab focus", "tab_focus"),
    page!("tab rename", "tab_rename"),
    page!("tab close", "tab_close"),
    page!("notification", "notification"),
    page!("notification show", "notification_show"),
    page!("agent", "agent"),
    page!("agent list", "agent_list"),
    page!("agent get", "agent_get"),
    page!("agent read", "agent_read"),
    page!("agent send-keys", "agent_send-keys"),
    page!("agent prompt", "agent_prompt"),
    page!("agent rename", "agent_rename"),
    page!("agent focus", "agent_focus"),
    page!("agent wait", "agent_wait"),
    page!("agent attach", "agent_attach"),
    page!("agent start", "agent_start"),
    page!("agent explain", "agent_explain"),
    page!("pane", "pane"),
    page!("pane list", "pane_list"),
    page!("pane current", "pane_current"),
    page!("pane get", "pane_get"),
    page!("pane layout", "pane_layout"),
    page!("pane process-info", "pane_process-info"),
    page!("pane neighbor", "pane_neighbor"),
    page!("pane edges", "pane_edges"),
    page!("pane focus", "pane_focus"),
    page!("pane resize", "pane_resize"),
    page!("pane zoom", "pane_zoom"),
    page!("pane read", "pane_read"),
    page!("pane rename", "pane_rename"),
    page!("pane input", "pane_input"),
    page!("pane split", "pane_split"),
    page!("pane swap", "pane_swap"),
    page!("pane move", "pane_move"),
    page!("pane close", "pane_close"),
    page!("pane send-text", "pane_send-text"),
    page!("pane send-keys", "pane_send-keys"),
    page!("pane wait-output", "pane_wait-output"),
    page!("pane run", "pane_run"),
    page!("pane report-agent", "pane_report-agent"),
    page!("pane report-agent-session", "pane_report-agent-session"),
    page!("pane release-agent", "pane_release-agent"),
    page!("pane report-metadata", "pane_report-metadata"),
    page!("terminal", "terminal"),
    page!("terminal attach", "terminal_attach"),
    page!("terminal session", "terminal_session"),
    page!("terminal session control", "terminal_session_control"),
    page!("terminal session observe", "terminal_session_observe"),
    page!("terminal title", "terminal_title"),
    page!("terminal title set", "terminal_title_set"),
    page!("terminal title clear", "terminal_title_clear"),
    page!("session", "session"),
    page!("session list", "session_list"),
    page!("session attach", "session_attach"),
    page!("session stop", "session_stop"),
    page!("session delete", "session_delete"),
    page!("integration", "integration"),
    page!("integration install", "integration_install"),
    page!("integration uninstall", "integration_uninstall"),
    page!("integration status", "integration_status"),
    page!("plugin", "plugin"),
    page!("plugin install", "plugin_install"),
    page!("plugin uninstall", "plugin_uninstall"),
    page!("plugin link", "plugin_link"),
    page!("plugin unlink", "plugin_unlink"),
    page!("plugin enable", "plugin_enable"),
    page!("plugin disable", "plugin_disable"),
    page!("plugin list", "plugin_list"),
    page!("plugin config-dir", "plugin_config-dir"),
    page!("plugin action", "plugin_action"),
    page!("plugin action list", "plugin_action_list"),
    page!("plugin action invoke", "plugin_action_invoke"),
    page!("plugin log", "plugin_log"),
    page!("plugin log list", "plugin_log_list"),
    page!("plugin pane", "plugin_pane"),
    page!("plugin pane open", "plugin_pane_open"),
    page!("plugin pane focus", "plugin_pane_focus"),
    page!("plugin pane close", "plugin_pane_close"),
];

/// Herdr's `print_requested_help`: `--help`/`-h` right after a complete command path (and not
/// after `--`) prints that command's help.
fn requested_help(args: &[String]) -> Option<&'static str> {
    let i = args.iter().position(|a| a == "--help" || a == "-h")?;
    if i == 0 || args[..i].iter().any(|a| a == "--") {
        return None;
    }
    let key = args[..i].join(" ");
    HELP_PAGES.iter().find(|(k, _)| *k == key).map(|(_, p)| *p)
}

/// The agent kinds `agent start --kind` accepts, from the captured help.
fn agent_kinds() -> String {
    let page = HELP_PAGES
        .iter()
        .find(|(k, _)| *k == "agent start")
        .map(|(_, p)| *p)
        .unwrap_or_default();
    page.split_once("[possible values: ")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(list, _)| list.split(", ").collect::<Vec<_>>().join("|"))
        .unwrap_or_default()
}

// ---- entry point ----------------------------------------------------------------------------

/// Remove Herdr's global `--session NAME` / `--session=NAME` flag from a command line (it may
/// appear anywhere before a `--` separator). Returns the selected session, if any, and the
/// remaining arguments; an empty or path-like name is a usage error.
pub fn take_session(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut out = Vec::with_capacity(args.len());
    let mut session = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            out.extend(args[i..].iter().cloned());
            break;
        }
        let v = if a == "--session" {
            i += 1;
            Some(
                args.get(i)
                    .cloned()
                    .ok_or("--session needs a session name")?,
            )
        } else {
            a.strip_prefix("--session=").map(str::to_string)
        };
        match v {
            Some(name) => {
                if !super::valid_session_name(&name) {
                    return Err(format!("invalid session name `{name}`"));
                }
                session = Some(name);
            }
            None => out.push(a.clone()),
        }
        i += 1;
    }
    Ok((session, out))
}

/// Parse a shim command line (without the program name) with this process's environment.
pub fn parse(args: &[String]) -> Parsed {
    parse_with(args, &Env::from_process())
}

/// Parse a shim command line (without the program name).
pub fn parse_with(args: &[String], env: &Env) -> Parsed {
    if let Some(page) = requested_help(args) {
        return Parsed::Help(page.to_string());
    }
    let noun = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let r = match noun {
        "workspace" => workspace(rest),
        "worktree" => worktree(rest, env),
        "tab" => tab(rest),
        "pane" => pane(rest, env),
        "agent" => agent(rest),
        "notification" => notification(rest),
        "plugin" => plugin(rest),
        "session" => session(rest),
        "status" => status(rest),
        "config" => config(rest),
        "api" => api(rest),
        "server" => server(rest),
        "terminal" => terminal(rest),
        "completion" | "completions" => completion_cmd(rest),
        "integration" => Ok(refused(
            &format!(
                "integration {}",
                rest.first().map(String::as_str).unwrap_or("")
            ),
            "installs need `--yes` (or `--dry-run`) and write Vibeke's own hooks, never Herdr's; or use `vibeke integration install <harness>`",
        )),
        "channel" => Ok(refused(
            "channel",
            "Vibeke has its own update channel (`vibeke update`); the shim never changes Herdr's",
        )),
        "machine" => Ok(refused(
            "machine",
            "saved SSH machines are not available through the shim; use `vibeke remote`",
        )),
        _ => Ok(top_level(args)),
    };
    r.unwrap_or_else(|p| p)
}

/// Commands Herdr's `main` handles after the CLI dispatcher: global flags, `update`, the TUI
/// launch, and unknown commands or options.
fn top_level(args: &[String]) -> Parsed {
    if args.is_empty() {
        return Parsed::Help(HELP.into());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Parsed::Help(HELP.into());
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        return Parsed::Version;
    }
    if args.iter().any(|a| a == "--default-config") {
        return Parsed::Help(super::cli_config::DEFAULT_CONFIG.into());
    }
    if args.iter().any(|a| a == "--skill") {
        return refused(
            "--skill",
            "the agent skill file ships with Herdr, not with Vibeke",
        );
    }
    const KNOWN_FLAGS: &[&str] = &[
        "--session",
        "--machine",
        "--remote",
        "--remote-keybindings",
        "--version",
        "-V",
        "--default-config",
        "--skill",
        "--help",
        "-h",
    ];
    const KNOWN_COMMANDS: &[&str] = &[
        "server",
        "client",
        "remote-client-bridge",
        "update",
        "status",
        "config",
        "channel",
        "machine",
        "workspace",
        "worktree",
        "pane",
        "session",
        "integration",
    ];
    for a in args {
        let name = a.split_once('=').map(|(n, _)| n).unwrap_or(a);
        if a.starts_with('-') && !KNOWN_FLAGS.contains(&name) {
            return usage(format!("unknown option: {a}\nrun 'herdr --help' for usage"));
        }
        if !a.starts_with('-') && !KNOWN_COMMANDS.contains(&a.as_str()) {
            return usage(format!(
                "unknown command: {a}\nrun 'herdr --help' for usage"
            ));
        }
    }
    if args[0] == "update" {
        return refused(
            "update",
            "Vibeke updates itself with `vibeke update`; the shim never upgrades Herdr",
        );
    }
    refused(
        &args.join(" "),
        "the shim does not launch or attach a Herdr client; run `vibeke`",
    )
}

// ---- workspace ------------------------------------------------------------------------------

const WORKSPACE_HELP: &str = "herdr workspace commands:
  herdr workspace list
  herdr workspace create [--cwd PATH] [--label TEXT] [--env KEY=VALUE] [--focus] [--no-focus]
  herdr workspace get <workspace_id>
  herdr workspace focus <workspace_id>
  herdr workspace rename <workspace_id> <label>
  herdr workspace report-metadata <workspace_id> --source ID [--token NAME=VALUE] [--clear-token NAME] [--seq N] [--ttl-ms N]
  herdr workspace close <workspace_id> [--group]";

fn workspace(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(WORKSPACE_HELP, 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "list" => {
            if !a.is_empty() {
                return Err(usage("usage: herdr workspace list"));
            }
            Ok(call("cli:workspace:list", "workspace.list", json!({})))
        }
        "create" => {
            let (mut cwd, mut label, mut focus, mut env) = (None, None, false, Map::new());
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--cwd" => {
                        cwd = Some(value(a, i, "--cwd")?);
                        i += 2;
                    }
                    "--label" => {
                        label = Some(value(a, i, "--label")?);
                        i += 2;
                    }
                    "--focus" => {
                        focus = true;
                        i += 1;
                    }
                    "--no-focus" => {
                        focus = false;
                        i += 1;
                    }
                    "--env" => {
                        let (k, v) = parse_env_assignment(&value(a, i, "--env")?).map_err(usage)?;
                        env.insert(k, Value::String(v));
                        i += 2;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(call(
                "cli:workspace:create",
                "workspace.create",
                Obj::new()
                    .opt("cwd", cwd)
                    .put("focus", focus)
                    .opt("label", label)
                    .map("env", env)
                    .done(),
            ))
        }
        "get" | "focus" => {
            let id = one_arg(a, &format!("usage: herdr workspace {sub} <workspace_id>"))?;
            Ok(call(
                &format!("cli:workspace:{sub}"),
                &format!("workspace.{sub}"),
                json!({"workspace_id": id}),
            ))
        }
        "rename" => {
            if a.len() < 2 {
                return Err(usage(
                    "usage: herdr workspace rename <workspace_id> <label>",
                ));
            }
            Ok(call(
                "cli:workspace:rename",
                "workspace.rename",
                json!({"workspace_id": a[0], "label": a[1..].join(" ")}),
            ))
        }
        "report-metadata" => workspace_report_metadata(a),
        "close" => {
            let (id, close_group) = match a {
                [id] => (id, false),
                [id, flag] if flag == "--group" => (id, true),
                _ => {
                    return Err(usage(
                        "usage: herdr workspace close <workspace_id> [--group]",
                    ));
                }
            };
            Ok(call(
                "cli:workspace:close",
                "workspace.close",
                Obj::new()
                    .put("workspace_id", id.as_str())
                    .flag("close_group", close_group)
                    .done(),
            ))
        }
        "help" | "--help" | "-h" => Ok(group(WORKSPACE_HELP, 0)),
        _ => Ok(group(WORKSPACE_HELP, 2)),
    }
}

fn workspace_report_metadata(a: &[String]) -> R {
    let Some(id) = a.first() else {
        return Err(usage(
            "usage: herdr workspace report-metadata <workspace_id> --source ID [--token NAME=VALUE] [--clear-token NAME] [--seq N] [--ttl-ms N]",
        ));
    };
    let (mut source, mut tokens, mut seq, mut ttl) = (None::<String>, Map::new(), None, None);
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--source" => {
                source = Some(value(a, i, "--source")?);
                i += 2;
            }
            "--token" => {
                let (k, v) = parse_token_assignment(&value(a, i, "--token")?).map_err(usage)?;
                tokens.insert(k, v.map(Value::String).unwrap_or(Value::Null));
                i += 2;
            }
            "--clear-token" => {
                tokens.insert(value(a, i, "--clear-token")?, Value::Null);
                i += 2;
            }
            "--seq" => {
                seq =
                    Some(parse_u64_flag("--seq", &value(a, i, "--seq")?).map_err(Parsed::Failed)?);
                i += 2;
            }
            "--ttl-ms" => {
                ttl = Some(
                    parse_u64_flag("--ttl-ms", &value(a, i, "--ttl-ms")?)
                        .map_err(Parsed::Failed)?,
                );
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    let Some(source) = source.filter(|s| !s.trim().is_empty()) else {
        return Err(usage("missing required --source"));
    };
    if tokens.is_empty() {
        return Err(usage("missing token to set or clear"));
    }
    Ok(quiet(
        "workspace.report_metadata",
        Obj::new()
            .put("workspace_id", id.as_str())
            .put("source", source)
            .put("tokens", Value::Object(tokens))
            .opt("seq", seq)
            .opt("ttl_ms", ttl)
            .done(),
    ))
}

// ---- worktree -------------------------------------------------------------------------------

const WORKTREE_HELP: &str = "herdr worktree commands:
  herdr worktree list [--workspace ID | --cwd PATH] [--trust-repository]
  herdr worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--trust-repository]
  herdr worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--trust-repository]
  herdr worktree remove --workspace ID [--force] [--trust-repository]";

/// Herdr's `normalize_path_arg`: `~` and `~/…` against `HOME`, relative paths against the
/// working directory.
fn absolute_path(v: &str, env: &Env) -> Result<String, Parsed> {
    let expanded = match (v, &env.home) {
        ("~", Some(h)) => h.clone(),
        (p, Some(h)) if p.starts_with("~/") => h.join(&p[2..]),
        (p, _) => PathBuf::from(p),
    };
    if expanded.is_absolute() {
        return Ok(expanded.display().to_string());
    }
    match &env.cwd {
        Some(cwd) => Ok(cwd.join(expanded).display().to_string()),
        None => Err(Parsed::Failed(
            "the current directory is not available".into(),
        )),
    }
}

fn worktree(a: &[String], env: &Env) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(WORKTREE_HELP, 2));
    };
    let a = &a[1..];
    let (mut ws, mut cwd, mut branch, mut base, mut path, mut label) =
        (None, None, None, None, None, None);
    let (mut focus, mut trust, mut force) = (false, false, false);
    let allowed: &[&str] = match sub.as_str() {
        "list" => &["--workspace", "--cwd", "--trust-repository", "--json"],
        "create" => &[
            "--workspace",
            "--cwd",
            "--branch",
            "--base",
            "--path",
            "--label",
            "--focus",
            "--no-focus",
            "--trust-repository",
            "--json",
        ],
        "open" => &[
            "--workspace",
            "--cwd",
            "--path",
            "--branch",
            "--label",
            "--focus",
            "--no-focus",
            "--trust-repository",
            "--json",
        ],
        "remove" => &["--workspace", "--force", "--trust-repository", "--json"],
        "help" | "--help" | "-h" => return Ok(group(WORKTREE_HELP, 0)),
        _ => return Ok(group(WORKTREE_HELP, 2)),
    };
    let mut i = 0;
    while i < a.len() {
        let f = a[i].as_str();
        if !allowed.contains(&f) {
            return Err(unknown(f));
        }
        match f {
            "--workspace" => ws = Some(value(a, i, f)?),
            "--cwd" => cwd = Some(absolute_path(&value(a, i, f)?, env)?),
            "--path" => path = Some(absolute_path(&value(a, i, f)?, env)?),
            "--branch" => branch = Some(value(a, i, f)?),
            "--base" => base = Some(value(a, i, f)?),
            "--label" => label = Some(value(a, i, f)?),
            "--focus" => focus = true,
            "--no-focus" => focus = false,
            "--trust-repository" => trust = true,
            "--force" => force = true,
            _ => {} // --json: output is always JSON
        }
        i += if matches!(
            f,
            "--workspace" | "--cwd" | "--path" | "--branch" | "--base" | "--label"
        ) {
            2
        } else {
            1
        };
    }
    match sub.as_str() {
        "list" => {
            if ws.is_some() && cwd.is_some() {
                return Err(usage(
                    "usage: herdr worktree list [--workspace ID | --cwd PATH] [--trust-repository]",
                ));
            }
            Ok(call(
                "cli:worktree:list",
                "worktree.list",
                Obj::new()
                    .opt("workspace_id", ws)
                    .opt("cwd", cwd)
                    .flag("trust_repository", trust)
                    .done(),
            ))
        }
        "create" => {
            if ws.is_some() && cwd.is_some() {
                return Err(usage(
                    "usage: herdr worktree create [--workspace ID | --cwd PATH] [--branch NAME] [--base REF] [--path PATH] [--label TEXT] [--focus] [--no-focus] [--trust-repository]",
                ));
            }
            Ok(call(
                "cli:worktree:create",
                "worktree.create",
                Obj::new()
                    .opt("workspace_id", ws)
                    .opt("cwd", cwd)
                    .opt("branch", branch)
                    .opt("base", base)
                    .opt("path", path)
                    .opt("label", label)
                    .put("focus", focus)
                    .flag("trust_repository", trust)
                    .done(),
            ))
        }
        "open" => {
            const OPEN_USAGE: &str = "usage: herdr worktree open [--workspace ID | --cwd PATH] (--path PATH | --branch NAME) [--label TEXT] [--focus] [--no-focus] [--trust-repository]";
            if (ws.is_some() && cwd.is_some()) || path.is_some() == branch.is_some() {
                return Err(usage(OPEN_USAGE));
            }
            Ok(call(
                "cli:worktree:open",
                "worktree.open",
                Obj::new()
                    .opt("workspace_id", ws)
                    .opt("cwd", cwd)
                    .opt("path", path)
                    .opt("branch", branch)
                    .opt("label", label)
                    .put("focus", focus)
                    .flag("trust_repository", trust)
                    .done(),
            ))
        }
        _ => {
            let Some(ws) = ws else {
                return Err(usage(
                    "usage: herdr worktree remove --workspace ID [--force] [--trust-repository]",
                ));
            };
            Ok(call(
                "cli:worktree:remove",
                "worktree.remove",
                Obj::new()
                    .put("workspace_id", ws)
                    .put("force", force)
                    .flag("trust_repository", trust)
                    .done(),
            ))
        }
    }
}

// ---- tab ------------------------------------------------------------------------------------

const TAB_HELP: &str = "herdr tab commands:
  herdr tab list [--workspace <workspace_id>]
  herdr tab create [--workspace <workspace_id>] [--cwd PATH] [--label TEXT] [--env KEY=VALUE] [--focus] [--no-focus]
  herdr tab get <tab_id>
  herdr tab focus <tab_id>
  herdr tab rename <tab_id> <label>
  herdr tab close <tab_id>";

fn tab(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(TAB_HELP, 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "list" => {
            let mut ws = None;
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--workspace" => {
                        ws = Some(value(a, i, "--workspace")?);
                        i += 2;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(call(
                "cli:tab:list",
                "tab.list",
                Obj::new().opt("workspace_id", ws).done(),
            ))
        }
        "create" => {
            let (mut ws, mut cwd, mut label, mut focus, mut env) =
                (None, None, None, false, Map::new());
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--workspace" => {
                        ws = Some(value(a, i, "--workspace")?);
                        i += 2;
                    }
                    "--cwd" => {
                        cwd = Some(value(a, i, "--cwd")?);
                        i += 2;
                    }
                    "--label" => {
                        label = Some(value(a, i, "--label")?);
                        i += 2;
                    }
                    "--focus" => {
                        focus = true;
                        i += 1;
                    }
                    "--no-focus" => {
                        focus = false;
                        i += 1;
                    }
                    "--env" => {
                        let (k, v) = parse_env_assignment(&value(a, i, "--env")?).map_err(usage)?;
                        env.insert(k, Value::String(v));
                        i += 2;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(call(
                "cli:tab:create",
                "tab.create",
                Obj::new()
                    .opt("workspace_id", ws)
                    .opt("cwd", cwd)
                    .put("focus", focus)
                    .opt("label", label)
                    .map("env", env)
                    .done(),
            ))
        }
        "get" | "focus" | "close" => {
            let id = one_arg(a, &format!("usage: herdr tab {sub} <tab_id>"))?;
            Ok(call(
                &format!("cli:tab:{sub}"),
                &format!("tab.{sub}"),
                json!({"tab_id": id}),
            ))
        }
        "rename" => {
            if a.len() < 2 {
                return Err(usage("usage: herdr tab rename <tab_id> <label>"));
            }
            Ok(call(
                "cli:tab:rename",
                "tab.rename",
                json!({"tab_id": a[0], "label": a[1..].join(" ")}),
            ))
        }
        "help" | "--help" | "-h" => Ok(group(TAB_HELP, 0)),
        _ => Ok(group(TAB_HELP, 2)),
    }
}

// ---- pane -----------------------------------------------------------------------------------

const PANE_HELP: &str = "herdr pane commands:
  herdr pane list [--workspace <workspace_id>]
  herdr pane current [--pane ID|--current]
  herdr pane get <pane_id>
  herdr pane layout [--pane ID|--current]
  herdr pane process-info [--pane ID|--current]
  herdr pane neighbor --direction left|right|up|down [--pane ID|--current]
  herdr pane edges [--pane ID|--current]
  herdr pane focus --direction left|right|up|down [--pane ID|--current]
  herdr pane resize --direction left|right|up|down [--amount FLOAT] [--pane ID|--current]
  herdr pane zoom [<pane_id>|--pane ID|--current] [--toggle|--on|--off]
  herdr pane rename <pane_id> <label>|--clear
  herdr pane read <pane_id> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]
  herdr pane input [<pane_id>|--pane ID|--current] --right-click herdr|pane
  herdr pane split [<pane_id>|--pane ID|--current] --direction right|down [--ratio FLOAT] [--cwd PATH] [--env KEY=VALUE] [--right-click herdr|pane] [--focus] [--no-focus]
  herdr pane swap --direction left|right|up|down [--pane ID|--current]
  herdr pane swap --source-pane ID --target-pane ID
  herdr pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]
  herdr pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]
  herdr pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]
  herdr pane close <pane_id>
  herdr pane send-text <pane_id> <text>
  herdr pane send-keys <pane_id> <key> [key ...]
  herdr pane wait-output <pane_id> (--match TEXT | --regex PATTERN) [--source visible|recent|recent-unwrapped] [--lines N] [--timeout MS] [--raw]
  herdr pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH]
  herdr pane report-agent-session <pane_id> --source ID --agent LABEL [--seq N] [--agent-session-id ID] [--agent-session-path PATH]
  herdr pane release-agent <pane_id> --source ID --agent LABEL [--seq N]
  herdr pane report-metadata <pane_id> --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--state-label STATUS=TEXT] [--clear-state-labels] [--token NAME=VALUE] [--clear-token NAME] [--seq N] [--ttl-ms N]
  herdr pane run <pane_id> <command>";

fn pane(a: &[String], env: &Env) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(PANE_HELP, 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "list" => {
            let mut ws = None;
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--workspace" => {
                        ws = Some(value(a, i, "--workspace")?);
                        i += 2;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(call(
                "cli:pane:list",
                "pane.list",
                Obj::new().opt("workspace_id", ws).done(),
            ))
        }
        "current" => {
            let mut caller = env.pane_id.clone();
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--pane" => {
                        caller = Some(value(a, i, "--pane")?);
                        i += 2;
                    }
                    "--current" => {
                        caller = env.pane_id.clone();
                        i += 1;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(call(
                "cli:pane:current",
                "pane.current",
                Obj::new().opt("caller_pane_id", caller).done(),
            ))
        }
        "get" => {
            let id = one_arg(a, "usage: herdr pane get <pane_id>")?;
            Ok(call("cli:pane:get", "pane.get", json!({"pane_id": id})))
        }
        "layout" | "process-info" | "edges" => {
            let pane = optional_current_pane(a, env)?;
            let (id, method) = match sub.as_str() {
                "layout" => ("cli:pane:layout", "pane.layout"),
                "process-info" => ("cli:pane:process_info", "pane.process_info"),
                _ => ("cli:pane:edges", "pane.edges"),
            };
            Ok(call(id, method, Obj::new().opt("pane_id", pane).done()))
        }
        "neighbor" => {
            let (pane, dir) = neighbor_args(a).map_err(usage)?;
            Ok(call(
                "cli:pane:neighbor",
                "pane.neighbor",
                Obj::new().opt("pane_id", pane).put("direction", dir).done(),
            ))
        }
        "focus" => {
            let (pane, dir) = neighbor_args(a).map_err(|_| {
                usage(
                    "usage: herdr pane focus --direction left|right|up|down [--pane ID|--current]",
                )
            })?;
            Ok(call(
                "cli:pane:focus",
                "pane.focus_direction",
                Obj::new().opt("pane_id", pane).put("direction", dir).done(),
            ))
        }
        "resize" => pane_resize(a),
        "zoom" => pane_zoom(a),
        "read" => pane_read(a),
        "rename" => {
            if a.len() < 2 {
                return Err(usage("usage: herdr pane rename <pane_id> <label>|--clear"));
            }
            let label = if a.len() == 2 && a[1] == "--clear" {
                None
            } else {
                Some(a[1..].join(" "))
            };
            Ok(call(
                "cli:pane:rename",
                "pane.rename",
                Obj::new()
                    .put("pane_id", a[0].as_str())
                    .opt("label", label)
                    .done(),
            ))
        }
        "input" => pane_input(a, env),
        "split" => pane_split(a, env),
        "swap" => pane_swap(a),
        "move" => pane_move(a),
        "close" => {
            let id = one_arg(a, "usage: herdr pane close <pane_id>")?;
            Ok(call("cli:pane:close", "pane.close", json!({"pane_id": id})))
        }
        "send-text" => {
            if a.len() < 2 {
                return Err(usage("usage: herdr pane send-text <pane_id> <text>"));
            }
            Ok(quiet(
                "pane.send_text",
                json!({"pane_id": a[0], "text": a[1..].join(" ")}),
            ))
        }
        "send-keys" => {
            if a.len() < 2 {
                return Err(usage(
                    "usage: herdr pane send-keys <pane_id> <key> [key ...]",
                ));
            }
            Ok(quiet(
                "pane.send_keys",
                json!({"pane_id": a[0], "keys": a[1..]}),
            ))
        }
        "run" => {
            if a.len() < 2 {
                return Err(usage("usage: herdr pane run <pane_id> <command>"));
            }
            // Herdr's CLI types the command and presses Enter (`pane.send_input`).
            let text = a[1..].join(" ");
            Ok(quiet(
                "pane.send_input",
                Obj::new()
                    .put("pane_id", a[0].as_str())
                    .opt("text", (!text.is_empty()).then_some(text))
                    .put("keys", json!(["Enter"]))
                    .done(),
            ))
        }
        "wait-output" => pane_wait_output(a),
        "report-agent" => pane_report_agent(a, true),
        "report-agent-session" => pane_report_agent(a, false),
        "release-agent" => pane_release_agent(a),
        "report-metadata" => pane_report_metadata(a),
        "help" | "--help" | "-h" => Ok(group(PANE_HELP, 0)),
        _ => Ok(group(PANE_HELP, 2)),
    }
}

/// `[--pane ID | --current]`; `--current` is the caller's pane, if any.
fn optional_current_pane(a: &[String], env: &Env) -> Result<Option<String>, Parsed> {
    let mut pane = None;
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                pane = Some(value(a, i, "--pane")?);
                i += 2;
            }
            "--current" => {
                pane = env.pane_id.clone();
                i += 1;
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(pane)
}

/// `--direction D [--pane ID | --current]` (`neighbor`, `focus`); `--current` leaves the pane
/// to the server, as Herdr does.
fn neighbor_args(a: &[String]) -> Result<(Option<String>, &'static str), String> {
    let (mut pane, mut dir) = (None, None);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                pane = Some(a.get(i + 1).cloned().ok_or("missing value for --pane")?);
                i += 2;
            }
            "--current" => {
                pane = None;
                i += 1;
            }
            "--direction" => {
                let v = a.get(i + 1).ok_or("missing value for --direction")?;
                dir = Some(parse_pane_direction(v)?);
                i += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    let dir = dir
        .ok_or("usage: herdr pane neighbor --direction left|right|up|down [--pane ID|--current]")?;
    Ok((pane, dir))
}

fn pane_resize(a: &[String]) -> R {
    let (mut pane, mut dir, mut amount) = (None, None, None);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                pane = Some(value(a, i, "--pane")?);
                i += 2;
            }
            "--current" => {
                pane = None;
                i += 1;
            }
            "--direction" => {
                dir = Some(parse_pane_direction(&value(a, i, "--direction")?).map_err(usage)?);
                i += 2;
            }
            "--amount" => {
                let v = value(a, i, "--amount")?;
                amount = Some(finite_f32(&v).ok_or_else(|| usage(format!("invalid amount: {v}")))?);
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    let Some(dir) = dir else {
        return Err(usage(
            "usage: herdr pane resize --direction left|right|up|down [--amount FLOAT] [--pane ID|--current]",
        ));
    };
    Ok(call(
        "cli:pane:resize",
        "pane.resize",
        Obj::new()
            .opt("pane_id", pane)
            .put("direction", dir)
            .opt("amount", amount)
            .done(),
    ))
}

fn pane_zoom(a: &[String]) -> R {
    let (mut pane, mut mode, mut seen) = (None, "toggle", false);
    let mut i = 0;
    if a.first().is_some_and(|x| !x.starts_with("--")) {
        pane = Some(a[0].clone());
        i = 1;
    }
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                pane = Some(value(a, i, "--pane")?);
                i += 2;
            }
            "--current" => {
                pane = None;
                i += 1;
            }
            m @ ("--toggle" | "--on" | "--off") => {
                if seen {
                    return Err(usage("provide only one of --toggle, --on, or --off"));
                }
                mode = &m[2..];
                seen = true;
                i += 1;
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(call(
        "cli:pane:zoom",
        "pane.zoom",
        Obj::new().opt("pane_id", pane).put("mode", mode).done(),
    ))
}

fn pane_read(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr pane read <pane_id> [--source visible|recent|recent-unwrapped|detection] [--lines N] [--format text|ansi] [--ansi] [--raw]";
    let a = expand_equals(a, &["--source", "--lines", "--format"]);
    let (mut pane, mut source, mut lines, mut format, mut strip) =
        (None::<String>, "recent", None, "text", true);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--source" => {
                source = parse_read_source(&value(&a, i, "--source")?).map_err(usage)?;
                i += 2;
            }
            "--lines" => {
                lines = Some(parse_u32_flag("--lines", &value(&a, i, "--lines")?).map_err(usage)?);
                i += 2;
            }
            "--format" => {
                format = parse_read_format(&value(&a, i, "--format")?).map_err(usage)?;
                i += 2;
            }
            "--ansi" => {
                format = "ansi";
                i += 1;
            }
            "--raw" => {
                format = "ansi";
                strip = false;
                i += 1;
            }
            o if o.starts_with('-') => return Err(unknown(o)),
            p => {
                if pane.is_some() {
                    return Err(usage(format!("unexpected argument: {p}")));
                }
                pane = Some(p.to_string());
                i += 1;
            }
        }
    }
    let Some(pane) = pane else {
        return Err(usage(USAGE));
    };
    Ok(Parsed::Call {
        id: "cli:pane:read".into(),
        method: "pane.read".into(),
        params: Obj::new()
            .put("pane_id", pane)
            .put("source", source)
            .opt("lines", lines)
            .put("format", format)
            .put("strip_ansi", strip)
            .done(),
        output: Output::ReadText,
    })
}

fn pane_input(a: &[String], env: &Env) -> R {
    const USAGE: &str =
        "usage: herdr pane input [<pane_id>|--pane ID|--current] --right-click herdr|pane";
    let a = expand_equals(a, &["--pane", "--right-click"]);
    let (mut pane, mut right_click) = (None::<String>, None);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                if pane.is_some() {
                    return Err(usage("provide only one pane selector"));
                }
                pane = Some(value(&a, i, "--pane")?);
                i += 2;
            }
            "--current" => {
                if pane.is_some() {
                    return Err(usage("provide only one pane selector"));
                }
                pane = Some(
                    env.pane_id
                        .clone()
                        .ok_or_else(|| usage("--current requires HERDR_PANE_ID"))?,
                );
                i += 1;
            }
            "--right-click" => {
                right_click =
                    Some(parse_right_click(&value(&a, i, "--right-click")?).map_err(usage)?);
                i += 2;
            }
            o if o.starts_with('-') => return Err(unknown(o)),
            p => {
                if pane.is_some() {
                    return Err(usage(format!("unexpected argument: {p}")));
                }
                pane = Some(p.to_string());
                i += 1;
            }
        }
    }
    let (Some(pane), Some(right_click)) = (pane, right_click) else {
        return Err(usage(USAGE));
    };
    Ok(call(
        "cli:pane:input:set",
        "pane.input.set",
        json!({"pane_id": pane, "right_click": right_click}),
    ))
}

fn pane_split(a: &[String], env: &Env) -> R {
    let a = expand_equals(a, &["--right-click"]);
    let mut pane = env.pane_id.clone();
    let (mut dir, mut ratio, mut cwd, mut focus, mut right_click, mut penv) =
        (None, None, None, false, "herdr", Map::new());
    let mut i = 0;
    if a.first().is_some_and(|x| !x.starts_with("--")) {
        pane = Some(a[0].clone());
        i = 1;
    }
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                pane = Some(value(&a, i, "--pane")?);
                i += 2;
            }
            "--current" => {
                pane = Some(
                    env.pane_id
                        .clone()
                        .ok_or_else(|| usage("--current requires HERDR_PANE_ID"))?,
                );
                i += 1;
            }
            "--direction" => {
                let v = value(&a, i, "--direction")?;
                dir = Some(match v.as_str() {
                    "right" => "right",
                    "down" => "down",
                    _ => return Err(usage(format!("invalid split direction: {v}"))),
                });
                i += 2;
            }
            "--ratio" => {
                let v = value(&a, i, "--ratio")?;
                ratio = Some(finite_f32(&v).ok_or_else(|| usage(format!("invalid ratio: {v}")))?);
                i += 2;
            }
            "--cwd" => {
                cwd = Some(value(&a, i, "--cwd")?);
                i += 2;
            }
            "--right-click" => {
                right_click = parse_right_click(&value(&a, i, "--right-click")?).map_err(usage)?;
                i += 2;
            }
            "--focus" => {
                focus = true;
                i += 1;
            }
            "--no-focus" => {
                focus = false;
                i += 1;
            }
            "--env" => {
                let (k, v) = parse_env_assignment(&value(&a, i, "--env")?).map_err(usage)?;
                penv.insert(k, Value::String(v));
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    let Some(dir) = dir else {
        return Err(usage(
            "usage: herdr pane split [<pane_id>|--pane ID|--current] --direction right|down [--ratio FLOAT] [--cwd PATH] [--env KEY=VALUE] [--right-click herdr|pane] [--focus] [--no-focus]",
        ));
    };
    Ok(call(
        "cli:pane:split",
        "pane.split",
        Obj::new()
            .opt("target_pane_id", pane)
            .put("direction", dir)
            .opt("ratio", ratio)
            .opt("cwd", cwd)
            .put("focus", focus)
            .put("right_click", right_click)
            .map("env", penv)
            .done(),
    ))
}

fn pane_swap(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr pane swap --direction left|right|up|down [--pane ID|--current]\n       herdr pane swap --source-pane ID --target-pane ID";
    let (mut pane, mut dir, mut src, mut dst) = (None, None, None, None);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--pane" => {
                pane = Some(value(a, i, "--pane")?);
                i += 2;
            }
            "--current" => {
                pane = None;
                i += 1;
            }
            "--direction" => {
                dir = Some(parse_pane_direction(&value(a, i, "--direction")?).map_err(usage)?);
                i += 2;
            }
            "--source-pane" => {
                src = Some(value(a, i, "--source-pane")?);
                i += 2;
            }
            "--target-pane" => {
                dst = Some(value(a, i, "--target-pane")?);
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    let params = match (dir, src.is_some() || dst.is_some()) {
        (Some(d), false) => Obj::new().opt("pane_id", pane).put("direction", d).done(),
        (None, true) if pane.is_none() && src.is_some() && dst.is_some() => Obj::new()
            .opt("source_pane_id", src)
            .opt("target_pane_id", dst)
            .done(),
        _ => return Err(usage(USAGE)),
    };
    Ok(call("cli:pane:swap", "pane.swap", params))
}

fn pane_move(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr pane move <pane_id> --tab <tab_id> --split right|down [--target-pane ID] [--ratio FLOAT] [--focus|--no-focus]\n       herdr pane move <pane_id> --new-tab [--workspace ID] [--label TEXT] [--focus|--no-focus]\n       herdr pane move <pane_id> --new-workspace [--label TEXT] [--tab-label TEXT] [--focus|--no-focus]";
    let Some(id) = a.first().filter(|x| !x.starts_with('-')) else {
        return Err(usage(USAGE));
    };
    let (mut tab, mut new_tab, mut new_ws, mut ws, mut target, mut split, mut ratio) =
        (None, false, false, None, None, None, None);
    let (mut label, mut tab_label, mut focus) = (None, None, true);
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--tab" => {
                tab = Some(value(a, i, "--tab")?);
                i += 2;
            }
            "--new-tab" => {
                new_tab = true;
                i += 1;
            }
            "--new-workspace" => {
                new_ws = true;
                i += 1;
            }
            "--workspace" => {
                ws = Some(value(a, i, "--workspace")?);
                i += 2;
            }
            "--target-pane" => {
                target = Some(value(a, i, "--target-pane")?);
                i += 2;
            }
            "--split" => {
                let v = value(a, i, "--split")?;
                split = Some(match v.as_str() {
                    "right" => "right",
                    "down" => "down",
                    _ => {
                        return Err(usage(format!(
                            "invalid split direction: {v} (expected right or down)"
                        )));
                    }
                });
                i += 2;
            }
            "--ratio" => {
                let v = value(a, i, "--ratio")?;
                ratio = Some(finite_f32(&v).ok_or_else(|| usage(format!("invalid ratio: {v}")))?);
                i += 2;
            }
            "--label" => {
                label = Some(value(a, i, "--label")?);
                i += 2;
            }
            "--tab-label" => {
                tab_label = Some(value(a, i, "--tab-label")?);
                i += 2;
            }
            "--focus" => {
                focus = true;
                i += 1;
            }
            "--no-focus" => {
                focus = false;
                i += 1;
            }
            other => return Err(unknown(other)),
        }
    }
    let destinations = usize::from(tab.is_some()) + usize::from(new_tab) + usize::from(new_ws);
    if destinations != 1 {
        return Err(usage(USAGE));
    }
    let destination = if let Some(tab) = tab {
        let Some(split) = split else {
            return Err(usage(USAGE));
        };
        if ws.is_some() || label.is_some() || tab_label.is_some() {
            return Err(usage(USAGE));
        }
        Obj::new()
            .put("type", "tab")
            .put("tab_id", tab)
            .opt("target_pane_id", target)
            .put("split", split)
            .opt("ratio", ratio)
            .done()
    } else if new_tab {
        if split.is_some() || target.is_some() || tab_label.is_some() {
            return Err(usage(USAGE));
        }
        Obj::new()
            .put("type", "new_tab")
            .opt("workspace_id", ws)
            .opt("label", label)
            .done()
    } else {
        if split.is_some() || target.is_some() || ws.is_some() {
            return Err(usage(USAGE));
        }
        Obj::new()
            .put("type", "new_workspace")
            .opt("label", label)
            .opt("tab_label", tab_label)
            .done()
    };
    Ok(call(
        "cli:pane:move",
        "pane.move",
        json!({"pane_id": id, "destination": destination, "focus": focus}),
    ))
}

fn pane_wait_output(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr pane wait-output <pane_id> (--match TEXT | --regex PATTERN) [--source visible|recent|recent-unwrapped] [--lines N] [--timeout MS] [--raw]";
    let a = expand_equals(
        a,
        &["--match", "--regex", "--source", "--lines", "--timeout"],
    );
    let (mut pane, mut source, mut lines, mut timeout, mut strip, mut matcher) =
        (None::<String>, "recent", None, None, true, None);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            o @ ("--match" | "--regex") => {
                let v = value(&a, i, o)?;
                if matcher.is_some() {
                    return Err(usage("--match and --regex are mutually exclusive"));
                }
                let kind = if o == "--regex" { "regex" } else { "substring" };
                matcher = Some(json!({"type": kind, "value": v}));
                i += 2;
            }
            "--source" => {
                source = parse_read_source(&value(&a, i, "--source")?).map_err(usage)?;
                i += 2;
            }
            "--lines" => {
                lines = Some(parse_u32_flag("--lines", &value(&a, i, "--lines")?).map_err(usage)?);
                i += 2;
            }
            "--timeout" => {
                timeout =
                    Some(parse_u64_flag("--timeout", &value(&a, i, "--timeout")?).map_err(usage)?);
                i += 2;
            }
            "--raw" => {
                strip = false;
                i += 1;
            }
            o if o.starts_with('-') => return Err(unknown(o)),
            p => {
                if pane.is_some() {
                    return Err(usage(format!("unexpected argument: {p}")));
                }
                pane = Some(p.to_string());
                i += 1;
            }
        }
    }
    let Some(pane) = pane else {
        return Err(usage(USAGE));
    };
    let Some(matcher) = matcher else {
        return Err(usage("missing required --match or --regex"));
    };
    Ok(call(
        "cli:pane:wait-output",
        "pane.wait_for_output",
        Obj::new()
            .put("pane_id", pane)
            .put("source", source)
            .opt("lines", lines)
            .put("match", matcher)
            .opt("timeout_ms", timeout)
            .put("strip_ansi", strip)
            .done(),
    ))
}

/// `pane report-agent` (`with_state`) and `pane report-agent-session`; everything after `--`
/// is the resume command.
fn pane_report_agent(a: &[String], with_state: bool) -> R {
    let usage_line = if with_state {
        "usage: herdr pane report-agent <pane_id> --source ID --agent LABEL --state idle|working|blocked|unknown [--message TEXT] [--seq N] [--agent-session-id ID] [--agent-session-path PATH] [-- <resume-command...>]"
    } else {
        "usage: herdr pane report-agent-session <pane_id> --source ID --agent LABEL [--seq N] [--agent-session-id ID] [--agent-session-path PATH] [--session-start-source SOURCE] [-- <resume-command...>]"
    };
    let (a, resume) = match a.iter().position(|x| x == "--") {
        Some(s) => (&a[..s], Some(a[s + 1..].to_vec())),
        None => (a, None),
    };
    let text_flags: &[&str] = if with_state {
        &[
            "--source",
            "--agent",
            "--message",
            "--agent-session-id",
            "--agent-session-path",
        ]
    } else {
        &[
            "--source",
            "--agent",
            "--agent-session-id",
            "--agent-session-path",
            "--session-start-source",
        ]
    };
    let mut equals: Vec<&str> = text_flags.to_vec();
    equals.push("--seq");
    if with_state {
        equals.push("--state");
    }
    let a = expand_equals(a, &equals);
    let mut texts: Map<String, Value> = Map::new();
    let (mut pane, mut state, mut seq) = (None::<String>, None, None);
    let mut i = 0;
    while i < a.len() {
        let f = a[i].as_str();
        if text_flags.contains(&f) {
            texts.insert(f.to_string(), Value::String(value(&a, i, f)?));
            i += 2;
            continue;
        }
        match f {
            "--state" if with_state => {
                state = Some(
                    parse_pane_agent_state(&value(&a, i, "--state")?).map_err(Parsed::Failed)?,
                );
                i += 2;
            }
            "--seq" => {
                seq =
                    Some(parse_u64_flag("--seq", &value(&a, i, "--seq")?).map_err(Parsed::Failed)?);
                i += 2;
            }
            o if o.starts_with('-') => return Err(unknown(o)),
            p => {
                if pane.is_some() {
                    return Err(usage(format!("unexpected argument: {p}")));
                }
                pane = Some(p.to_string());
                i += 1;
            }
        }
    }
    let Some(pane) = pane else {
        return Err(usage(usage_line));
    };
    let text = |k: &str| texts.get(k).and_then(Value::as_str).map(str::to_string);
    let Some(source) = text("--source")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Err(usage("missing required --source"));
    };
    let Some(agent) = text("--agent") else {
        return Err(usage("missing required --agent"));
    };
    let mut p = Obj::new()
        .put("pane_id", pane)
        .put("source", source)
        .put("agent", agent);
    if with_state {
        let Some(state) = state else {
            return Err(usage("missing required --state"));
        };
        p = p.put("state", state).opt("message", text("--message"));
    }
    p = p
        .opt("seq", seq)
        .opt("agent_session_id", text("--agent-session-id"))
        .opt("agent_session_path", text("--agent-session-path"));
    if !with_state {
        p = p.opt("session_start_source", text("--session-start-source"));
    }
    let p = p.opt("resume_argv", resume).done();
    Ok(quiet(
        if with_state {
            "pane.report_agent"
        } else {
            "pane.report_agent_session"
        },
        p,
    ))
}

fn pane_release_agent(a: &[String]) -> R {
    let Some(pane) = a.first() else {
        return Err(usage(
            "usage: herdr pane release-agent <pane_id> --source ID --agent LABEL [--seq N]",
        ));
    };
    let (mut source, mut agent, mut seq) = (None::<String>, None, None);
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--source" => {
                source = Some(value(a, i, "--source")?);
                i += 2;
            }
            "--agent" => {
                agent = Some(value(a, i, "--agent")?);
                i += 2;
            }
            "--seq" => {
                seq =
                    Some(parse_u64_flag("--seq", &value(a, i, "--seq")?).map_err(Parsed::Failed)?);
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    let Some(source) = source
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Err(usage("missing required --source"));
    };
    let Some(agent) = agent else {
        return Err(usage("missing required --agent"));
    };
    Ok(quiet(
        "pane.release_agent",
        Obj::new()
            .put("pane_id", pane.as_str())
            .put("source", source)
            .put("agent", agent)
            .opt("seq", seq)
            .done(),
    ))
}

fn pane_report_metadata(a: &[String]) -> R {
    let Some(pane) = a.first() else {
        return Err(usage(
            "usage: herdr pane report-metadata <pane_id> --source ID [--agent LABEL] [--applies-to-source ID] [--title TEXT|--clear-title] [--display-agent TEXT|--clear-display-agent] [--state-label STATUS=TEXT] [--clear-state-labels] [--token NAME=VALUE] [--clear-token NAME] [--seq N] [--ttl-ms N]",
        ));
    };
    let (mut source, mut agent, mut applies, mut title, mut display) =
        (None::<String>, None, None::<String>, None, None);
    let (mut labels, mut tokens) = (Map::new(), Map::new());
    let (mut clear_title, mut clear_display, mut clear_labels) = (false, false, false);
    let (mut seq, mut ttl) = (None, None);
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--source" => {
                source = Some(value(a, i, "--source")?);
                i += 2;
            }
            "--agent" => {
                agent = Some(value(a, i, "--agent")?);
                i += 2;
            }
            "--applies-to-source" => {
                applies = Some(value(a, i, "--applies-to-source")?);
                i += 2;
            }
            "--title" => {
                title = Some(value(a, i, "--title")?);
                i += 2;
            }
            "--clear-title" => {
                clear_title = true;
                i += 1;
            }
            "--display-agent" => {
                display = Some(value(a, i, "--display-agent")?);
                i += 2;
            }
            "--clear-display-agent" => {
                clear_display = true;
                i += 1;
            }
            "--state-label" => {
                let v = value(a, i, "--state-label")?;
                let Some((status, label)) = v.split_once('=') else {
                    return Err(usage("expected --state-label STATUS=TEXT"));
                };
                let status = status.trim().to_ascii_lowercase();
                if !matches!(
                    status.as_str(),
                    "idle" | "working" | "blocked" | "done" | "unknown"
                ) {
                    return Err(usage(format!("unknown state label: {status}")));
                }
                labels.insert(status, Value::String(label.to_string()));
                i += 2;
            }
            "--clear-state-labels" => {
                clear_labels = true;
                i += 1;
            }
            "--token" => {
                let (k, v) = parse_token_assignment(&value(a, i, "--token")?).map_err(usage)?;
                tokens.insert(k, v.map(Value::String).unwrap_or(Value::Null));
                i += 2;
            }
            "--clear-token" => {
                tokens.insert(value(a, i, "--clear-token")?, Value::Null);
                i += 2;
            }
            "--seq" => {
                seq =
                    Some(parse_u64_flag("--seq", &value(a, i, "--seq")?).map_err(Parsed::Failed)?);
                i += 2;
            }
            "--ttl-ms" => {
                ttl = Some(
                    parse_u64_flag("--ttl-ms", &value(a, i, "--ttl-ms")?)
                        .map_err(Parsed::Failed)?,
                );
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    let Some(source) = source
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Err(usage("missing required --source"));
    };
    if applies.as_deref().is_some_and(|s| s.trim().is_empty()) {
        return Err(usage("missing value for --applies-to-source"));
    }
    if (title.is_some() && clear_title)
        || (display.is_some() && clear_display)
        || (!labels.is_empty() && clear_labels)
    {
        return Err(usage("cannot set and clear the same metadata field"));
    }
    if title.is_none()
        && display.is_none()
        && labels.is_empty()
        && tokens.is_empty()
        && !clear_title
        && !clear_display
        && !clear_labels
    {
        return Err(usage("missing metadata field to set or clear"));
    }
    Ok(quiet(
        "pane.report_metadata",
        Obj::new()
            .put("pane_id", pane.as_str())
            .put("source", source)
            .opt("agent", agent)
            .opt("applies_to_source", applies)
            .opt("title", title)
            .opt("display_agent", display)
            .map("state_labels", labels)
            .map("tokens", tokens)
            .put("clear_title", clear_title)
            .put("clear_display_agent", clear_display)
            .put("clear_state_labels", clear_labels)
            .opt("seq", seq)
            .opt("ttl_ms", ttl)
            .done(),
    ))
}

// ---- agent ----------------------------------------------------------------------------------

fn agent_help() -> String {
    format!(
        "herdr agent commands:
  herdr agent list
  herdr agent get <target>
  herdr agent read <target> [--source visible|recent|recent-unwrapped|detection] [--lines N] [--format text|ansi] [--ansi]
  herdr agent send-keys <target> <key> [key ...]
  herdr agent prompt <target> <text> [--wait] [--until STATUS]... [--timeout MS]
  herdr agent rename <target> <name>|--clear
  herdr agent focus <target>
  herdr agent wait <target> [--until STATUS]... [--timeout MS]
  herdr agent attach <target> [--takeover]
  herdr agent start <name> --kind KIND --pane ID [--timeout MS] [-- <agent-args...>]
  herdr agent explain <target> [--json|--format text|json] [--verbose]
  herdr agent explain --file PATH --agent LABEL [--json|--format text|json] [--verbose]
  targets accept unique agent names and pane ids that currently host agents
  kinds: {}",
        agent_kinds()
    )
}

fn agent(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(&agent_help(), 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "list" => {
            if !a.is_empty() {
                return Err(usage("usage: herdr agent list"));
            }
            Ok(call("cli:agent:list", "agent.list", json!({})))
        }
        "get" | "focus" => {
            let target = one_arg(a, &format!("usage: herdr agent {sub} <target>"))?;
            Ok(call(
                &format!("cli:agent:{sub}"),
                &format!("agent.{sub}"),
                json!({"target": target}),
            ))
        }
        "read" => agent_read(a),
        "send-keys" => {
            if a.len() < 2 {
                return Err(usage(
                    "usage: herdr agent send-keys <target> <key> [key ...]",
                ));
            }
            Ok(call(
                "cli:agent:send-keys",
                "agent.send_keys",
                json!({"target": a[0], "keys": a[1..]}),
            ))
        }
        "prompt" => agent_prompt(a),
        "rename" => {
            let [target, v] = a else {
                return Err(usage("usage: herdr agent rename <target> <name>|--clear"));
            };
            let name = (v != "--clear").then(|| v.clone());
            Ok(call(
                "cli:agent:rename",
                "agent.rename",
                Obj::new()
                    .put("target", target.as_str())
                    .opt("name", name)
                    .done(),
            ))
        }
        "wait" => agent_wait(a),
        "attach" => Ok(refused(
            "agent attach",
            "attaching a terminal to an agent needs Herdr's client; use `vibeke` to open the pane",
        )),
        "start" => agent_start(a),
        "explain" => agent_explain(a),
        "help" | "--help" | "-h" => Ok(group(&agent_help(), 0)),
        _ => Ok(group(&agent_help(), 2)),
    }
}

fn agent_read(a: &[String]) -> R {
    let Some(target) = a.first() else {
        return Err(usage(
            "usage: herdr agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]",
        ));
    };
    let (mut source, mut lines, mut format, mut strip) = ("recent", None, "text", true);
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--source" => {
                source = parse_read_source(&value(a, i, "--source")?).map_err(Parsed::Failed)?;
                i += 2;
            }
            "--lines" => {
                lines = Some(
                    parse_u32_flag("--lines", &value(a, i, "--lines")?).map_err(Parsed::Failed)?,
                );
                i += 2;
            }
            "--format" => {
                format = parse_read_format(&value(a, i, "--format")?).map_err(Parsed::Failed)?;
                strip = format != "ansi";
                i += 2;
            }
            "--ansi" => {
                format = "ansi";
                strip = false;
                i += 1;
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(Parsed::Call {
        id: "cli:agent:read".into(),
        method: "agent.read".into(),
        params: Obj::new()
            .put("target", target.as_str())
            .put("source", source)
            .opt("lines", lines)
            .put("format", format)
            .put("strip_ansi", strip)
            .done(),
        output: Output::ReadText,
    })
}

/// `--until STATUS` and `--timeout MS` of `agent wait` / `agent prompt`.
fn until_or_timeout(
    a: &[String],
    i: usize,
    until: &mut Vec<Value>,
    timeout: &mut Option<u64>,
) -> Result<bool, Parsed> {
    match a[i].as_str() {
        "--until" => {
            let v = a
                .get(i + 1)
                .ok_or_else(|| usage("--until requires at least one status"))?;
            until.push(Value::String(parse_agent_status(v).map_err(usage)?.into()));
            Ok(true)
        }
        "--timeout" => {
            let v = value(a, i, "--timeout")?;
            *timeout = Some(parse_u64_flag("--timeout", &v).map_err(usage)?);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn agent_wait(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr agent wait <target> [--until STATUS]... [--timeout MS]";
    let Some(target) = a.first() else {
        return Err(usage(USAGE));
    };
    let (mut until, mut timeout) = (Vec::new(), None);
    let mut i = 1;
    while i < a.len() {
        if until_or_timeout(a, i, &mut until, &mut timeout)? {
            i += 2;
            continue;
        }
        match a[i].as_str() {
            "help" | "--help" | "-h" => return Err(group(USAGE, 0)),
            other => return Err(unknown(other)),
        }
    }
    let mut p = Obj::new().put("target", target.as_str());
    if !until.is_empty() {
        p = p.put("until", Value::Array(until));
    }
    Ok(call(
        "cli:agent:wait",
        "agent.wait",
        p.opt("timeout_ms", timeout).done(),
    ))
}

fn agent_prompt(a: &[String]) -> R {
    let Some(target) = a.first() else {
        return Err(usage(
            "usage: herdr agent prompt <target> <text> [--wait] [--until STATUS]... [--timeout MS]",
        ));
    };
    let Some(text) = a.get(1) else {
        return Err(usage("agent prompt requires text"));
    };
    let (mut wait, mut until, mut timeout) = (false, Vec::new(), None);
    let mut i = 2;
    while i < a.len() {
        if until_or_timeout(a, i, &mut until, &mut timeout)? {
            i += 2;
            continue;
        }
        match a[i].as_str() {
            "--wait" => {
                wait = true;
                i += 1;
            }
            other => return Err(unknown(other)),
        }
    }
    if !until.is_empty() && !wait {
        return Err(usage("--until requires --wait"));
    }
    if timeout.is_some() && !wait {
        return Err(usage("--timeout requires --wait"));
    }
    let wait = wait.then(|| {
        let mut w = Obj::new();
        if !until.is_empty() {
            w = w.put("until", Value::Array(until));
        }
        w.opt("timeout_ms", timeout).done()
    });
    Ok(call(
        "cli:agent:prompt",
        "agent.prompt",
        Obj::new()
            .put("target", target.as_str())
            .put("text", text.as_str())
            .opt("wait", wait)
            .done(),
    ))
}

fn agent_start(a: &[String]) -> R {
    let Some(name) = a.first() else {
        return Err(usage(
            "usage: herdr agent start <name> --kind KIND --pane ID [--timeout MS] [-- <agent-args...>]",
        ));
    };
    let sep = a.iter().position(|x| x == "--").unwrap_or(a.len());
    let (mut kind, mut pane, mut timeout) = (None, None, None);
    let mut i = 1;
    while i < sep {
        let f = a[i].as_str();
        let v = || {
            a.get(i + 1)
                .filter(|_| i + 1 < sep)
                .cloned()
                .ok_or_else(|| usage(format!("missing value for {f}")))
        };
        match f {
            "--kind" => kind = Some(v()?),
            "--pane" => pane = Some(v()?),
            "--timeout" => {
                timeout = Some(parse_u64_flag("--timeout", &v()?).map_err(usage)?);
            }
            other => return Err(unknown(other)),
        }
        i += 2;
    }
    let Some(kind) = kind else {
        return Err(usage("missing required --kind"));
    };
    let Some(pane) = pane else {
        return Err(usage("missing required --pane"));
    };
    let args: Vec<String> = a.get(sep + 1..).map(<[String]>::to_vec).unwrap_or_default();
    let mut p = Obj::new()
        .put("name", name.as_str())
        .put("kind", kind)
        .put("pane_id", pane);
    if !args.is_empty() {
        p = p.put("args", json!(args));
    }
    Ok(call(
        "cli:agent:start",
        "agent.start",
        p.opt("timeout_ms", timeout).done(),
    ))
}

fn agent_explain(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr agent explain <target> [--json]\nusage: herdr agent explain --file PATH --agent LABEL [--json]";
    let (mut file, mut agent, mut json_out, mut verbose, mut target) =
        (false, false, false, false, None::<String>);
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--file" => {
                value(a, i, "--file")?;
                file = true;
                i += 2;
            }
            "--agent" => {
                value(a, i, "--agent")?;
                agent = true;
                i += 2;
            }
            "--json" => {
                json_out = true;
                i += 1;
            }
            "--format" => {
                match value(a, i, "--format")?.as_str() {
                    "json" => json_out = true,
                    "text" => json_out = false,
                    other => {
                        return Err(usage(format!(
                            "invalid --format: {other} (expected text or json)"
                        )));
                    }
                }
                i += 2;
            }
            "--verbose" | "-v" => {
                verbose = true;
                i += 1;
            }
            "help" | "--help" | "-h" => {
                return Err(group(
                    "usage: herdr agent explain <target> [--json|--verbose]\nusage: herdr agent explain --file PATH --agent LABEL [--json|--verbose]",
                    0,
                ));
            }
            o if o.starts_with('-') => return Err(unknown(o)),
            t => {
                if target.is_some() {
                    return Err(usage("usage: herdr agent explain <target> [--json]"));
                }
                target = Some(t.to_string());
                i += 1;
            }
        }
    }
    if file {
        return Ok(refused(
            "agent explain --file",
            "explaining a saved screen needs Herdr's detection manifests; explain a live agent instead",
        ));
    }
    let Some(target) = target else {
        return Err(usage(USAGE));
    };
    if agent {
        return Err(usage("--agent is only valid with --file"));
    }
    Ok(Parsed::Call {
        id: "cli:agent:explain".into(),
        method: "agent.explain".into(),
        params: json!({"target": target}),
        output: Output::Explain {
            json: json_out,
            verbose,
        },
    })
}

/// Herdr's text summary of `agent explain` (`print_agent_explain_text`).
pub fn explain_text(explain: &Value, verbose: bool) -> String {
    let s = |v: &Value, d: &'static str| v.as_str().map(str::to_string).unwrap_or(d.into());
    let mut out = String::new();
    let mut line = |l: String| {
        out.push_str(&l);
        out.push('\n');
    };
    line(format!("agent: {}", s(&explain["agent"], "unknown")));
    line(format!("state: {}", s(&explain["state"], "unknown")));
    line(format!(
        "manifest: {} {}",
        s(&explain["manifest_source"], "none"),
        s(&explain["manifest_version"], "unknown")
    ));
    if let Some(rule) = explain["matched_rule"].as_object() {
        let rule_id = rule.get("id").and_then(Value::as_str).unwrap_or("-");
        line(format!(
            "rule: {} (region={} priority={})",
            rule_id,
            rule.get("region").and_then(Value::as_str).unwrap_or("-"),
            rule.get("priority").and_then(Value::as_i64).unwrap_or(0),
        ));
        let preview = explain["evaluated_rules"].as_array().and_then(|rules| {
            rules
                .iter()
                .find(|r| r["id"].as_str() == Some(rule_id))
                .and_then(|r| r["evidence"]["region_preview"].as_str())
                .filter(|p| !p.is_empty())
        });
        if let Some(p) = preview {
            line(format!("evidence: {p:?}"));
        }
    } else {
        line("rule: none".into());
    }
    for k in [
        "fallback_reason",
        "screen_detection_skip_reason",
        "skipped_update_reason",
        "warning",
    ] {
        if let Some(v) = explain[k].as_str() {
            line(format!("{k}: {v}"));
        }
    }
    if !verbose {
        return out;
    }
    let b = |k: &str| explain[k].as_bool().unwrap_or(false);
    line(format!(
        "visible: idle={} blocker={} working={}",
        b("visible_idle"),
        b("visible_blocker"),
        b("visible_working")
    ));
    line(format!(
        "cached_remote_version: {}",
        s(&explain["cached_remote_version"], "none")
    ));
    line(format!(
        "local_override_shadowing_remote: {}",
        b("local_override_shadowing_remote")
    ));
    for k in ["remote_update_status", "remote_update_error"] {
        if let Some(v) = explain[k].as_str() {
            line(format!("{k}: {v}"));
        }
    }
    if let Some(rules) = explain["evaluated_rules"]
        .as_array()
        .filter(|r| !r.is_empty())
    {
        line("evaluated_rules:".into());
        for rule in rules {
            line(format!(
                "  {} {} priority={} region={} state={}",
                if rule["matched"].as_bool().unwrap_or(false) {
                    "✓"
                } else {
                    "✗"
                },
                rule["id"].as_str().unwrap_or("-"),
                rule["priority"].as_i64().unwrap_or(0),
                rule["region"].as_str().unwrap_or("-"),
                rule["state"].as_str().unwrap_or("unknown")
            ));
            let e = &rule["evidence"];
            line(format!(
                "    matchers: contains={:?} regex={:?} line_regex={:?} all={} any={} not={}",
                e["contains"],
                e["regex"],
                e["line_regex"],
                e["all_count"].as_u64().unwrap_or(0),
                e["any_count"].as_u64().unwrap_or(0),
                e["not_count"].as_u64().unwrap_or(0)
            ));
            line(format!(
                "    region: bytes={} preview={:?}",
                e["region_bytes"].as_u64().unwrap_or(0),
                e["region_preview"].as_str().unwrap_or("")
            ));
        }
    }
    out
}

// ---- notification ---------------------------------------------------------------------------

const NOTIFICATION_HELP: &str = "herdr notification commands:
  herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]";

fn notification(a: &[String]) -> R {
    match a.first().map(String::as_str) {
        Some("show") => {}
        Some("help" | "--help" | "-h") => return Ok(group(NOTIFICATION_HELP, 0)),
        _ => return Ok(group(NOTIFICATION_HELP, 2)),
    }
    let a = &a[1..];
    let Some(title) = a
        .first()
        .filter(|t| !matches!(t.as_str(), "help" | "--help" | "-h"))
    else {
        return Err(usage(
            "usage: herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request]",
        ));
    };
    let (mut body, mut position, mut sound) = (None, None, None);
    let mut i = 1;
    while i < a.len() {
        match a[i].as_str() {
            "--body" => {
                body = Some(value(a, i, "--body")?);
                i += 2;
            }
            "--position" => {
                let v = value(a, i, "--position")?;
                if !matches!(
                    v.as_str(),
                    "top-left" | "top-right" | "bottom-left" | "bottom-right"
                ) {
                    return Err(usage(format!(
                        "invalid position: {v} (expected top-left, top-right, bottom-left, or bottom-right)"
                    )));
                }
                position = Some(v);
                i += 2;
            }
            "--sound" => {
                let v = value(a, i, "--sound")?;
                match v.as_str() {
                    "none" => sound = None,
                    "done" | "request" => sound = Some(v),
                    _ => {
                        return Err(usage(format!(
                            "invalid sound: {v} (expected none, done, or request)"
                        )));
                    }
                }
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(call(
        "cli:notification:show",
        "notification.show",
        Obj::new()
            .put("title", title.as_str())
            .opt("body", body)
            .opt("position", position)
            .opt("sound", sound)
            .done(),
    ))
}

// ---- plugin ---------------------------------------------------------------------------------

const PLUGIN_HELP: &str = "herdr plugin commands:
  herdr plugin install <owner>/<repo>[/subdir...] [--ref REF] [--yes]
  herdr plugin uninstall <plugin_id|owner/repo[/subdir...]>
  herdr plugin link <path> [--disabled]
  herdr plugin list [--plugin ID] [--json]
  herdr plugin config-dir <plugin_id>
  herdr plugin unlink <plugin_id>
  herdr plugin enable <plugin_id>
  herdr plugin disable <plugin_id>
  herdr plugin action <list|invoke>
  herdr plugin log list [--plugin ID] [--limit N]
  herdr plugin pane <open|focus|close>";

const PLUGIN_ACTION_HELP: &str = "herdr plugin action commands:
  herdr plugin action list [--plugin ID]
  herdr plugin action invoke <action_id> [--plugin ID]";

const PLUGIN_PANE_HELP: &str = "herdr plugin pane commands:
  herdr plugin pane open --plugin ID --entrypoint ID [--placement overlay|popup|split|tab|zoomed] [--width SIZE] [--height SIZE] [--workspace ID] [--target-pane PANE] [--direction right|down] [--cwd PATH] [--env KEY=VALUE] [--focus|--no-focus]
  herdr plugin pane focus <pane_id>
  herdr plugin pane close <pane_id>";

fn plugin_call(method: &str, params: Value) -> Parsed {
    call("cli:plugin", method, params)
}

/// `--plugin ID` as the only option.
fn plugin_filter(a: &[String]) -> Result<Option<String>, Parsed> {
    let mut id = None;
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--plugin" => {
                id = Some(value(a, i, "--plugin")?);
                i += 2;
            }
            other => return Err(unknown(other)),
        }
    }
    Ok(id)
}

fn plugin(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(PLUGIN_HELP, 2));
    };
    let a = &a[1..];
    let id_arg = |verb: &str| one_arg(a, &format!("usage: herdr plugin {verb} <plugin_id>"));
    match sub.as_str() {
        "install" => {
            let (mut source, mut git_ref, mut yes) = (None::<String>, None, false);
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--ref" => {
                        git_ref = Some(value(a, i, "--ref")?);
                        i += 2;
                    }
                    "--yes" | "-y" => {
                        yes = true;
                        i += 1;
                    }
                    o if o.starts_with('-') || source.is_some() => return Err(unknown(o)),
                    s => {
                        source = Some(s.to_string());
                        i += 1;
                    }
                }
            }
            let Some(source) = source else {
                return Err(usage(
                    "usage: herdr plugin install [--ref REF] [--yes|-y] <owner>/<repo>[/subdir...]",
                ));
            };
            Ok(Parsed::Local(Local::PluginInstall {
                source,
                git_ref,
                yes,
            }))
        }
        "uninstall" => Ok(Parsed::Local(Local::PluginUninstall {
            id: one_arg(
                a,
                "usage: herdr plugin uninstall <plugin_id|owner/repo[/subdir...]>",
            )?,
        })),
        "link" => {
            let Some(path) = a.first() else {
                return Err(usage("usage: herdr plugin link <path> [--disabled]"));
            };
            let mut enabled = true;
            for f in &a[1..] {
                match f.as_str() {
                    "--disabled" => enabled = false,
                    "--enabled" => enabled = true,
                    other => return Err(unknown(other)),
                }
            }
            Ok(Parsed::Local(Local::PluginLink {
                path: path.clone(),
                yes: false,
                enabled,
            }))
        }
        "list" => {
            let (mut plugin, mut json) = (None, false);
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "--json" => {
                        json = true;
                        i += 1;
                    }
                    "--plugin" => {
                        plugin = Some(value(a, i, "--plugin")?);
                        i += 2;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(Parsed::Local(Local::PluginList { plugin, json }))
        }
        "config-dir" => Ok(Parsed::Local(Local::PluginConfigDir {
            id: id_arg("config-dir")?,
        })),
        "unlink" => Ok(Parsed::Local(Local::PluginUnlink {
            id: id_arg("unlink")?,
        })),
        "enable" => Ok(Parsed::Local(Local::PluginEnable {
            id: id_arg("enable")?,
        })),
        "disable" => Ok(Parsed::Local(Local::PluginDisable {
            id: id_arg("disable")?,
        })),
        // A Vibeke addition (Herdr has no `plugin update`).
        "update" => {
            let mut id = None;
            let mut yes = false;
            for f in a {
                match f.as_str() {
                    "--yes" | "-y" => yes = true,
                    o if o.starts_with('-') || id.is_some() => return Err(unknown(o)),
                    x => id = Some(x.to_string()),
                }
            }
            let Some(id) = id else {
                return Err(usage("usage: herdr plugin update <plugin_id> [--yes]"));
            };
            Ok(Parsed::Local(Local::PluginUpdate { id, yes }))
        }
        "action" => {
            let Some(sub) = a.first() else {
                return Ok(group(PLUGIN_ACTION_HELP, 2));
            };
            let a = &a[1..];
            match sub.as_str() {
                "list" => Ok(plugin_call(
                    "plugin.action.list",
                    Obj::new().opt("plugin_id", plugin_filter(a)?).done(),
                )),
                "invoke" => {
                    let Some(action) = a.first() else {
                        return Err(usage(
                            "usage: herdr plugin action invoke <action_id> [--plugin ID]",
                        ));
                    };
                    let plugin = plugin_filter(&a[1..])?;
                    Ok(plugin_call(
                        "plugin.action.invoke",
                        Obj::new()
                            .put("action_id", action.as_str())
                            .opt("plugin_id", plugin)
                            .put("context", json!({"invocation_source": "cli"}))
                            .done(),
                    ))
                }
                "help" | "--help" | "-h" => Ok(group(PLUGIN_ACTION_HELP, 0)),
                _ => Ok(group(PLUGIN_ACTION_HELP, 2)),
            }
        }
        "log" | "logs" => {
            let (mut plugin, mut limit) = (None, None);
            let mut i = 0;
            while i < a.len() {
                match a[i].as_str() {
                    "list" if i == 0 => i += 1,
                    "--plugin" => {
                        plugin = Some(value(a, i, "--plugin")?);
                        i += 2;
                    }
                    "--limit" => {
                        let v = value(a, i, "--limit")?;
                        limit = Some(
                            v.parse::<u64>()
                                .map_err(|_| usage(format!("invalid --limit value: {v}")))?,
                        );
                        i += 2;
                    }
                    other => return Err(unknown(other)),
                }
            }
            Ok(plugin_call(
                "plugin.log.list",
                Obj::new()
                    .opt("plugin_id", plugin)
                    .opt("limit", limit)
                    .done(),
            ))
        }
        "pane" => {
            let Some(sub) = a.first() else {
                return Ok(group(PLUGIN_PANE_HELP, 2));
            };
            let a = &a[1..];
            match sub.as_str() {
                "open" => plugin_pane_open(a),
                "focus" | "close" => {
                    let pane = one_arg(a, &format!("usage: herdr plugin pane {sub} <pane_id>"))?;
                    Ok(plugin_call(
                        &format!("plugin.pane.{sub}"),
                        json!({"pane_id": pane}),
                    ))
                }
                "help" | "--help" | "-h" => Ok(group(PLUGIN_PANE_HELP, 0)),
                _ => Ok(group(PLUGIN_PANE_HELP, 2)),
            }
        }
        "help" | "--help" | "-h" => Ok(group(PLUGIN_HELP, 0)),
        _ => Ok(group(PLUGIN_HELP, 2)),
    }
}

fn plugin_pane_open(a: &[String]) -> R {
    let mut text: Map<String, Value> = Map::new();
    let (mut placement, mut width, mut height, mut direction) = (None, None, None, None);
    let (mut focus, mut env) = (true, Map::new());
    let mut i = 0;
    while i < a.len() {
        let f = a[i].as_str();
        let key = match f {
            "--plugin" => Some("plugin_id"),
            "--entrypoint" => Some("entrypoint"),
            "--workspace" => Some("workspace_id"),
            "--target-pane" => Some("target_pane_id"),
            "--cwd" => Some("cwd"),
            _ => None,
        };
        if let Some(key) = key {
            text.insert(key.to_string(), Value::String(value(a, i, f)?));
            i += 2;
            continue;
        }
        match f {
            "--placement" => {
                let v = value(a, i, f)?;
                placement = Some(match v.as_str() {
                    "overlay" => "overlay",
                    "popup" => "popup",
                    "split" => "split",
                    "tab" => "tab",
                    "zoomed" | "fullscreen" => "zoomed",
                    _ => return Err(usage(format!("invalid pane placement: {v}"))),
                });
                i += 2;
            }
            "--width" | "--height" => {
                let v =
                    parse_popup_size(&value(a, i, f)?).map_err(|m| usage(format!("{f} {m}")))?;
                if f == "--width" {
                    width = Some(v);
                } else {
                    height = Some(v);
                }
                i += 2;
            }
            "--direction" => {
                let v = value(a, i, f)?;
                direction = Some(match v.as_str() {
                    "right" => "right",
                    "down" => "down",
                    _ => return Err(usage(format!("invalid split direction: {v}"))),
                });
                i += 2;
            }
            "--env" => {
                let (k, v) = parse_env_assignment(&value(a, i, f)?).map_err(usage)?;
                env.insert(k, Value::String(v));
                i += 2;
            }
            "--focus" => {
                focus = true;
                i += 1;
            }
            "--no-focus" => {
                focus = false;
                i += 1;
            }
            other => return Err(unknown(other)),
        }
    }
    let t = |k: &str| text.get(k).cloned();
    let Some(plugin) = t("plugin_id") else {
        return Err(usage("missing required --plugin"));
    };
    let Some(entrypoint) = t("entrypoint") else {
        return Err(usage("missing required --entrypoint"));
    };
    Ok(plugin_call(
        "plugin.pane.open",
        Obj::new()
            .put("plugin_id", plugin)
            .put("entrypoint", entrypoint)
            .opt("placement", placement)
            .opt("width", width)
            .opt("height", height)
            .opt("workspace_id", t("workspace_id"))
            .opt("target_pane_id", t("target_pane_id"))
            .opt("direction", direction)
            .opt("cwd", t("cwd"))
            .put("focus", focus)
            .map("env", env)
            .done(),
    ))
}

// ---- session, status, config, api, server, terminal, completion ------------------------------

const SESSION_HELP: &str = "herdr session commands:
  herdr session list [--json]
  herdr session attach <name>
  herdr session stop <name> [--json]
  herdr session delete <name> [--json]
  use 'default' as <name> to target the default session for stop";

fn session(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(SESSION_HELP, 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "list" => match a {
            [] => Ok(Parsed::SessionList { json: false }),
            [f] if f == "--json" => Ok(Parsed::SessionList { json: true }),
            _ => Err(usage("usage: herdr session list [--json]")),
        },
        "attach" => Ok(refused(
            "session attach",
            "attaching a client to a session needs Herdr's client; run `vibeke --session <name>`",
        )),
        "stop" | "delete" => Ok(refused(
            &format!("session {sub}"),
            "Vibeke sessions are stopped with `vibeke --session <name> server stop`; the shim never stops or deletes one",
        )),
        "help" | "--help" | "-h" => Ok(group(SESSION_HELP, 0)),
        _ => Ok(group(SESSION_HELP, 2)),
    }
}

const STATUS_HELP: &str = "herdr status commands:
  herdr status [--json]         show local client and running server status
  herdr status server [--json]  show running server status
  herdr status client [--json]  show local client binary status";

fn status(a: &[String]) -> R {
    let scoped = |scope: StatusScope, name: &str| match a.get(1).map(String::as_str) {
        None => Ok(Parsed::Status { scope, json: false }),
        Some("--json") if a.len() == 2 => Ok(Parsed::Status { scope, json: true }),
        _ => Err(usage(format!("usage: herdr status {name} [--json]"))),
    };
    match a.first().map(String::as_str) {
        None => Ok(Parsed::Status {
            scope: StatusScope::Full,
            json: false,
        }),
        Some("--json") if a.len() == 1 => Ok(Parsed::Status {
            scope: StatusScope::Full,
            json: true,
        }),
        Some("server") => scoped(StatusScope::Server, "server"),
        Some("client") => scoped(StatusScope::Client, "client"),
        Some("help" | "--help" | "-h") if a.len() == 1 => Ok(group(STATUS_HELP, 0)),
        _ => Ok(group(STATUS_HELP, 2)),
    }
}

const CONFIG_HELP: &str = "herdr config commands:
  herdr config check  validate config.toml and print diagnostics
  herdr config reset-keys  back up config.toml and remove custom keybindings";

fn config(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(CONFIG_HELP, 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "check" => match a {
            [] => Ok(Parsed::ConfigCheck),
            [f] if matches!(f.as_str(), "help" | "--help" | "-h") => {
                Ok(group("usage: herdr config check", 0))
            }
            _ => Err(usage("usage: herdr config check")),
        },
        "reset-keys" => Ok(refused(
            "config reset-keys",
            "the shim does not rewrite Herdr's config.toml",
        )),
        "help" | "--help" | "-h" => Ok(group(CONFIG_HELP, 0)),
        _ => Ok(group(CONFIG_HELP, 2)),
    }
}

const API_HELP: &str = "herdr api commands:
  herdr api snapshot
  herdr api schema [--json | --output PATH]";

fn api(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(API_HELP, 2));
    };
    let a = &a[1..];
    match sub.as_str() {
        "schema" => match a {
            [] => Ok(Parsed::ApiSchema(ApiSchema::Summary)),
            [f] if f == "--json" => Ok(Parsed::ApiSchema(ApiSchema::Json)),
            [f, path] if f == "--output" => Ok(Parsed::ApiSchema(ApiSchema::Output(path.clone()))),
            [f] if f == "--output" => Err(usage("missing value for --output")),
            [f] if matches!(f.as_str(), "help" | "--help" | "-h") => {
                Ok(group("usage: herdr api schema [--json | --output PATH]", 0))
            }
            [o] if o.starts_with('-') => Err(unknown(o)),
            _ => Err(usage("usage: herdr api schema [--json | --output PATH]")),
        },
        "snapshot" => {
            if !a.is_empty() {
                return Err(usage("usage: herdr api snapshot"));
            }
            Ok(call("cli:api:snapshot", "session.snapshot", json!({})))
        }
        "help" | "--help" | "-h" => Ok(group(API_HELP, 0)),
        _ => Ok(group(API_HELP, 2)),
    }
}

/// `herdr api schema` without flags: Herdr's summary of the captured schema.
pub fn api_schema_summary() -> String {
    let v: Value = serde_json::from_str(API_SCHEMA_JSON).unwrap_or(Value::Null);
    let mut schemas: Vec<&str> = v["schemas"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    schemas.sort_unstable();
    format!(
        "Herdr API schema\nprotocol: {}\nschema_version: {}\nschemas: {}\n\nUse `herdr api schema --json` to print the full schema.\nUse `herdr api schema --output PATH` to write it to a file.\n",
        v["protocol"],
        v["schema_version"],
        schemas.join(", ")
    )
}

const SERVER_HELP: &str = "herdr server commands:
  herdr server                run as headless server
  herdr server stop           stop the running server via the API socket
  herdr server live-handoff   hand off live panes to a new local server
  herdr server reload-config  reload config.toml in the running server
  herdr server agent-manifests [--json]  show agent detection manifest status
  herdr server update-agent-manifests [--json]  fetch and reload agent detection manifests
  herdr server reload-agent-manifests  reload agent detection manifests in the running server";

fn server(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(refused(
            "server",
            "the shim does not run a Herdr server; Vibeke's server serves the Herdr API",
        ));
    };
    let a = &a[1..];
    match sub.as_str() {
        "stop" => {
            if !a.is_empty() {
                return Err(usage("usage: herdr server stop"));
            }
            Ok(quiet("server.stop", json!({})))
        }
        "reload-config" => {
            if !a.is_empty() {
                return Err(usage("usage: herdr server reload-config"));
            }
            Ok(call(
                "cli:server:reload-config",
                "server.reload_config",
                json!({}),
            ))
        }
        "agent-manifests" | "update-agent-manifests" | "reload-agent-manifests" => Ok(refused(
            &format!("server {sub}"),
            "Vibeke detects agents without Herdr's detection manifests",
        )),
        "live-handoff" => Ok(refused(
            "server live-handoff",
            "Vibeke keeps panes alive across server restarts on its own",
        )),
        "help" | "--help" | "-h" => Ok(group(SERVER_HELP, 0)),
        _ => Ok(group(SERVER_HELP, 2)),
    }
}

const TERMINAL_HELP: &str = "herdr terminal commands:
  herdr terminal attach <terminal_id> [--takeover]
  herdr terminal session control <target> [--takeover] [--cols N] [--rows N]
  herdr terminal session observe <target> [--cols N] [--rows N]
  herdr terminal title set <title>
  herdr terminal title clear
  detach from direct attach with ctrl+b q; send literal ctrl+b with ctrl+b ctrl+b";

fn terminal(a: &[String]) -> R {
    let Some(sub) = a.first() else {
        return Ok(group(TERMINAL_HELP, 2));
    };
    let a = &a[1..];
    const TITLE_USAGE: &str =
        "usage: herdr terminal title set <title>\n       herdr terminal title clear";
    match sub.as_str() {
        "attach" | "session" => Ok(refused(
            &format!("terminal {sub}"),
            "attaching to a terminal needs Herdr's client; open the pane in `vibeke`",
        )),
        "title" => match a.first().map(String::as_str) {
            Some("set") => {
                if a.len() != 2 {
                    return Err(usage("usage: herdr terminal title set <title>"));
                }
                Ok(call(
                    "cli:terminal:title:set",
                    "client.window_title.set",
                    json!({"title": a[1]}),
                ))
            }
            Some("clear") => {
                if a.len() != 1 {
                    return Err(usage("usage: herdr terminal title clear"));
                }
                Ok(call(
                    "cli:terminal:title:clear",
                    "client.window_title.clear",
                    json!({}),
                ))
            }
            Some("help" | "--help" | "-h") => Ok(group(TITLE_USAGE, 0)),
            _ => Ok(group(TITLE_USAGE, 2)),
        },
        "help" | "--help" | "-h" => Ok(group(TERMINAL_HELP, 0)),
        _ => Ok(group(TERMINAL_HELP, 2)),
    }
}

fn completion_cmd(a: &[String]) -> R {
    const USAGE: &str = "usage: herdr completion <bash|elvish|fish|powershell|zsh>";
    let Some(shell) = a.first() else {
        return Ok(group(USAGE, 2));
    };
    if matches!(shell.as_str(), "help" | "--help" | "-h") {
        return Ok(group(USAGE, 0));
    }
    if a.len() != 1 {
        return Ok(group(USAGE, 2));
    }
    match shell.as_str() {
        "bash" | "zsh" | "fish" => Ok(Parsed::Help(completion(shell))),
        "elvish" | "powershell" => Ok(refused(
            &format!("completion {shell}"),
            "the shim generates bash, zsh and fish completions only",
        )),
        other => Err(usage(format!("unknown shell: {other}\n{USAGE}"))),
    }
}

/// Command groups and their subcommands, for completion.
const GRAMMAR: &[(&str, &[&str])] = &[
    (
        "workspace",
        &[
            "list",
            "create",
            "get",
            "focus",
            "rename",
            "report-metadata",
            "close",
        ],
    ),
    ("worktree", &["list", "create", "open", "remove"]),
    (
        "tab",
        &["list", "create", "get", "focus", "rename", "close"],
    ),
    (
        "pane",
        &[
            "list",
            "current",
            "get",
            "layout",
            "process-info",
            "neighbor",
            "edges",
            "focus",
            "resize",
            "zoom",
            "read",
            "rename",
            "input",
            "split",
            "swap",
            "move",
            "close",
            "send-text",
            "send-keys",
            "wait-output",
            "run",
            "report-agent",
            "report-agent-session",
            "release-agent",
            "report-metadata",
        ],
    ),
    (
        "agent",
        &[
            "list",
            "get",
            "read",
            "send-keys",
            "prompt",
            "rename",
            "focus",
            "wait",
            "start",
            "explain",
        ],
    ),
    ("notification", &["show"]),
    (
        "plugin",
        &[
            "install",
            "uninstall",
            "link",
            "unlink",
            "enable",
            "disable",
            "list",
            "config-dir",
            "action",
            "log",
            "pane",
            "update",
        ],
    ),
    ("session", &["list"]),
    ("status", &["server", "client"]),
    ("config", &["check"]),
    ("api", &["snapshot", "schema"]),
    ("server", &["stop", "reload-config"]),
    ("terminal", &["title"]),
    ("integration", &["install", "uninstall", "status"]),
    ("completion", &["bash", "zsh", "fish"]),
];

/// A shell completion script for the shim grammar (`herdr completion bash|zsh|fish`).
pub fn completion(shell: &str) -> String {
    let nouns: Vec<&str> = GRAMMAR.iter().map(|(n, _)| *n).collect();
    match shell {
        "fish" => {
            let mut s = String::from("# herdr (Vibeke compatibility shim) completions\n");
            s.push_str(&format!(
                "complete -c herdr -n '__fish_use_subcommand' -a '{}'\n",
                nouns.join(" ")
            ));
            for (n, vs) in GRAMMAR {
                s.push_str(&format!(
                    "complete -c herdr -n '__fish_seen_subcommand_from {n}' -a '{}'\n",
                    vs.join(" ")
                ));
            }
            s
        }
        "zsh" => {
            let mut s = String::from("#compdef herdr\n_herdr() {\n  local -a nouns\n");
            s.push_str(&format!("  nouns=({})\n", nouns.join(" ")));
            s.push_str("  if (( CURRENT == 2 )); then\n    compadd -a nouns\n    return\n  fi\n");
            s.push_str("  case $words[2] in\n");
            for (n, vs) in GRAMMAR {
                s.push_str(&format!("    {n}) compadd {} ;;\n", vs.join(" ")));
            }
            s.push_str("  esac\n}\ncompdef _herdr herdr\n");
            s
        }
        _ => {
            let mut s = String::from(
                "# herdr (Vibeke compatibility shim) completions\n_herdr() {\n  local cur=${COMP_WORDS[COMP_CWORD]}\n",
            );
            s.push_str(&format!(
                "  if [ $COMP_CWORD -eq 1 ]; then COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")); return; fi\n",
                nouns.join(" ")
            ));
            s.push_str("  case ${COMP_WORDS[1]} in\n");
            for (n, vs) in GRAMMAR {
                s.push_str(&format!(
                    "    {n}) COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;\n",
                    vs.join(" ")
                ));
            }
            s.push_str("  esac\n}\ncomplete -F _herdr herdr\n");
            s
        }
    }
}

/// Socket methods the shim grammar can send (for the inventory).
pub fn shim_methods() -> Vec<&'static str> {
    vec![
        "workspace.list",
        "workspace.create",
        "workspace.get",
        "workspace.focus",
        "workspace.rename",
        "workspace.report_metadata",
        "workspace.close",
        "worktree.list",
        "worktree.create",
        "worktree.open",
        "worktree.remove",
        "tab.list",
        "tab.create",
        "tab.get",
        "tab.focus",
        "tab.rename",
        "tab.close",
        "pane.list",
        "pane.current",
        "pane.get",
        "pane.layout",
        "pane.process_info",
        "pane.neighbor",
        "pane.edges",
        "pane.focus_direction",
        "pane.resize",
        "pane.zoom",
        "pane.read",
        "pane.rename",
        "pane.input.set",
        "pane.split",
        "pane.swap",
        "pane.move",
        "pane.close",
        "pane.send_text",
        "pane.send_keys",
        "pane.send_input",
        "pane.wait_for_output",
        "pane.report_agent",
        "pane.report_agent_session",
        "pane.release_agent",
        "pane.report_metadata",
        "agent.list",
        "agent.get",
        "agent.focus",
        "agent.read",
        "agent.send_keys",
        "agent.prompt",
        "agent.rename",
        "agent.wait",
        "agent.start",
        "agent.explain",
        "notification.show",
        "plugin.action.list",
        "plugin.action.invoke",
        "plugin.log.list",
        "plugin.pane.open",
        "plugin.pane.focus",
        "plugin.pane.close",
        "session.snapshot",
        "server.stop",
        "server.reload_config",
        "client.window_title.set",
        "client.window_title.clear",
        "ping",
    ]
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
