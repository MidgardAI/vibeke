//! Differential conformance harness: pinned Herdr vs Vibeke's compat layer (07 §8.4, 10 §4.8).
//!
//! **Gated and not run by default.** It runs only with `VIBEKE_HERDR_DIFF=1` and
//! `VIBEKE_HERDR_BIN=/absolute/path/to/herdr` (a reference binary of the pinned baseline, never
//! the operator's installed Herdr on PATH). Both sides get a fresh temp HOME, XDG dirs, TMPDIR
//! and runtime dir; the operator's `~/.config/herdr`, sockets and sessions are never visible.
//! The reference binary must report the pinned version (and, when `VIBEKE_HERDR_SHA256` is
//! given, match that checksum) or the harness refuses to run.
//!
//! Each scenario runs the same Herdr CLI script against both sides and compares, per step, the
//! exit code and the full **values** of stdout and stderr after a small, explicit normalization:
//!
//! * generated ids (`*_id` keys) are replaced by tokens in first-seen order on each side, so equal
//!   token sequences mean the two sides' ids correspond one to one (a bijection) — a response
//!   that points at a different pane, or reuses one id for two objects, differs;
//! * volatile numbers (`*_at`, `*_ms`, `revision`, `pid`) become `<n>` and the side's temp root
//!   becomes `<root>`;
//! * error `message` text (wording differs between implementations) becomes `<message>`; the
//!   error `code` is compared exactly.
//!
//! Everything else is compared exactly: object keys (missing and extra fields), array lengths
//! and order, strings (statuses, labels, error codes), numbers and booleans (focus). Output that
//! is neither empty, one JSON document nor JSON lines is a failure on its own. The comparator is
//! unit-tested here, including negative cases, without any Herdr binary.
//!
//! Not covered yet: event streams (`events.subscribe`) and unmodified plugins; the inventory
//! records both as gaps.

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const BASELINE: &str = "0.9.3";

/// One scripted step: Herdr CLI arguments. `{cwd}` is replaced by a per-side temp dir.
struct Scenario {
    name: &'static str,
    steps: &'static [&'static [&'static str]],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "workspace lifecycle",
        steps: &[
            &["workspace", "create", "--cwd", "{cwd}", "--label", "a"],
            &["workspace", "list"],
            &["workspace", "rename", "w1", "renamed"],
            &["workspace", "list"],
            &["tab", "create", "--workspace-id", "w1", "--label", "t2"],
            &["tab", "list"],
            &["pane", "list"],
        ],
    },
    Scenario {
        name: "pane io",
        steps: &[
            &["workspace", "create", "--cwd", "{cwd}"],
            &["pane", "send-text", "w1:p1", "echo diff-ok\n"],
            &["pane", "wait-output", "w1:p1", "--match", "diff-ok"],
            &[
                "pane", "read", "w1:p1", "--source", "recent", "--lines", "5",
            ],
        ],
    },
    Scenario {
        name: "errors",
        steps: &[
            &["pane", "get", "w9:p9"],
            &["workspace", "focus", "w9"],
            &["pane", "send-keys", "w1:p1", "NotAKey"],
        ],
    },
];

/// Replace generated ids with a first-seen bijection, volatile numbers and error messages with
/// placeholders, and temp roots with `<root>`.
#[derive(Default)]
struct Normalizer {
    ids: HashMap<String, String>,
    root: String,
}

fn is_id_key(k: &str) -> bool {
    k.ends_with("_id")
}

fn is_volatile_number_key(k: &str) -> bool {
    k.ends_with("_at") || k.ends_with("_ms") || k == "revision" || k == "pid"
}

impl Normalizer {
    fn new(root: &Path) -> Self {
        Normalizer {
            ids: HashMap::new(),
            root: root.to_string_lossy().into_owned(),
        }
    }
    fn token(&mut self, s: &str) -> Value {
        let n = self.ids.len();
        Value::String(
            self.ids
                .entry(s.to_string())
                .or_insert_with(|| format!("<id{n}>"))
                .clone(),
        )
    }
    fn value(&mut self, key: Option<&str>, v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let in_error = key == Some("error");
                Value::Object(
                    o.iter()
                        .map(|(k, x)| {
                            if in_error && k == "message" && x.is_string() {
                                (k.clone(), Value::String("<message>".into()))
                            } else {
                                (k.clone(), self.value(Some(k), x))
                            }
                        })
                        .collect::<Map<_, _>>(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(key, x)).collect()),
            Value::String(s) if key.is_some_and(is_id_key) => self.token(s),
            Value::Number(_) if key.is_some_and(is_volatile_number_key) => {
                Value::String("<n>".into())
            }
            Value::String(s) if !self.root.is_empty() && s.contains(&self.root) => {
                Value::String(s.replace(&self.root, "<root>"))
            }
            other => other.clone(),
        }
    }
}

/// Parse one side's output stream: empty → `null`, one JSON document, or JSON lines (an
/// array). Anything else is malformed and fails the step.
fn parse_output(bytes: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let t = text.trim();
    if t.is_empty() {
        return Ok(Value::Null);
    }
    if let Ok(v) = serde_json::from_str(t) {
        return Ok(v);
    }
    t.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("malformed output ({e}): {l}")))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// Every difference between two normalized values, as `path: herdr … vibeke …` lines.
fn compare(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, va) in x {
                let p = format!("{path}.{k}");
                match y.get(k) {
                    Some(vb) => compare(&p, va, vb, out),
                    None => out.push(format!("{p}: missing in vibeke (herdr {va})")),
                }
            }
            for (k, vb) in y {
                if !x.contains_key(k) {
                    out.push(format!("{path}.{k}: extra in vibeke ({vb})"));
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!(
                    "{path}: herdr has {} element(s), vibeke {}",
                    x.len(),
                    y.len()
                ));
            }
            for (i, (va, vb)) in x.iter().zip(y).enumerate() {
                compare(&format!("{path}[{i}]"), va, vb, out);
            }
        }
        _ if a == b => {}
        _ => out.push(format!("{path}: herdr {a} vibeke {b}")),
    }
}

/// One side's result for one step: exit code, stdout, stderr (each parsed or malformed).
type StepOut = (i32, Result<Value, String>, Result<Value, String>);

/// Compare one step of both sides; returns the differences (empty when they agree).
fn diff_step(nh: &mut Normalizer, nv: &mut Normalizer, h: &StepOut, v: &StepOut) -> Vec<String> {
    let mut out = Vec::new();
    if h.0 != v.0 {
        out.push(format!("exit code: herdr {} vibeke {}", h.0, v.0));
    }
    for (name, a, b) in [("stdout", &h.1, &v.1), ("stderr", &h.2, &v.2)] {
        match (a, b) {
            (Ok(a), Ok(b)) => {
                let (a, b) = (nh.value(None, a), nv.value(None, b));
                compare(name, &a, &b, &mut out);
            }
            (Err(e), _) => out.push(format!("{name}: herdr output malformed: {e}")),
            (_, Err(e)) => out.push(format!("{name}: vibeke output malformed: {e}")),
        }
    }
    out
}

struct Side {
    home: tempfile::TempDir,
    argv0: PathBuf,
    prefix: Vec<String>,
}

impl Side {
    fn isolated_env(&self, c: &mut Command) {
        let h = self.home.path();
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", h)
            .env("XDG_CONFIG_HOME", h.join(".config"))
            .env("XDG_STATE_HOME", h.join(".local/state"))
            .env("XDG_DATA_HOME", h.join(".local/share"))
            .env("XDG_RUNTIME_DIR", h.join("run"))
            .env("TMPDIR", h.join("tmp"))
            .env("VIBEKE_RUNTIME_DIR", h.join("vk-run"))
            .env("VIBEKE_STATE_DIR", h.join("vk-state"))
            .env("VIBEKE_CONFIG", h.join("vk-config.toml"));
        for d in ["run", "tmp", ".config", "work"] {
            let _ = std::fs::create_dir_all(h.join(d));
        }
    }
    fn run(&self, step: &[&str]) -> StepOut {
        let cwd = self.home.path().join("work");
        let mut c = Command::new(&self.argv0);
        self.isolated_env(&mut c);
        c.args(&self.prefix);
        c.args(
            step.iter()
                .map(|a| a.replace("{cwd}", &cwd.to_string_lossy())),
        );
        let out = c.output().expect("run side");
        (
            out.status.code().unwrap_or(-1),
            parse_output(&out.stdout),
            parse_output(&out.stderr),
        )
    }
}

#[test]
fn differential_against_pinned_herdr() {
    if std::env::var("VIBEKE_HERDR_DIFF").as_deref() != Ok("1") {
        eprintln!(
            "skipped: set VIBEKE_HERDR_DIFF=1 and VIBEKE_HERDR_BIN to run the differential suite"
        );
        return;
    }
    let bin = PathBuf::from(
        std::env::var("VIBEKE_HERDR_BIN").expect("VIBEKE_HERDR_BIN=/abs/path/to/herdr"),
    );
    assert!(
        bin.is_absolute(),
        "VIBEKE_HERDR_BIN must be an absolute path to a reference binary"
    );
    let real_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let herdr = Side {
        home: tempfile::tempdir().unwrap(),
        argv0: bin,
        prefix: vec![],
    };
    assert!(
        !herdr
            .home
            .path()
            .starts_with(real_home.join(".config/herdr"))
    );
    // Pin the version (and checksum) before running anything else.
    let mut v = Command::new(&herdr.argv0);
    herdr.isolated_env(&mut v);
    let out = v.arg("--version").output().expect("reference herdr");
    let version = String::from_utf8_lossy(&out.stdout);
    assert!(
        version.contains(BASELINE),
        "reference binary is not Herdr {BASELINE}: {version}"
    );
    if let Ok(want) = std::env::var("VIBEKE_HERDR_SHA256") {
        let bytes = std::fs::read(&herdr.argv0).unwrap();
        assert_eq!(
            vk_compat::herdr::registry::sha256_hex(&bytes),
            want.to_lowercase()
        );
    }
    let vibeke = Side {
        home: tempfile::tempdir().unwrap(),
        argv0: PathBuf::from(env!("CARGO_BIN_EXE_vibeke")),
        prefix: vec!["compat".into(), "herdr".into()],
    };
    let mut failures = Vec::new();
    for sc in SCENARIOS {
        let (mut nh, mut nv) = (
            Normalizer::new(herdr.home.path()),
            Normalizer::new(vibeke.home.path()),
        );
        for step in sc.steps {
            let h = herdr.run(step);
            let v = vibeke.run(step);
            for d in diff_step(&mut nh, &mut nv, &h, &v) {
                failures.push(format!("{} / {step:?}: {d}", sc.name));
            }
        }
    }
    for side in [&herdr, &vibeke] {
        let mut c = Command::new(&side.argv0);
        side.isolated_env(&mut c);
        // Vibeke's shim never forwards `server stop`; stop its isolated server natively.
        let args: &[&str] = if side.prefix.is_empty() {
            &["server", "stop"]
        } else {
            &["server", "stop", "--kill-panes"]
        };
        let _ = c.args(args).output();
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Compare two raw outputs as the harness would (fresh normalizers, same exit code).
fn differs(herdr: &str, vibeke: &str) -> Vec<String> {
    let (mut nh, mut nv) = (
        Normalizer::new(Path::new("/tmp/h")),
        Normalizer::new(Path::new("/tmp/v")),
    );
    diff_step(
        &mut nh,
        &mut nv,
        &(0, parse_output(herdr.as_bytes()), Ok(Value::Null)),
        &(0, parse_output(vibeke.as_bytes()), Ok(Value::Null)),
    )
}

#[test]
fn normalizer_bijection_and_volatile_fields() {
    let mut n = Normalizer::new(Path::new("/tmp/x"));
    let a = n.value(
        None,
        &serde_json::json!({"workspace_id": "w7", "panes": [{"pane_id": "w7:p3", "cwd": "/tmp/x/work", "revision": 12, "started_unix_ms": 99}], "focused_pane_id": "w7:p3", "error": {"code": "pane_not_found", "message": "pane w7:p3 is gone"}}),
    );
    assert_eq!(a["workspace_id"], "<id0>");
    assert_eq!(a["panes"][0]["pane_id"], "<id1>");
    assert_eq!(a["focused_pane_id"], "<id1>", "same id, same token");
    assert_eq!(a["panes"][0]["cwd"], "<root>/work");
    assert_eq!(a["panes"][0]["revision"], "<n>");
    assert_eq!(a["panes"][0]["started_unix_ms"], "<n>");
    assert_eq!(a["error"]["message"], "<message>");
    assert_eq!(a["error"]["code"], "pane_not_found", "codes are kept");
    assert!(SCENARIOS.iter().all(|s| !s.steps.is_empty()));
}

#[test]
fn comparator_accepts_equivalent_outputs() {
    // Different concrete ids and temp roots, same structure and correspondence.
    let d = differs(
        r#"{"type":"pane_list","panes":[{"pane_id":"w1:p1","cwd":"/tmp/h/a","focused":false},{"pane_id":"w1:p2","cwd":"/tmp/h/b","focused":true}],"focused_pane_id":"w1:p2"}"#,
        r#"{"type":"pane_list","panes":[{"pane_id":"w3:p7","cwd":"/tmp/v/a","focused":false},{"pane_id":"w3:p9","cwd":"/tmp/v/b","focused":true}],"focused_pane_id":"w3:p9"}"#,
    );
    assert!(d.is_empty(), "{d:?}");
    assert!(differs("", "").is_empty());
    assert!(
        differs(
            r#"{"error":{"code":"pane_not_found","message":"no pane w9:p9"}}"#,
            r#"{"error":{"code":"pane_not_found","message":"pane not found: w9:p9"}}"#
        )
        .is_empty(),
        "message wording is not compared"
    );
}

#[test]
fn comparator_rejects_real_differences() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "error code",
            r#"{"error":{"code":"pane_not_found","message":"x"}}"#,
            r#"{"error":{"code":"invalid_params","message":"x"}}"#,
        ),
        (
            "status",
            r#"{"logs":[{"log_id":"a","status":"succeeded"}]}"#,
            r#"{"logs":[{"log_id":"b","status":"completed"}]}"#,
        ),
        (
            "result type",
            r#"{"type":"pane_info"}"#,
            r#"{"type":"pane_list"}"#,
        ),
        (
            "id correspondence (focus points at the other pane)",
            r#"{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w1:p2"}],"focused_pane_id":"w1:p2"}"#,
            r#"{"panes":[{"pane_id":"w1:p1"},{"pane_id":"w1:p2"}],"focused_pane_id":"w1:p1"}"#,
        ),
        (
            "one id reused for two objects",
            r#"{"a_id":"x","b_id":"y"}"#,
            r#"{"a_id":"x","b_id":"x"}"#,
        ),
        (
            "focus value",
            r#"{"pane":{"pane_id":"p","focused":true}}"#,
            r#"{"pane":{"pane_id":"p","focused":false}}"#,
        ),
        (
            "missing later array element",
            r#"{"panes":[{"pane_id":"a"},{"pane_id":"b"}]}"#,
            r#"{"panes":[{"pane_id":"a"}]}"#,
        ),
        (
            "array order",
            r#"{"labels":["one","two"]}"#,
            r#"{"labels":["two","one"]}"#,
        ),
        ("missing field", r#"{"a":1,"b":2}"#, r#"{"a":1}"#),
        ("extra field", r#"{"a":1}"#, r#"{"a":1,"b":2}"#),
        ("number", r#"{"number":1}"#, r#"{"number":2}"#),
        ("malformed vibeke output", r#"{"a":1}"#, "{not json"),
        ("malformed herdr output", "Error: boom", r#"{"a":1}"#),
        ("output vs none", r#"{"a":1}"#, ""),
    ];
    for (what, h, v) in cases {
        assert!(!differs(h, v).is_empty(), "{what} must be a difference");
    }
    // Exit codes are compared too.
    let (mut nh, mut nv) = (Normalizer::default(), Normalizer::default());
    let d = diff_step(
        &mut nh,
        &mut nv,
        &(1, Ok(Value::Null), Ok(Value::Null)),
        &(0, Ok(Value::Null), Ok(Value::Null)),
    );
    assert_eq!(d.len(), 1, "{d:?}");
    // JSON lines (event streams) parse as arrays and compare element by element.
    let lines = |n: usize| {
        (0..n)
            .map(|i| format!("{{\"event\":\"e{i}\"}}\n"))
            .collect::<String>()
    };
    assert!(differs(&lines(3), &lines(3)).is_empty());
    assert!(
        differs(&lines(3), &lines(2))
            .iter()
            .any(|x| x.contains("element")),
        "a missing event is a difference"
    );
}
