//! Secret redaction for logs, events, debug bundles and notifications (09 §9.2).
//!
//! [`redact`] scrubs known token shapes and `password=…`-style assignments from text.
//! [`redact_json`] additionally blanks the values of keys that look secret-bearing.
//! Both are best-effort pattern matching, not a guarantee; they are meant for data that may
//! leave the machine, never for the live pane view.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;

/// Replacement for a redacted value.
pub const REDACTED: &str = "[REDACTED]";

enum Rule {
    /// Replace the whole match with `[REDACTED]`.
    Whole(Regex),
    /// Keep capture group 1 (a prefix such as `Bearer `), replace the rest.
    KeepPrefix(Regex),
    /// `key<delim>value` assignments: keep key and delimiter, replace the value.
    Assignment(Regex),
    /// Like [`Rule::Assignment`], but the value (group 3) is replaced only when it looks random
    /// ([`looks_random`]; 09 §9.2 high-entropy heuristic).
    Entropy(Regex),
}

/// Shannon entropy in bits per character.
fn entropy(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::<char, usize>::new();
    for c in s.chars() {
        *counts.entry(c).or_default() += 1;
    }
    let n = s.chars().count() as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// A 32+ character base64/hex run that is random-looking: mixed character classes (or hex)
/// and high entropy, so long identifiers like `some_long_function_name_here_ok` stay.
fn looks_random(s: &str) -> bool {
    if s.len() < 32 {
        return false;
    }
    let hex = s.chars().all(|c| c.is_ascii_hexdigit());
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let upper = s.chars().filter(char::is_ascii_uppercase).count();
    let lower = s.chars().filter(char::is_ascii_lowercase).count();
    let classes = [digits, upper, lower].iter().filter(|&&n| n > 0).count();
    let e = entropy(s);
    if hex {
        digits > 0 && e >= 3.0
    } else {
        classes >= 2 && digits > 0 && e >= 3.5
    }
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("built-in redaction pattern is valid")
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // Private key PEM blocks, including truncated ones (no END line).
        Rule::Whole(re(
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|\z)",
        )),
        // Anthropic before OpenAI: both start with `sk-`.
        Rule::Whole(re(r"sk-ant-[A-Za-z0-9_\-]{8,}")),
        Rule::Whole(re(r"sk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}")),
        Rule::Whole(re(r"\bgh[pousr]_[A-Za-z0-9]{30,}")),
        Rule::Whole(re(r"\bgithub_pat_[A-Za-z0-9_]{20,}")),
        Rule::Whole(re(r"\bglpat-[A-Za-z0-9_\-]{16,}")),
        Rule::Whole(re(r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA)[A-Z0-9]{16}\b")),
        Rule::Whole(re(r"\bxox[abprs]-[A-Za-z0-9\-]{8,}")),
        // Vibeke elevated tokens (09 §3.2).
        Rule::Whole(re(r"\bvke_[0-9a-f]{32,}")),
        // JWT: header and payload are both base64url JSON objects, so both start with `eyJ`.
        Rule::Whole(re(
            r"\beyJ[A-Za-z0-9_\-]{6,}\.eyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]*",
        )),
        // Authorization header or field (any scheme).
        Rule::KeepPrefix(re(
            r#"(?i)(\bauthorization["']?\s*[:=]\s*["']?)(?:(?:bearer|basic|token)\s+)?[^\s"',;]{4,}"#,
        )),
        // Loose `Bearer <token>`.
        Rule::KeepPrefix(re(r"(?i)(\bbearer\s+)[A-Za-z0-9._~+/=\-]{8,}")),
        // URLs with userinfo: scheme://user:pass@host
        Rule::KeepPrefix(re(r"(?i)(\b[a-z][a-z0-9+.\-]*://)[^\s/:@]+:[^\s/@]+@")),
        // password=…, token: "…", api_key=…, client_secret=…
        Rule::Assignment(re(
            r#"(?i)\b([a-z0-9_.\-]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|credentials?))(["']?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s"',;&]+)"#,
        )),
        // High-entropy heuristic: a random-looking 32+ character base64/hex run right after a
        // secret-ish key, also without `=`/`:` (`X-Api-Key abcd…`, `"auth": "…"`, `sig=…`).
        Rule::Entropy(re(
            r#"(?i)\b([a-z0-9_.\-]*(?:key|token|secret|auth|cred|pass|sig|session)[a-z0-9_.\-]*)(["']?\s*[:=]?\s*["']?)([A-Za-z0-9+/_\-]{32,}={0,2})"#,
        )),
    ]
});

fn apply_rules<'a>(input: &'a str, rules: &[Rule]) -> Cow<'a, str> {
    let mut cur: Cow<'a, str> = Cow::Borrowed(input);
    for rule in rules {
        let changed: Option<String> = match rule {
            Rule::Whole(r) => match r.replace_all(&cur, REDACTED) {
                Cow::Borrowed(_) => None,
                Cow::Owned(s) => Some(s),
            },
            Rule::KeepPrefix(r) => {
                let out = r.replace_all(&cur, |c: &Captures| {
                    // A rule that redacts the userinfo keeps the `@`.
                    let whole = &c[0];
                    let tail = if whole.ends_with('@') { "@" } else { "" };
                    format!("{}{REDACTED}{tail}", &c[1])
                });
                match out {
                    Cow::Borrowed(_) => None,
                    Cow::Owned(s) => Some(s),
                }
            }
            Rule::Assignment(r) => {
                let out = r.replace_all(&cur, |c: &Captures| {
                    let value = &c[3];
                    if value.starts_with(REDACTED) || value.starts_with("\"[REDACTED") {
                        return c[0].to_string();
                    }
                    let quote = match value.chars().next() {
                        Some(q @ ('"' | '\'')) => q.to_string(),
                        _ => String::new(),
                    };
                    format!("{}{}{quote}{REDACTED}{quote}", &c[1], &c[2])
                });
                match out {
                    Cow::Borrowed(_) => None,
                    Cow::Owned(s) => Some(s),
                }
            }
            Rule::Entropy(r) => {
                let out = r.replace_all(&cur, |c: &Captures| {
                    if looks_random(&c[3]) {
                        format!("{}{}{REDACTED}", &c[1], &c[2])
                    } else {
                        c[0].to_string()
                    }
                });
                match out {
                    Cow::Owned(s) if s != *cur => Some(s),
                    _ => None,
                }
            }
        };
        if let Some(s) = changed {
            cur = Cow::Owned(s);
        }
    }
    cur
}

/// Redact known secret patterns. Returns the input unchanged (borrowed) when nothing matched.
pub fn redact(input: &str) -> Cow<'_, str> {
    apply_rules(input, &RULES)
}

static SECRET_KEY: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(key|token|secret|password|authorization)"));

/// True when a JSON object key looks like it holds a secret.
pub fn is_secret_key(key: &str) -> bool {
    SECRET_KEY.is_match(key)
}

/// Redact in place: string values under secret-looking keys are replaced wholesale (empty
/// strings are kept so absence stays visible), strings inside containers under such keys
/// likewise; every other string goes through [`redact`]. Numbers, booleans and nulls are
/// left alone.
pub fn redact_json(value: &mut Value) {
    walk(value, false);
}

fn walk(value: &mut Value, secret_ctx: bool) {
    match value {
        Value::String(s) => {
            if secret_ctx {
                if !s.is_empty() {
                    *s = REDACTED.to_string();
                }
            } else if let Cow::Owned(new) = redact(s) {
                *s = new;
            }
        }
        Value::Array(items) => {
            for v in items {
                walk(v, secret_ctx);
            }
        }
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                walk(v, secret_ctx || is_secret_key(k));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// A redactor with extra user patterns (`[security.redact] patterns = [...]`) applied after
/// the built-in set. Each extra match is replaced wholesale.
pub struct Redactor {
    extra: Vec<Regex>,
}

impl Redactor {
    pub fn new(extra_patterns: &[String]) -> Result<Self, regex::Error> {
        let extra = extra_patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Redactor { extra })
    }

    pub fn redact<'a>(&self, input: &'a str) -> Cow<'a, str> {
        let mut cur = apply_rules(input, &RULES);
        for r in &self.extra {
            let changed = match r.replace_all(&cur, REDACTED) {
                Cow::Borrowed(_) => None,
                Cow::Owned(s) => Some(s),
            };
            if let Some(s) = changed {
                cur = Cow::Owned(s);
            }
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn a(n: usize) -> String {
        "a1B2".repeat(n)
    }

    fn is_redacted(input: &str, secret: &str) {
        let out = redact(input);
        assert!(!out.contains(secret), "leaked {secret:?} in {out:?}");
        assert!(out.contains(REDACTED), "no marker in {out:?}");
    }

    #[test]
    fn clean_text_is_borrowed() {
        let s = "cargo test -p vk-config --all-targets; token_count=5 max_tokens=100";
        assert!(matches!(redact(s), Cow::Borrowed(_)));
        assert_eq!(redact(s), s);
    }

    #[test]
    fn anthropic_and_openai() {
        let ant = format!("sk-ant-api03-{}", a(10));
        is_redacted(&format!("key is {ant} ok"), &ant);
        let oa = format!("sk-{}", a(12));
        is_redacted(&format!("OPENAI {oa}"), &oa);
        let proj = format!("sk-proj-{}", a(12));
        is_redacted(&proj, &proj);
        // Short things are not keys.
        assert_eq!(
            redact("sk-learn is a task-name sk-short"),
            "sk-learn is a task-name sk-short"
        );
    }

    #[test]
    fn github_gitlab() {
        for prefix in ["ghp_", "gho_", "ghs_", "ghu_", "ghr_"] {
            let t = format!("{prefix}{}", "Ab1".repeat(14));
            is_redacted(&format!("x {t} y"), &t);
        }
        let pat = format!("github_pat_{}", "11AB_".repeat(10));
        is_redacted(&pat, &pat);
        let gl = format!("glpat-{}", a(6));
        is_redacted(&gl, &gl);
    }

    #[test]
    fn aws_and_slack() {
        let aws = format!("AKIA{}", "ABCD1234EFGH5678");
        is_redacted(&format!("aws {aws}"), &aws);
        // 20-char constraint: a longer word starting with AKIA is not matched.
        assert_eq!(
            redact("AKIAIOSFODNN7EXAMPLEXTRA"),
            "AKIAIOSFODNN7EXAMPLEXTRA"
        );
        for p in ["xoxb", "xoxa", "xoxp", "xoxr", "xoxs"] {
            let t = format!("{p}-123456789012-abcdefABCDEF");
            is_redacted(&t, &t);
        }
    }

    #[test]
    fn jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r";
        is_redacted(&format!("cookie={jwt};"), jwt);
        // Not a JWT.
        assert_eq!(redact("a.b.c and eyJ.x.y"), "a.b.c and eyJ.x.y");
    }

    #[test]
    fn bearer_and_authorization() {
        let out = redact("curl -H 'Authorization: Bearer abcdef0123456789xyz' https://x");
        assert!(!out.contains("abcdef0123456789xyz"));
        assert!(out.contains("Authorization: [REDACTED]"));
        let out = redact("authorization: Basic dXNlcjpwYXNz");
        assert!(!out.contains("dXNlcjpwYXNz"));
        let out = redact("got header bearer ABCDEFGH12345678 from client");
        assert!(!out.contains("ABCDEFGH12345678"));
        assert!(out.contains("bearer [REDACTED]"));
        let out = redact(r#"{"Authorization":"Bearer abcdefghijkl"}"#);
        assert!(!out.contains("abcdefghijkl"), "{out}");
    }

    #[test]
    fn pem_blocks() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nQyNTUxOQAAACD\n-----END OPENSSH PRIVATE KEY-----";
        let input = format!("before\n{pem}\nafter");
        let out = redact(&input);
        assert_eq!(out, format!("before\n{REDACTED}\nafter"));
        let out = redact("-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n(truncated)");
        assert_eq!(out, REDACTED);
        // Public keys / certs are not secret.
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(redact(cert), cert);
    }

    #[test]
    fn userinfo_urls() {
        let out = redact("git clone https://user:hunter2@github.com/o/r.git");
        assert_eq!(out, "git clone https://[REDACTED]@github.com/o/r.git");
        assert_eq!(
            redact("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        // user without password is left alone
        assert_eq!(redact("ssh://git@host/x"), "ssh://git@host/x");
    }

    #[test]
    fn assignments() {
        assert_eq!(redact("password=hunter2 next"), "password=[REDACTED] next");
        assert_eq!(redact("PASSWORD: hunter2"), "PASSWORD: [REDACTED]");
        assert_eq!(
            redact("export API_KEY=abc123xyz"),
            "export API_KEY=[REDACTED]"
        );
        assert_eq!(
            redact(r#"{"client_secret": "has space", "ok": 1}"#),
            r#"{"client_secret": "[REDACTED]", "ok": 1}"#
        );
        assert_eq!(redact("db.passwd='x y z'!"), "db.passwd='[REDACTED]'!");
        assert_eq!(
            redact("GITHUB_TOKEN=ghx&b=1"),
            "GITHUB_TOKEN=[REDACTED]&b=1"
        );
        // Idempotent.
        let once = redact("token=abcdef").into_owned();
        assert_eq!(redact(&once), once.as_str());
        // Names that merely contain the word are not assignments.
        assert_eq!(redact("tokens=5 secretary=bob"), "tokens=5 secretary=bob");
    }

    /// 09 §9.2: random-looking 32+ character values next to secret-ish keys, with or without
    /// `=`/`:`; ordinary long identifiers and hashes without such a key stay.
    #[test]
    fn high_entropy_near_secret_keys() {
        let b64 = "Zk9xR2h3TmJ0YVc4dkQ1cEx6M2tRbTdZ";
        is_redacted(&format!("X-Api-Key {b64}"), b64);
        is_redacted(&format!(r#"{{"auth": "{b64}"}}"#), b64);
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        is_redacted(&format!("signature {hex}"), hex);
        is_redacted(&format!("session_id={hex}"), hex);
        // Not random: a long identifier.
        let ident = "x-api-key some_long_function_name_here_ok_fine";
        assert_eq!(redact(ident), ident);
        // Random but no secret-ish key nearby: a commit hash stays.
        let commit = format!("commit {hex}");
        assert_eq!(redact(&commit), commit.as_str());
        // Vibeke's own elevated tokens.
        let vke = format!("vke_{hex}");
        is_redacted(&format!("export X={vke}"), &vke);
    }

    #[test]
    fn multiple_in_one_string() {
        let ant = format!("sk-ant-{}", a(8));
        let input = format!("a={ant} password=x https://u:p@h/");
        let out = redact(&input);
        assert!(!out.contains(&ant) && !out.contains("password=x") && !out.contains("u:p@"));
    }

    #[test]
    fn json_keys() {
        let ant = format!("sk-ant-{}", a(8));
        let mut v = json!({
            "api_key": "abc",
            "Authorization": "Bearer zzz",
            "nested": {"client_secret": {"inner": ["a", "b"]}, "name": "ok"},
            "token": "",
            "max_tokens": 100,
            "secret_flag": true,
            "note": format!("use {ant} here"),
            "list": [{"password": "p"}, "token=abc", "plain"],
        });
        redact_json(&mut v);
        assert_eq!(v["api_key"], REDACTED);
        assert_eq!(v["Authorization"], REDACTED);
        assert_eq!(
            v["nested"]["client_secret"]["inner"],
            json!([REDACTED, REDACTED])
        );
        assert_eq!(v["nested"]["name"], "ok");
        assert_eq!(v["token"], "");
        assert_eq!(v["max_tokens"], 100);
        assert_eq!(v["secret_flag"], true);
        assert_eq!(v["note"], format!("use {REDACTED} here"));
        assert_eq!(v["list"][0]["password"], REDACTED);
        assert_eq!(v["list"][1], "token=[REDACTED]");
        assert_eq!(v["list"][2], "plain");
    }

    #[test]
    fn json_scalars_and_null() {
        let mut v = json!(null);
        redact_json(&mut v);
        assert_eq!(v, json!(null));
        let mut v = json!("password=abc");
        redact_json(&mut v);
        assert_eq!(v, json!("password=[REDACTED]"));
    }

    #[test]
    fn custom_patterns() {
        let r = Redactor::new(&["corp-[0-9]{6}".to_string()]).unwrap();
        assert_eq!(
            r.redact("id corp-123456 pw=1 token=zz"),
            "id [REDACTED] pw=1 token=[REDACTED]"
        );
        assert!(Redactor::new(&["(".to_string()]).is_err());
    }
}
