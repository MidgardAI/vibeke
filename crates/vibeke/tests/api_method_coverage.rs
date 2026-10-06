//! Per-method API test enforcement (spec 10 §4.3): every method in the API catalog must be
//! exercised by at least one integration test under `crates/*/tests`, or be listed with a
//! reason in `api_method_allowlist.txt`.
//!
//! A method counts as exercised when a test source contains either the quoted method name
//! (`"pane.list"`, raw JSON-RPC) or the CLI spelling that maps to it in `vk_cli::COMMANDS`
//! (`"pane", "list"` or `vibeke pane list`). The check is textual on purpose: it needs no
//! server, runs in milliseconds, and fails the build the moment someone adds a method without
//! a test. The allowlist only shrinks: a listed method that is now covered, or that no longer
//! exists, fails too, so entries are removed as tests arrive.
//!
//! Regenerate the allowlist skeleton for a batch of new exceptions with
//! `VIBEKE_PRINT_UNCOVERED=1 cargo test -p vibeke --test api_method_coverage -- --nocapture`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root")
        .to_path_buf()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Integration test sources: `crates/<crate>/tests/**/*.rs`, minus this file (it names every
/// method in its own docs) and with whitespace squeezed so rustfmt line breaks don't matter.
fn test_sources() -> String {
    let mut files = Vec::new();
    let crates = repo_root().join("crates");
    for c in std::fs::read_dir(&crates).unwrap().flatten() {
        rs_files(&c.path().join("tests"), &mut files);
    }
    // vk-server's in-process API tests (`*_tests.rs`, `tests.rs`) drive the real handlers
    // through the dispatcher on a real core and store; they count as integration tests.
    let mut server_src = Vec::new();
    rs_files(&crates.join("vk-server/src"), &mut server_src);
    files.extend(server_src.into_iter().filter(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "tests.rs" || n.ends_with("_tests.rs"))
    }));
    files.sort();
    let mut all = String::new();
    for f in files {
        if f.file_name().is_some_and(|n| n == "api_method_coverage.rs") {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(&f) {
            all.push_str(&squeeze(&s));
            all.push('\n');
        }
    }
    all
}

fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn catalog() -> BTreeSet<String> {
    vk_server::api_schema::bundle()["x-methods"]
        .as_object()
        .expect("x-methods")
        .keys()
        .cloned()
        .collect()
}

/// method -> CLI spellings (`"noun","verb"` squeezed) that call it.
fn cli_spellings() -> BTreeMap<&'static str, Vec<String>> {
    let mut m: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    for (noun, verb, method, _, _) in vk_cli::COMMANDS {
        m.entry(method)
            .or_default()
            .push(format!("\"{noun}\",\"{verb}\""));
        m.entry(method).or_default().push(format!("{noun}{verb}\""));
    }
    m
}

fn covered(method: &str, src: &str, cli: &BTreeMap<&'static str, Vec<String>>) -> bool {
    if src.contains(&format!("\"{method}\"")) {
        return true;
    }
    cli.get(method)
        .is_some_and(|sp| sp.iter().any(|s| src.contains(s.as_str())))
}

fn allowlist() -> BTreeMap<String, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/api_method_allowlist.txt");
    let text = std::fs::read_to_string(&path).expect("api_method_allowlist.txt");
    let mut out = BTreeMap::new();
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(c) = line.strip_prefix('#') {
            group = c.trim().to_string();
            continue;
        }
        let (name, reason) = match line.split_once('#') {
            Some((n, r)) => (n.trim(), r.trim().to_string()),
            None => (line, group.clone()),
        };
        assert!(
            !reason.is_empty(),
            "allowlist entry {name} needs a reason (a `# reason` suffix or a preceding comment)"
        );
        out.insert(name.to_string(), reason);
    }
    out
}

#[test]
fn every_api_method_has_an_integration_test_or_an_allowlisted_reason() {
    let src = test_sources();
    let cli = cli_spellings();
    let allow = allowlist();
    let methods = catalog();
    assert!(methods.len() > 100, "catalog suspiciously small");

    let uncovered: Vec<&String> = methods.iter().filter(|m| !covered(m, &src, &cli)).collect();
    if std::env::var_os("VIBEKE_PRINT_UNCOVERED").is_some() {
        for m in &uncovered {
            println!("{m}");
        }
    }
    let missing: Vec<&&String> = uncovered
        .iter()
        .filter(|m| !allow.contains_key(m.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "API methods with no integration test and no allowlist entry (add a test under \
         crates/vibeke/tests, or an entry with a reason in tests/api_method_allowlist.txt):\n  {}",
        missing
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let stale_covered: Vec<&String> = allow
        .keys()
        .filter(|a| methods.contains(*a) && covered(a, &src, &cli))
        .collect();
    assert!(
        stale_covered.is_empty(),
        "allowlisted methods that are now covered by a test (delete them from the allowlist):\n  {}",
        stale_covered
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
    let unknown: Vec<&String> = allow.keys().filter(|a| !methods.contains(*a)).collect();
    assert!(
        unknown.is_empty(),
        "allowlist names methods that do not exist (renamed or removed?):\n  {}",
        unknown
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

#[test]
fn the_matcher_recognises_both_spellings() {
    let cli = cli_spellings();
    assert!(covered(
        "pane.list",
        &squeeze(r#"s.json(&["pane", "list"])"#),
        &cli
    ));
    assert!(covered(
        "pane.list",
        &squeeze(r#"call("pane.list", json!({}))"#),
        &cli
    ));
    assert!(!covered(
        "pane.list",
        &squeeze(r#"["pane", "split"]"#),
        &cli
    ));
}
