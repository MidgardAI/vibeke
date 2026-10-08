//! Receiving handoffs from the command line (16 §15.2): `vibeke handoff incoming`, `accept`,
//! `decline`, `resume` and `prefs` over the server's incoming-handoff API.
//!
//! Inside a pane (a pane token and no elevated token), `send`, `cancel` and `redeem` ask the
//! user through `auth.approve` (09 §3.2 "Approved calls") and wait for the decision, which is
//! made outside the pane; Ctrl-C closes the connection, which withdraws the request, and
//! `--no-wait` prints the request id. `send` defaults to the CLI's own pane and offers the
//! workspace's only agent pane when its own pane has no agent. Outside panes `send` and `cancel`
//! are plain API commands and `redeem` is `vibeke gateway peer add`.
//!
//! `accept` without `--repo` / `--clone-to` takes the clone `handoff.incoming.get` suggests (and
//! its worktree path); with a repository given, the server places the worktree next to it unless
//! `--worktree` says otherwise. Relative paths are made absolute here when the command talks to
//! this machine; `~` is expanded by the server.

use crate::client::{CallError, Client};
use crate::{EXIT_API, EXIT_OK, EXIT_USAGE, Global, exit_code_for, print_error};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::Path;

/// The verbs this module runs.
pub const VERBS: &[&str] = &["incoming", "accept", "decline", "resume", "prefs", "redeem"];
/// Verbs that ask the user through `auth.approve` when run inside a pane.
pub const APPROVED_VERBS: &[&str] = &["send", "cancel", "redeem"];

pub const USAGE: &str = "vibeke handoff incoming\n\
vibeke handoff accept <id> [--repo PATH | --clone-to PATH] [--worktree PATH] [--branch NAME] [--no-resume] [--trust mise,direnv]\n\
vibeke handoff decline <id>\n\
vibeke handoff resume <id>\n\
vibeke handoff prefs [--always-ask on|off]\n\
vibeke handoff send <peer> [--pane P] [--interrupt] [--no-wait] [--reason TEXT] [--timeout-ms MS]\n\
vibeke handoff cancel <job> [--no-wait] [--reason TEXT] [--timeout-ms MS]\n\
vibeke handoff redeem <link> [--share-user] [--no-wait] [--reason TEXT] [--timeout-ms MS]";

/// The message the CLI prints while it waits for the user's decision.
pub const WAITING: &str =
    "Waiting for approval in Vibeke (prefix+shift+e, or the notice in the tab bar)…";

/// Inside a pane without an elevation: privileged handoff verbs go through `auth.approve`.
pub fn in_pane() -> bool {
    let set = |k: &str| std::env::var(k).ok().is_some_and(|v| !v.is_empty());
    set("VIBEKE_PANE_TOKEN") && !set("VIBEKE_ELEVATED_TOKEN")
}

/// Whether `vibeke handoff <verb>` runs here: one of [`VERBS`], and inside a pane also the
/// [`APPROVED_VERBS`].
pub fn handles(verb: Option<&str>) -> bool {
    handles_in(verb, in_pane())
}

pub fn handles_in(verb: Option<&str>, in_pane: bool) -> bool {
    verb.is_some_and(|v| VERBS.contains(&v) || (in_pane && APPROVED_VERBS.contains(&v)))
}

/// `vibeke handoff redeem <link> [--share-user]` outside panes: the arguments of the
/// equivalent `vibeke gateway peer add`, or `None` when the arguments are not that.
pub fn redeem_as_peer_add(args: &[String]) -> Option<Vec<String>> {
    match parse(args).ok()? {
        Cmd::Redeem {
            link, share_user, ..
        } => {
            let mut v = vec!["peer".to_string(), "add".to_string(), link];
            if share_user {
                v.push("--share-user".into());
            }
            Some(v)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoArg {
    Path(String),
    CloneTo(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptArgs {
    pub id: String,
    pub repo: Option<RepoArg>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub resume: bool,
    pub trust: Vec<String>,
}

/// How an approved verb asks (`auth.approve`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ask {
    pub no_wait: bool,
    pub reason: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Incoming,
    Accept(AcceptArgs),
    Decline(String),
    Resume(String),
    Prefs {
        always_ask: Option<bool>,
    },
    Send {
        peer: String,
        pane: Option<String>,
        interrupt: bool,
        ask: Ask,
    },
    Cancel {
        id: String,
        ask: Ask,
    },
    Redeem {
        link: String,
        share_user: bool,
        ask: Ask,
    },
}

/// `--flag value` / `--flag=value` / bare flags and positionals.
struct Args {
    flags: Vec<(String, Option<String>)>,
    pos: Vec<String>,
}

/// Flags that take a value.
const VALUED: &[&str] = &[
    "repo",
    "clone-to",
    "worktree",
    "branch",
    "trust",
    "always-ask",
    "pane",
    "reason",
    "timeout-ms",
];

fn split(args: &[String]) -> Result<Args, String> {
    let mut flags = Vec::new();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            pos.extend(args[i + 1..].iter().cloned());
            break;
        }
        if let Some(f) = a.strip_prefix("--") {
            let (name, inline) = match f.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (f.to_string(), None),
            };
            let value = if VALUED.contains(&name.as_str()) {
                match inline {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        Some(
                            args.get(i)
                                .cloned()
                                .ok_or_else(|| format!("--{name} needs a value"))?,
                        )
                    }
                }
            } else if inline.is_some() {
                return Err(format!("--{name} takes no value"));
            } else {
                None
            };
            flags.push((name, value));
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    Ok(Args { flags, pos })
}

fn one_id(verb: &str, a: &Args) -> Result<String, String> {
    one_pos(verb, a, "the handoff id")
}

fn one_pos(verb: &str, a: &Args, what: &str) -> Result<String, String> {
    match a.pos.as_slice() {
        [id] if !id.is_empty() => Ok(id.clone()),
        [] => Err(format!("vibeke handoff {verb} needs {what}")),
        _ => Err(format!("unexpected argument `{}`", a.pos[1])),
    }
}

/// The `auth.approve` flags of an approved verb; `other` takes the verb's own flags.
fn ask_flags(
    a: &Args,
    mut other: impl FnMut(&str, &Option<String>) -> Result<bool, String>,
) -> Result<Ask, String> {
    let mut ask = Ask::default();
    for (n, v) in &a.flags {
        match (n.as_str(), v) {
            ("no-wait", None) => ask.no_wait = true,
            ("reason", Some(v)) => ask.reason = Some(v.clone()),
            ("timeout-ms", Some(v)) => {
                ask.timeout_ms = Some(
                    v.parse()
                        .map_err(|_| format!("--timeout-ms takes milliseconds, not `{v}`"))?,
                );
            }
            _ => {
                if !other(n, v)? {
                    return Err(format!("unknown flag --{n}"));
                }
            }
        }
    }
    Ok(ask)
}

fn no_flags(a: &Args) -> Result<(), String> {
    match a.flags.first() {
        Some((n, _)) => Err(format!("unknown flag --{n}")),
        None => Ok(()),
    }
}

fn on_off(v: &str) -> Result<bool, String> {
    match v {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("--always-ask takes on or off, not `{v}`")),
    }
}

/// Parse the arguments after `vibeke handoff`.
pub fn parse(args: &[String]) -> Result<Cmd, String> {
    let Some(verb) = args.first() else {
        return Err("missing verb".into());
    };
    let a = split(&args[1..])?;
    match verb.as_str() {
        "incoming" => {
            no_flags(&a)?;
            if let Some(p) = a.pos.first() {
                return Err(format!("unexpected argument `{p}`"));
            }
            Ok(Cmd::Incoming)
        }
        "decline" | "resume" => {
            no_flags(&a)?;
            let id = one_id(verb, &a)?;
            Ok(if verb == "decline" {
                Cmd::Decline(id)
            } else {
                Cmd::Resume(id)
            })
        }
        "prefs" => {
            if let Some(p) = a.pos.first() {
                return Err(format!("unexpected argument `{p}`"));
            }
            let mut always_ask = None;
            for (n, v) in &a.flags {
                match (n.as_str(), v) {
                    ("always-ask", Some(v)) => always_ask = Some(on_off(v)?),
                    _ => return Err(format!("unknown flag --{n}")),
                }
            }
            Ok(Cmd::Prefs { always_ask })
        }
        "accept" => {
            let id = one_id(verb, &a)?;
            let mut out = AcceptArgs {
                id,
                repo: None,
                worktree: None,
                branch: None,
                resume: true,
                trust: Vec::new(),
            };
            for (n, v) in a.flags {
                match (n.as_str(), v) {
                    ("repo" | "clone-to", Some(v)) => {
                        if out.repo.is_some() {
                            return Err("give one of --repo and --clone-to".into());
                        }
                        out.repo = Some(if n == "repo" {
                            RepoArg::Path(v)
                        } else {
                            RepoArg::CloneTo(v)
                        });
                    }
                    ("worktree", Some(v)) => out.worktree = Some(v),
                    ("branch", Some(v)) => out.branch = Some(v),
                    ("no-resume", None) => out.resume = false,
                    ("resume", None) => out.resume = true,
                    ("trust", Some(v)) => {
                        for t in v.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                            if !matches!(t, "mise" | "direnv") {
                                return Err(format!("--trust takes mise and direnv, not `{t}`"));
                            }
                            if !out.trust.iter().any(|x| x == t) {
                                out.trust.push(t.to_string());
                            }
                        }
                    }
                    _ => return Err(format!("unknown flag --{n}")),
                }
            }
            Ok(Cmd::Accept(out))
        }
        "send" => {
            let peer = one_pos(verb, &a, "the peer (`vibeke handoff peers` lists them)")?;
            let (mut pane, mut interrupt) = (None, false);
            let ask = ask_flags(&a, |n, v| {
                Ok(match (n, v) {
                    ("pane", Some(v)) => {
                        pane = Some(v.clone());
                        true
                    }
                    ("interrupt", None) => {
                        interrupt = true;
                        true
                    }
                    _ => false,
                })
            })?;
            Ok(Cmd::Send {
                peer,
                pane,
                interrupt,
                ask,
            })
        }
        "cancel" => {
            let id = one_pos(verb, &a, "the job id (`vibeke handoff jobs` lists them)")?;
            let ask = ask_flags(&a, |_, _| Ok(false))?;
            Ok(Cmd::Cancel { id, ask })
        }
        "redeem" => {
            let link = one_pos(verb, &a, "the invitation link")?;
            let mut share_user = false;
            let ask = ask_flags(&a, |n, v| {
                Ok(match (n, v) {
                    ("share-user", None) => {
                        share_user = true;
                        true
                    }
                    _ => false,
                })
            })?;
            Ok(Cmd::Redeem {
                link,
                share_user,
                ask,
            })
        }
        other => Err(format!("unknown verb `{other}`")),
    }
}

/// An absolute path for the server: relative ones against `cwd` (when the command talks to this
/// machine); `~` and absolute paths unchanged.
pub fn absolute(p: &str, cwd: Option<&Path>) -> String {
    if p.starts_with('/') || p == "~" || p.starts_with("~/") {
        return p.to_string();
    }
    match cwd {
        Some(c) => {
            let joined = c.join(p);
            // `./x` and `x/` read naturally; keep the rest as given.
            let s = joined.to_string_lossy().to_string();
            let s = s.replace("/./", "/");
            match s.strip_suffix("/.") {
                Some(t) => t.to_string(),
                None => s,
            }
        }
        None => p.to_string(),
    }
}

/// `handoff.accept` params from the flags and, when the repository isn't given, the suggestion
/// of `handoff.incoming.get` (`suggested`).
pub fn accept_params(
    a: &AcceptArgs,
    suggested: Option<&Value>,
    cwd: Option<&Path>,
) -> Result<Value, String> {
    let mut worktree = a.worktree.as_deref().map(|w| absolute(w, cwd));
    let repo = match &a.repo {
        Some(RepoArg::Path(p)) => json!({"path": absolute(p, cwd)}),
        Some(RepoArg::CloneTo(p)) => json!({"clone_to": absolute(p, cwd)}),
        None => {
            let s = suggested.cloned().unwrap_or(Value::Null);
            let Some(repo) = s["repo"].as_str().filter(|r| !r.is_empty()) else {
                return Err(
                    "no clone of this repository was found here; pass --repo PATH (a clone) or --clone-to PATH (a new folder)"
                        .into(),
                );
            };
            if worktree.is_none() {
                worktree = s["worktree_path"].as_str().map(str::to_string);
            }
            json!({"path": repo})
        }
    };
    let mut p = json!({"id": a.id, "repo": repo, "start_agent": a.resume});
    if let Some(w) = worktree.filter(|w| !w.is_empty()) {
        p["worktree_path"] = json!(w);
    }
    if let Some(b) = a.branch.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        p["branch"] = json!(b);
    }
    if !a.trust.is_empty() {
        p["trust"] = json!(a.trust);
    }
    Ok(p)
}

fn age(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The `incoming` table: id, state, sender, repository, branch, age.
pub fn incoming_table(v: &Value, now: i64) -> String {
    let rows = v["incoming"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no incoming handoffs".into();
    }
    let mut out = vec![format!(
        "{:<26} {:<10} {:<24} {:<20} {:<28} {}",
        "ID", "STATE", "FROM", "REPO", "BRANCH", "AGE"
    )];
    for r in &rows {
        let from = match (r["from"]["host"].as_str(), r["from"]["owner"].as_str()) {
            (Some(h), Some("teammate")) => format!("{h} (teammate)"),
            (Some(h), _) => h.to_string(),
            _ => String::new(),
        };
        let mut state = r["state"].as_str().unwrap_or_default().to_string();
        if r["result"]["agent_error"].is_object() {
            state.push('*');
        }
        out.push(format!(
            "{:<26} {:<10} {:<24} {:<20} {:<28} {}",
            r["id"].as_str().unwrap_or_default(),
            state,
            from,
            r["manifest"]["repo_name"].as_str().unwrap_or_default(),
            r["manifest"]["branch"].as_str().unwrap_or("(detached)"),
            age(now - r["created_at_ms"].as_i64().unwrap_or(now)),
        ));
    }
    if rows.iter().any(|r| r["result"]["agent_error"].is_object()) {
        out.push("* imported, but the agent did not start: vibeke handoff resume <id>".into());
    }
    out.join("\n")
}

/// What an accept (or resume) did, for people.
pub fn accept_summary(v: &Value) -> String {
    let inc = &v["incoming"];
    let r = &inc["result"];
    let mut out = vec![format!(
        "imported {} into {}",
        r["branch"].as_str().unwrap_or_default(),
        r["worktree"].as_str().unwrap_or_default()
    )];
    if r["cloned"].as_bool() == Some(true) {
        out.push(format!(
            "cloned into {}",
            r["repo"].as_str().unwrap_or_default()
        ));
    }
    if let Some(p) = r["pane"].as_str() {
        out.push(format!("pane {p}"));
    }
    for f in r["not_written"].as_array().into_iter().flatten() {
        out.push(format!(
            "not written: {}",
            f.as_str().map_or_else(|| f.to_string(), str::to_string)
        ));
    }
    for t in r["trust"].as_array().into_iter().flatten() {
        if let (Some(tool), Some(st)) = (t["tool"].as_str(), t["status"].as_str()) {
            let err = t["error"]
                .as_str()
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            out.push(format!("{tool}: {st}{err}"));
        }
    }
    let agent_error = v
        .get("agent_error")
        .filter(|e| !e.is_null())
        .or_else(|| r.get("agent_error").filter(|e| !e.is_null()));
    if let Some(e) = agent_error {
        out.push(format!(
            "the agent did not start: {}; fix it, then: vibeke handoff resume {}",
            e["message"].as_str().unwrap_or("unknown error"),
            inc["id"].as_str().unwrap_or("<id>")
        ));
    } else if r["run"].is_object() || v["run"].is_object() {
        out.push("agent started".into());
    }
    if let Some(e) = r["workspace_error"].as_str() {
        out.push(format!("no workspace was opened: {e}"));
    }
    out.join("\n")
}

fn print(g: &Global, v: &Value, human: impl FnOnce(&Value) -> String) {
    if g.quiet {
        return;
    }
    if g.json.unwrap_or(!std::io::stdout().is_terminal()) {
        println!("{}", serde_json::to_string(v).unwrap_or_default());
    } else {
        println!("{}", human(v));
    }
}

fn fail(e: &CallError) -> i32 {
    print_error(e);
    // `repo_mismatch`: say which remotes the clone has, for people reading the terminal.
    if let CallError::Rpc(r) = e
        && r.data.details["reason"] == "repo_mismatch"
        && std::io::stderr().is_terminal()
    {
        let remotes: Vec<&str> = r.data.details["remotes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        eprintln!(
            "{} is not a clone of {} (its remotes: {}); pass another --repo or --clone-to",
            r.data.details["repo"].as_str().unwrap_or("that folder"),
            r.data.details["origin"].as_str().unwrap_or("the origin"),
            if remotes.is_empty() {
                "none".to_string()
            } else {
                remotes.join(", ")
            }
        );
    }
    exit_code_for(e)
}

/// The pane `send` hands over by default: the CLI's own pane, or (asked on the terminal) the
/// workspace's only agent pane when the own pane has no agent.
async fn pick_pane<S>(client: &mut Client<S>) -> Result<String, CallError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let me = client.call("pane.get", json!({"pane": "@current"})).await?;
    let id = me["pane"]["id"].as_str().unwrap_or("@current").to_string();
    if !me["run"].is_null() || !std::io::stdin().is_terminal() {
        return Ok(id);
    }
    let Some(ws) = me["pane"]["workspace"].as_str() else {
        return Ok(id);
    };
    let agents = client
        .call("pane.list", json!({"workspace": ws, "has_agent": true}))
        .await?;
    let panes = agents["panes"].as_array().cloned().unwrap_or_default();
    let [other] = panes.as_slice() else {
        return Ok(id);
    };
    let (Some(oid), handle) = (other["id"].as_str(), other["handle"].as_str()) else {
        return Ok(id);
    };
    if oid == id {
        return Ok(id);
    }
    eprint!(
        "This pane has no agent. Send the agent pane {} instead? [Y/n] ",
        handle.unwrap_or(oid)
    );
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    Ok(if yes(&answer) { oid.to_string() } else { id })
}

/// A `[Y/n]` answer: empty means yes.
pub fn yes(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    )
}

/// `handoff.send` / `handoff.cancel` results, for people.
pub fn job_line(v: &Value) -> String {
    let j = &v["job"];
    format!(
        "{} handoff {} of pane {} to {}",
        j["state"].as_str().unwrap_or("?"),
        j["id"].as_str().unwrap_or("?"),
        j["pane"].as_str().unwrap_or("?"),
        j["peer_name"].as_str().unwrap_or("?")
    )
}

/// `peer.redeem` results, for people.
pub fn paired_line(v: &Value) -> String {
    let p = &v["peer"];
    format!(
        "Paired with {} ({}). Peer id {}.",
        p["name"].as_str().unwrap_or("?"),
        if p["owner"] == "self" {
            "your host"
        } else {
            "teammate"
        },
        p["id"].as_str().unwrap_or("?")
    )
}

/// Ask the user for `method params` through `auth.approve` and wait for the decision (or with
/// `--no-wait`, print the request id). The waiting call returns the method's own result.
async fn approved<S>(
    client: &mut Client<S>,
    g: &Global,
    method: &str,
    params: Value,
    ask: &Ask,
    human: fn(&Value) -> String,
) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if !in_pane() {
        eprintln!(
            "`vibeke handoff` asks for approval only inside a pane; run `vibeke {}` directly",
            match method {
                "handoff.send" => "handoff send <peer> --pane <pane>",
                "handoff.cancel" => "handoff cancel <job>",
                _ => "gateway peer add <link>",
            }
        );
        return EXIT_USAGE;
    }
    let mut p = json!({"method": method, "params": params});
    if let Some(r) = &ask.reason {
        p["reason"] = json!(r);
    }
    if let Some(t) = ask.timeout_ms {
        p["timeout_ms"] = json!(t);
    }
    if ask.no_wait {
        p["wait"] = json!(false);
        return match client.call("auth.approve", p).await {
            // A standing grant ran it at once.
            Ok(v) if v.get("status").is_none() => {
                print(g, &v, human);
                EXIT_OK
            }
            Ok(v) => {
                print(g, &v, |v| {
                    format!(
                        "{}
{}",
                        v["request"].as_str().unwrap_or_default(),
                        v["summary"].as_str().unwrap_or_default()
                    )
                });
                EXIT_OK
            }
            Err(e) => fail(&e),
        };
    }
    if !g.quiet {
        eprintln!("{WAITING}");
    }
    // Ctrl-C ends the process and with it the connection, which withdraws the request.
    let r = tokio::select! {
        r = client.call("auth.approve", p) => r,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nwithdrawn");
            return 130;
        }
    };
    match r {
        Ok(v) => {
            print(g, &v, human);
            EXIT_OK
        }
        Err(e) => {
            if let CallError::Rpc(r) = &e {
                if r.message.starts_with("approval_denied") {
                    eprintln!("denied: the request was not approved");
                } else if r.data.kind == "timeout"
                    && let Some(id) = r.data.details["request"].as_str()
                {
                    let _ = client
                        .call("auth.approve.withdraw", json!({"request": id}))
                        .await;
                    eprintln!("no decision in time; request {id} withdrawn");
                }
            }
            fail(&e)
        }
    }
}

/// `vibeke handoff <verb> …` (`args` starts at the verb).
pub async fn run<S>(client: &mut Client<S>, g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    if let Err(e) = client.hello("cli").await {
        return fail(&e);
    }
    match &cmd {
        Cmd::Send {
            peer,
            pane,
            interrupt,
            ask,
        } => {
            let pane = match pane {
                Some(p) => p.clone(),
                None => match pick_pane(client).await {
                    Ok(p) => p,
                    Err(e) => return fail(&e),
                },
            };
            let params = json!({"pane": pane, "peer": peer, "interrupt": interrupt});
            return approved(client, g, "handoff.send", params, ask, job_line).await;
        }
        Cmd::Cancel { id, ask } => {
            return approved(
                client,
                g,
                "handoff.cancel",
                json!({"id": id}),
                ask,
                job_line,
            )
            .await;
        }
        Cmd::Redeem {
            link,
            share_user,
            ask,
        } => {
            let params = json!({"method": "peer.redeem", "params": {"link": link, "share_user": share_user}});
            return approved(client, g, "gateway.call", params, ask, paired_line).await;
        }
        _ => {}
    }
    let (method, params) = match &cmd {
        Cmd::Incoming => ("handoff.incoming.list", json!({})),
        Cmd::Decline(id) => ("handoff.decline", json!({"id": id})),
        Cmd::Resume(id) => ("handoff.resume", json!({"id": id})),
        Cmd::Prefs { always_ask } => (
            "handoff.prefs",
            match always_ask {
                Some(on) => json!({"always_ask": on}),
                None => json!({}),
            },
        ),
        Cmd::Send { .. } | Cmd::Cancel { .. } | Cmd::Redeem { .. } => unreachable!("handled above"),
        Cmd::Accept(a) => {
            let suggested = if a.repo.is_none() {
                match client
                    .call("handoff.incoming.get", json!({"id": a.id}))
                    .await
                {
                    Ok(v) => Some(v["suggested"].clone()),
                    Err(e) => return fail(&e),
                }
            } else {
                None
            };
            // Relative paths only mean something on this machine.
            let cwd = if g.machine.is_none() {
                std::env::current_dir().ok()
            } else {
                None
            };
            match accept_params(a, suggested.as_ref(), cwd.as_deref()) {
                Ok(p) => ("handoff.accept", p),
                Err(e) => {
                    eprintln!("{e}");
                    return EXIT_API;
                }
            }
        }
    };
    let v = match client.call(method, params).await {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    match &cmd {
        Cmd::Incoming => print(g, &v, |v| incoming_table(v, now_ms())),
        Cmd::Accept(_) | Cmd::Resume(_) => print(g, &v, accept_summary),
        Cmd::Decline(id) => print(g, &v, |_| format!("declined {id}")),
        Cmd::Send { .. } | Cmd::Cancel { .. } | Cmd::Redeem { .. } => {}
        Cmd::Prefs { .. } => print(g, &v, |v| {
            let n = v["placement"].as_object().map_or(0, |o| o.len());
            format!(
                "always ask: {}\nremembered placements: {n}",
                if v["always_ask"].as_bool() == Some(true) {
                    "on (own handoffs wait to be accepted)"
                } else {
                    "off (own handoffs import into a known clone at the remembered place)"
                }
            )
        }),
    }
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_every_verb() {
        assert_eq!(parse(&args("incoming")), Ok(Cmd::Incoming));
        assert_eq!(parse(&args("decline h1")), Ok(Cmd::Decline("h1".into())));
        assert_eq!(parse(&args("resume h1")), Ok(Cmd::Resume("h1".into())));
        assert_eq!(parse(&args("prefs")), Ok(Cmd::Prefs { always_ask: None }));
        assert_eq!(
            parse(&args("prefs --always-ask on")),
            Ok(Cmd::Prefs {
                always_ask: Some(true)
            })
        );
        assert_eq!(
            parse(&args("prefs --always-ask=off")),
            Ok(Cmd::Prefs {
                always_ask: Some(false)
            })
        );
        assert_eq!(
            parse(&args("accept h1")),
            Ok(Cmd::Accept(AcceptArgs {
                id: "h1".into(),
                repo: None,
                worktree: None,
                branch: None,
                resume: true,
                trust: vec![],
            }))
        );
        assert_eq!(
            parse(&args(
                "accept h1 --repo ~/code/vibeke --worktree=/w/x --branch fix/y --no-resume --trust mise,direnv --trust mise"
            )),
            Ok(Cmd::Accept(AcceptArgs {
                id: "h1".into(),
                repo: Some(RepoArg::Path("~/code/vibeke".into())),
                worktree: Some("/w/x".into()),
                branch: Some("fix/y".into()),
                resume: false,
                trust: vec!["mise".into(), "direnv".into()],
            }))
        );
        assert_eq!(
            parse(&args("accept --clone-to /src/v h1")),
            Ok(Cmd::Accept(AcceptArgs {
                id: "h1".into(),
                repo: Some(RepoArg::CloneTo("/src/v".into())),
                worktree: None,
                branch: None,
                resume: true,
                trust: vec![],
            }))
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        for (a, want) in [
            ("accept", "needs the handoff id"),
            ("accept h1 h2", "unexpected argument `h2`"),
            (
                "accept h1 --repo /a --clone-to /b",
                "one of --repo and --clone-to",
            ),
            ("accept h1 --trust nix", "--trust takes mise and direnv"),
            ("accept h1 --repo", "--repo needs a value"),
            ("accept h1 --force", "unknown flag --force"),
            ("accept h1 --no-resume=1", "takes no value"),
            ("decline", "needs the handoff id"),
            ("resume h1 --x", "unknown flag --x"),
            ("prefs --always-ask maybe", "takes on or off"),
            ("prefs extra", "unexpected argument"),
            ("incoming h1", "unexpected argument"),
            ("send", "needs the peer"),
            ("send a b", "unexpected argument `b`"),
            ("send a --timeout-ms soon", "takes milliseconds"),
            ("cancel j1 --pane p", "unknown flag --pane"),
            ("redeem", "needs the invitation link"),
            ("redeem l --interrupt", "unknown flag --interrupt"),
            ("transmit h1", "unknown verb"),
        ] {
            let e = parse(&args(a)).unwrap_err();
            assert!(e.contains(want), "{a}: {e}");
        }
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn parses_the_approved_verbs() {
        assert_eq!(
            parse(&args("send marvin")),
            Ok(Cmd::Send {
                peer: "marvin".into(),
                pane: None,
                interrupt: false,
                ask: Ask::default(),
            })
        );
        assert_eq!(
            parse(&args(
                "send marvin --pane p2 --interrupt --no-wait --reason=ship --timeout-ms 500"
            )),
            Ok(Cmd::Send {
                peer: "marvin".into(),
                pane: Some("p2".into()),
                interrupt: true,
                ask: Ask {
                    no_wait: true,
                    reason: Some("ship".into()),
                    timeout_ms: Some(500),
                },
            })
        );
        assert_eq!(
            parse(&args("cancel j1 --no-wait")),
            Ok(Cmd::Cancel {
                id: "j1".into(),
                ask: Ask {
                    no_wait: true,
                    ..Ask::default()
                },
            })
        );
        assert_eq!(
            parse(&args("redeem vibeke://x --share-user")),
            Ok(Cmd::Redeem {
                link: "vibeke://x".into(),
                share_user: true,
                ask: Ask::default(),
            })
        );
        // Outside panes `redeem` is `vibeke gateway peer add`.
        assert_eq!(
            redeem_as_peer_add(&args("redeem vibeke://x --share-user")).unwrap(),
            args("peer add vibeke://x --share-user")
        );
        assert_eq!(redeem_as_peer_add(&args("send x")), None);
        for (a, y) in [
            ("", true),
            ("y\n", true),
            ("Yes", true),
            ("n", false),
            ("no\n", false),
        ] {
            assert_eq!(yes(a), y, "{a:?}");
        }
        let job =
            json!({"job": {"id": "j1", "state": "queued", "pane": "p1", "peer_name": "marvin"}});
        assert_eq!(job_line(&job), "queued handoff j1 of pane p1 to marvin");
        let peer = json!({"peer": {"id": "pr1", "name": "marvin", "owner": "self"}});
        assert_eq!(
            paired_line(&peer),
            "Paired with marvin (your host). Peer id pr1."
        );
    }

    #[test]
    fn handles_only_its_verbs() {
        for v in VERBS {
            assert!(handles_in(Some(v), false), "{v}");
        }
        // `send` and `cancel` are plain API commands outside panes and ask for approval inside
        // one; `jobs` and `peers` are always plain API commands.
        for v in ["send", "cancel"] {
            assert!(!handles_in(Some(v), false), "{v}");
            assert!(handles_in(Some(v), true), "{v}");
        }
        for v in ["jobs", "peers"] {
            assert!(!handles_in(Some(v), true), "{v}");
        }
        assert!(!handles_in(None, true));
        let tree = crate::verbs::command_tree();
        for v in VERBS {
            assert!(tree["handoff"].contains(&v.to_string()), "{v}");
        }
    }

    #[test]
    fn accept_params_use_the_suggestion_unless_a_repository_is_given() {
        let suggested = json!({"repos": ["/src/vibeke"], "repo": "/src/vibeke",
            "worktree_path": "/src/vibeke-handoff-x", "branch": "handoff/x"});
        let a = |s: &str| match parse(&args(s)).unwrap() {
            Cmd::Accept(a) => a,
            c => panic!("{c:?}"),
        };
        assert_eq!(
            accept_params(&a("accept h1"), Some(&suggested), None).unwrap(),
            json!({"id": "h1", "repo": {"path": "/src/vibeke"},
                   "worktree_path": "/src/vibeke-handoff-x", "start_agent": true})
        );
        // An explicit worktree wins over the suggested one.
        assert_eq!(
            accept_params(&a("accept h1 --worktree /w"), Some(&suggested), None).unwrap()["worktree_path"],
            "/w"
        );
        // A repository given: no suggestion needed; the server places the worktree.
        assert_eq!(
            accept_params(
                &a("accept h1 --repo ../other --branch b --no-resume --trust direnv"),
                None,
                Some(Path::new("/home/u/code/vibeke"))
            )
            .unwrap(),
            json!({"id": "h1", "repo": {"path": "/home/u/code/vibeke/../other"},
                   "branch": "b", "start_agent": false, "trust": ["direnv"]})
        );
        assert_eq!(
            accept_params(
                &a("accept h1 --clone-to ./vk --worktree wt"),
                None,
                Some(Path::new("/home/u"))
            )
            .unwrap(),
            json!({"id": "h1", "repo": {"clone_to": "/home/u/vk"},
                   "worktree_path": "/home/u/wt", "start_agent": true})
        );
        // `~` is the server's to expand; a remote machine gets relative paths as typed.
        assert_eq!(
            accept_params(&a("accept h1 --repo ~/code/v"), None, Some(Path::new("/x"))).unwrap()["repo"],
            json!({"path": "~/code/v"})
        );
        assert_eq!(
            accept_params(&a("accept h1 --repo v"), None, None).unwrap()["repo"],
            json!({"path": "v"})
        );
        // No clone found and none given.
        let none = json!({"repos": [], "repo": null, "worktree_path": null, "branch": "handoff/x"});
        let e = accept_params(&a("accept h1"), Some(&none), None).unwrap_err();
        assert!(e.contains("--clone-to"), "{e}");
    }

    #[test]
    fn human_output() {
        let now = 10_000_000;
        let v = json!({"incoming": [
            {"id": "h1", "state": "pending", "from": {"host": "laptop", "owner": "teammate"},
             "manifest": {"repo_name": "vibeke", "branch": "feature/x"},
             "created_at_ms": now - 120_000, "result": null},
            {"id": "h2", "state": "imported", "from": {"host": "marvin", "owner": "self"},
             "manifest": {"repo_name": "api", "branch": null},
             "created_at_ms": now - 7_200_000,
             "result": {"agent_error": {"message": "x"}}}
        ]});
        let t = incoming_table(&v, now);
        assert!(t.starts_with("ID"), "{t}");
        assert!(t.contains("h1"), "{t}");
        assert!(t.contains("laptop (teammate)"), "{t}");
        assert!(t.contains("feature/x"), "{t}");
        assert!(t.contains("2m"), "{t}");
        assert!(t.contains("imported*"), "{t}");
        assert!(t.contains("(detached)"), "{t}");
        assert!(t.contains("vibeke handoff resume <id>"), "{t}");
        assert_eq!(
            incoming_table(&json!({"incoming": []}), now),
            "no incoming handoffs"
        );
        let s = accept_summary(&json!({"incoming": {"id": "h1", "result": {
            "branch": "handoff/x", "worktree": "/src/v-handoff-x", "repo": "/src/v",
            "cloned": true, "pane": "p3", "not_written": ["a.bin"],
            "trust": [{"tool": "mise", "status": "trusted"}],
            "agent_error": {"message": "claude: not found"}}}}));
        assert!(
            s.starts_with("imported handoff/x into /src/v-handoff-x"),
            "{s}"
        );
        assert!(s.contains("cloned into /src/v"), "{s}");
        assert!(s.contains("not written: a.bin"), "{s}");
        assert!(s.contains("mise: trusted"), "{s}");
        assert!(
            s.contains(
                "the agent did not start: claude: not found; fix it, then: vibeke handoff resume h1"
            ),
            "{s}"
        );
    }
}
