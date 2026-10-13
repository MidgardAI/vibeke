//! Share, review and continue links in the TUI (spec 20 §7.1): what the pieces of the People,
//! Hosts and Handoffs views share.
//!
//! - **Continue links** (`<app>/#/continue?d=<base64url JSON>`): read locally, without contacting
//!   anyone, so the Hosts paste field can show the brief (repository, branch, agent, title, last
//!   message, sender, expiry) before an import. The JSON is `{v: 1, server, blob, k, sha256,
//!   size, exp, brief: {repo, branch, harness, title, last, from}}`.
//! - **Link jobs** ([`Job`]): `handoff.link.create` and `handoff.link.redeem` answer `{job}`; the
//!   view then asks `handoff.link.status {job}` about once a second until the job is done or
//!   failed.
//! - **Errors** ([`share_error`]): the codes `share.create` adds map to readable messages.
//! - **GitHub logins** ([`github_default`]): the `[sharing] github` list of the config.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::app::{App, RpcErr};
use crate::inbox::fmt_age;
use crate::sharing::{Invite, bridge_error, parse_link};
use crate::time::{Duration, Instant};

/// Where a bare share or review code opens in the browser.
pub const APP_URL: &str = "https://app.vibeke.dev";

/// How often a running link job is asked about.
pub const POLL: Duration = Duration::from_secs(1);

fn b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

fn text(v: &Value, k: &str, max: usize) -> String {
    crate::plugins::sanitize(v.get(k).and_then(Value::as_str).unwrap_or(""), max)
}

// ---- the config's GitHub list --------------------------------------------------------------------

/// One login as typed: no `@`, no spaces; None when nothing is left.
pub fn clean_login(s: &str) -> Option<String> {
    let l: String = s
        .trim()
        .trim_start_matches('@')
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!l.is_empty()).then_some(l)
}

/// `[sharing] github` of the config: the logins a new link is made for when the user does not
/// name any, cleaned and without repeats.
pub fn github_default(app: &App) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in app
        .config
        .sharing
        .github
        .iter()
        .filter_map(|s| clean_login(s))
    {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&l)) {
            out.push(l);
        }
    }
    out
}

/// "@alice, @bob".
pub fn logins_text(list: &[String]) -> String {
    list.iter()
        .map(|l| format!("@{l}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The login in a `github` value: `{id, login}` or a bare string.
pub fn login_of(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.as_str(),
        Value::Object(_) => v.get("login").and_then(Value::as_str)?,
        _ => return None,
    };
    clean_login(s)
}

/// The logins in a `github` value that is a list (`[{id, login}]`), or None for `"*"` / absent.
pub fn logins_of(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(login_of).collect())
        .unwrap_or_default()
}

// ---- errors --------------------------------------------------------------------------------------

/// What a failed `share.create` or `handoff.link.*` call says, for the codes spec 20 adds.
pub fn share_error(e: &RpcErr) -> String {
    match e.kind.as_str() {
        "account_required" => {
            "this host is not signed in: run `vibeke login`, then try again".into()
        }
        "unavailable" => "the account server cannot be reached: try again in a moment".into(),
        "unknown_user" => {
            let who = e
                .details
                .get("login")
                .and_then(Value::as_str)
                .and_then(clean_login)
                .map(|l| format!("@{l}"))
                .unwrap_or_else(|| "one of the logins".into());
            format!("{who} is not a GitHub user: check the login")
        }
        _ => bridge_error(e),
    }
}

// ---- continue links ------------------------------------------------------------------------------

/// What a continue link says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Continue {
    pub repo: String,
    pub branch: String,
    pub harness: String,
    pub title: String,
    pub last: String,
    pub from: String,
    /// Unix seconds: the link stops working.
    pub exp: i64,
    pub size: u64,
}

/// A pasted link: a continue link, or a pairing link of the other kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Continue(Continue),
    Pairing(Invite),
}

/// The decoded `d` value of a link (`<app>/#/continue?d=…`, `vibeke://…?d=…` or the bare code).
fn payload(s: &str) -> Option<Value> {
    let s: String = s.split_whitespace().collect();
    let d = match s.find("d=") {
        Some(i) if s.contains('#') || s.contains('?') => &s[i + 2..],
        _ => s.as_str(),
    };
    let d = d.split('&').next().unwrap_or(d);
    b64(d)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
}

/// Does `s` look like a continue link (by path, or by the payload's `blob`)?
pub fn is_continue(s: &str) -> bool {
    s.contains("/continue") || payload(s).is_some_and(|v| v.get("blob").is_some())
}

/// Parse a continue link locally.
pub fn parse_continue(s: &str) -> Result<Continue, String> {
    const NOT: &str = "not a Vibeke continue link";
    let v = payload(s).ok_or_else(|| NOT.to_string())?;
    match v.get("v").and_then(Value::as_u64) {
        Some(1) => {}
        Some(_) => return Err("this link needs a newer Vibeke".into()),
        None => return Err(NOT.into()),
    }
    if v.get("blob").and_then(Value::as_str).is_none()
        || v.get("k").and_then(Value::as_str).and_then(b64).is_none()
    {
        return Err(NOT.into());
    }
    let brief = v.get("brief").cloned().unwrap_or(Value::Null);
    Ok(Continue {
        repo: text(&brief, "repo", 80),
        branch: text(&brief, "branch", 80),
        harness: text(&brief, "harness", 40),
        title: text(&brief, "title", 120),
        last: text(&brief, "last", 240),
        from: text(&brief, "from", 64),
        exp: v.get("exp").and_then(Value::as_i64).unwrap_or(0),
        size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Read any pasted link: continue links first, then the pairing kinds.
pub fn classify(s: &str) -> Result<Link, String> {
    if s.trim().is_empty() {
        return Err("paste an invitation link first".into());
    }
    if is_continue(s) {
        parse_continue(s).map(Link::Continue)
    } else {
        parse_link(s).map(Link::Pairing)
    }
}

impl Continue {
    /// Why this link can't be imported, if so.
    pub fn refusal(&self, now_s: i64) -> Option<String> {
        (self.exp > 0 && self.exp <= now_s).then(|| {
            format!(
                "this link expired {} ago: ask for a new one",
                fmt_age((now_s - self.exp) * 1000)
            )
        })
    }

    /// The brief as display lines: (label, value), empty values left out.
    pub fn lines(&self, now_s: i64) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        let mut add = |k: &'static str, s: String| {
            if !s.is_empty() {
                v.push((k, s));
            }
        };
        add("From", self.from.clone());
        add("Repository", self.repo.clone());
        add("Branch", self.branch.clone());
        add("Agent", self.harness.clone());
        add("Title", self.title.clone());
        add("Last message", self.last.clone());
        if self.exp > 0 {
            add(
                "Expires",
                if self.exp > now_s {
                    format!("in {}", fmt_age((self.exp - now_s) * 1000))
                } else {
                    "expired".into()
                },
            );
        }
        v
    }
}

/// The URL to open in the browser for a pasted share or review link: the link as pasted when it
/// is a web address, else the app's page for the bare code.
pub fn browser_url(input: &str, path: &str) -> String {
    let s: String = input.split_whitespace().collect();
    if s.starts_with("http://") || s.starts_with("https://") {
        return s;
    }
    let d = match s.find("d=") {
        Some(i) => s[i + 2..].split('&').next().unwrap_or("").to_string(),
        None => s,
    };
    format!("{APP_URL}/#/{path}?d={d}")
}

// ---- link jobs -----------------------------------------------------------------------------------

/// How far a link job got.
#[derive(Debug, Clone, PartialEq)]
pub enum Finish {
    Running,
    Done,
    Failed(String),
}

/// A `handoff.link.create` or `handoff.link.redeem` job being followed.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub state: String,
    /// 0.0 to 1.0.
    pub progress: Option<f64>,
    /// The last status answer, kept for the fields only a finished job has.
    pub last: Value,
    /// When `handoff.link.status` was last sent.
    pub asked: Option<Instant>,
}

impl Job {
    pub fn new(id: &str) -> Job {
        Job {
            id: id.to_string(),
            state: "exporting".into(),
            progress: None,
            last: Value::Null,
            asked: None,
        }
    }

    /// Time to ask again (the first ask is immediate).
    pub fn due(&self, now: Instant) -> bool {
        self.asked.is_none_or(|t| now.duration_since(t) >= POLL)
    }

    /// When the next ask is due.
    pub fn next_at(&self, now: Instant) -> Instant {
        self.asked.map_or(now, |t| t + POLL)
    }

    /// Fold in a `handoff.link.status` answer.
    pub fn apply(&mut self, v: &Value) -> Finish {
        self.last = v.clone();
        if let Some(s) = v.get("state").and_then(Value::as_str) {
            self.state = s.to_string();
        }
        self.progress = v.get("progress").and_then(Value::as_f64).map(|p| {
            if p > 1.0 {
                (p / 100.0).min(1.0)
            } else {
                p.max(0.0)
            }
        });
        match self.state.as_str() {
            "done" => Finish::Done,
            "failed" => Finish::Failed(
                v.get("error")
                    .and_then(Value::as_str)
                    .map(|e| crate::plugins::sanitize(e, 200))
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| "the job failed".into()),
            ),
            _ => Finish::Running,
        }
    }

    /// "exporting the session…", "uploading 42%…".
    pub fn text(&self) -> String {
        let what = match self.state.as_str() {
            "exporting" => "exporting the session",
            "uploading" => "uploading",
            "downloading" => "downloading",
            "importing" => "importing",
            "done" => "done",
            other => return format!("{}…", crate::plugins::sanitize(other, 24)),
        };
        match self.progress {
            Some(p) if self.state != "done" => format!("{what} {}%…", (p * 100.0).round() as u32),
            _ => format!("{what}…"),
        }
    }

    /// The incoming handoff a finished redeem made (`incoming` as an id or an object).
    pub fn incoming(&self) -> Option<String> {
        let i = self.last.get("incoming")?;
        i.as_str()
            .or_else(|| i.get("id").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
pub(crate) mod tests;
