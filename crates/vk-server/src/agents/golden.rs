//! Golden-corpus replay (04 §12.1, §12.3): every manifest's screen rules run against every
//! recorded screen of its harness (`tests/harness-golden/<id>/<version>/screens/*.txt` with
//! `expected.jsonl`), and every `signals.jsonl` is replayed through the harness's signal mapping.
//!
//! Drift detection: a manifest with screen rules but no corpus fails; for the code-backed
//! harnesses (Claude, Codex, pi, omp) the manifest's rules and the in-code evaluator must agree
//! with the expectation, so the declarative copy cannot drift from the code path unnoticed.
//! The current corpus is SYNTHETIC (see each `meta.toml`), not live recordings.

use super::harness::Harness;
use super::screen;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use vk_agents::manifest::{KeyIntent, plan_keys};
use vk_proto::model::*;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/harness-golden")
}

fn read_jsonl(p: &Path) -> Vec<Value> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{}: {e}: {l}", p.display())))
        .collect()
}

/// Version dirs of a harness corpus (`synthetic`, or recorded versions).
fn version_dirs(id: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(corpus().join(id))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    v
}

fn intent(k: &str) -> KeyIntent {
    match k {
        "allow" => KeyIntent::Allow,
        "allow_always" => KeyIntent::AllowAlways,
        "deny" => KeyIntent::Deny,
        other => KeyIntent::Option(other.to_string()),
    }
}

fn answer(k: &str) -> Answer {
    Answer {
        decision: match k {
            "allow" => Some(Decision::Allow),
            "allow_always" => Some(Decision::AllowAlways),
            _ => Some(Decision::Deny),
        },
        ..Default::default()
    }
}

fn check_dialog(
    ctx: &str,
    want: &Value,
    kind: &str,
    title: &str,
    command: Option<&str>,
    n: usize,
    pointer: Option<u8>,
) {
    assert_eq!(want["kind"].as_str(), Some(kind), "{ctx}: dialog kind");
    if let Some(o) = want["options"].as_u64() {
        assert_eq!(n as u64, o, "{ctx}: option count");
    }
    if let Some(p) = want["pointer"].as_u64() {
        assert_eq!(pointer.map(u64::from), Some(p), "{ctx}: pointer");
    }
    if let Some(t) = want["title_contains"].as_str() {
        assert!(title.contains(t), "{ctx}: title {title:?} lacks {t:?}");
    }
    if let Some(c) = want["command"].as_str() {
        assert_eq!(command, Some(c), "{ctx}: command");
    }
}

#[test]
fn every_manifest_replays_its_golden_screens() {
    let mut replayed = 0;
    for (_, l) in super::manifests::all() {
        if !l.has_screen_rules() || l.m.id.starts_with("acp:") || l.m.id.starts_with("repo:") {
            continue;
        }
        let id = l.m.id.clone();
        // A harness without its own corpus uses its `extends` root's (omp → pi).
        let dirs = match version_dirs(&id) {
            d if !d.is_empty() => d,
            _ => version_dirs(&l.family),
        };
        assert!(
            !dirs.is_empty(),
            "drift: manifest {id} has screen rules but no corpus under tests/harness-golden/{id}"
        );
        let h = Harness::from_id(&id).unwrap_or_else(|| panic!("{id}: no harness"));
        let code_backed = vk_agents::manifest::CODE_BACKED.contains(&h.base().id())
            && matches!(
                h.base(),
                Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp
            );
        for dir in dirs {
            let meta = std::fs::read_to_string(dir.join("meta.toml")).unwrap_or_default();
            assert!(
                meta.contains("synthetic = true") || meta.contains("recorded_version = \""),
                "{}: meta.toml missing provenance",
                dir.display()
            );
            let expected = read_jsonl(&dir.join("expected.jsonl"));
            assert!(
                !expected.is_empty(),
                "{}: empty expected.jsonl",
                dir.display()
            );
            for e in expected {
                let file = e["screen"].as_str().expect("screen");
                let text = std::fs::read_to_string(dir.join("screens").join(file))
                    .unwrap_or_else(|err| panic!("{}/{file}: {err}", dir.display()));
                let ctx = format!("{id} {}/{file}", dir.file_name().unwrap().to_string_lossy());
                // 1. The manifest's declarative rules.
                let r = l.evaluate(&text);
                match (&e["dialog"], &r.dialog) {
                    (Value::Null, None) => {}
                    (Value::Null, Some(d)) => panic!("{ctx}: unexpected dialog {d:?}"),
                    (want, None) => panic!("{ctx}: manifest found no dialog, want {want}"),
                    (want, Some(d)) => {
                        check_dialog(
                            &format!("{ctx} [manifest]"),
                            want,
                            &d.kind,
                            &d.title,
                            d.command.as_deref(),
                            d.options.len(),
                            d.pointer,
                        );
                        if !code_backed {
                            let spec = l.dialog_spec(&d.rule_id).cloned().unwrap_or_default();
                            for (k, keys) in e["keys"].as_object().into_iter().flatten() {
                                let got = plan_keys(d, &spec, &intent(k));
                                assert_eq!(
                                    got.map(|v| json!(v)),
                                    Some(keys.clone()),
                                    "{ctx}: keys for {k}"
                                );
                            }
                        }
                    }
                }
                if e["dialog"].is_null() {
                    assert_eq!(
                        r.state.as_ref().map(|s| s.0.as_str()),
                        e["state"].as_str(),
                        "{ctx} [manifest]: state"
                    );
                }
                // 2. The server's evaluator (code path for code-backed harnesses, manifest
                //    otherwise) through the same entry point the pane uses.
                let m = screen::evaluate(h, &text);
                match (&e["dialog"], &m.dialog) {
                    (Value::Null, None) => {
                        assert_eq!(
                            m.state.as_ref().map(|s| s.0.as_str()),
                            e["state"].as_str(),
                            "{ctx} [server]: state"
                        );
                    }
                    (Value::Null, Some(d)) => panic!("{ctx} [server]: unexpected dialog {d:?}"),
                    (want, None) => panic!("{ctx} [server]: no dialog, want {want}"),
                    (want, Some(d)) => {
                        let mut w = want.clone();
                        if code_backed {
                            // The code evaluator extracts commands differently (box above the question).
                            w.as_object_mut().map(|o| o.remove("command"));
                        }
                        check_dialog(
                            &format!("{ctx} [server]"),
                            &w,
                            d.kind.as_str(),
                            &d.title,
                            d.command.as_deref(),
                            d.options.len(),
                            d.pointer,
                        );
                        let it = super::harness_tests_blank();
                        for (k, keys) in e["keys"].as_object().into_iter().flatten() {
                            let got = screen::keys_for(h, d, &it, &answer(k));
                            assert_eq!(
                                got.map(|v| json!(v)),
                                Some(keys.clone()),
                                "{ctx} [server]: keys for {k}"
                            );
                        }
                    }
                }
                replayed += 1;
            }
        }
    }
    assert!(replayed >= 10, "only {replayed} golden screens replayed");
}

#[test]
fn golden_signals_replay_through_the_mappings() {
    for (id, translate) in [
        (
            "opencode",
            super::opencode::translate as fn(&str, &str, &Value) -> Vec<(&'static str, Value)>,
        ),
        ("gemini", super::gemini::translate),
    ] {
        let dirs = version_dirs(id);
        assert!(!dirs.is_empty(), "{id}: no corpus");
        for dir in dirs {
            let signals = read_jsonl(&dir.join("signals.jsonl"));
            let expected = read_jsonl(&dir.join("signals.expected.jsonl"));
            assert_eq!(
                signals.len(),
                expected.len(),
                "{}: signals vs expected",
                dir.display()
            );
            let pane = format!("golden-{id}-{}", dir.display());
            for (i, (s, e)) in signals.iter().zip(&expected).enumerate() {
                let out = translate(&pane, s["event"].as_str().unwrap(), &s["payload"]);
                let events: Vec<&str> = out.iter().map(|(e, _)| *e).collect();
                let want: Vec<&str> = e["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                assert_eq!(events, want, "{id} line {}: events", i + 1);
                for (path, v) in e["fields"].as_object().into_iter().flatten() {
                    let (n, ptr) = path.split_once('/').unwrap();
                    let n: usize = n.parse().unwrap();
                    assert_eq!(
                        out[n].1.pointer(&format!("/{ptr}")),
                        Some(v),
                        "{id} line {}: {path}",
                        i + 1
                    );
                }
            }
        }
    }
}
