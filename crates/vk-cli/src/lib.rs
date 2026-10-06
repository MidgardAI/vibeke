#![allow(clippy::result_large_err)]
//! `vibeke <noun> <verb> …` — the CLI mirrors the API (07 §5). Every noun is an API namespace and
//! every verb a method; `--flag value` pairs become params (dashes → underscores), with a few
//! positional arguments per verb. `vibeke <noun>` alone prints help and never executes.

pub mod browser_console;
pub mod client;
pub mod compat;
pub mod mcp;
pub mod show;

use anyhow::Result;
use client::{CallError, Client};
use serde_json::{Map, Value, json};
use std::io::IsTerminal;
use std::path::PathBuf;

pub const EXIT_OK: i32 = 0;
pub const EXIT_API: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 3;
pub const EXIT_NO_SERVER: i32 = 4;
pub const EXIT_PERMISSION: i32 = 5;

#[derive(Debug, Clone, Default)]
pub struct Global {
    pub session: String,
    pub machine: Option<String>,
    pub socket: Option<PathBuf>,
    pub json: Option<bool>,
    pub quiet: bool,
    pub no_spawn: bool,
    pub timeout_ms: Option<u64>,
}

/// (noun, verb, method, positional param names, help)
pub const COMMANDS: &[(&str, &str, &str, &[&str], &str)] = &[
    (
        "server",
        "status",
        "server.status",
        &[],
        "server pid, version, panes, clients",
    ),
    (
        "server",
        "stop",
        "server.stop",
        &[],
        "Stop the server. Holders remain active unless you use --kill-panes.",
    ),
    (
        "server",
        "reload-config",
        "server.reload_config",
        &[],
        "reload config.toml",
    ),
    (
        "session",
        "snapshot",
        "session.snapshot",
        &[],
        "full session state",
    ),
    (
        "workspace",
        "list",
        "workspace.list",
        &[],
        "workspaces with agent summaries",
    ),
    ("workspace", "get", "workspace.get", &["workspace"], ""),
    (
        "workspace",
        "create",
        "workspace.create",
        &["cwd"],
        "--cwd DIR [--name N] [--focus] [--layout NAME] [--group G]",
    ),
    (
        "workspace",
        "rename",
        "workspace.rename",
        &["workspace", "name"],
        "",
    ),
    ("workspace", "focus", "workspace.focus", &["workspace"], ""),
    ("workspace", "close", "workspace.close", &["workspace"], ""),
    ("tab", "list", "tab.list", &[], "[--workspace w]"),
    (
        "tab",
        "create",
        "tab.create",
        &[],
        "[--workspace w] [--cwd d] [--title t]",
    ),
    ("tab", "rename", "tab.rename", &["tab", "title"], ""),
    ("tab", "focus", "tab.focus", &["tab"], ""),
    ("tab", "close", "tab.close", &["tab"], ""),
    (
        "pane",
        "list",
        "pane.list",
        &[],
        "[--workspace w] [--tab t] [--has-agent]",
    ),
    ("pane", "get", "pane.get", &["pane"], ""),
    (
        "pane",
        "current",
        "pane.current",
        &[],
        "the calling pane (needs VIBEKE_PANE_TOKEN)",
    ),
    (
        "pane",
        "split",
        "pane.split",
        &["pane"],
        "[--direction right|down|left|up] [--cwd d] [--command c] [--focus]",
    ),
    (
        "pane",
        "focus",
        "pane.focus",
        &["pane"],
        "[--direction left|right|up|down]",
    ),
    ("pane", "close", "pane.close", &["pane"], ""),
    ("pane", "zoom", "pane.zoom", &["pane"], ""),
    (
        "pane",
        "resize",
        "pane.resize",
        &["pane"],
        "--direction d [--percent n]",
    ),
    ("pane", "rename", "pane.rename", &["pane", "title"], ""),
    (
        "pane",
        "send-text",
        "pane.send_text",
        &["pane", "text"],
        "[--paste auto|bracketed|raw]",
    ),
    (
        "pane",
        "send-keys",
        "pane.send_keys",
        &["pane", "keys..."],
        "keys use the key grammar: enter ctrl+c alt+x y",
    ),
    (
        "pane",
        "run",
        "pane.run",
        &["pane", "command"],
        "[--wait] [--timeout-ms n]",
    ),
    (
        "pane",
        "read",
        "pane.read",
        &["pane"],
        "[--source visible|recent|recent_unwrapped|scrollback|archive] [--lines n] [--from N --to M (archive: absolute lines)]",
    ),
    (
        "pane",
        "wait-output",
        "pane.wait_output",
        &["pane", "match"],
        "[--regex r] [--timeout-ms n]",
    ),
    (
        "pane",
        "wait-idle",
        "pane.wait_idle",
        &["pane"],
        "[--quiet-ms n] [--timeout-ms n]",
    ),
    ("pane", "mark-unread", "pane.mark_unread", &["pane"], ""),
    ("pane", "pin", "pane.pin", &["pane"], ""),
    (
        "agent",
        "list",
        "agent.list",
        &[],
        "[--workspace w] [--harness h] [--all-machines]",
    ),
    ("agent", "get", "agent.get", &["target"], ""),
    (
        "agent",
        "start",
        "agent.start",
        &["name"],
        "--harness claude|codex [--pane p] [--args a,b] [--yolo] [--isolate host|sandbox] [--network p]",
    ),
    (
        "agent",
        "spawn",
        "agent.spawn",
        &["name"],
        "--harness h [--split-of p] [--prompt text] [--focus]",
    ),
    (
        "agent",
        "prompt",
        "agent.prompt",
        &["target", "text"],
        "[--wait] [--timeout-ms n]",
    ),
    (
        "agent",
        "wait",
        "agent.wait",
        &["target"],
        "[--until idle,done,needs_approval,...] [--timeout-ms n]",
    ),
    ("agent", "interrupt", "agent.interrupt", &["target"], ""),
    (
        "agent",
        "send-keys",
        "agent.send_keys",
        &["target", "keys..."],
        "",
    ),
    (
        "agent",
        "read",
        "agent.read",
        &["target"],
        "[--source visible|recent|transcript] [--lines n]",
    ),
    (
        "agent",
        "transcript",
        "agent.transcript",
        &["target"],
        "[--limit n]",
    ),
    ("agent", "rename", "agent.rename", &["target", "name"], ""),
    ("agent", "release", "agent.release", &["target"], ""),
    ("agent", "resume", "agent.resume", &["run"], "[--pane p]"),
    (
        "agent",
        "resumable",
        "agent.resumable",
        &[],
        "ended runs that can be resumed",
    ),
    ("agent", "harnesses", "agent.harnesses", &[], ""),
    (
        "interaction",
        "list",
        "interaction.list",
        &[],
        "[--status open]",
    ),
    (
        "interaction",
        "get",
        "interaction.get",
        &["interaction"],
        "",
    ),
    (
        "interaction",
        "answer",
        "interaction.answer",
        &["interaction"],
        "--allow | --deny | --allow-always | --choice q=o | --text t",
    ),
    (
        "interaction",
        "cancel",
        "interaction.cancel",
        &["interaction"],
        "",
    ),
    (
        "ask",
        "list",
        "interaction.list",
        &[],
        "alias of interaction list",
    ),
    (
        "ask",
        "answer",
        "interaction.answer",
        &["interaction"],
        "alias of interaction answer",
    ),
    ("notification", "list", "notification.list", &[], ""),
    (
        "notification",
        "send",
        "notification.send",
        &["title", "body"],
        "",
    ),
    (
        "events",
        "read",
        "events.read",
        &[],
        "[--after-seq n] [--types agent.*]",
    ),
    (
        "events",
        "wait",
        "events.wait",
        &[],
        "--types t [--timeout-ms n]",
    ),
    (
        "search",
        "query",
        "search.query",
        &["q"],
        "[--pane p] [--workspace w] [--since 2h|epoch-ms] [--limit n] [--context n] [--regex] [--sources live,archive]  (also: vibeke search <q>)",
    ),
    (
        "task",
        "new",
        "task.create",
        &["title"],
        "[--repo .] [--agent claude:name] [--base ref] [--root sibling] [--isolation worktree|jj_workspace|none|auto] [--yolo] [--isolate host|sandbox|container] [--network none|harness-apis|package-registries|dev|open] [--image ref] [--code clone|worktree] [--devcontainer] [--build]",
    ),
    ("task", "list", "task.list", &[], ""),
    (
        "task",
        "sync",
        "task.sync",
        &["task"],
        "<task> [--direction pull|push|both] [--force]: host-side fetch of a container task's commits (push = host commits into the box)",
    ),
    (
        "sandbox",
        "status",
        "sandbox.status",
        &[],
        "which isolation levels and providers work here (13 §11)",
    ),
    (
        "sandbox",
        "list",
        "sandbox.list",
        &[],
        "live sandbox contexts, proxies, approvals",
    ),
    (
        "sandbox",
        "allow",
        "sandbox.allow",
        &["task", "host"],
        "allow a domain for a sandboxed task's egress proxy",
    ),
    (
        "sandbox",
        "start",
        "sandbox.start",
        &["task"],
        "create or start a container task's box",
    ),
    (
        "sandbox",
        "stop",
        "sandbox.stop",
        &["task"],
        "Stop the task container. Its pane processes end.",
    ),
    (
        "sandbox",
        "rm",
        "sandbox.remove",
        &["task"],
        "<task> [--force]: sync, then remove a container box (kept when unsynced unless --force)",
    ),
    (
        "policy",
        "trust",
        "policy.trust",
        &["path"],
        "Trust repository automation at its current digest. Print the setup script.",
    ),
    ("task", "get", "task.get", &["task"], ""),
    (
        "task",
        "track",
        "task.track",
        &[],
        "[--pane @current|--run r] [--turn N] [--title t] [--criterion text]... [--stop-at draft_pr] — track the agent's work (no send, no spawn)",
    ),
    (
        "task",
        "sources",
        "task.sources",
        &[],
        "[--pane p|--run r] — recent requests you can track",
    ),
    (
        "task",
        "show",
        "task.detail",
        &["task"],
        "intent, bindings, baseline, messages",
    ),
    (
        "task",
        "intent",
        "task.intent.get",
        &["task"],
        "[--revision N]",
    ),
    (
        "task",
        "edit",
        "task.intent.update",
        &["task"],
        "[--title t] [--add-criterion text] [--stop-at x] [--expected-revision N] — record-only",
    ),
    (
        "task",
        "bind",
        "task.bind",
        &["task"],
        "[--pane p|--run r] [--role implementation]",
    ),
    ("task", "unbind", "task.unbind", &["task"], "[--binding b]"),
    (
        "task",
        "set",
        "task.set",
        &["task"],
        "[--priority N] [--effort quick|minutes|deep|unknown] [--effort-source heuristic|assistant:<request>]",
    ),
    // 15 T4.
    (
        "task",
        "snapshot",
        "task.review.snapshot",
        &["task"],
        "<task> — capture uncommitted work as an immutable, accept-capable subject (files, index and branches stay as they are)",
    ),
    (
        "task",
        "snapshot-gc",
        "task.review.snapshot.gc",
        &["task"],
        "[task] | --repo path [--dry-run] [--include-unrecorded] — Delete unused snapshot refs. Preserve refs used by candidates, accepted reviews, review agents, or active checks. Print deleted refs.",
    ),
    (
        "task",
        "effort",
        "task.effort.estimate",
        &["task"],
        "<task> — your effort, the deterministic heuristic (model estimate: assist generate effort_estimate --task)",
    ),
    (
        "task",
        "reviewer",
        "task.review.request_reviewer",
        &["task"],
        "<task> [--harness claude] [--subject s] [--prompt text] — Show the review prompt. Do not start an agent.",
    ),
    (
        "task",
        "reviewer-start",
        "task.review.start_reviewer",
        &["request", "prompt_digest"],
        "<request> <prompt-digest> [--pane p | --split-of p] — start the confirmed reviewer run (role review)",
    ),
    (
        "task",
        "notes",
        "task.review.notes",
        &["task"],
        "<task> — reviewer findings (agent opinions, not evidence) and reviewer runs",
    ),
    (
        "task",
        "note-classify",
        "task.review.note.classify",
        &["note", "classification"],
        "<note> blocking|not_blocking|dismissed [--reason r]",
    ),
    (
        "task",
        "depend",
        "task.dependency.add",
        &["task", "depends_on"],
        "<task> <depends-on> [--kind blocks|related] — confirm a dependency link (cycles refused)",
    ),
    (
        "task",
        "undepend",
        "task.dependency.remove",
        &["task", "depends_on"],
        "<task> <depends-on> [--kind k] | --edge e",
    ),
    (
        "task",
        "deps",
        "task.dependency.list",
        &["task"],
        "[task] — confirmed links and how many open tasks each blocks",
    ),
    (
        "task",
        "message",
        "task.message.prepare",
        &["task", "text"],
        "[--communicates-intent] — draft only",
    ),
    (
        "task",
        "send",
        "task.message.send",
        &["message"],
        "send a prepared message (refuses with zero bytes when unsafe)",
    ),
    (
        "task",
        "message-status",
        "task.message.get",
        &["message"],
        "",
    ),
    (
        "task",
        "operation",
        "task.operation.get",
        &["idempotency_key"],
        "look up a mutation receipt",
    ),
    (
        "task",
        "finish",
        "task.finish",
        &["task"],
        "[--remove-worktree] [--force]",
    ),
    (
        "task",
        "setup",
        "task.setup",
        &["task"],
        "run the task's setup again (after `policy trust`); shows the setup pane",
    ),
    (
        "task",
        "pr",
        "task.pr",
        &["task"],
        "[--refresh] pull request status through gh (cached 60 s; needs gh installed and logged in)",
    ),
    (
        "task",
        "reconcile",
        "task.reconcile",
        &[],
        "[--repo r] compare tasks with the worktrees on disk (marks missing, reports orphans, deletes nothing)",
    ),
    ("worktree", "list", "worktree.list", &[], "[--cwd d]"),
    (
        "worktree",
        "remove",
        "worktree.remove",
        &["path"],
        "[--force] (async)",
    ),
    ("worktree", "repo-root", "worktree.repo_root", &["cwd"], ""),
    (
        "layout",
        "export",
        "layout.export",
        &["tab"],
        "[tab] | --workspace w  [--format toml|json]",
    ),
    (
        "layout",
        "apply",
        "layout.apply",
        &["name"],
        "<name from [layouts.*] | file.toml|.json> | --file f | --doc text  [--workspace w | --cwd d --ws-name n] [--focus]",
    ),
    (
        "layout",
        "list",
        "layout.list",
        &[],
        "named layouts in config",
    ),
    ("layout", "get", "layout.get", &["name"], ""),
    ("group", "list", "group.list", &[], "workspace groups"),
    ("group", "create", "group.create", &["name"], "[--parent g]"),
    ("group", "rename", "group.rename", &["group", "name"], ""),
    (
        "group",
        "move",
        "group.move",
        &["group"],
        "[--parent g] [--index n | --delta n]",
    ),
    (
        "group",
        "delete",
        "group.delete",
        &["group"],
        "members move to the parent",
    ),
    (
        "group",
        "collapse",
        "group.collapse",
        &["group"],
        "[--collapsed true|false]",
    ),
    (
        "group",
        "add",
        "group.add",
        &["group", "workspace"],
        "[--index n]",
    ),
    ("group", "remove", "group.remove", &["workspace"], ""),
    (
        "workspace",
        "move",
        "workspace.move",
        &["workspace"],
        "[--group g | --group ''] [--delta n]",
    ),
    (
        "pane",
        "float",
        "pane.float",
        &["pane"],
        "[pane] float/move it | --tab t [--command c] [--cwd d]  [--rect '{\"x\":15,\"y\":15,\"w\":70,\"h\":70}'] [--focus]",
    ),
    (
        "pane",
        "embed",
        "pane.embed",
        &["pane"],
        "[--target p] [--direction right|down|left|up]",
    ),
    (
        "tab",
        "floats",
        "tab.floats",
        &["tab"],
        "show/hide floats [--visible true|false]",
    ),
    (
        "theme",
        "get",
        "theme.get",
        &[],
        "effective light/dark theme",
    ),
    (
        "theme",
        "set-mode",
        "theme.set_mode",
        &["mode"],
        "auto|light|dark (runtime, not persisted)",
    ),
    (
        "client",
        "appearance",
        "client.appearance",
        &[],
        "--dark true|false (host terminal appearance)",
    ),
    (
        "client",
        "focus",
        "client.focus",
        &["pane"],
        "[--url vibeke://focus?…] [--no-raise]  (also: vibeke focus <pane|url>)",
    ),
    (
        "notification",
        "config",
        "notification.config",
        &[],
        "channels, native backend, rules",
    ),
    (
        "status",
        "segments",
        "status.segments",
        &[],
        "[--pane p] [--client c] status-bar segment data",
    ),
    ("blob", "put", "blob.put", &[], "--path file | --data-b64 …"),
    (
        "machine",
        "list",
        "machine.list",
        &[],
        "saved remote machines",
    ),
    (
        "machine",
        "add",
        "machine.add",
        &["label", "address"],
        "user@host",
    ),
    ("machine", "remove", "machine.remove", &["machine"], ""),
    ("machine", "connect", "machine.connect", &["machine"], ""),
    (
        "machine",
        "disconnect",
        "machine.disconnect",
        &["machine"],
        "",
    ),
    ("machine", "status", "machine.status", &["machine"], ""),
    (
        "machine",
        "show",
        "machine.show",
        &["machine"],
        "[--offline] saved settings, remote version, link state, local artifact trust",
    ),
    (
        "machine",
        "upgrade",
        "machine.upgrade",
        &["machine"],
        "[--from artifact [--version v]] [--stage-only] [--force] verified install/upgrade",
    ),
    (
        "preview",
        "declare",
        "preview.declare",
        &["port"],
        "--port N [--path /p] [--label l] [--pane p] [--task k] [--tls-origin | --no-tls-origin] (proxy mode: serve this preview over https)",
    ),
    (
        "preview",
        "list",
        "preview.list",
        &[],
        "[--machine m] [--task k] [--pane p] [--all] (suggestions with --all)",
    ),
    ("preview", "get", "preview.get", &["preview"], ""),
    (
        "preview",
        "open",
        "preview.open",
        &["preview"],
        "<v4|devbox/v4|url> [--split right|down|tab|float | --window | --proxy [--no-open] [--tls-origin | --no-tls-origin]] [--pane p] [--machine m] [--viewport WxH | --device iphone-15]",
    ),
    ("preview", "url", "preview.url", &["preview"], ""),
    (
        "preview",
        "mirror",
        "preview.mirror",
        &["preview"],
        "<devbox/v4> Bind the remote port to this host's loopback address. This explicit connection is unauthenticated.",
    ),
    (
        "preview",
        "unmirror",
        "preview.unmirror",
        &["preview"],
        "<devbox/v4 | port> stop a mirror",
    ),
    (
        "preview",
        "promote",
        "preview.promote",
        &["preview"],
        "accept a suggestion",
    ),
    ("preview", "forget", "preview.forget", &["preview"], ""),
    (
        "preview",
        "show",
        "screenshot.list",
        &["preview"],
        "<v4|devbox/v4> [--no-image] the latest screenshot of a preview, inline (kitty graphics, iTerm2) or path + metadata",
    ),
    (
        "preview",
        "trust-ca",
        "preview.trust_ca",
        &[],
        "[--install] [--path] print the CA file, fingerprint and per-OS instructions to trust the tls_origin CA (local; never installs without --install and a typed confirmation)",
    ),
    (
        "preview",
        "profile",
        "preview.profile",
        &["action", "profile"],
        "list | reset <profile>",
    ),
    (
        "preview",
        "status",
        "preview.status",
        &[],
        "SOCKS port, managed browsers, links",
    ),
    (
        "browser",
        "open",
        "browser.open",
        &["target"],
        "<preview|url> [--viewport 390x844] [--device iphone-15|pixel-8|ipad|desktop-1280|desktop-1440|desktop-1920] [--dark] — headless session on this machine",
    ),
    (
        "browser",
        "navigate",
        "browser.navigate",
        &["session", "url"],
        "<session> <url|/path> [--wait load|domcontentloaded|none]",
    ),
    (
        "browser",
        "click",
        "browser.click",
        &["session", "selector"],
        "<session> <css|text=…> | --x N --y N [--timeout-ms n]",
    ),
    (
        "browser",
        "type",
        "browser.type",
        &["session", "selector", "text"],
        "<session> [selector] <text> [--submit] [--clear]",
    ),
    (
        "browser",
        "press",
        "browser.press",
        &["session", "key"],
        "<session> <key> (enter, tab, ctrl+a, ArrowDown)",
    ),
    (
        "browser",
        "wait",
        "browser.wait",
        &["session", "for"],
        "<session> load|networkidle|selector:<css>|ms:<n>",
    ),
    (
        "browser",
        "eval",
        "browser.eval",
        &["session", "expression"],
        "<session> <js> (from a pane: needs preview.browser_script)",
    ),
    (
        "browser",
        "screenshot",
        "browser.screenshot",
        &["session"],
        "<session|preview|url> [--full-page] [--selector css] [--out f.png] — A preview or URL uses a new context for one capture. Options: [--device d] [--viewport WxH].",
    ),
    (
        "browser",
        "snapshot",
        "browser.snapshot",
        &["session"],
        "<session> [--format a11y|text|html] [--selector css]",
    ),
    (
        "browser",
        "dom",
        "browser.dom",
        &["session"],
        "alias of snapshot",
    ),
    (
        "browser",
        "console",
        "browser.console",
        &["session"],
        "<session> [--level error|warn|all] [--since 5m] | --pane <browser pane> [--follow] [--console|--network] [--errors] (follow keys: c n e a q)",
    ),
    (
        "browser",
        "console-split",
        "browser.pane.console",
        &["pane"],
        "<browser pane> toggle the console/network split under it (prefix+alt+c)",
    ),
    (
        "browser",
        "viewport",
        "browser.pane.update",
        &["pane", "viewport"],
        "<browser pane> <WxH | fit> | --device iphone-15|pixel-8|ipad|desktop-1280|desktop-1440|desktop-1920 — pin the page size (letterboxed)",
    ),
    (
        "browser",
        "network",
        "browser.network",
        &["session"],
        "<session> [--failed] [--since 5m]",
    ),
    ("browser", "close", "browser.close", &["session"], ""),
    (
        "browser",
        "list",
        "browser.list",
        &[],
        "sessions you can see + browser status",
    ),
    ("browser", "status", "browser.status", &[], ""),
    (
        "browser",
        "install",
        "browser.install",
        &[],
        "[--yes] [--sha256 hex] [--url u] — asks before downloading Chrome for Testing",
    ),
    (
        "browser",
        "take-over",
        "browser.take_over",
        &["session"],
        "agent calls fail with human_control until release",
    ),
    ("browser", "release", "browser.release", &["session"], ""),
    (
        "browser",
        "watch",
        "browser.watch",
        &["session"],
        "<session> [--pane p] [--split right|down|tab] — View an agent session in a read-only browser pane. Press prefix+t to take control.",
    ),
    (
        "browser",
        "pane-status",
        "browser.pane.status",
        &[],
        "browser panes rendered here: browsers, targets, fps",
    ),
    (
        "browser",
        "panes",
        "browser.pane.list",
        &[],
        "browser panes in this server's layout",
    ),
    (
        "browser",
        "pane",
        "browser.pane.create",
        &["url"],
        "<url> [--pane p] [--split right|down|tab] [--viewport WxH | --device iphone-15]",
    ),
    (
        "browser",
        "command",
        "browser.command",
        &["pane", "cmd"],
        "<pane> back|forward|reload|stop|navigate --url u|screenshot|window|pane",
    ),
    (
        "browser",
        "diff",
        "browser.diff",
        &["a", "b"],
        "<shotA> <shotB> [--threshold 0.1] [--force] [--out diff.png] — changed ratio, regions, diff image",
    ),
    (
        "screenshot",
        "list",
        "screenshot.list",
        &[],
        "[--task t] [--preview v4] [--run r] [--since 1h] [--limit 50]",
    ),
    (
        "screenshot",
        "get",
        "screenshot.get",
        &["id"],
        "<sN|id> [--out f.png] — environment, code state, running build, binding, path",
    ),
    (
        "screenshot",
        "open",
        "screenshot.open",
        &["id"],
        "<sN|id> — copy to a local temp file and open it",
    ),
    (
        "screenshot",
        "diff",
        "browser.diff",
        &["a", "b"],
        "<shotA> <shotB> [--threshold 0.1] [--force] [--out diff.png]",
    ),
    (
        "screenshot",
        "delete",
        "screenshot.delete",
        &["id"],
        "<sN|id> [--force] Human access only. Use --force for screenshots in an accepted review.",
    ),
    (
        "screenshot",
        "code-state",
        "screenshot.code_state",
        &["path"],
        "[dir] — Print local code state as JSON. Serve the result at /__vibeke_build.",
    ),
    (
        "desk",
        "search",
        "desk.search",
        &["text..."],
        "<words> [--repo dir] [--harness h] [--since 7d|YYYY-MM-DD] [--until …] [--limit n] [--sort recent]",
    ),
    (
        "desk",
        "sessions",
        "desk.sessions",
        &[],
        "[--repo dir] [--harness h] — live / resumable / neither",
    ),
    (
        "desk",
        "open",
        "desk.open",
        &["session"],
        "<session> [--turn n] [--focus] — Change focus only with --focus. Otherwise, show resume options.",
    ),
    (
        "desk",
        "resume",
        "desk.resume",
        &["session"],
        "<session> [--pane p] = Resume native session | --mode new_agent [--start --harness h] = Start new agent with context (an unsent draft)",
    ),
    (
        "desk",
        "context",
        "desk.context",
        &["session"],
        "<session> [--turns 3-5] [--objective text] — editable context package",
    ),
    (
        "desk",
        "forget",
        "desk.forget",
        &["session"],
        "<session> | --repo dir | --workspace w | --before date — purge from the conversation index",
    ),
    (
        "desk",
        "status",
        "desk.status",
        &[],
        "indexed sources, selection, exclusions, retention",
    ),
    (
        "desk",
        "index",
        "desk.index",
        &[],
        "run an indexing pass now",
    ),
    (
        "draft",
        "new",
        "draft.create",
        &["text"],
        "<text|-> [--workspace w | --task t] [--file path]… [--screenshot path]… [--title t]",
    ),
    (
        "draft",
        "list",
        "draft.list",
        &[],
        "[--workspace w | --task t] [--all]",
    ),
    ("draft", "show", "draft.get", &["draft"], ""),
    (
        "draft",
        "edit",
        "draft.update",
        &["draft"],
        "<draft> [--text t|-] [--title t] [--file path] [--remove-attachment i] [--expected-rev n]",
    ),
    (
        "draft",
        "check",
        "draft.check",
        &["draft"],
        "<draft> --run r — send_path prompt_input | open_pane_only and why",
    ),
    (
        "draft",
        "send",
        "draft.send",
        &["draft"],
        "<draft> --run r [--include-notes] [--keep] [--retry-despite-unknown] (zero bytes when unsafe)",
    ),
    (
        "draft",
        "reconcile",
        "draft.reconcile",
        &["draft"],
        "inspect an uncertain send before retrying",
    ),
    (
        "draft",
        "combine",
        "draft.combine",
        &["ids..."],
        "<draft> <draft>… [--title t] [--delete-sources]",
    ),
    (
        "draft",
        "reorder",
        "draft.reorder",
        &["order..."],
        "<draft>… in their new order",
    ),
    ("draft", "rm", "draft.delete", &["draft"], ""),
    (
        "notes",
        "get",
        "notes.get",
        &[],
        "[--workspace w] — never sent unless included",
    ),
    (
        "notes",
        "set",
        "notes.set",
        &["text"],
        "<text|-> [--workspace w]",
    ),
    (
        "assist",
        "status",
        "assistant.status",
        &[],
        "enabled/configured state, coordinator, profile, budgets, consents (no secrets)",
    ),
    (
        "assist",
        "providers",
        "assistant.providers",
        &[],
        "configured connections and profiles (no secrets)",
    ),
    (
        "assist",
        "consent",
        "assistant.consent",
        &["workspace"],
        "[workspace] [--connection c] [--classes selected_text,structured_state,review_package,screen] [--operations op,...] [--auto-send op,...]",
    ),
    (
        "assist",
        "revoke",
        "assistant.revoke",
        &["workspace"],
        "[workspace] [--connection c] — also cancels unfinished requests there",
    ),
    (
        "assist",
        "generate",
        "assistant.generate",
        &["operation"],
        "suggest_task_details|review_summary|pane_title|briefing|handoff|effort_estimate [--run r] [--turns 3,4] [--pane p] [--task t] [--workspace w] [--include-screen] — Show the exact payload. Do not send it.",
    ),
    (
        "assist",
        "confirm",
        "assistant.confirm",
        &["request", "preview_digest"],
        "<request> <preview-digest> — send the previewed payload",
    ),
    (
        "assist",
        "show",
        "assistant.get",
        &["request"],
        "<request> — lifecycle, usage, cost, sources and the generated draft",
    ),
    (
        "assist",
        "list",
        "assistant.list",
        &[],
        "[--workspace w] [--state done] [--limit 50]",
    ),
    (
        "assist",
        "cancel",
        "assistant.cancel",
        &["request"],
        "<request>",
    ),
    (
        "assist",
        "purge",
        "assistant.purge",
        &["request"],
        "<request> | --workspace w | --all — forget generated outputs",
    ),
    ("api", "methods", "api.methods", &[], "list API methods"),
    ("client", "list", "client.list", &[], ""),
    // Server security (09): policy rules, token revocation and elevation, the audit log.
    (
        "policy",
        "list",
        "policy.list",
        &[],
        "[--scope dir] — merged rules: config.toml, added with `policy add`, trusted repositories",
    ),
    (
        "policy",
        "add",
        "policy.add",
        &[],
        "--effect allow|deny|ask [--tool T] [--command-regex RE] [--path-glob G] [--url-glob G] [--scope dir] [--note text]",
    ),
    (
        "policy",
        "remove",
        "policy.remove",
        &["rule_id"],
        "<rule> — only rules added with `policy add` (p-…)",
    ),
    (
        "policy",
        "test",
        "policy.test",
        &[],
        "--tool T [--command C] [--path P] [--url U] [--scope dir] — what an approval would get (dry run)",
    ),
    (
        "pane",
        "revoke-token",
        "auth.revoke_token",
        &["pane"],
        "<pane> — Revoke the pane's API token. Its agent keeps running without API access until the pane restarts.",
    ),
    (
        "auth",
        "revoke-token",
        "auth.revoke_token",
        &["pane"],
        "<pane> — same as `pane revoke-token`",
    ),
    (
        "auth",
        "elevate",
        "auth.elevate",
        &["reason"],
        "[reason] [--timeout-ms 120000] — From a pane: ask the user for 10 minutes of full API access. Prints the token for VIBEKE_ELEVATED_TOKEN.",
    ),
    (
        "auth",
        "decide",
        "auth.elevate.decide",
        &["request", "decision"],
        "<request> approve|deny — Decide an elevation request. Run outside any pane.",
    ),
    (
        "auth",
        "list",
        "auth.list",
        &[],
        "pending elevation requests, live elevations, revoked panes",
    ),
    (
        "audit",
        "tail",
        "audit.tail",
        &[],
        "[--limit 50] [--types policy.*,auth.*]",
    ),
    (
        "audit",
        "search",
        "audit.search",
        &["query"],
        "<text> [--types t] [--since-ms ms] [--limit 200]",
    ),
    (
        "audit",
        "verify",
        "audit.verify",
        &[],
        "recompute the audit log's hash chain (also part of `vibeke doctor`)",
    ),
];

pub fn nouns() -> Vec<&'static str> {
    let mut v: Vec<&str> = COMMANDS.iter().map(|c| c.0).collect();
    let mut seen = std::collections::HashSet::new();
    v.retain(|n| seen.insert(*n));
    v
}

pub fn noun_help(noun: &str) -> String {
    let mut s = format!("vibeke {noun} <verb>\n\n");
    for (n, verb, method, pos, help) in COMMANDS {
        if *n == noun {
            let pos: Vec<String> = pos.iter().map(|p| format!("[{p}]")).collect();
            s.push_str(&format!(
                "  {verb:<14} {:<28} {method:<22} {help}\n",
                pos.join(" ")
            ));
        }
    }
    s
}

/// Parse a scalar CLI value: true/false, integers, JSON objects/arrays, else string.
fn scalar(v: &str) -> Value {
    match v {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if (v.starts_with('{') || v.starts_with('['))
        && let Ok(j) = serde_json::from_str(v)
    {
        return j;
    }
    if (!v.starts_with('0') || v == "0")
        && let Ok(n) = v.parse::<i64>()
    {
        return Value::from(n);
    }
    Value::String(v.to_string())
}

/// Build params from argv after `<noun> <verb>`.
pub fn build_params(positional: &[&str], args: &[String]) -> Result<Value, String> {
    let mut map = Map::new();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            pos.extend(args[i + 1..].iter().cloned());
            break;
        }
        if let Some(flag) = a.strip_prefix("--") {
            let (k, inline) = match flag.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (flag.to_string(), None),
            };
            if let Some(neg) = k.strip_prefix("no-") {
                map.insert(neg.replace('-', "_"), Value::Bool(false));
                i += 1;
                continue;
            }
            let key = k.replace('-', "_");
            let value = match inline {
                Some(v) => scalar(&v),
                None if i + 1 < args.len() && !args[i + 1].starts_with("--") => {
                    i += 1;
                    scalar(&args[i])
                }
                None => Value::Bool(true),
            };
            // Repeated flags accumulate into arrays.
            match map.get_mut(&key) {
                Some(Value::Array(a)) => a.push(value),
                Some(prev) => {
                    let p = prev.take();
                    *prev = Value::Array(vec![p, value]);
                }
                None => {
                    map.insert(key, value);
                }
            }
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    let mut pi = 0;
    for name in positional {
        if let Some(rest) = name.strip_suffix("...") {
            let rest_vals: Vec<Value> = pos[pi.min(pos.len())..]
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect();
            if !rest_vals.is_empty() {
                map.insert(rest.to_string(), Value::Array(rest_vals));
            }
            pi = pos.len();
            break;
        }
        if pi < pos.len() {
            map.entry(name.to_string())
                .or_insert_with(|| scalar(&pos[pi]));
            pi += 1;
        }
    }
    if pi < pos.len() {
        return Err(format!("unexpected argument `{}`", pos[pi]));
    }
    Ok(Value::Object(map))
}

/// Verb-specific param massaging so the CLI reads naturally.
fn adjust(method: &str, p: &mut Value) {
    let o = p.as_object_mut().expect("object");
    // Values given positionally as "@current"-style or numbers stay strings for targets.
    for k in [
        "pane",
        "tab",
        "workspace",
        "target",
        "interaction",
        "task",
        "run",
        "name",
        "title",
        "text",
        "machine",
        "label",
        "session",
        "key",
        "selector",
        "expression",
        "q",
        "group",
        "mode",
        "layout",
        "draft",
    ] {
        if let Some(v) = o.get_mut(k)
            && (v.is_number() || v.is_boolean())
        {
            *v = Value::String(v.to_string());
        }
    }
    if o.get("current").and_then(Value::as_bool) == Some(true) {
        o.remove("current");
        o.insert("pane".into(), json!("@current"));
    }
    match method {
        "browser.open" => {
            if let Some(Value::String(t)) = o.remove("target") {
                let k = if t.contains("://") { "url" } else { "preview" };
                o.entry(k).or_insert(json!(t));
            }
            if let Some(Value::String(v)) = o.get("viewport").cloned() {
                o.insert("viewport".into(), json!(v));
            }
        }
        "browser.screenshot" => {
            // `screenshot <session|preview|url>`: `b3` is a session, `v4` a preview, anything
            // with a scheme a URL (those two are one-shot captures).
            if let Some(Value::String(t)) = o.get("session").cloned()
                && !t.starts_with('b')
            {
                o.remove("session");
                let k = if t.contains("://") { "url" } else { "preview" };
                o.entry(k).or_insert(json!(t));
            }
        }
        "browser.type" => {
            // `type <session> <text>`: one positional after the session is the text.
            if !o.contains_key("text")
                && let Some(sel) = o.remove("selector")
            {
                o.insert("text".into(), sel);
            }
        }
        "browser.network" => {
            if let Some(f) = o.remove("failed") {
                o.insert("failed_only".into(), f);
            }
        }
        "browser.diff" | "screenshot.get" | "screenshot.open" | "screenshot.delete" => {
            for k in ["a", "b", "id"] {
                if let Some(v) = o.get_mut(k)
                    && (v.is_number() || v.is_boolean())
                {
                    *v = Value::String(v.to_string());
                }
            }
        }
        "desk.search" => {
            if let Some(Value::Array(words)) = o.get("text").cloned() {
                let t: Vec<&str> = words.iter().filter_map(Value::as_str).collect();
                o.insert("text".into(), json!(t.join(" ")));
            }
        }
        "draft.create" | "draft.update" | "notes.set" | "draft.list" => {
            if o.get("text").and_then(Value::as_str) == Some("-") {
                let mut s = String::new();
                let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
                o.insert("text".into(), json!(s));
            }
            if method != "notes.set" {
                if let Some(t) = o.remove("task") {
                    o.insert("scope".into(), json!("task"));
                    o.insert("id".into(), t);
                } else if let Some(w) = o.remove("workspace") {
                    o.insert("id".into(), w);
                }
            }
            let abs = |v: &Value| {
                let s = v.as_str().unwrap_or("");
                std::fs::canonicalize(s)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| s.to_string())
            };
            let mut att: Vec<Value> = vec![];
            for (flag, kind) in [("file", "file"), ("screenshot", "screenshot")] {
                let items = match o.remove(flag) {
                    Some(Value::Array(a)) => a,
                    Some(v) => vec![v],
                    None => vec![],
                };
                att.extend(items.iter().map(|v| json!({"kind": kind, "path": abs(v)})));
            }
            if method == "draft.update" {
                if let Some(first) = att.into_iter().next() {
                    o.insert("add_attachment".into(), first);
                }
            } else if !att.is_empty() {
                o.insert("attachments".into(), json!(att));
            }
        }
        "draft.send" | "draft.check" => {
            if let Some(r) = o.remove("run") {
                o.insert("target_run".into(), r);
            }
            if method == "draft.send" && !o.contains_key("idempotency_key") {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                o.insert(
                    "idempotency_key".into(),
                    json!(format!("cli-{}-{nanos}", std::process::id())),
                );
            }
        }
        "interaction.answer" => {
            for (flag, d) in [
                ("allow", "allow"),
                ("deny", "deny"),
                ("allow_always", "allow_always"),
            ] {
                if o.remove(flag).and_then(|v| v.as_bool()) == Some(true) {
                    o.insert("decision".into(), json!(d));
                }
            }
            if let Some(c) = o.remove("choice") {
                let items: Vec<Value> = match c {
                    Value::Array(a) => a,
                    v => vec![v],
                };
                let mut choices = Map::new();
                for it in items {
                    if let Some((q, opt)) = it.as_str().and_then(|s| s.split_once('=')) {
                        let e = choices.entry(q.to_string()).or_insert(json!([]));
                        e.as_array_mut().unwrap().push(json!(opt));
                    }
                }
                o.insert("choices".into(), Value::Object(choices));
            }
        }
        "agent.wait" => {
            if let Some(Value::String(u)) = o.get("until").cloned() {
                o.insert("until".into(), json!(u.split(',').collect::<Vec<_>>()));
            }
        }
        "agent.start" | "agent.spawn" => {
            if let Some(Value::String(a)) = o.get("args").cloned() {
                o.insert("args".into(), json!(a.split(',').collect::<Vec<_>>()));
            }
        }
        "task.create" => {
            if let Some(a) = o.remove("agent") {
                let items: Vec<Value> = match a {
                    Value::Array(a) => a,
                    v => vec![v],
                };
                let agents: Vec<Value> = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| match s.split_once(':') {
                        Some((h, n)) => json!({"harness": h, "name": n}),
                        None => json!({"harness": s}),
                    })
                    .collect();
                o.insert("agents".into(), json!(agents));
            }
            if !o.contains_key("repo") {
                o.insert(
                    "repo".into(),
                    json!(
                        std::env::current_dir()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                );
            }
        }
        "workspace.create" | "tab.create" | "pane.split" => {
            if let Some(Value::String(c)) = o.get("cwd").cloned() {
                let abs = std::fs::canonicalize(&c)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(c);
                o.insert("cwd".into(), json!(abs));
            } else if method == "workspace.create" && o.contains_key("layout") {
                // A layout's own cwd wins; the caller's cwd is only the fallback.
                o.insert(
                    "default_cwd".into(),
                    json!(
                        std::env::current_dir()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                );
            } else if method == "workspace.create" {
                o.insert(
                    "cwd".into(),
                    json!(
                        std::env::current_dir()
                            .map(|d| d.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                );
            }
        }
        // `layout apply <file.toml>` / `--file f`: the server parses the text (07 §2.14).
        "layout.apply" => {
            let file = o
                .remove("file")
                .and_then(|v| v.as_str().map(str::to_string))
                .or_else(|| {
                    o.get("name")
                        .and_then(Value::as_str)
                        .filter(|n| {
                            (n.ends_with(".toml") || n.ends_with(".json") || n.contains('/'))
                                && std::path::Path::new(n).is_file()
                        })
                        .map(str::to_string)
                });
            if let Some(f) = file {
                o.remove("name");
                match std::fs::read_to_string(&f) {
                    Ok(text) => {
                        o.insert("doc".into(), json!(text));
                    }
                    Err(e) => {
                        o.insert("doc".into(), json!(format!("# unreadable {f}: {e}")));
                    }
                }
            }
            if let Some(Value::String(c)) = o.get("cwd").cloned() {
                let abs = std::fs::canonicalize(&c)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(c);
                o.insert("cwd".into(), json!(abs));
            }
            o.insert(
                "default_cwd".into(),
                json!(
                    std::env::current_dir()
                        .map(|d| d.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
            );
        }
        "client.focus" => {
            if let Some(Value::String(t)) = o.get("pane").cloned()
                && t.starts_with("vibeke://")
            {
                o.remove("pane");
                o.insert("url".into(), json!(t));
            }
        }
        "blob.put" => {
            if let Some(Value::String(path)) = o.get("path").cloned() {
                let abs = std::fs::canonicalize(&path)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(path);
                o.insert("path".into(), json!(abs));
            }
        }
        _ => {}
    }
}

pub fn exit_code_for(e: &CallError) -> i32 {
    match e {
        CallError::Rpc(r) if r.data.kind == "timeout" => EXIT_TIMEOUT,
        CallError::Rpc(r) if r.data.kind == "permission_denied" || r.data.kind == "untrusted" => {
            EXIT_PERMISSION
        }
        _ => EXIT_API,
    }
}

pub fn print_error(e: &CallError) {
    match e {
        CallError::Rpc(r) => eprintln!(
            "{}",
            json!({"error": {"kind": r.data.kind, "message": r.message, "details": r.data.details}})
        ),
        CallError::Io(err) => eprintln!(
            "{}",
            json!({"error": {"kind": "io", "message": format!("{err:#}")}})
        ),
    }
}

/// Human output for a few list results; everything else is pretty JSON.
pub fn pretty(method: &str, v: &Value) -> String {
    let rows = |key: &str| {
        v.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    match method {
        "workspace.list" => rows("workspaces")
            .iter()
            .map(|w| {
                format!(
                    "{:<5} {:<24} {}",
                    w["handle"].as_str().unwrap_or(""),
                    w["name"].as_str().unwrap_or(""),
                    w["root_path"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "pane.list" => rows("panes")
            .iter()
            .map(|p| {
                let agent = p
                    .get("agent")
                    .filter(|a| !a.is_null())
                    .map(|a| {
                        format!(
                            "{} {}",
                            a["harness"].as_str().unwrap_or(""),
                            a["state"].as_str().unwrap_or("")
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "{:<10} {:<20} {:<40} {}",
                    p["handle"].as_str().unwrap_or(""),
                    p["auto_title"].as_str().unwrap_or(""),
                    p["cwd"].as_str().unwrap_or(""),
                    agent
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "agent.list" => rows("runs")
            .iter()
            .map(|r| {
                let src = r["execution"]["source"].as_str().unwrap_or("");
                let inferred = if src == "Structured" || src == "SelfReport" {
                    ""
                } else {
                    "~"
                };
                format!(
                    "{:<5} {:<10} {:<8} {:<13} {:<10} {}",
                    r["handle"].as_str().unwrap_or(""),
                    r["name"].as_str().unwrap_or("-"),
                    r["harness"].as_str().unwrap_or(""),
                    format!(
                        "{}{inferred}",
                        r["execution"]["value"]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase()
                    ),
                    r["pane_handle"].as_str().unwrap_or(""),
                    r["last_message"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "interaction.list" => rows("interactions")
            .iter()
            .map(|i| {
                format!(
                    "{:<5} {:<10} {:<10} {}",
                    i["handle"].as_str().unwrap_or(""),
                    i["kind"].as_str().unwrap_or(""),
                    i["pane_handle"].as_str().unwrap_or(""),
                    i["title"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "screenshot.list" => rows("screenshots")
            .iter()
            .map(|m| {
                let code = match (
                    m["code"]["head_sha"].as_str(),
                    m["code"]["dirty_state"].as_str(),
                ) {
                    (Some(h), Some("dirty")) => format!("{}+dirty", &h[..h.len().min(7)]),
                    (Some(h), _) => h[..h.len().min(7)].to_string(),
                    _ => "-".into(),
                };
                format!(
                    "{:<5} {:<36} {:<12} {:<14} {}",
                    m["handle"].as_str().unwrap_or(""),
                    m["label"].as_str().unwrap_or(""),
                    m["binding"].as_str().unwrap_or(""),
                    code,
                    m["final_url"].as_str().or(m["url"].as_str()).unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "assistant.generate" => {
            let pv = &v["preview"];
            let r = &v["request"];
            let mut out = format!(
                "{} — {} via {} ({}) on {}\n{} bytes, ~{} input tokens, max {} output tokens, {} redaction(s)\n{}\n\n--- system ---\n{}\n--- user ---\n{}\n",
                r["id"].as_str().unwrap_or(""),
                r["operation"].as_str().unwrap_or(""),
                pv["model"].as_str().unwrap_or(""),
                pv["endpoint_host"].as_str().unwrap_or(""),
                pv["execution_machine"].as_str().unwrap_or(""),
                pv["bytes"],
                pv["estimated_input_tokens"],
                pv["max_output_tokens"],
                pv["redactions"],
                pv["notice"].as_str().unwrap_or(""),
                pv["system"].as_str().unwrap_or(""),
                pv["user"].as_str().unwrap_or(""),
            );
            if v["requires_confirmation"] == true {
                out.push_str(&format!(
                    "\nNothing has been sent. To send exactly this: vibeke assist confirm {} {}",
                    r["id"].as_str().unwrap_or(""),
                    pv["digest"].as_str().unwrap_or("")
                ));
            } else {
                out.push_str(
                    "\nSent automatically (auto_send is enabled for this operation and workspace).",
                );
            }
            out
        }
        "assistant.list" => rows("requests")
            .iter()
            .map(|r| {
                format!(
                    "{:<32} {:<22} {:<22} {:<24} {}",
                    r["id"].as_str().unwrap_or(""),
                    r["operation"].as_str().unwrap_or(""),
                    r["state"].as_str().unwrap_or(""),
                    r["model"].as_str().unwrap_or(""),
                    r["workspace"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "pane.read" | "agent.read" => v["text"].as_str().unwrap_or("").to_string(),
        "preview.open" if v["opened_in"] == "proxy" => {
            let mut out = format!(
                "{} via the preview proxy: {}",
                v["preview"].as_str().unwrap_or(""),
                v["url"].as_str().unwrap_or("")
            );
            if v["tls_origin"] == true {
                out.push_str(&format!(
                    "\nhttps origin signed by the local preview CA ({}, sha256 {}); the browser must trust it: `vibeke preview trust-ca`",
                    v["ca"]["path"].as_str().unwrap_or("?"),
                    v["ca"]["sha256"].as_str().unwrap_or("?"),
                ));
            }
            if v["opened"] != true
                && let Some(u) = v["open_url"].as_str()
            {
                out.push_str(&format!(
                    "\none-time login link ({} s): {u}",
                    v["token_ttl_s"].as_u64().unwrap_or(60)
                ));
            }
            out
        }
        "preview.mirror" => format!(
            "{}/{} mirrored on {} — {}",
            v["machine"].as_str().unwrap_or(""),
            v["preview_handle"].as_str().unwrap_or(""),
            v["url"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("localhost:{}", v["local_port"])),
            v["warning"].as_str().unwrap_or("")
        ),
        _ => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// Run one API command. Returns the process exit code.
pub async fn run_api<S>(client: &mut Client<S>, g: &Global, method: &str, mut params: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    adjust(method, &mut params);
    // `browser screenshot|diff --out f.png`, `screenshot get --out`: fetch the image inline and
    // write it here (the server never writes to caller-chosen paths). `screenshot open` writes
    // a temp file and opens it.
    let images = matches!(
        method,
        "browser.screenshot" | "browser.diff" | "screenshot.get" | "screenshot.open"
    );
    let mut out = images
        .then(|| params.as_object_mut().and_then(|o| o.remove("out")))
        .flatten()
        .and_then(|v| v.as_str().map(PathBuf::from));
    let open = method == "screenshot.open";
    if open && out.is_none() {
        let dir = std::env::temp_dir().join("vibeke-screenshots");
        let _ = std::fs::create_dir_all(&dir);
        let name = params
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("screenshot")
            .replace(['/', '\\'], "_");
        out = Some(dir.join(format!("{name}.png")));
    }
    if out.is_some() {
        params["inline"] = json!(true);
    }
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    match client.call(method, params).await {
        Ok(mut v) => {
            if let Some(path) = &out {
                use base64::Engine as _;
                let data = v
                    .as_object_mut()
                    .and_then(|o| o.remove("data_b64"))
                    .and_then(|d| d.as_str().map(str::to_string))
                    .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok());
                match data {
                    Some(bytes) => {
                        if let Err(e) = std::fs::write(path, bytes) {
                            eprintln!("write {}: {e}", path.display());
                            return EXIT_API;
                        }
                        v["out"] = json!(path);
                        if open {
                            v["opened"] = json!(open_locally(path));
                        }
                    }
                    None => {
                        eprintln!(
                            "the screenshot was not returned inline (too large?); it is at {}",
                            v["path_on_machine"]
                        );
                        return EXIT_API;
                    }
                }
            }
            if !g.quiet {
                let as_json = g.json.unwrap_or(!std::io::stdout().is_terminal());
                if as_json {
                    println!("{}", serde_json::to_string(&v).unwrap_or_default());
                } else {
                    println!("{}", pretty(method, &v));
                }
            }
            EXIT_OK
        }
        Err(e) => {
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

/// Open a local file with the platform opener (`open` / `xdg-open`). `VIBEKE_NO_OPEN=1` (tests,
/// headless sessions) only reports the path. Returns whether an opener was started.
fn open_locally(path: &std::path::Path) -> bool {
    if std::env::var_os("VIBEKE_NO_OPEN").is_some() {
        return false;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// `vibeke screenshot code-state [dir]`: the checkout's code state (06 B6 `CodeState`) as JSON,
/// computed locally with read-only git. A dev/build script can serve this at
/// `/__vibeke_build` so screenshots of that build become `bound` (15 §6.4).
pub fn code_state(args: &[String]) -> i32 {
    let dir = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    match vk_review::screenshot::capture_code_state(&dir) {
        Ok(c) => {
            println!("{}", serde_json::to_string(&c).unwrap_or_default());
            EXIT_OK
        }
        Err(e) => {
            eprintln!(
                "{}",
                json!({"error": {"kind": "not_a_checkout", "message": e.to_string(), "path": dir}})
            );
            EXIT_API
        }
    }
}

/// `vibeke browser install`: show the plan, ask (or require `--yes` without a terminal), then
/// install with `confirm: true` (06 B5: installing a browser always asks first).
pub async fn browser_install<S>(client: &mut Client<S>, g: &Global, mut params: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let yes = params
        .as_object_mut()
        .and_then(|o| o.remove("yes").or_else(|| o.remove("y")))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let plan = match client.call("browser.install", params.clone()).await {
        Ok(v) => v["plan"].clone(),
        Err(e) => {
            print_error(&e);
            return exit_code_for(&e);
        }
    };
    if plan["installed"] == true {
        println!(
            "already installed: {}",
            plan["binary"].as_str().unwrap_or("")
        );
        return EXIT_OK;
    }
    let where_ = g.machine.as_deref().unwrap_or("this machine");
    eprintln!(
        "vibeke browser install will download chrome-headless-shell {} ({}) onto {where_}:\n  from {}\n  into {}\n  sha256 {}",
        plan["version"].as_str().unwrap_or("?"),
        plan["platform"].as_str().unwrap_or("?"),
        plan["url"].as_str().unwrap_or("?"),
        plan["dir"].as_str().unwrap_or("?"),
        plan["sha256"]
            .as_str()
            .unwrap_or("(none recorded: pass --sha256 <hex> after verifying the download)"),
    );
    if plan["checksum_known"] != true {
        return EXIT_USAGE;
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("not a terminal: rerun with --yes to download");
            return EXIT_USAGE;
        }
        eprint!("Download and install? [y/N] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes")
        {
            eprintln!("cancelled");
            return EXIT_OK;
        }
    }
    params["confirm"] = json!(true);
    match client.call("browser.install", params).await {
        Ok(v) => {
            if !g.quiet {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            EXIT_OK
        }
        Err(e) => {
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

pub const FORGET_USAGE: &str = "vibeke forget --pane <p> | --workspace <w> | --before <time> | --all  [--yes] [--dry-run]\n  Deletes archived scrollback (segments, search index rows, archive metadata) for the scope.\n  Does not delete the event log, blobs, the session desk index, drafts, notes, or what a live pane still holds in memory.\n  --before takes a date, an RFC 3339 time or a duration back from now (7d, 12h); it is segment-granular.";

/// `vibeke forget`: preview the scope with `scrollback.forget {dry_run}`, ask (or require
/// `--yes` without a terminal), then delete. Idempotent.
pub async fn forget<S>(client: &mut Client<S>, g: &Global, mut params: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let flag = |params: &mut Value, names: &[&str]| {
        names
            .iter()
            .filter_map(|n| params.as_object_mut().and_then(|o| o.remove(*n)))
            .any(|v| v.as_bool().unwrap_or(false))
    };
    let yes = flag(&mut params, &["yes", "y"]);
    let dry = flag(&mut params, &["dry_run"]);
    for k in ["pane", "workspace"] {
        if let Some(v) = params.get_mut(k)
            && v.is_number()
        {
            *v = json!(v.to_string());
        }
    }
    let scopes = ["pane", "workspace", "before", "all"]
        .iter()
        .filter(|k| {
            params
                .get(**k)
                .is_some_and(|v| !v.is_null() && *v != json!(false))
        })
        .count();
    if scopes != 1 || params.as_object().is_some_and(|o| o.len() != 1) {
        eprintln!("{FORGET_USAGE}");
        return EXIT_USAGE;
    }
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let mut plan_params = params.clone();
    plan_params["dry_run"] = json!(true);
    let plan = match client.call("scrollback.forget", plan_params).await {
        Ok(v) => v,
        Err(e) => {
            print_error(&e);
            return exit_code_for(&e);
        }
    };
    let n = |k: &str| plan[k].as_u64().unwrap_or(0);
    let empty =
        n("segments_deleted") == 0 && n("fts_rows_deleted") == 0 && n("archive_panes_dropped") == 0;
    let where_ = g.machine.as_deref().unwrap_or("this machine");
    eprintln!(
        "vibeke forget {} on {where_} {} {} segments ({} bytes) of {} panes, {} search rows and {} archive records.",
        plan["scope"],
        if dry { "would delete" } else { "will delete" },
        n("segments_deleted"),
        n("bytes_deleted"),
        n("panes"),
        n("fts_rows_deleted"),
        n("archive_panes_dropped"),
    );
    if dry {
        if !g.quiet {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).unwrap_or_default()
            );
        }
        return EXIT_OK;
    }
    if empty {
        eprintln!("nothing to forget");
        return EXIT_OK;
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("not a terminal: rerun with --yes to delete");
            return EXIT_USAGE;
        }
        eprint!("This cannot be undone. Delete? [y/N] ");
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes")
        {
            eprintln!("cancelled");
            return EXIT_OK;
        }
    }
    // Execute exactly the plan that was shown: its resolved scope (pane/workspace id, absolute
    // cutoff) and digest, never the original `@focused` or relative `--before` again. The
    // server refuses if the scope no longer resolves to that plan.
    match client
        .call("scrollback.forget", forget_confirmed_params(&plan))
        .await
    {
        Ok(v) => {
            if !g.quiet {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            EXIT_OK
        }
        Err(e) => {
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

/// The confirmed `scrollback.forget` call for a dry run's result: its canonical `scope` plus
/// its `plan` digest.
fn forget_confirmed_params(plan: &Value) -> Value {
    let mut p = match &plan["scope"] {
        Value::Object(o) => Value::Object(o.clone()),
        _ => json!({}),
    };
    if let Some(d) = plan["plan"].as_str() {
        p["plan"] = json!(d);
    }
    p
}

/// `vibeke preview show <handle>`: print the newest screenshot of a preview inline when the
/// terminal can show it ([`show::detect`]), else its path and metadata.
pub async fn preview_show<S>(client: &mut Client<S>, g: &Global, params: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let tty = std::io::stdout().is_terminal();
    let mode = show::detect(&|k| std::env::var(k).ok(), tty);
    let mut out = std::io::stdout().lock();
    preview_show_to(client, g, params, mode, tty, &mut out).await
}

/// [`preview_show`] with the terminal decision and the output made explicit (tests).
pub async fn preview_show_to<S>(
    client: &mut Client<S>,
    g: &Global,
    params: Value,
    mode: show::Mode,
    tty: bool,
    out: &mut dyn std::io::Write,
) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some(handle) = params.get("preview").and_then(Value::as_str) else {
        eprintln!("usage: vibeke preview show <v4|devbox/v4> [--no-image]");
        return EXIT_USAGE;
    };
    let handle = handle.rsplit('/').next().unwrap_or(handle).to_string();
    let no_image = params
        .get("no_image")
        .or_else(|| params.get("no-image"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let list = match client
        .call("screenshot.list", json!({"preview": handle, "limit": 1}))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            print_error(&e);
            return exit_code_for(&e);
        }
    };
    let Some(newest) = list["screenshots"].as_array().and_then(|a| a.first()) else {
        eprintln!(
            "no screenshots of {handle} yet — take one with `vibeke browser screenshot {handle}`"
        );
        return EXIT_API;
    };
    let id = newest["id"].as_str().unwrap_or("").to_string();
    let as_json = g.json.unwrap_or(!tty);
    let mode = if no_image || as_json {
        show::Mode::Text
    } else {
        mode
    };
    let inline = mode != show::Mode::Text;
    let shot = match client
        .call("screenshot.get", json!({"id": id, "inline": inline}))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            print_error(&e);
            return exit_code_for(&e);
        }
    };
    if g.quiet {
        return EXIT_OK;
    }
    if as_json {
        let _ = writeln!(out, "{}", serde_json::to_string(&shot).unwrap_or_default());
        return EXIT_OK;
    }
    let png = shot["data_b64"].as_str().and_then(|d| {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(d).ok()
    });
    let mut shown = false;
    if let Some(png) = png.filter(|_| inline) {
        let name = format!("{}.png", shot["handle"].as_str().unwrap_or("screenshot"));
        let bytes = show::image_bytes(mode, &png, &name, show::terminal_cols().min(100));
        shown = out.write_all(&bytes).and_then(|_| out.flush()).is_ok();
    }
    let _ = writeln!(out, "{}", show::describe(&shot, shown));
    EXIT_OK
}

/// Look up `(method, positional)` for `noun verb`.
/// Methods that act on the *viewing* machine (they launch a local browser or manage local
/// profiles) even when `--machine m` is given: the CLI sends them to the local server with
/// `machine: m` instead of forwarding them to `m` (06 B3).
pub fn runs_on_viewing_machine(method: &str) -> bool {
    matches!(
        method,
        "preview.open"
            | "preview.profile"
            | "preview.status"
            | "preview.url"
            | "preview.mirror"
            | "preview.unmirror"
    )
}

pub fn lookup(noun: &str, verb: &str) -> Option<(&'static str, &'static [&'static str])> {
    COMMANDS
        .iter()
        .find(|c| c.0 == noun && c.1 == verb)
        .map(|c| (c.2, c.3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_from_flags_and_positionals() {
        let args: Vec<String> = ["w1:p1", "--direction", "down", "--no-focus", "--ratio=0.3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = build_params(&["pane"], &args).unwrap();
        assert_eq!(
            p,
            json!({"pane": "w1:p1", "direction": "down", "focus": false, "ratio": "0.3"})
        );
        let args: Vec<String> = ["w1:p1", "ctrl+c", "enter"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = build_params(&["pane", "keys..."], &args).unwrap();
        assert_eq!(p["keys"], json!(["ctrl+c", "enter"]));
        let mut p = build_params(&["interaction"], &["i3".into(), "--allow".into()]).unwrap();
        adjust("interaction.answer", &mut p);
        assert_eq!(p, json!({"interaction": "i3", "decision": "allow"}));
        assert!(build_params(&[], &["x".into()]).is_err());
    }

    #[test]
    fn browser_params() {
        let (m, pos) = lookup("browser", "open").unwrap();
        let mut p =
            build_params(pos, &["v4".into(), "--viewport".into(), "390x844".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p, json!({"preview": "v4", "viewport": "390x844"}));
        let mut p = build_params(pos, &["http://localhost:3000/x".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p, json!({"url": "http://localhost:3000/x"}));
        let (m, pos) = lookup("browser", "type").unwrap();
        let mut p = build_params(pos, &["b1".into(), "hello".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p, json!({"session": "b1", "text": "hello"}));
        let mut p = build_params(
            pos,
            &["b1".into(), "#q".into(), "42".into(), "--submit".into()],
        )
        .unwrap();
        adjust(m, &mut p);
        assert_eq!(
            p,
            json!({"session": "b1", "selector": "#q", "text": "42", "submit": true})
        );
        let (m, pos) = lookup("browser", "press").unwrap();
        let mut p = build_params(pos, &["b1".into(), "1".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p["key"], "1");
        let (m, pos) = lookup("browser", "network").unwrap();
        let mut p = build_params(pos, &["b1".into(), "--failed".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p, json!({"session": "b1", "failed_only": true}));
    }

    #[test]
    fn draft_and_desk_params() {
        let (m, pos) = lookup("draft", "new").unwrap();
        let mut p = build_params(
            pos,
            &[
                "hello".into(),
                "--task".into(),
                "t1".into(),
                "--screenshot".into(),
                "/tmp".into(),
            ],
        )
        .unwrap();
        adjust(m, &mut p);
        assert_eq!(p["scope"], "task");
        assert_eq!(p["id"], "t1");
        assert_eq!(p["attachments"][0]["kind"], "screenshot");
        let (m, pos) = lookup("draft", "send").unwrap();
        let mut p = build_params(pos, &["d1".into(), "--run".into(), "r1".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p["target_run"], "r1");
        assert!(p["idempotency_key"].as_str().unwrap().starts_with("cli-"));
        let (m, pos) = lookup("desk", "search").unwrap();
        let mut p = build_params(pos, &["login".into(), "redirect".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p["text"], "login redirect");
        let (_, pos) = lookup("draft", "combine").unwrap();
        let p = build_params(pos, &["a".into(), "b".into()]).unwrap();
        assert_eq!(p["ids"], json!(["a", "b"]));
    }

    #[test]
    fn every_command_has_a_unique_noun_verb() {
        let mut seen = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(seen.insert((c.0, c.1)), "duplicate {} {}", c.0, c.1);
        }
    }

    /// Review finding 6: the confirmed call executes the dry run's canonical plan (resolved pane
    /// id, absolute cutoff, digest), not `@focused`/a relative `--before` evaluated again, so
    /// a focus change while the prompt is open can't redirect the deletion.
    #[tokio::test]
    async fn forget_confirms_the_canonical_plan() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let g = Global {
            session: "t".into(),
            machine: None,
            socket: None,
            json: Some(false),
            quiet: true,
            no_spawn: true,
            timeout_ms: None,
        };
        for (args, scope) in [
            (
                json!({"pane": "@focused", "yes": true}),
                json!({"pane": "PANE-A"}),
            ),
            (
                json!({"before": "7d", "yes": true}),
                json!({"before": 1_700_000_000_000_i64}),
            ),
        ] {
            let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            let rec = seen.clone();
            let plan_scope = scope.clone();
            tokio::spawn(async move {
                let (rd, mut wr) = tokio::io::split(theirs);
                let mut lines = BufReader::new(rd).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&l).unwrap();
                    let result = if req["method"] == "scrollback.forget" {
                        rec.lock().unwrap().push(req["params"].clone());
                        json!({"scope": plan_scope, "pane_ids": null, "plan": "fp1-abc",
                            "dry_run": req["params"]["dry_run"] == true, "panes": 1,
                            "segments_deleted": 2, "bytes_deleted": 10, "fts_rows_deleted": 5,
                            "archive_panes_dropped": 1})
                    } else {
                        json!({})
                    };
                    let mut s = serde_json::to_string(
                        &json!({"jsonrpc": "2.0", "id": req["id"], "result": result}),
                    )
                    .unwrap();
                    s.push('\n');
                    wr.write_all(s.as_bytes()).await.unwrap();
                }
            });
            let mut c = Client::new(ours);
            assert_eq!(forget(&mut c, &g, args.clone()).await, EXIT_OK);
            let calls = seen.lock().unwrap().clone();
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert_eq!(calls[0]["dry_run"], true);
            let mut want = scope.clone();
            want["plan"] = json!("fp1-abc");
            assert_eq!(calls[1], want, "{args}");
        }
    }

    /// Review finding 9: page-controlled metadata (an escape-bearing build id inside
    /// `binding_reason`, the page's URL) is printed escaped in text and image modes, while the
    /// intentional image-protocol sequences stay intact.
    #[tokio::test]
    async fn preview_show_escapes_page_controlled_metadata() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let evil = "\x1b]52;c;cHduZWQ=\x07\x1b[2J\u{9b}31m";
        let spawn = || {
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let (rd, mut wr) = tokio::io::split(theirs);
                let mut lines = BufReader::new(rd).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&l).unwrap();
                    let result = match req["method"].as_str().unwrap() {
                        "screenshot.list" => json!({"screenshots": [{"id": "S9"}], "count": 1}),
                        "screenshot.get" => json!({"id": "S9", "handle": "s9",
                            "label": format!("devbox{evil}"),
                            "url": format!("http://localhost:5173/{evil}"),
                            "binding": "illustrative",
                            "binding_reason": format!("Build not verified: build id {evil} is not tied to a checkout state"),
                            "created_at_ms": 0, "path_on_machine": format!("/state/blobs/{evil}.png"),
                            "data_b64": "iVBORw0KGgo="}),
                        _ => json!({}),
                    };
                    let mut s = serde_json::to_string(
                        &json!({"jsonrpc": "2.0", "id": req["id"], "result": result}),
                    )
                    .unwrap();
                    s.push('\n');
                    wr.write_all(s.as_bytes()).await.unwrap();
                }
            });
            Client::new(ours)
        };
        let g = Global {
            session: "t".into(),
            machine: None,
            socket: None,
            json: Some(false),
            quiet: false,
            no_spawn: true,
            timeout_ms: None,
        };
        for mode in [show::Mode::Kitty, show::Mode::Iterm, show::Mode::Text] {
            let mut c = spawn();
            let mut out = Vec::new();
            let code =
                preview_show_to(&mut c, &g, json!({"preview": "v4"}), mode, true, &mut out).await;
            assert_eq!(code, EXIT_OK);
            let text = String::from_utf8_lossy(&out).to_string();
            // The image protocol's own escapes come first (graphics modes) and are intact.
            let meta = match mode {
                show::Mode::Kitty => {
                    assert!(text.starts_with("\x1b_Ga=T,"), "{text:?}");
                    let end = text.find("\x1b\\\n").unwrap() + 3;
                    &text[end..]
                }
                show::Mode::Iterm => {
                    assert!(text.starts_with("\x1b]1337;File=inline=1;"), "{text:?}");
                    let end = text.find("\x07\n").unwrap() + 2;
                    &text[end..]
                }
                show::Mode::Text => text.as_str(),
            };
            assert!(
                !meta.chars().any(|c| c.is_control() && c != '\n'),
                "{mode:?}: {meta:?}"
            );
            assert!(meta.contains("build id \\x1b]52;c;"), "{meta}");
            assert!(meta.contains("\\u{9b}31m"), "{meta}");
        }
    }

    /// `preview show`: newest screenshot of the preview, drawn inline for graphics terminals,
    /// path + metadata otherwise; `--json`/pipes get the record.
    #[tokio::test]
    async fn preview_show_draws_or_describes() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let have = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let spawn = |seen: Arc<Mutex<Vec<(String, Value)>>>,
                     have: Arc<std::sync::atomic::AtomicBool>| {
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let (rd, mut wr) = tokio::io::split(theirs);
                let mut lines = BufReader::new(rd).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&l).unwrap();
                    let m = req["method"].as_str().unwrap().to_string();
                    seen.lock()
                        .unwrap()
                        .push((m.clone(), req["params"].clone()));
                    let result = match m.as_str() {
                        "screenshot.list" if have.load(std::sync::atomic::Ordering::SeqCst) => {
                            json!({"screenshots": [{"id": "S9", "handle": "s9"}], "count": 1})
                        }
                        "screenshot.list" => json!({"screenshots": [], "count": 0}),
                        "screenshot.get" => json!({"id": "S9", "handle": "s9",
                            "label": "devbox · headless · fresh context", "url": "http://localhost:5173/",
                            "binding": "bound", "created_at_ms": 0,
                            "path_on_machine": "/state/blobs/ab/x.png",
                            "data_b64": "iVBORw0KGgo="}),
                        _ => json!({}),
                    };
                    let mut s = serde_json::to_string(
                        &json!({"jsonrpc": "2.0", "id": req["id"], "result": result}),
                    )
                    .unwrap();
                    s.push('\n');
                    wr.write_all(s.as_bytes()).await.unwrap();
                }
            });
            Client::new(ours)
        };
        let g = Global {
            session: "t".into(),
            machine: None,
            socket: None,
            json: Some(false),
            quiet: false,
            no_spawn: true,
            timeout_ms: None,
        };
        // Kitty terminal: the image goes through the escape, then the metadata line.
        let mut c = spawn(seen.clone(), have.clone());
        let mut out = Vec::new();
        let code = preview_show_to(
            &mut c,
            &g,
            json!({"preview": "devbox/v4"}),
            show::Mode::Kitty,
            true,
            &mut out,
        )
        .await;
        assert_eq!(code, EXIT_OK);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.starts_with("\x1b_Ga=T,"), "{text:?}");
        assert!(text.contains("s9 · devbox · headless · fresh context"));
        assert!(
            !text.contains("image: "),
            "the image was shown, no path needed"
        );
        {
            let s = seen.lock().unwrap();
            let list = s.iter().find(|(m, _)| m == "screenshot.list").unwrap();
            assert_eq!(list.1, json!({"preview": "v4", "limit": 1}));
            let get = s.iter().find(|(m, _)| m == "screenshot.get").unwrap();
            assert_eq!(get.1, json!({"id": "S9", "inline": true}));
        }
        // Plain terminal: no image bytes requested, path and metadata printed.
        seen.lock().unwrap().clear();
        let mut c = spawn(seen.clone(), have.clone());
        let mut out = Vec::new();
        preview_show_to(
            &mut c,
            &g,
            json!({"preview": "v4"}),
            show::Mode::Text,
            true,
            &mut out,
        )
        .await;
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            !text.contains("\x1b") && text.contains("image: /state/blobs/ab/x.png"),
            "{text}"
        );
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .find(|(m, _)| m == "screenshot.get")
                .unwrap()
                .1["inline"],
            false
        );
        // --no-image beats a graphics terminal; a pipe gets JSON.
        let mut c = spawn(seen.clone(), have.clone());
        let mut out = Vec::new();
        preview_show_to(
            &mut c,
            &g,
            json!({"preview": "v4", "no_image": true}),
            show::Mode::Kitty,
            true,
            &mut out,
        )
        .await;
        assert!(!String::from_utf8_lossy(&out).contains("\x1b"));
        let gj = Global {
            json: None,
            ..g_clone(&g)
        };
        let mut c = spawn(seen.clone(), have.clone());
        let mut out = Vec::new();
        preview_show_to(
            &mut c,
            &gj,
            json!({"preview": "v4"}),
            show::Mode::Kitty,
            false,
            &mut out,
        )
        .await;
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["handle"], "s9");
        // Nothing captured yet.
        have.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut c = spawn(seen.clone(), have.clone());
        let mut out = Vec::new();
        let code = preview_show_to(
            &mut c,
            &g,
            json!({"preview": "v4"}),
            show::Mode::Kitty,
            true,
            &mut out,
        )
        .await;
        assert_eq!(code, EXIT_API);
        assert!(out.is_empty());
    }

    fn g_clone(g: &Global) -> Global {
        Global {
            session: g.session.clone(),
            machine: g.machine.clone(),
            socket: g.socket.clone(),
            json: g.json,
            quiet: g.quiet,
            no_spawn: g.no_spawn,
            timeout_ms: g.timeout_ms,
        }
    }

    #[test]
    fn screenshot_positional_is_a_session_a_preview_or_a_url() {
        let (m, pos) = lookup("browser", "screenshot").unwrap();
        assert_eq!(m, "browser.screenshot");
        let mut p = build_params(pos, &["b3".into(), "--full-page".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p["session"], "b3");
        let mut p =
            build_params(pos, &["v4".into(), "--device".into(), "iphone-15".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(
            (
                p.get("session"),
                p["preview"].as_str(),
                p["device"].as_str()
            ),
            (None, Some("v4"), Some("iphone-15"))
        );
        let mut p = build_params(pos, &["http://localhost:3000/x".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p["url"], "http://localhost:3000/x");
        // `--url` / `--preview` flags work without a positional.
        let mut p = build_params(pos, &["--url".into(), "http://localhost:3000/".into()]).unwrap();
        adjust(m, &mut p);
        assert_eq!(p["url"], "http://localhost:3000/");
        let (_, pos) = lookup("browser", "open").unwrap();
        let p = build_params(pos, &["v4".into(), "--device".into(), "pixel-8".into()]).unwrap();
        assert_eq!(p["device"], "pixel-8");
        assert!(lookup("preview", "show").is_some());
    }
}
