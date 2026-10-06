//! Manifest parsing over the 99 real Herdr plugin manifests captured by the 2026-10-06 plugin
//! review (`tests/compat/herdr/0.9.3/manifests/`, with their source record in
//! `index.json`). Each fixture's SHA-256 must equal the
//! source record, every manifest must parse, and the parsed ids, actions and declared events must
//! match what the record lists.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;
use vk_compat::herdr::manifest::Manifest;
use vk_compat::herdr::registry::sha256_hex;
use vk_compat::herdr::{events, launch};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/compat/herdr/0.9.3/manifests")
}

fn index() -> Vec<Value> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(dir().join("index.json")).unwrap()).unwrap();
    v["manifests"].as_array().unwrap().clone()
}

fn strings(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn all_real_manifests_parse_and_match_the_source_record() {
    let list = index();
    assert_eq!(list.len(), 99);
    let mut warnings = 0;
    let mut stats = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for entry in &list {
        let file = dir().join(entry["fixture"].as_str().unwrap());
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(
            sha256_hex(&bytes),
            entry["sha256"].as_str().unwrap(),
            "{} differs from the pinned source",
            file.display()
        );
        let text = String::from_utf8(bytes).unwrap();
        let m = Manifest::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", entry["repository"]));
        assert_eq!(
            m.id,
            entry["plugin_id"].as_str().unwrap(),
            "{}",
            entry["repository"]
        );
        let actions: BTreeSet<String> = m.actions.iter().map(|a| a.id.clone()).collect();
        assert_eq!(actions, strings(&entry["actions"]), "{} actions", m.id);
        let evs: BTreeSet<String> = m.events.iter().map(|e| e.on.clone()).collect();
        assert_eq!(evs, strings(&entry["events"]), "{} events", m.id);
        // Every action resolves both bare and qualified.
        for a in &m.actions {
            assert!(m.resolve_action(&a.id).is_some());
            assert!(m.resolve_action(&format!("{}.{}", m.id, a.id)).is_some());
            // Commands resolve to an argv with the program first.
            assert!(!launch::resolve_argv("/root".as_ref(), &a.command).is_empty());
        }
        warnings += m.warnings.len();
        stats.0 += m.actions.len();
        stats.1 += m.events.len();
        stats.2 += m.panes.len();
        stats.3 += m.build.len();
        stats.4 += m.startup.len();
        stats.5 += m.link_handlers.len();
        // macOS/Linux is the M5 platform scope: every plugin offers something there.
        let pf = vk_compat::herdr::current_platform();
        if m.supports(pf) {
            let runnable = m.actions_on(pf).len()
                + m.panes
                    .iter()
                    .filter(|p| m.entry_on(p.platforms.as_ref(), pf))
                    .count()
                + m.events
                    .iter()
                    .filter(|e| m.entry_on(e.platforms.as_ref(), pf))
                    .count()
                + m.startup_on(pf).len()
                + m.build_on(pf).len(); // herdmates is build-only
            assert!(runnable > 0, "{} has nothing for {pf}", m.id);
        }
    }
    // Totals over the corpus, as counted from the TOML independently of this parser.
    assert_eq!(
        stats,
        (390, 120, 143, 89, 26, 9),
        "(actions, events, panes, build, startup, link handlers)"
    );
    eprintln!("manifest warnings across corpus: {warnings}");
}

#[test]
fn corpus_events_are_mostly_baseline_names() {
    let mut unknown = BTreeSet::new();
    let mut used = BTreeSet::new();
    for entry in index() {
        for e in strings(&entry["events"]) {
            if events::BASELINE_EVENTS.contains(&e.as_str()) {
                used.insert(e);
            } else {
                unknown.insert(e);
            }
        }
    }
    // One plugin declares `workspace.reordered`, which the spec does not name.
    assert_eq!(unknown, BTreeSet::from(["workspace.reordered".to_string()]));
    assert!(used.len() >= 20, "{used:?}");
}
