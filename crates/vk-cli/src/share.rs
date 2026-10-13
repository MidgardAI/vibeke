//! Unattended sharing from the command line: `vibeke share watch|review|list|revoke` and
//! `vibeke handoff link|open`. Every verb runs over the server's `gateway.call` bridge, so a
//! program outside every pane (for example a messaging bot on the host) can use them.
//!
//! Without `--github`, `--any-github` or `--link` the verbs send `access: github` with the
//! `[sharing] github` list from the config. Without `--json` they print the link alone.

use crate::client::{CallError, Client};
use crate::{EXIT_API, EXIT_OK, EXIT_USAGE, Global, exit_code_for, print_error};
use serde_json::{Value, json};
use std::time::Duration;

pub const USAGE: &str = "vibeke share watch [--pane P | --workspace W] [--scope view|approve] [--github a,b | --any-github | --link] [--uses N] [--ttl 24h] [--label L] [--json]\n\
vibeke share review <preview> [--github a,b | --any-github | --link] [--notes direct|held] [--uses N] [--ttl 24h] [--label L] [--json]\n\
vibeke share list [--json]\n\
vibeke share revoke <id>";

pub const HANDOFF_USAGE: &str = "vibeke handoff link [--pane P] [--github a,b | --any-github | --link] [--ttl 7d] [--interrupt] [--pr] [--json]\n\
vibeke handoff open <link> [--json]";

/// How long one status poll waits before it asks again.
const POLL: Duration = Duration::from_secs(1);

/// Who may open a link, as the flags said it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccessFlags {
    pub github: Option<Vec<String>>,
    pub any_github: bool,
    pub link: bool,
}

/// The options both `share` and `handoff link` take.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Common {
    pub access: AccessFlags,
    pub uses: Option<u64>,
    pub ttl_s: Option<u64>,
    pub label: Option<String>,
    pub pane: Option<String>,
    pub workspace: Option<String>,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Watch {
        common: Common,
        scope: Option<String>,
    },
    Review {
        common: Common,
        preview: String,
        notes: Option<String>,
    },
    List {
        json: bool,
    },
    Revoke {
        id: String,
    },
    HandoffLink {
        common: Common,
        interrupt: bool,
        pr: bool,
    },
    HandoffOpen {
        link: String,
        json: bool,
    },
}

/// Flags that take a value.
const VALUED: &[&str] = &[
    "pane",
    "workspace",
    "scope",
    "github",
    "uses",
    "ttl",
    "label",
    "notes",
];

struct Args {
    flags: Vec<(String, Option<String>)>,
    pos: Vec<String>,
}

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

/// `30m`, `24h`, `7d` (also `90s`, and a bare number of seconds) as seconds.
pub fn parse_ttl(s: &str) -> Result<u64, String> {
    let bad = || format!("--ttl takes a duration such as 30m, 24h or 7d, not `{s}`");
    let s = s.trim();
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num.parse().map_err(|_| bad())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(bad()),
    };
    if n == 0 {
        return Err(bad());
    }
    n.checked_mul(mult).ok_or_else(bad)
}

/// `a,@b, c` as logins.
pub fn parse_logins(s: &str) -> Vec<String> {
    s.split(',')
        .map(|l| l.trim().trim_start_matches('@').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Take the options shared by all link verbs; `other` handles a verb's own flags.
fn common(
    a: &Args,
    mut other: impl FnMut(&str, &Option<String>) -> Result<bool, String>,
) -> Result<Common, String> {
    let mut c = Common::default();
    for (n, v) in &a.flags {
        match (n.as_str(), v) {
            ("github", Some(v)) => {
                let logins = parse_logins(v);
                if logins.is_empty() {
                    return Err("--github needs at least one login".into());
                }
                c.access.github = Some(logins);
            }
            ("any-github", None) => c.access.any_github = true,
            ("link", None) => c.access.link = true,
            ("uses", Some(v)) => {
                c.uses = Some(
                    v.parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("--uses takes a positive number, not `{v}`"))?,
                );
            }
            ("ttl", Some(v)) => c.ttl_s = Some(parse_ttl(v)?),
            ("label", Some(v)) => c.label = Some(v.clone()),
            ("pane", Some(v)) => c.pane = Some(v.clone()),
            ("workspace", Some(v)) => c.workspace = Some(v.clone()),
            ("json", None) => c.json = true,
            _ => {
                if !other(n, v)? {
                    return Err(format!("unknown flag --{n}"));
                }
            }
        }
    }
    let chosen = [
        c.access.github.is_some(),
        c.access.any_github,
        c.access.link,
    ];
    if chosen.iter().filter(|b| **b).count() > 1 {
        return Err("give only one of --github, --any-github and --link".into());
    }
    Ok(c)
}

fn no_pos(a: &Args) -> Result<(), String> {
    match a.pos.first() {
        Some(p) => Err(format!("unexpected argument `{p}`")),
        None => Ok(()),
    }
}

/// Parse the arguments after `vibeke share`.
pub fn parse_share(args: &[String]) -> Result<Cmd, String> {
    let Some(verb) = args.first() else {
        return Err("missing verb".into());
    };
    let a = split(&args[1..])?;
    match verb.as_str() {
        "watch" => {
            no_pos(&a)?;
            let mut scope = None;
            let c = common(&a, |n, v| {
                Ok(match (n, v) {
                    ("scope", Some(v)) => {
                        if !matches!(v.as_str(), "view" | "approve") {
                            return Err(format!("--scope takes view or approve, not `{v}`"));
                        }
                        scope = Some(v.clone());
                        true
                    }
                    _ => false,
                })
            })?;
            if c.pane.is_some() && c.workspace.is_some() {
                return Err("give one of --pane and --workspace".into());
            }
            Ok(Cmd::Watch { common: c, scope })
        }
        "review" => {
            let preview = match a.pos.as_slice() {
                [p] if !p.is_empty() => p.clone(),
                [] => {
                    return Err(
                        "vibeke share review needs a preview (an id or a handle such as v4)".into(),
                    );
                }
                _ => return Err(format!("unexpected argument `{}`", a.pos[1])),
            };
            let mut notes = None;
            let c = common(&a, |n, v| {
                Ok(match (n, v) {
                    ("notes", Some(v)) => {
                        if !matches!(v.as_str(), "direct" | "held") {
                            return Err(format!("--notes takes direct or held, not `{v}`"));
                        }
                        notes = Some(v.clone());
                        true
                    }
                    _ => false,
                })
            })?;
            Ok(Cmd::Review {
                common: c,
                preview,
                notes,
            })
        }
        "list" => {
            no_pos(&a)?;
            let mut json = false;
            for (n, v) in &a.flags {
                match (n.as_str(), v) {
                    ("json", None) => json = true,
                    _ => return Err(format!("unknown flag --{n}")),
                }
            }
            Ok(Cmd::List { json })
        }
        "revoke" => {
            if let Some((n, _)) = a.flags.first() {
                return Err(format!("unknown flag --{n}"));
            }
            match a.pos.as_slice() {
                [id] if !id.is_empty() => Ok(Cmd::Revoke { id: id.clone() }),
                [] => {
                    Err("vibeke share revoke needs an id (`vibeke share list` shows them)".into())
                }
                _ => Err(format!("unexpected argument `{}`", a.pos[1])),
            }
        }
        other => Err(format!("unknown verb `{other}`")),
    }
}

/// Parse the arguments after `vibeke handoff` for `link` and `open`.
pub fn parse_handoff(args: &[String]) -> Result<Cmd, String> {
    let Some(verb) = args.first() else {
        return Err("missing verb".into());
    };
    let a = split(&args[1..])?;
    match verb.as_str() {
        "link" => {
            no_pos(&a)?;
            let (mut interrupt, mut pr) = (false, false);
            let c = common(&a, |n, v| {
                Ok(match (n, v) {
                    ("interrupt", None) => {
                        interrupt = true;
                        true
                    }
                    ("pr", None) => {
                        pr = true;
                        true
                    }
                    _ => false,
                })
            })?;
            if c.workspace.is_some() || c.uses.is_some() || c.label.is_some() {
                return Err("handoff link takes --pane, --github, --any-github, --link, --ttl, --interrupt, --pr and --json".into());
            }
            Ok(Cmd::HandoffLink {
                common: c,
                interrupt,
                pr,
            })
        }
        "open" => {
            let mut json = false;
            for (n, v) in &a.flags {
                match (n.as_str(), v) {
                    ("json", None) => json = true,
                    _ => return Err(format!("unknown flag --{n}")),
                }
            }
            match a.pos.as_slice() {
                [l] if !l.is_empty() => Ok(Cmd::HandoffOpen {
                    link: l.clone(),
                    json,
                }),
                [] => Err("vibeke handoff open needs a continue link".into()),
                _ => Err(format!("unexpected argument `{}`", a.pos[1])),
            }
        }
        other => Err(format!("unknown verb `{other}`")),
    }
}

/// `access` and `github` params for the flags. `default_github` is the `[sharing] github` list.
pub fn access_params(
    f: &AccessFlags,
    default_github: &[String],
) -> Result<(&'static str, Option<Vec<String>>), String> {
    if f.link {
        return Ok(("link", None));
    }
    if f.any_github {
        return Ok(("github", Some(vec!["*".into()])));
    }
    if let Some(g) = &f.github {
        return Ok(("github", Some(g.clone())));
    }
    if default_github.is_empty() {
        return Err(
            "no GitHub accounts to allow: set [sharing] github in config or pass --github".into(),
        );
    }
    Ok(("github", Some(default_github.to_vec())))
}

fn put_access(p: &mut Value, f: &AccessFlags, default_github: &[String]) -> Result<(), String> {
    let (access, github) = access_params(f, default_github)?;
    p["access"] = json!(access);
    if let Some(g) = github {
        p["github"] = json!(g);
    }
    Ok(())
}

/// The `gateway.call` method and params for a command. Targets (`pane`, `workspace`) must be
/// resolved already.
pub fn build_call(cmd: &Cmd, default_github: &[String]) -> Result<(&'static str, Value), String> {
    match cmd {
        Cmd::Watch { common: c, scope } => {
            let mut p = json!({"kind": "share"});
            put_access(&mut p, &c.access, default_github)?;
            put_common(&mut p, c);
            if let Some(w) = &c.workspace {
                p["workspace"] = json!(w);
            }
            if let Some(s) = scope {
                p["scope"] = json!(s);
            }
            Ok(("share.create", p))
        }
        Cmd::Review {
            common: c,
            preview,
            notes,
        } => {
            let mut p = json!({"kind": "review", "preview": preview});
            put_access(&mut p, &c.access, default_github)?;
            put_common(&mut p, c);
            if let Some(w) = &c.workspace {
                p["workspace"] = json!(w);
            }
            if let Some(n) = notes {
                p["notes"] = json!(n);
            }
            Ok(("share.create", p))
        }
        Cmd::List { .. } => Ok(("share.list", json!({}))),
        Cmd::Revoke { id } => Ok(("share.revoke", json!({"id": id}))),
        Cmd::HandoffLink {
            common: c,
            interrupt,
            ..
        } => {
            let mut p = json!({"pane": c.pane.clone().unwrap_or_else(|| "@current".into())});
            put_access(&mut p, &c.access, default_github)?;
            if let Some(t) = c.ttl_s {
                p["ttl_s"] = json!(t);
            }
            if *interrupt {
                p["interrupt"] = json!(true);
            }
            Ok(("handoff.link.create", p))
        }
        Cmd::HandoffOpen { link, .. } => Ok(("handoff.link.redeem", json!({"link": link}))),
    }
}

fn put_common(p: &mut Value, c: &Common) {
    if let Some(u) = c.uses {
        p["uses"] = json!(u);
    }
    if let Some(t) = c.ttl_s {
        p["ttl_s"] = json!(t);
    }
    if let Some(l) = &c.label {
        p["label"] = json!(l);
    }
    if let Some(pane) = &c.pane {
        p["pane"] = json!(pane);
    }
}

/// A one-line fix for the errors people hit most, or `None`.
pub fn hint(e: &CallError) -> Option<String> {
    let CallError::Rpc(r) = e else { return None };
    let kind = r.data.kind.as_str();
    let msg = r.message.to_ascii_lowercase();
    if kind == "remote_unavailable" || msg.contains("gateway isn't running") {
        return Some("the gateway is not running: start it with `vibeke gateway on`".into());
    }
    if kind == "account_required" || msg.contains("account_required") {
        return Some(
            "this host is not signed in to an account: sign in with `vibeke login`".into(),
        );
    }
    if kind == "unknown_user" {
        return Some("GitHub does not know one of those logins: check the spelling".into());
    }
    if kind == "invalid_params" && msg.contains("github") {
        return Some("set [sharing] github in config or pass --github".into());
    }
    None
}

fn fail(e: &CallError) -> i32 {
    print_error(e);
    if let Some(h) = hint(e) {
        eprintln!("vibeke: {h}");
    }
    exit_code_for(e)
}

/// The `[sharing] github` list of the user's config; empty when the config cannot be read.
pub fn config_github() -> Vec<String> {
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.sharing.github)
        .unwrap_or_default()
}

async fn gateway_call<S>(
    client: &mut Client<S>,
    method: &str,
    params: Value,
) -> Result<Value, CallError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    client
        .call(
            "gateway.call",
            json!({"method": method, "params": params, "timeout_ms": 30000}),
        )
        .await
}

/// Replace a pane or workspace target (`%3`, `w2:p5`, a name, `@current`) with its id: the
/// gateway compares share limits with canonical ids.
async fn resolve_targets<S>(client: &mut Client<S>, c: &mut Common) -> Result<(), CallError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Some(p) = c.pane.clone() {
        let v = client.call("pane.get", json!({"pane": p})).await?;
        c.pane = v["pane"]["id"].as_str().map(str::to_string);
    }
    if let Some(w) = c.workspace.clone() {
        let v = client
            .call("workspace.get", json!({"workspace": w}))
            .await?;
        c.workspace = v["workspace"]["id"].as_str().map(str::to_string);
    }
    Ok(())
}

/// The text a command prints without `--json`.
pub fn link_text(v: &Value) -> Option<String> {
    v["link"].as_str().map(str::to_string)
}

fn list_text(v: &Value) -> String {
    let mut out = Vec::new();
    for i in v["invitations"].as_array().into_iter().flatten() {
        out.push(format!(
            "{}  {}  {}  {}/{}  {}",
            i["id"].as_str().or(i["pid"].as_str()).unwrap_or("?"),
            i["kind"].as_str().unwrap_or("share"),
            i["access"].as_str().unwrap_or("link"),
            i["redeemed"].as_u64().unwrap_or(0),
            i["uses"].as_u64().unwrap_or(1),
            i["label"].as_str().unwrap_or("")
        ));
    }
    if out.is_empty() {
        out.push("no open invitations".into());
    }
    out.join("\n")
}

/// `vibeke share <verb> …` (`args` starts at the verb).
pub async fn run_share<S>(client: &mut Client<S>, g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let cmd = match parse_share(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vibeke share: {e}\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    run_cmd(client, g, cmd).await
}

/// `vibeke handoff link|open …` (`args` starts at the verb).
pub async fn run_handoff<S>(client: &mut Client<S>, g: &Global, args: &[String]) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let cmd = match parse_handoff(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vibeke handoff: {e}\n{HANDOFF_USAGE}");
            return EXIT_USAGE;
        }
    };
    run_cmd(client, g, cmd).await
}

async fn run_cmd<S>(client: &mut Client<S>, g: &Global, mut cmd: Cmd) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Err(e) = client.hello("cli").await {
        return fail(&e);
    }
    match &mut cmd {
        Cmd::Watch { common: c, .. } | Cmd::HandoffLink { common: c, .. } => {
            if let Err(e) = resolve_targets(client, c).await {
                return fail(&e);
            }
        }
        _ => {}
    }
    let (method, params) = match build_call(&cmd, &config_github()) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("vibeke: {e}");
            return EXIT_USAGE;
        }
    };
    let as_json = g.json == Some(true)
        || match &cmd {
            Cmd::Watch { common, .. }
            | Cmd::Review { common, .. }
            | Cmd::HandoffLink { common, .. } => common.json,
            Cmd::List { json } | Cmd::HandoffOpen { json, .. } => *json,
            Cmd::Revoke { .. } => false,
        };
    let first = match gateway_call(client, method, params.clone()).await {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    match &cmd {
        Cmd::Watch { .. } | Cmd::Review { .. } => {
            // An older gateway ignores `access` and returns a plain link: never print that for
            // a GitHub request. Cancel it instead.
            if params["access"] == "github" && first["access"] != "github" {
                if let Some(pid) = first["pid"].as_str() {
                    let _ = gateway_call(client, "share.revoke", json!({"id": pid})).await;
                }
                eprintln!(
                    "vibeke share: this host's gateway cannot make GitHub links yet (update vibeke); the plain link it made was cancelled"
                );
                return EXIT_API;
            }
            print_result(g, as_json, &first, |v| link_text(v).unwrap_or_default());
            EXIT_OK
        }
        Cmd::List { .. } => {
            print_result(g, as_json, &first, list_text);
            EXIT_OK
        }
        Cmd::Revoke { id } => {
            if !g.quiet {
                println!("revoked {id}");
            }
            EXIT_OK
        }
        Cmd::HandoffLink { pr, .. } => {
            let Some(job) = first["job"].as_str().map(str::to_string) else {
                eprintln!("vibeke handoff link: the gateway returned no job");
                return EXIT_API;
            };
            let done = match wait_job(client, &job).await {
                Ok(v) => v,
                Err(code) => return code,
            };
            print_result(g, as_json, &done, |v| link_text(v).unwrap_or_default());
            if *pr && let Err(e) = post_pr_comment(client, &params, &done).await {
                eprintln!("vibeke handoff link: could not comment on the pull request: {e}");
                return EXIT_API;
            }
            EXIT_OK
        }
        Cmd::HandoffOpen { .. } => {
            let Some(job) = first["job"].as_str().map(str::to_string) else {
                eprintln!("vibeke handoff open: the gateway returned no job");
                return EXIT_API;
            };
            let done = match wait_job(client, &job).await {
                Ok(v) => v,
                Err(code) => return code,
            };
            print_result(g, as_json, &done, incoming_text);
            EXIT_OK
        }
    }
}

fn print_result(g: &Global, as_json: bool, v: &Value, human: impl FnOnce(&Value) -> String) {
    if g.quiet {
        return;
    }
    if as_json {
        println!("{}", serde_json::to_string(v).unwrap_or_default());
    } else {
        println!("{}", human(v));
    }
}

/// The id of the incoming handoff a redeemed link created.
pub fn incoming_text(v: &Value) -> String {
    let i = &v["incoming"];
    i.as_str()
        .or_else(|| i["id"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "done".into())
}

/// A progress line for stderr.
pub fn progress_line(status: &Value) -> String {
    let state = status["state"].as_str().unwrap_or("working");
    match status["progress"].as_f64() {
        Some(p) if (0.0..=1.0).contains(&p) => format!("{state} {:.0}%", p * 100.0),
        Some(p) => format!("{state} {p}"),
        None => state.to_string(),
    }
}

/// Poll `handoff.link.status` every second until the job is done. A failed job prints its error
/// and returns the exit code.
async fn wait_job<S>(client: &mut Client<S>, job: &str) -> Result<Value, i32>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut last = String::new();
    loop {
        let st = gateway_call(client, "handoff.link.status", json!({"job": job}))
            .await
            .map_err(|e| fail(&e))?;
        match st["state"].as_str() {
            Some("done") => return Ok(st),
            Some("failed") => {
                let why = st["error"].as_str().unwrap_or("unknown error");
                eprintln!("vibeke handoff: failed: {why}");
                if why.contains("account_required") {
                    eprintln!("vibeke: sign in with `vibeke login`");
                }
                return Err(EXIT_API);
            }
            _ => {
                let line = progress_line(&st);
                if line != last {
                    eprintln!("{line}");
                    last = line;
                }
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Add the Markdown snippet to the pull request of the branch checked out in the pane's folder.
async fn post_pr_comment<S>(
    client: &mut Client<S>,
    params: &Value,
    done: &Value,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let markdown = done["markdown"]
        .as_str()
        .filter(|m| !m.is_empty())
        .ok_or("the gateway returned no Markdown snippet")?;
    let pane = params["pane"].as_str().unwrap_or("@current");
    let me = client
        .call("pane.get", json!({"pane": pane}))
        .await
        .map_err(|e| e.to_string())?;
    let cwd = me["pane"]["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .ok_or("the pane has no working folder")?
        .to_string();
    let markdown = markdown.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("gh")
            .args(pr_comment_args(&markdown))
            .current_dir(cwd)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("could not run gh: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The arguments of `gh` that comment on the current branch's pull request.
pub fn pr_comment_args(markdown: &str) -> Vec<String> {
    vec![
        "pr".into(),
        "comment".into(),
        "--body".into(),
        markdown.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::rpc::{ErrorData, RpcError};

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn call(a: &[&str], default: &[&str]) -> Result<(&'static str, Value), String> {
        let cmd = parse_share(&v(a))?;
        let d: Vec<String> = default.iter().map(|s| s.to_string()).collect();
        build_call(&cmd, &d)
    }

    #[test]
    fn ttl_units() {
        assert_eq!(parse_ttl("30m"), Ok(1800));
        assert_eq!(parse_ttl("24h"), Ok(86400));
        assert_eq!(parse_ttl("7d"), Ok(604800));
        assert_eq!(parse_ttl("90"), Ok(90));
        assert!(parse_ttl("").is_err());
        assert!(parse_ttl("0h").is_err());
        assert!(parse_ttl("3w").is_err());
        assert!(parse_ttl("h").is_err());
    }

    #[test]
    fn watch_uses_config_list_by_default() {
        let (m, p) = call(&["watch", "--workspace", "ws1"], &["alice", "bob"]).unwrap();
        assert_eq!(m, "share.create");
        assert_eq!(
            p,
            json!({"kind": "share", "access": "github", "github": ["alice", "bob"], "workspace": "ws1"})
        );
    }

    #[test]
    fn default_list_empty_is_an_error_naming_the_fix() {
        let e = call(&["watch"], &[]).unwrap_err();
        assert!(
            e.contains("[sharing] github") && e.contains("--github"),
            "{e}"
        );
        assert!(call(&["watch", "--link"], &[]).is_ok());
    }

    #[test]
    fn access_flags() {
        let (_, p) = call(&["watch", "--any-github"], &[]).unwrap();
        assert_eq!(p["github"], json!(["*"]));
        assert_eq!(p["access"], "github");
        let (_, p) = call(&["watch", "--link"], &["x"]).unwrap();
        assert_eq!(p["access"], "link");
        assert!(p.get("github").is_none());
        let (_, p) = call(&["watch", "--github", "@a, b"], &["x"]).unwrap();
        assert_eq!(p["github"], json!(["a", "b"]));
        assert!(parse_share(&v(&["watch", "--link", "--any-github"])).is_err());
        assert!(parse_share(&v(&["watch", "--github", "a", "--link"])).is_err());
    }

    #[test]
    fn watch_options() {
        let (_, p) = call(
            &[
                "watch",
                "--pane=%3",
                "--scope",
                "approve",
                "--uses",
                "5",
                "--ttl",
                "24h",
                "--label",
                "demo",
                "--link",
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            p,
            json!({"kind": "share", "access": "link", "pane": "%3", "scope": "approve",
                   "uses": 5, "ttl_s": 86400, "label": "demo"})
        );
        assert!(parse_share(&v(&["watch", "--scope", "full"])).is_err());
        assert!(parse_share(&v(&["watch", "--pane", "p", "--workspace", "w"])).is_err());
        assert!(parse_share(&v(&["watch", "--uses", "0"])).is_err());
    }

    #[test]
    fn review_params() {
        let (m, p) = call(&["review", "v4", "--notes", "held"], &["alice"]).unwrap();
        assert_eq!(m, "share.create");
        assert_eq!(
            p,
            json!({"kind": "review", "preview": "v4", "access": "github",
                   "github": ["alice"], "notes": "held"})
        );
        assert!(parse_share(&v(&["review"])).is_err());
        assert!(parse_share(&v(&["review", "v4", "--notes", "maybe"])).is_err());
    }

    #[test]
    fn list_and_revoke() {
        assert_eq!(call(&["list"], &[]).unwrap(), ("share.list", json!({})));
        assert_eq!(
            call(&["revoke", "pid1"], &[]).unwrap(),
            ("share.revoke", json!({"id": "pid1"}))
        );
        assert!(parse_share(&v(&["revoke"])).is_err());
        assert!(parse_share(&v(&["bogus"])).is_err());
    }

    #[test]
    fn handoff_link_params() {
        let cmd = parse_handoff(&v(&[
            "link",
            "--pane",
            "%2",
            "--ttl",
            "7d",
            "--pr",
            "--any-github",
        ]))
        .unwrap();
        let Cmd::HandoffLink { pr, .. } = &cmd else {
            panic!()
        };
        assert!(*pr);
        let (m, p) = build_call(&cmd, &[]).unwrap();
        assert_eq!(m, "handoff.link.create");
        assert_eq!(
            p,
            json!({"pane": "%2", "access": "github", "github": ["*"], "ttl_s": 604800})
        );
        let cmd = parse_handoff(&v(&["link", "--link", "--interrupt"])).unwrap();
        let (_, p) = build_call(&cmd, &[]).unwrap();
        assert_eq!(
            p,
            json!({"pane": "@current", "access": "link", "interrupt": true})
        );
        assert!(parse_handoff(&v(&["link", "--label", "x"])).is_err());
    }

    #[test]
    fn handoff_open_params() {
        let cmd = parse_handoff(&v(&[
            "open",
            "https://app.example/#/continue?d=abc",
            "--json",
        ]))
        .unwrap();
        let (m, p) = build_call(&cmd, &[]).unwrap();
        assert_eq!(m, "handoff.link.redeem");
        assert_eq!(p, json!({"link": "https://app.example/#/continue?d=abc"}));
        assert!(parse_handoff(&v(&["open"])).is_err());
    }

    #[test]
    fn pr_comment_command() {
        assert_eq!(
            pr_comment_args("**Continue**"),
            v(&["pr", "comment", "--body", "**Continue**"])
        );
    }

    #[test]
    fn progress_lines() {
        assert_eq!(
            progress_line(&json!({"state": "uploading", "progress": 0.5})),
            "uploading 50%"
        );
        assert_eq!(progress_line(&json!({"state": "exporting"})), "exporting");
    }

    #[test]
    fn incoming_ids() {
        assert_eq!(incoming_text(&json!({"incoming": {"id": "h1"}})), "h1");
        assert_eq!(incoming_text(&json!({"incoming": "h2"})), "h2");
        assert_eq!(incoming_text(&json!({})), "done");
    }

    fn rpc(kind: &str, msg: &str) -> CallError {
        CallError::Rpc(RpcError {
            code: -32000,
            message: msg.into(),
            data: ErrorData {
                kind: kind.into(),
                details: Value::Null,
                retryable: false,
            },
        })
    }

    #[test]
    fn error_hints() {
        let h = hint(&rpc("remote_unavailable", "the gateway isn't running")).unwrap();
        assert!(h.contains("vibeke gateway on"));
        assert!(
            hint(&rpc("account_required", "x"))
                .unwrap()
                .contains("vibeke login")
        );
        assert!(
            hint(&rpc("invalid_params", "github list is empty"))
                .unwrap()
                .contains("[sharing] github")
        );
        assert!(hint(&rpc("invalid_params", "uses too big")).is_none());
        assert!(hint(&rpc("not_found", "x")).is_none());
    }
}
