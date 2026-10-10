//! `vibeke cloud login | send | bring-back` (spec 17 §8): the cloud verbs that need the
//! terminal. The other verbs (`providers`, `logout`, `ls`, `rm`, `prune`, `adopt`, `suspend`,
//! `resume`, `jobs`, `cancel`, `move`) are plain API commands in `vk_cli::COMMANDS`, and
//! `cloud exec` is the provider shim that box panes run.
//!
//! - `login <provider> [--import <source>]` asks the server to check a token (`cloud.auth.set`)
//!   or to import one from another tool (`cloud.auth.import`). On a terminal the token is typed
//!   with echo off; otherwise it is one line of stdin. It never appears in argv, in output or in
//!   an error message.
//! - `send [--pane P] [--provider X] [--box B] [--interrupt]` sends an agent to a sandbox
//!   (`cloud.move`). The pane defaults to the one the command runs in.
//! - `bring-back <pane|box> [--to local|<peer>]` brings it back. A `provider/id` argument is a
//!   box.
//!
//! `send` and `bring-back` follow the job (`cloud.jobs`) and print each state change until it
//! ends; `--no-wait` prints the job instead. Without a credential they print
//! `Sign in first: vibeke cloud login <provider>` and exit with the permission code. Inside a
//! pane `cloud.move` is not allowed, so both verbs ask for approval in Vibeke first
//! (`auth.approve`, like `vibeke handoff send`) and do not follow the job.

use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use serde_json::{Value, json};
use vk_cli::client::{CallError, Client};
use vk_cli::handoff::{Ask, in_pane};
use vk_cli::{EXIT_API, EXIT_OK, EXIT_PERMISSION, EXIT_USAGE, Global, exit_code_for, print_error};

/// The verbs this module runs.
pub const VERBS: &[&str] = &["login", "send", "bring-back"];

pub const USAGE: &str = "vibeke cloud login <provider> [--import <source>]\n\
vibeke cloud send [--pane P] [--provider X] [--box <provider/id>] [--interrupt] [--no-wait]\n\
vibeke cloud bring-back <pane|provider/id> [--to local|<peer>] [--source-after keep|suspend|destroy] [--no-wait]\n\
  (inside a pane both ask for approval in Vibeke first: [--reason TEXT] [--timeout-ms MS])";

/// How often the job is read while following it.
const POLL: Duration = Duration::from_millis(1000);

pub fn handles(verb: Option<&str>) -> bool {
    verb.is_some_and(|v| VERBS.contains(&v))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Login {
        provider: String,
        import: Option<String>,
    },
    Send {
        pane: Option<String>,
        provider: Option<String>,
        target_box: Option<String>,
        interrupt: bool,
        wait: bool,
        ask: Ask,
    },
    BringBack {
        target: String,
        to: Option<String>,
        source_after: Option<String>,
        wait: bool,
        ask: Ask,
    },
}

/// Flags that take a value.
const VALUED: &[&str] = &[
    "import",
    "pane",
    "provider",
    "box",
    "to",
    "source-after",
    "reason",
    "timeout-ms",
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

impl Args {
    fn value(&self, name: &str) -> Option<String> {
        self.flags
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.clone())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }

    /// Only these flags are allowed.
    fn only(&self, allowed: &[&str]) -> Result<(), String> {
        match self
            .flags
            .iter()
            .find(|(n, _)| !allowed.contains(&n.as_str()))
        {
            Some((n, _)) => Err(format!("unknown flag --{n}")),
            None => Ok(()),
        }
    }

    fn ask(&self) -> Result<Ask, String> {
        let timeout_ms = match self.value("timeout-ms") {
            Some(t) => Some(
                t.parse::<u64>()
                    .map_err(|_| "--timeout-ms needs a number".to_string())?,
            ),
            None => None,
        };
        Ok(Ask {
            no_wait: self.has("no-wait"),
            reason: self.value("reason"),
            timeout_ms,
        })
    }
}

/// `args[0]` is the verb.
pub fn parse(args: &[String]) -> Result<Cmd, String> {
    let (verb, rest) = args.split_first().ok_or("missing verb")?;
    let a = split(rest)?;
    match verb.as_str() {
        "login" => {
            a.only(&["import"])?;
            match a.pos.as_slice() {
                [p] if !p.is_empty() => Ok(Cmd::Login {
                    provider: p.clone(),
                    import: a.value("import"),
                }),
                [] => Err("vibeke cloud login needs a provider".into()),
                _ => Err(format!("unexpected argument `{}`", a.pos[1])),
            }
        }
        "send" => {
            a.only(&[
                "pane",
                "provider",
                "box",
                "interrupt",
                "no-wait",
                "reason",
                "timeout-ms",
            ])?;
            if let Some(extra) = a.pos.first() {
                return Err(format!("unexpected argument `{extra}`"));
            }
            Ok(Cmd::Send {
                pane: a.value("pane"),
                provider: a.value("provider"),
                target_box: a.value("box"),
                interrupt: a.has("interrupt"),
                wait: !a.has("no-wait"),
                ask: a.ask()?,
            })
        }
        "bring-back" => {
            a.only(&["to", "source-after", "no-wait", "reason", "timeout-ms"])?;
            match a.pos.as_slice() {
                [t] if !t.is_empty() => Ok(Cmd::BringBack {
                    target: t.clone(),
                    to: a.value("to"),
                    source_after: a.value("source-after"),
                    wait: !a.has("no-wait"),
                    ask: a.ask()?,
                }),
                [] => Err("vibeke cloud bring-back needs a pane or a sandbox".into()),
                _ => Err(format!("unexpected argument `{}`", a.pos[1])),
            }
        }
        other => Err(format!("unknown command `cloud {other}`")),
    }
}

// ---- params ----------------------------------------------------------------------------------

/// The pane the command runs in.
fn own_pane() -> Option<String> {
    ["VIBEKE_PANE_ULID", "VIBEKE_PANE_ID", "VIBEKE_PANE"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

/// `cloud.move` params for `send`.
pub fn send_params(
    pane: &str,
    provider: Option<&str>,
    target_box: Option<&str>,
    interrupt: bool,
) -> Value {
    let mut to = json!({"kind": "cloud"});
    if let Some(p) = provider {
        to["provider"] = json!(p);
    }
    if let Some(b) = target_box {
        to["box"] = json!(b);
    }
    json!({"pane": pane, "to": to, "interrupt": interrupt})
}

/// `cloud.move` params for `bring-back`: `provider/id` is a sandbox, anything else a pane;
/// `--to` is `local` (the default) or a peer.
pub fn bring_back_params(target: &str, to: Option<&str>, source_after: Option<&str>) -> Value {
    let mut p = if target.contains('/') {
        json!({"box": target})
    } else {
        json!({"pane": target})
    };
    p["to"] = match to {
        None | Some("local" | "") => json!({"kind": "local"}),
        Some(peer) => json!({"kind": "peer", "peer": peer}),
    };
    if let Some(s) = source_after {
        p["source_after"] = json!(s);
    }
    p
}

// ---- the token -------------------------------------------------------------------------------

/// Echo off on stdin until dropped.
struct NoEcho(libc::termios);

impl NoEcho {
    fn enter() -> Option<NoEcho> {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a termios we own.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(NoEcho(saved))
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restoring the attributes read in `enter`.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.0);
        }
    }
}

/// One line of `r`, trimmed: the token when it is piped in.
pub fn read_line_from(r: &mut dyn BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// The token: typed with echo off on a terminal, else one line of stdin. Never echoed.
fn read_token(provider: &str) -> Result<String, String> {
    let stdin = std::io::stdin();
    let token = if stdin.is_terminal() {
        let Some(guard) = NoEcho::enter() else {
            return Err(
                "cannot turn off echo on this terminal: pipe the token on stdin instead".into(),
            );
        };
        eprint!("{provider} token (input is hidden): ");
        let _ = std::io::stderr().flush();
        let r = read_line_from(&mut stdin.lock());
        drop(guard);
        eprintln!();
        r.map_err(|e| format!("cannot read the token: {e}"))?
    } else {
        read_line_from(&mut stdin.lock()).map_err(|e| format!("cannot read the token: {e}"))?
    };
    if token.is_empty() {
        return Err("no token given".into());
    }
    if token.chars().any(char::is_control) {
        return Err("the token has control characters: paste only the token".into());
    }
    Ok(token)
}

// ---- running ---------------------------------------------------------------------------------

fn want_json(g: &Global) -> bool {
    g.json.unwrap_or(!std::io::stdout().is_terminal())
}

pub async fn run(g: &Global, args: &[String]) -> i32 {
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    crate::with_client(g, |mut c| async move { run_with(&mut c, g, cmd).await }).await
}

pub async fn run_with<S>(client: &mut Client<S>, g: &Global, cmd: Cmd) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    match cmd {
        Cmd::Login { provider, import } => login(client, g, &provider, import.as_deref()).await,
        Cmd::Send {
            pane,
            provider,
            target_box,
            interrupt,
            wait,
            ask,
        } => {
            let Some(pane) = pane.or_else(own_pane) else {
                eprintln!("vibeke cloud send needs --pane when it does not run in a pane\n{USAGE}");
                return EXIT_USAGE;
            };
            let params = send_params(&pane, provider.as_deref(), target_box.as_deref(), interrupt);
            start_move(client, g, params, provider.as_deref(), wait, &ask).await
        }
        Cmd::BringBack {
            target,
            to,
            source_after,
            wait,
            ask,
        } => {
            let params = bring_back_params(&target, to.as_deref(), source_after.as_deref());
            start_move(client, g, params, None, wait, &ask).await
        }
    }
}

async fn login<S>(client: &mut Client<S>, g: &Global, provider: &str, import: Option<&str>) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Which providers exist, and where a token comes from (best effort: an older server has none).
    if let Ok(v) = client.call("cloud.providers", json!({})).await {
        let list = v["providers"].as_array().cloned().unwrap_or_default();
        let known: Vec<&str> = list.iter().filter_map(|p| p["id"].as_str()).collect();
        if !known.is_empty() && !known.contains(&provider) {
            eprintln!(
                "unknown provider `{provider}`; available: {}",
                known.join(", ")
            );
            return EXIT_USAGE;
        }
        if import.is_none()
            && std::io::stdin().is_terminal()
            && let Some(url) = list
                .iter()
                .find(|p| p["id"].as_str() == Some(provider))
                .and_then(|p| p["methods"].as_array())
                .and_then(|m| m.iter().find(|m| m["kind"] == "paste_token"))
                .and_then(|m| m["help_url"].as_str())
        {
            eprintln!("Create a token at {url}");
        }
    }
    let (method, params) = match import {
        Some(source) => (
            "cloud.auth.import",
            json!({"provider": provider, "source": source}),
        ),
        None => {
            let token = match read_token(provider) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("{e}");
                    return EXIT_USAGE;
                }
            };
            (
                "cloud.auth.set",
                json!({"provider": provider, "token": token}),
            )
        }
    };
    match client.call(method, params).await {
        Ok(v) => {
            if g.quiet {
                return EXIT_OK;
            }
            if want_json(g) {
                println!("{}", serde_json::to_string(&v).unwrap_or_default());
            } else {
                println!("{}", signed_in_line(&v, provider));
            }
            EXIT_OK
        }
        Err(e) => {
            if let CallError::Rpc(r) = &e
                && r.data.details["reason"] == "needs_auth"
            {
                eprintln!("{provider} did not accept the token.");
            }
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

/// "Signed in to sprites as acme."
pub fn signed_in_line(v: &Value, provider: &str) -> String {
    let p = v["provider"].as_str().unwrap_or(provider);
    match v["account"].as_str().filter(|a| !a.is_empty()) {
        Some(a) => format!("Signed in to {p} as {a}."),
        None => format!("Signed in to {p}."),
    }
}

/// The provider a `needs_auth` error names, else `hint`.
pub fn needs_auth_provider(details: &Value, hint: Option<&str>) -> Option<String> {
    if details["reason"] != "needs_auth" {
        return None;
    }
    Some(
        details["provider"]
            .as_str()
            .or(hint)
            .unwrap_or("<provider>")
            .to_string(),
    )
}

async fn start_move<S>(
    client: &mut Client<S>,
    g: &Global,
    params: Value,
    provider_hint: Option<&str>,
    wait: bool,
    ask: &Ask,
) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // `cloud.move` is not allowed from a pane: the user approves it in Vibeke.
    if in_pane() {
        return vk_cli::handoff::approved(client, g, "cloud.move", params, ask, job_line).await;
    }
    match client.call("cloud.move", params).await {
        Ok(v) => {
            let job = v.get("job").filter(|j| j.is_object()).unwrap_or(&v).clone();
            if !wait {
                print_job(g, &job);
                return EXIT_OK;
            }
            if !g.quiet {
                eprintln!(
                    "{} (Ctrl-C stops waiting; the move goes on: `vibeke cloud jobs`)",
                    job_line(&job)
                );
            }
            follow(client, g, job, provider_hint).await
        }
        Err(e) => {
            if let CallError::Rpc(r) = &e
                && let Some(p) = needs_auth_provider(&r.data.details, provider_hint)
            {
                eprintln!("Sign in first: vibeke cloud login {p}");
                return EXIT_PERMISSION;
            }
            print_error(&e);
            exit_code_for(&e)
        }
    }
}

fn print_job(g: &Global, job: &Value) {
    if g.quiet {
        return;
    }
    if want_json(g) {
        println!("{}", serde_json::to_string(job).unwrap_or_default());
    } else {
        println!("{}", job_line(job));
    }
}

/// One line about a job: "send j1: creating (pane p1)".
pub fn job_line(v: &Value) -> String {
    let j = v.get("job").filter(|j| j.is_object()).unwrap_or(v);
    let what = if let Some(p) = j["pane"].as_str() {
        format!("pane {p}")
    } else if let Some(b) = j["box"].as_str() {
        format!("sandbox {b}")
    } else {
        "move".to_string()
    };
    format!(
        "{} {}: {} ({what})",
        j["direction"].as_str().unwrap_or("move").replace('_', " "),
        j["id"].as_str().unwrap_or("?"),
        j["state"].as_str().unwrap_or("?").replace('_', " ")
    )
}

/// What a state change prints: "creating the sandbox (25%)".
pub fn progress_line(job: &Value) -> String {
    let state = job["state"].as_str().unwrap_or("queued");
    let text = match state {
        "queued" => "queued",
        "waiting_turn" => "waiting for the agent's turn to end",
        "creating" => "creating the sandbox",
        "bootstrapping" => "setting up Vibeke in the sandbox",
        "exporting" => "exporting the work",
        "uploading" => "uploading",
        "importing" => "importing",
        "resuming" => "resuming the agent",
        "done" => "done",
        "failed" => "failed",
        "cancelled" => "cancelled",
        other => other,
    };
    let (done, total) = (
        job["progress"]["done"].as_u64().unwrap_or(0),
        job["progress"]["total"].as_u64().unwrap_or(0),
    );
    match (done * 100).checked_div(total) {
        Some(p) if !matches!(state, "done" | "failed" | "cancelled") => {
            format!("{text} ({}%)", p.min(100))
        }
        _ => text.to_string(),
    }
}

/// Print each state change until the job ends. Done is 0; a failure for lack of a sign-in says
/// how to sign in and exits with the permission code.
async fn follow<S>(
    client: &mut Client<S>,
    g: &Global,
    mut job: Value,
    provider_hint: Option<&str>,
) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let id = job["id"].as_str().unwrap_or_default().to_string();
    let mut last = String::new();
    loop {
        let line = progress_line(&job);
        if line != last {
            if !g.quiet {
                eprintln!("{line}");
            }
            last = line;
        }
        match job["state"].as_str() {
            Some("done") => {
                print_final(g, &job);
                return EXIT_OK;
            }
            Some("failed") => {
                let err = &job["error"];
                if let Some(p) = needs_auth_provider(&err["details"], provider_hint) {
                    eprintln!("Sign in first: vibeke cloud login {p}");
                    return EXIT_PERMISSION;
                }
                eprintln!(
                    "failed: {}",
                    err["message"].as_str().unwrap_or("the move failed")
                );
                if let Some(r) = err["details"]["reason"].as_str() {
                    eprintln!("reason: {r}");
                }
                if want_json(g) && !g.quiet {
                    println!("{}", serde_json::to_string(&job).unwrap_or_default());
                }
                return EXIT_API;
            }
            Some("cancelled") => return EXIT_API,
            _ => {}
        }
        tokio::time::sleep(POLL).await;
        match client.call("cloud.jobs", json!({})).await {
            Ok(v) => {
                let found = v["jobs"]
                    .as_array()
                    .and_then(|a| a.iter().find(|j| j["id"].as_str() == Some(id.as_str())));
                match found {
                    Some(j) => job = j.clone(),
                    None => {
                        eprintln!("the job {id} is no longer listed: see `vibeke cloud jobs`");
                        return EXIT_API;
                    }
                }
            }
            Err(e) => {
                print_error(&e);
                return exit_code_for(&e);
            }
        }
    }
}

fn print_final(g: &Global, job: &Value) {
    if g.quiet {
        return;
    }
    if want_json(g) {
        println!("{}", serde_json::to_string(job).unwrap_or_default());
        return;
    }
    let r = &job["result"];
    let mut out = String::from("Done.");
    if let Some(p) = r["pane"].as_str() {
        out.push_str(&format!(" New pane: {p}."));
    }
    if let Some(t) = r["task"].as_str() {
        out.push_str(&format!(" Task: {t}."));
    }
    if let Some(b) = r["box"].as_str() {
        out.push_str(&format!(" Sandbox: {b}."));
    }
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn only_the_interactive_verbs_are_handled_here() {
        for v in ["login", "send", "bring-back"] {
            assert!(handles(Some(v)));
        }
        // The plain API verbs and `exec` stay with their own dispatch.
        for v in [
            "exec",
            "providers",
            "logout",
            "ls",
            "rm",
            "prune",
            "adopt",
            "suspend",
            "resume",
            "jobs",
            "cancel",
            "move",
        ] {
            assert!(!handles(Some(v)), "{v}");
        }
        assert!(!handles(None));
    }

    #[test]
    fn parses_login() {
        assert_eq!(
            parse(&args(&["login", "sprites"])).unwrap(),
            Cmd::Login {
                provider: "sprites".into(),
                import: None
            }
        );
        assert_eq!(
            parse(&args(&["login", "sprites", "--import", "fly"])).unwrap(),
            Cmd::Login {
                provider: "sprites".into(),
                import: Some("fly".into())
            }
        );
        assert!(parse(&args(&["login"])).is_err());
        // A token on the command line would end up in the process list: there is no flag.
        assert!(parse(&args(&["login", "sprites", "--token", "x"])).is_err());
    }

    #[test]
    fn parses_send_and_bring_back() {
        match parse(&args(&[
            "send",
            "--pane",
            "p3",
            "--provider=e2b",
            "--box",
            "e2b/x1",
            "--interrupt",
            "--no-wait",
        ]))
        .unwrap()
        {
            Cmd::Send {
                pane,
                provider,
                target_box,
                interrupt,
                wait,
                ..
            } => {
                assert_eq!(pane.as_deref(), Some("p3"));
                assert_eq!(provider.as_deref(), Some("e2b"));
                assert_eq!(target_box.as_deref(), Some("e2b/x1"));
                assert!(interrupt && !wait);
            }
            c => panic!("{c:?}"),
        }
        assert!(parse(&args(&["send", "p3"])).is_err());
        match parse(&args(&["bring-back", "p3", "--to", "marvin"])).unwrap() {
            Cmd::BringBack {
                target, to, wait, ..
            } => {
                assert_eq!(target, "p3");
                assert_eq!(to.as_deref(), Some("marvin"));
                assert!(wait);
            }
            c => panic!("{c:?}"),
        }
        assert!(parse(&args(&["bring-back"])).is_err());
        assert!(parse(&args(&["bring-back", "p3", "--timeout-ms", "soon"])).is_err());
    }

    #[test]
    fn builds_cloud_move_params() {
        assert_eq!(
            send_params("p1", Some("sprites"), None, false),
            json!({"pane": "p1", "to": {"kind": "cloud", "provider": "sprites"}, "interrupt": false})
        );
        assert_eq!(
            send_params("p1", None, Some("e2b/x1"), true),
            json!({"pane": "p1", "to": {"kind": "cloud", "box": "e2b/x1"}, "interrupt": true})
        );
        assert_eq!(
            bring_back_params("p1", None, None),
            json!({"pane": "p1", "to": {"kind": "local"}})
        );
        assert_eq!(
            bring_back_params("sprites/b1", Some("local"), Some("destroy")),
            json!({"box": "sprites/b1", "to": {"kind": "local"}, "source_after": "destroy"})
        );
        assert_eq!(
            bring_back_params("p1", Some("marvin"), None),
            json!({"pane": "p1", "to": {"kind": "peer", "peer": "marvin"}})
        );
    }

    #[test]
    fn the_token_is_one_trimmed_line() {
        let mut r = std::io::Cursor::new(b"  sk-abc \nsecond\n".to_vec());
        assert_eq!(read_line_from(&mut r).unwrap(), "sk-abc");
        let mut empty = std::io::Cursor::new(Vec::new());
        assert_eq!(read_line_from(&mut empty).unwrap(), "");
    }

    #[test]
    fn needs_auth_names_the_provider_to_sign_in_to() {
        let d = json!({"reason": "needs_auth", "provider": "sprites"});
        assert_eq!(
            needs_auth_provider(&d, Some("e2b")).as_deref(),
            Some("sprites")
        );
        assert_eq!(
            needs_auth_provider(&json!({"reason": "needs_auth"}), Some("e2b")).as_deref(),
            Some("e2b")
        );
        assert_eq!(needs_auth_provider(&json!({"reason": "other"}), None), None);
        assert_eq!(needs_auth_provider(&Value::Null, None), None);
    }

    #[test]
    fn lines_for_people() {
        let j = json!({"id": "j1", "direction": "bring_back", "state": "waiting_turn",
                       "pane": "p1", "progress": {"done": 1, "total": 4}});
        assert_eq!(job_line(&j), "bring back j1: waiting turn (pane p1)");
        assert_eq!(
            progress_line(&j),
            "waiting for the agent's turn to end (25%)"
        );
        assert_eq!(progress_line(&json!({"state": "done"})), "done");
        assert_eq!(
            progress_line(&json!({"state": "done", "progress": {"done": 4, "total": 4}})),
            "done"
        );
        assert_eq!(
            signed_in_line(&json!({"provider": "sprites", "account": "acme"}), "x"),
            "Signed in to sprites as acme."
        );
        assert_eq!(signed_in_line(&json!({}), "e2b"), "Signed in to e2b.");
    }
}
