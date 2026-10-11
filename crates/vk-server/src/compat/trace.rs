//! `VIBEKE_HERDR_TRACE=<file>`: a narrow, test-only trace of Herdr plugin calls (spec 18 §9).
//!
//! The real-plugin harness reads it next to `events.read`, invocation logs and the
//! `plugin.api_call` audit, which miss two things: read-only broker calls and what happens in
//! the `herdr` launcher before a call reaches the broker. Tracing is on only when the variable
//! is set **and** `VIBEKE_TEST_HOOKS=1`, so a production server never writes it.
//!
//! One JSON object per line (the shape `vk-conformance`'s `trace` module reads):
//!
//! ```json
//! {"ts_ms":1760000000000,"plugin":"herdr-projects","invocation":"01J…","transport":"socket",
//!  "method":"pane.split","ok":true,"code":null}
//! ```
//!
//! * The server writes a line for every broker result ([`super::serve_wire`]) and for every
//!   `compat.herdr.call` made with a plugin identity (cross-session calls). `transport` is
//!   `cli` when the request came from Vibeke's `herdr` launcher and `socket` otherwise. On a
//!   broker socket the launcher marks its requests with the top-level key
//!   `"vibeke_client": "herdr-shim"` ([`SHIM_MARK`]). A raw client could copy that key, so the
//!   distinction is a test signal, never an authorization input.
//! * The launcher (`vk-cli`, `herdr` inside a plugin) writes `transport = "launcher"` lines:
//!   the redacted `argv`, the resolved method, and `parse_error` when parsing failed.
//! * `params` is written only with `VIBEKE_HERDR_TRACE_PARAMS=1`, redacted with
//!   [`vk_redact::redact_json`].
//!
//! How the launcher finds the file: action and hook invocations in host mode inherit the
//! server's environment, so the variables reach them unchanged, and the server adds
//! [`INVOCATION_ENV`] (the invocation's log id). Plugin panes drop `VIBEKE_*`, so the server
//! passes the three variables in the pane command explicitly. Restricted (sandboxed)
//! invocations write no launcher lines: the sandbox removes the variables and cannot write
//! the file. Their broker lines are still written by the server.
//!
//! Each line is appended with one `write` on a file opened for appending, so lines from
//! several processes never interleave. Write errors are ignored.

use serde_json::{Value, json};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::LazyLock;

pub const ENV: &str = "VIBEKE_HERDR_TRACE";
pub const PARAMS_ENV: &str = "VIBEKE_HERDR_TRACE_PARAMS";
/// The invocation's log id, set for the launcher in action and hook invocations while tracing.
pub const INVOCATION_ENV: &str = "VIBEKE_HERDR_TRACE_INVOCATION";
/// The top-level request key the `herdr` launcher adds on broker sockets.
pub const SHIM_MARK: (&str, &str) = ("vibeke_client", "herdr-shim");

/// Tracing settings read from an environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub path: PathBuf,
    pub params: bool,
}

/// The settings for an environment lookup: `None` unless both the trace file and
/// `VIBEKE_TEST_HOOKS=1` are set.
pub fn settings_from(get: impl Fn(&str) -> Option<String>) -> Option<Settings> {
    if get("VIBEKE_TEST_HOOKS").as_deref() != Some("1") {
        return None;
    }
    let path = get(ENV).filter(|p| !p.is_empty())?;
    Some(Settings {
        path: PathBuf::from(path),
        params: get(PARAMS_ENV).as_deref() == Some("1"),
    })
}

/// This process's settings, read once.
pub fn settings() -> Option<&'static Settings> {
    static S: LazyLock<Option<Settings>> =
        LazyLock::new(|| settings_from(|k| std::env::var(k).ok()));
    S.as_ref()
}

/// The environment the server adds for a plugin process while tracing (empty otherwise):
/// the variables a pane would lose, and the invocation id when there is one.
pub fn child_env(invocation: Option<&str>) -> Vec<(String, String)> {
    let Some(s) = settings() else {
        return vec![];
    };
    let mut env = vec![
        ("VIBEKE_TEST_HOOKS".to_string(), "1".to_string()),
        (ENV.to_string(), s.path.to_string_lossy().into_owned()),
    ];
    if s.params {
        env.push((PARAMS_ENV.to_string(), "1".to_string()));
    }
    if let Some(i) = invocation {
        env.push((INVOCATION_ENV.to_string(), i.to_string()));
    }
    env
}

/// One trace record before formatting.
#[derive(Debug, Clone, Default)]
pub struct Line<'a> {
    pub plugin: &'a str,
    pub invocation: Option<&'a str>,
    /// `cli`, `socket` or `launcher`.
    pub transport: &'a str,
    pub method: &'a str,
    pub ok: bool,
    pub code: Option<&'a str>,
    pub params: Option<&'a Value>,
    pub argv: Option<&'a [String]>,
    pub parse_error: Option<&'a str>,
}

/// Arguments with secrets removed: each one goes through [`vk_redact::redact`], and the value
/// after a flag whose name looks secret (`--token x`, `--api-key x`) is replaced.
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut secret_next = false;
    for a in argv {
        if secret_next {
            out.push(vk_redact::REDACTED.to_string());
            secret_next = false;
            continue;
        }
        match a.strip_prefix("--").and_then(|f| f.split_once('=')) {
            Some((flag, _)) if vk_redact::is_secret_key(flag) => {
                out.push(format!("--{flag}={}", vk_redact::REDACTED));
            }
            _ => {
                secret_next =
                    a.starts_with("--") && !a.contains('=') && vk_redact::is_secret_key(&a[2..]);
                out.push(vk_redact::redact(a).into_owned());
            }
        }
    }
    out
}

/// The JSON line for `l` (with its trailing newline). `params` is kept only when
/// `with_params`, and then redacted.
pub fn format_line(l: &Line, ts_ms: u64, with_params: bool) -> String {
    let mut o = json!({
        "ts_ms": ts_ms,
        "plugin": l.plugin,
        "invocation": l.invocation,
        "transport": l.transport,
        "method": l.method,
        "ok": l.ok,
        "code": l.code,
    });
    if with_params && let Some(p) = l.params {
        let mut p = p.clone();
        vk_redact::redact_json(&mut p);
        o["params"] = p;
    }
    if let Some(a) = l.argv {
        o["argv"] = json!(redact_argv(a));
    }
    if let Some(e) = l.parse_error {
        o["parse_error"] = json!(vk_redact::redact(e));
    }
    let mut s = o.to_string();
    s.push('\n');
    s
}

/// Append `l` to the trace file when tracing is on.
pub fn write(l: &Line) {
    let Some(s) = settings() else {
        return;
    };
    let line = format_line(l, vk_store::now_ms(), s.params);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.path)
    {
        // One write per line: appends from several processes stay whole lines.
        let _ = f.write_all(line.as_bytes());
    }
}

/// True when a raw broker request line carries the launcher's mark ([`SHIM_MARK`]).
pub fn from_shim(request: &[u8]) -> bool {
    serde_json::from_slice::<Value>(request)
        .ok()
        .and_then(|v| {
            v.get(SHIM_MARK.0)
                .and_then(Value::as_str)
                .map(|s| s == SHIM_MARK.1)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn settings_need_test_hooks() {
        assert_eq!(settings_from(env(&[(ENV, "/tmp/t.jsonl")])), None);
        assert_eq!(
            settings_from(env(&[("VIBEKE_TEST_HOOKS", "1"), (ENV, "")])),
            None
        );
        assert_eq!(
            settings_from(env(&[
                ("VIBEKE_TEST_HOOKS", "1"),
                (ENV, "/tmp/t.jsonl"),
                (PARAMS_ENV, "1")
            ])),
            Some(Settings {
                path: "/tmp/t.jsonl".into(),
                params: true
            })
        );
    }

    #[test]
    fn line_format() {
        let params = json!({"pane_id": "w1:p1", "token": "hunter2"});
        let l = Line {
            plugin: "herdr-projects",
            invocation: Some("01J"),
            transport: "socket",
            method: "pane.split",
            ok: true,
            params: Some(&params),
            ..Default::default()
        };
        assert_eq!(
            format_line(&l, 5, false),
            "{\"ts_ms\":5,\"plugin\":\"herdr-projects\",\"invocation\":\"01J\",\
             \"transport\":\"socket\",\"method\":\"pane.split\",\"ok\":true,\"code\":null}\n"
        );
        let v: Value = serde_json::from_str(format_line(&l, 5, true).trim_end()).unwrap();
        assert_eq!(v["params"]["pane_id"], "w1:p1");
        assert_eq!(v["params"]["token"], vk_redact::REDACTED);
        assert!(v.get("argv").is_none() && v.get("parse_error").is_none());

        let argv: Vec<String> = ["pane", "split", "--api-token", "s3cret", "--key=abc", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let l = Line {
            plugin: "p",
            transport: "launcher",
            method: "",
            code: Some("usage"),
            argv: Some(&argv),
            parse_error: Some("unknown command"),
            ..Default::default()
        };
        let v: Value = serde_json::from_str(format_line(&l, 1, false).trim_end()).unwrap();
        assert_eq!(v["invocation"], Value::Null);
        assert_eq!(v["ok"], false);
        assert_eq!(v["code"], "usage");
        assert_eq!(v["parse_error"], "unknown command");
        assert_eq!(
            v["argv"],
            json!([
                "pane",
                "split",
                "--api-token",
                vk_redact::REDACTED,
                format!("--key={}", vk_redact::REDACTED),
                "x"
            ])
        );
    }

    #[test]
    fn shim_mark() {
        assert!(from_shim(
            br#"{"id":"1","method":"ping","params":{},"vibeke_client":"herdr-shim"}"#
        ));
        assert!(!from_shim(br#"{"id":"1","method":"ping","params":{}}"#));
        assert!(!from_shim(b"not json"));
    }
}
