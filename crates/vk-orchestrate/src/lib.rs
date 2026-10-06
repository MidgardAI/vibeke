//! Orchestration above single tasks (spec 12 "Phase 2 outlook", Batch 4 of the gap audit).
//!
//! Everything here is pure logic over plain data plus the `git` CLI (through the same hardened
//! wrapper the task code uses), so the server only has to feed it state and apply the actions it
//! returns. Every feature is **off by default** behind its own `[orchestrate.*] enabled` flag
//! (see [`config`]), and every part that would need a live harness, account or provider has a
//! fake backend used by the tests:
//!
//! | Module | Spec | What |
//! |---|---|---|
//! | [`family`] | 05 §12 | best-of-N task families: agent spec parsing, child plans, per-run prompt suffix, compare, ranking |
//! | [`split`] | 05 §11 | "split into task": quiesce check, atomic capture, recovery ref, selection, validate, apply, verify, revert source |
//! | [`learn`] | 04 §7.7, 12 | learned policy: decision records, fingerprints, rule suggestions, repo policy snippets |
//! | [`merge`] | 12, 05 §10 | claims, conflict prediction across live worktrees, merge queue, integration merges |
//! | [`plan`] | 12 | goal to plan to tasks: plan model and validation, heuristic and scripted planners, routing, approval gate, briefings |
//! | [`quota`] | 12 | quota and cost scheduling: pause low-priority work near limits, resume after reset, account routing, price table |

pub mod config;
pub mod family;
pub mod gitx;
pub mod learn;
pub mod merge;
pub mod plan;
pub mod quota;
pub mod split;

use std::time::Duration;

pub use config::OrchestrateConfig;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("git {args}: {stderr}")]
    Git {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Refused(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn invalid(m: impl Into<String>) -> Error {
        Error::Invalid(m.into())
    }
}

/// `30s`, `5m`, `2h`, `30d`, `1w` or a bare number of seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: f64 = num.parse().ok()?;
    if n < 0.0 || !n.is_finite() {
        return None;
    }
    let mult = match unit.trim() {
        "ms" => 0.001,
        "s" | "" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        "w" => 604800.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(n * mult))
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A glob with `*` (not across `/`), `**` (across), `?` (one character, not `/`).
/// A pattern without wildcards must match exactly. Same dialect as the approval policy's
/// `path_glob` (09 §4) and claims (05 §10).
pub fn glob_match(pat: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') if p.get(1) == Some(&'*') => {
                let mut rest = &p[2..];
                // `**/` also matches zero directories.
                if rest.first() == Some(&'/') && go(&rest[1..], s) {
                    return true;
                }
                while rest.first() == Some(&'*') {
                    rest = &rest[1..];
                }
                (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            Some('*') => {
                let rest = &p[1..];
                let mut i = 0;
                loop {
                    if go(rest, &s[i..]) {
                        return true;
                    }
                    if i >= s.len() || s[i] == '/' {
                        return false;
                    }
                    i += 1;
                }
            }
            Some('?') => !s.is_empty() && s[0] != '/' && go(&p[1..], &s[1..]),
            Some(c) => !s.is_empty() && s[0] == *c && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    go(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1w"), Some(Duration::from_secs(604800)));
        assert_eq!(parse_duration("15"), Some(Duration::from_secs(15)));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("x"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("-1s"), None);
    }

    #[test]
    fn globs() {
        assert!(glob_match("src/auth/**", "src/auth/login.rs"));
        assert!(glob_match("src/auth/**", "src/auth/deep/x.rs"));
        assert!(!glob_match("src/auth/**", "src/other/x.rs"));
        assert!(glob_match("**/*.rs", "a/b/c.rs"));
        assert!(glob_match("**/*.rs", "c.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/a/main.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        assert!(glob_match("exact.txt", "exact.txt"));
        assert!(!glob_match("exact.txt", "exact.txt2"));
    }
}
