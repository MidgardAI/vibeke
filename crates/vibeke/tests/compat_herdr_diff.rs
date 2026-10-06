//! Differential conformance harness: pinned Herdr vs Vibeke's compat layer (07 §8.4, 10 §4.8).
//!
//! **Gated and not run by default.** It runs only with `VIBEKE_HERDR_DIFF=1` and
//! `VIBEKE_HERDR_BIN=/absolute/path/to/herdr` (a reference binary of the pinned baseline, never
//! the operator's installed Herdr on PATH). Both sides get a fresh temp HOME, XDG dirs, TMPDIR
//! and runtime dir; the operator's `~/.config/herdr`, sockets and sessions are never visible.
//! The reference binary must report the pinned version (and, when `VIBEKE_HERDR_SHA256` is
//! given, match that checksum) or the harness refuses to run.
//!
//! Each scenario runs the same Herdr CLI script against both sides and compares exit codes and
//! the JSON shape of stdout/stderr after normalization: generated ids are mapped through a
//! bijection (first-seen order), temp roots and timestamps are replaced. Missing fields, extra
//! fields, different error codes and different event sequences are failures; nothing else is
//! normalized away. The normalizer is unit-tested here without any Herdr binary.

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

/// Replace generated ids with a first-seen bijection and temp roots with `<root>`.
#[derive(Default)]
struct Normalizer {
    ids: HashMap<String, String>,
    root: String,
}

fn looks_generated(k: &str) -> bool {
    k.ends_with("_id") || k == "revision" || k.ends_with("_at") || k == "pid"
}

impl Normalizer {
    fn new(root: &Path) -> Self {
        Normalizer {
            ids: HashMap::new(),
            root: root.to_string_lossy().into_owned(),
        }
    }
    fn value(&mut self, key: Option<&str>, v: &Value) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), self.value(Some(k), x)))
                    .collect::<Map<_, _>>(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(key, x)).collect()),
            Value::String(s) if key.is_some_and(looks_generated) => {
                let n = self.ids.len();
                Value::String(
                    self.ids
                        .entry(s.clone())
                        .or_insert_with(|| format!("<id{n}>"))
                        .clone(),
                )
            }
            Value::Number(_)
                if key.is_some_and(|k| k.ends_with("_at") || k == "revision" || k == "pid") =>
            {
                Value::String("<n>".into())
            }
            Value::String(s) if !self.root.is_empty() && s.contains(&self.root) => {
                Value::String(s.replace(&self.root, "<root>"))
            }
            other => other.clone(),
        }
    }
}

/// The JSON *shape*: keys and value kinds, recursively (arrays by their first element).
fn shape(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), shape(x))).collect()),
        Value::Array(a) => Value::Array(a.first().map(shape).into_iter().collect()),
        Value::String(_) => Value::String("string".into()),
        Value::Number(_) => Value::String("number".into()),
        Value::Bool(_) => Value::String("bool".into()),
        Value::Null => Value::Null,
    }
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
    fn run(&self, step: &[&str]) -> (i32, Value, Value) {
        let cwd = self.home.path().join("work");
        let mut c = Command::new(&self.argv0);
        self.isolated_env(&mut c);
        c.args(&self.prefix);
        c.args(
            step.iter()
                .map(|a| a.replace("{cwd}", &cwd.to_string_lossy())),
        );
        let out = c.output().expect("run side");
        let parse = |b: &[u8]| serde_json::from_slice(b).unwrap_or(Value::Null);
        (
            out.status.code().unwrap_or(-1),
            parse(&out.stdout),
            parse(&out.stderr),
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
            let (ch, oh, eh) = herdr.run(step);
            let (cv, ov, ev) = vibeke.run(step);
            let (oh, ov) = (nh.value(None, &oh), nv.value(None, &ov));
            let (eh, ev) = (nh.value(None, &eh), nv.value(None, &ev));
            if ch != cv || shape(&oh) != shape(&ov) || shape(&eh) != shape(&ev) {
                failures.push(format!(
                    "{} / {step:?}: herdr exit {ch} {oh} {eh} — vibeke exit {cv} {ov} {ev}",
                    sc.name
                ));
            }
        }
    }
    for side in [&herdr, &vibeke] {
        let mut c = Command::new(&side.argv0);
        side.isolated_env(&mut c);
        let _ = c.args(&side.prefix).args(["server", "stop"]).output();
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn normalizer_bijection_and_shapes() {
    let mut n = Normalizer::new(Path::new("/tmp/x"));
    let a = n.value(
        None,
        &serde_json::json!({"workspace_id": "w7", "panes": [{"pane_id": "w7:p3", "cwd": "/tmp/x/work", "revision": 12}], "focused_pane_id": "w7:p3"}),
    );
    assert_eq!(a["workspace_id"], "<id0>");
    assert_eq!(a["panes"][0]["pane_id"], "<id1>");
    assert_eq!(a["focused_pane_id"], "<id1>", "same id, same token");
    assert_eq!(a["panes"][0]["cwd"], "<root>/work");
    assert_eq!(a["panes"][0]["revision"], "<n>");
    assert_eq!(
        shape(&serde_json::json!({"a": [1, 2], "b": "x", "c": null})),
        serde_json::json!({"a": ["number"], "b": "string", "c": null})
    );
    // Missing fields are not normalized away.
    assert_ne!(
        shape(&serde_json::json!({"a": 1})),
        shape(&serde_json::json!({"a": 1, "b": 2}))
    );
    assert!(SCENARIOS.iter().all(|s| !s.steps.is_empty()));
}
