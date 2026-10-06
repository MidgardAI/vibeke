//! M6 documentation groundwork: the generated API catalog (`docs/api/`), the CLI and config
//! references of the docs site (`docs/site/src/reference/`) and the `vibeke/1` freeze check.
//!
//! Regenerate everything with `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs`.
//! The freeze snapshot (`docs/api/vibeke-1.frozen.json`) is updated separately with
//! `VIBEKE_UPDATE_API_FREEZE=1`; that only adds methods unless `VIBEKE_API_FREEZE_ALLOW_BREAK=1`
//! is also set (which a reviewer should be able to justify while the freeze is still a draft).

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_default()
}

/// Every method table in the server: (table, name, mutating).
fn tables() -> Vec<(&'static str, &'static [(&'static str, bool)])> {
    vk_server::api_schema::method_tables()
}

/// One row of spec 07 §2: method -> (milestone, "params → result" text).
fn spec_rows() -> BTreeMap<String, (String, String)> {
    let spec = read("spec/07-api-cli-plugins.md");
    let start = spec.find("\n## 2. Method catalog").expect("spec 07 §2");
    let end = spec[start..]
        .find("\n## 3. ")
        .map_or(spec.len(), |e| start + e);
    let mut out = BTreeMap::new();
    let mut milestone = String::new();
    for line in spec[start..end].lines() {
        if let Some(h) = line.strip_prefix("### ") {
            milestone = h
                .rfind('[')
                .map(|i| h[i..].trim_matches(|c| c == '[' || c == ']').to_string())
                .unwrap_or_default();
            continue;
        }
        if !line.starts_with("| `") {
            continue;
        }
        // The first cell is one or more backtick spans; later cells may contain `|` inside code.
        let Some(i) = line.find("` | `") else {
            continue;
        };
        let (names, rest) = (&line[2..=i], line[i + 4..].trim_end_matches(" |").trim());
        let mut parts = names.split('`').skip(1).step_by(2);
        for name in parts.by_ref() {
            let ok = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_');
            if ok && name.contains('.') {
                out.entry(name.to_string())
                    .or_insert_with(|| (milestone.clone(), rest.to_string()));
            }
        }
    }
    out
}

/// `forbidden` | `own_target` | `open`, from the registry `api::authorize` itself consults.
fn pane_scope(method: &str) -> &'static str {
    vk_server::api::pane_scope_of(method).as_str()
}

/// `full`: only full-scope clients (TUI, CLI, plugins with a grant); `pane`: also callable with a
/// pane token (writes limited to the caller's own panes and runs where `own_target`).
fn scope(method: &str) -> &'static str {
    match pane_scope(method) {
        "forbidden" => "full",
        _ => "pane",
    }
}

pub fn catalog() -> Value {
    let spec = spec_rows();
    let mut methods: BTreeMap<&str, (bool, Vec<&str>)> = BTreeMap::new();
    for (table, rows) in tables() {
        for (name, mutating) in rows {
            let e = methods.entry(name).or_insert((*mutating, vec![]));
            assert_eq!(
                e.0, *mutating,
                "{name}: tables disagree on the mutating flag ({table} vs {:?})",
                e.1
            );
            e.1.push(table);
        }
    }
    let list: Vec<Value> = methods
        .iter()
        .map(|(name, (mutating, tabs))| {
            let (milestone, signature) = spec
                .get(*name)
                .map(|(m, s)| (json!(m), json!(s)))
                .unwrap_or((Value::Null, Value::Null));
            let (params, result) = vk_server::api_schema::method_sources(name)
                .map(|(p, r)| (json!(p), json!(r)))
                .unwrap_or((Value::Null, Value::Null));
            json!({
                "name": name,
                "mutating": mutating,
                "scope": scope(name),
                "pane_scope": pane_scope(name),
                "gateway": if *mutating { "actor_required" } else { "open" },
                "tables": tabs,
                "milestone": milestone,
                "spec": signature,
                "params": params,
                "result": result,
            })
        })
        .collect();
    json!({
        "api": vk_proto::API_VERSION,
        "generated_from": "vk-server METHODS tables; signatures from spec/07 §2; params/result shapes from the schema registry (crates/vk-server/src/api_schema.rs)",
        "schema": "vibeke-1.schema.json",
        "shape_language": "{field, field?: type = default, ...}; [T]; A|B (bare lowercase words are string literals); {*: T} maps; CamelCase names are shared definitions in the schema's $defs",
        "render_protocol": vk_proto::render::PROTOCOL,
        "holder_protocol": {"proto": vk_proto::holder::PROTO, "min": vk_proto::holder::PROTO_MIN},
        "method_count": list.len(),
        "methods": list,
    })
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

pub fn api_markdown(cat: &Value) -> String {
    let mut s = String::new();
    s.push_str("# Control API reference (`vibeke/1`)\n\n");
    s.push_str("<!-- Generated by `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs`. Do not edit. -->\n\n");
    let _ = writeln!(
        s,
        "API string `{}`. Render protocol `{}`. Holder protocol `{}` (minimum `{}`). {} methods. The machine-readable form is [`methods.json`](methods.json); the freeze policy is below.\n",
        cat["api"].as_str().unwrap(),
        cat["render_protocol"],
        cat["holder_protocol"]["proto"],
        cat["holder_protocol"]["min"],
        cat["method_count"],
    );
    s.push_str("## Reading the table\n\n");
    s.push_str(
        "- **Mutating**: the call changes state; gateway clients must pass `actor`, and every mutation is audited.\n\
         - **Scope**: `full` means only full-scope clients (the TUI, the CLI, granted plugins) may call it; a pane token is refused. `pane` means a pane token may call it too. `own` marks pane-scope calls that must target the caller's own panes or runs (09 §5.2).\n\
         - **Signature**: `params → result` as written in spec 07 §2. A dash means the spec does not tabulate the method yet (the code is authoritative; `api.schema` returns the live shapes).\n\n",
    );
    s.push_str("## Schema and clients\n\n");
    s.push_str(
        "Every method has a params and a result shape, every event type a subject and data shape, and every error kind a code, all written once in the schema registry (`crates/vk-server/src/api_schema.rs`). [`methods.json`](methods.json) carries each method's shapes in the registry's compact notation (`params`, `result`: `{field, field?: type = default}`, `[T]`, `A|B`, `{*: T}` maps, CamelCase shared types). [`vibeke-1.schema.json`](vibeke-1.schema.json) is the same information as JSON Schema 2020-12 (shared types under `$defs`; `x-methods`, `x-events`, `x-notifications`, `x-errors`); `vibeke debug api-schema [--out FILE]` prints it for the running binary and the `api.schema {method?}` method serves it. The typed clients in [`clients/typescript`](../../clients/typescript) (`@vibeke/client`, Node `net`, events as an async iterator) and [`clients/python`](../../clients/python) (`vibeke-client`, stdlib `asyncio`, TypedDicts) are generated from that schema; `cargo test -p vibeke --test api_clients` fails when a checked-in client file is stale (`VIBEKE_UPDATE_CLIENTS=1` regenerates). Results of a real server and the payloads of its events are validated against the shapes in tests.\n\n");
    s.push_str("## Versioning and the freeze\n\n");
    s.push_str(
        "Within `vibeke/1` only additions are allowed: new methods, new optional params, new result fields and new event types. Clients must ignore unknown fields and event types (spec 07 §1.5). The snapshot [`vibeke-1.frozen.json`](vibeke-1.frozen.json) lists the methods that may not be removed or have their mutating flag, scope or pane scope (`own_target`, `open`, `forbidden`) changed; a test fails when they do. **The freeze is a draft until 1.0**: the snapshot is regenerated deliberately (`VIBEKE_UPDATE_API_FREEZE=1`) and the flags may still move before the release.\n\n",
    );
    s.push_str("## Methods\n\n| Method | Mutating | Scope | Milestone | Signature |\n|---|---|---|---|---|\n");
    for m in cat["methods"].as_array().unwrap() {
        let scope = match (
            m["scope"].as_str().unwrap(),
            m["pane_scope"].as_str().unwrap(),
        ) {
            ("full", _) => "full".to_string(),
            (_, "own_target") => "pane (own)".to_string(),
            _ => "pane".to_string(),
        };
        let sig = m["spec"]
            .as_str()
            .map(md_cell)
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            s,
            "| `{}` | {} | {} | {} | {} |",
            m["name"].as_str().unwrap(),
            if m["mutating"].as_bool().unwrap() {
                "yes"
            } else {
                "no"
            },
            scope,
            m["milestone"].as_str().unwrap_or("-"),
            sig
        );
    }
    s
}

fn cli_markdown() -> String {
    let mut s = String::from(
        "# CLI reference\n\n<!-- Generated by `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs` from `vk_cli::COMMANDS`. Do not edit. -->\n\n",
    );
    s.push_str(
        "Every command is `vibeke <noun> <verb> [positionals] [--flag value]` and maps onto one control-API method (see the [API reference](api.md)). Global flags: `--session NAME`, `--machine NAME`, `--socket PATH`, `--timeout MS`, `--json`, `--pretty`, `--quiet`, `--no-spawn`. Results print as JSON on stdout when `--json` is set or stdout is not a terminal.\n\n\
         Top-level commands: `vibeke` / `vibeke attach` (attach the TUI, spawning the server if needed), `vibeke ssh <host>`, `vibeke update`, `vibeke doctor [--rebuild-index]` (offline rebuild of the scrollback search index, server stopped), `vibeke forget --pane p|--workspace w|--before t|--all [--yes] [--dry-run]` (delete archived scrollback; calls `scrollback.forget`), `vibeke --skill`, `vibeke --default-config`, `vibeke --version`.\n\n",
    );
    for noun in vk_cli::nouns() {
        let _ = writeln!(s, "## `vibeke {noun}`\n");
        s.push_str("| Verb | Positionals | Method | Description |\n|---|---|---|---|\n");
        for (n, verb, method, pos, help) in vk_cli::COMMANDS {
            if *n == noun {
                let pos: Vec<String> = pos.iter().map(|p| format!("`<{p}>`")).collect();
                let _ = writeln!(
                    s,
                    "| `{verb}` | {} | `{method}` | {} |",
                    if pos.is_empty() {
                        "-".into()
                    } else {
                        pos.join(" ")
                    },
                    md_cell(help)
                );
            }
        }
        s.push('\n');
    }
    s
}

fn config_markdown() -> String {
    let mut s = String::from(
        "# Configuration reference\n\n<!-- Generated by `VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs` from `vk-config`'s default config. Do not edit. -->\n\n",
    );
    s.push_str(
        "The file is `~/.config/vibeke/config.toml` (override with `VIBEKE_CONFIG`). It reloads on change; a file that fails to parse is ignored and the previous configuration stays in force. Unknown keys warn, never fail. `vibeke --default-config` prints the template below. Every setting is shown with its default; keys marked as requiring new panes apply only to panes created afterwards.\n\n```toml\n",
    );
    s.push_str(vk_config::default_config_toml().trim_end());
    s.push_str("\n```\n");
    s
}

fn freeze_doc(cat: &Value) -> Value {
    let mut methods = serde_json::Map::new();
    for m in cat["methods"].as_array().unwrap() {
        methods.insert(
            m["name"].as_str().unwrap().to_string(),
            json!({"mutating": m["mutating"], "scope": m["scope"], "pane_scope": m["pane_scope"]}),
        );
    }
    json!({
        "api": cat["api"],
        "status": "draft until 1.0",
        "rule": "frozen methods may not be removed and their mutating/scope/pane_scope flags may not change; additions are allowed",
        "render_protocol": cat["render_protocol"],
        "holder_protocol": cat["holder_protocol"],
        "methods": methods,
    })
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    s
}

/// Compare (or, with `VIBEKE_UPDATE_DOCS`, rewrite) a generated file.
fn check_generated(rel: &str, want: &str, stale: &mut Vec<String>) {
    let path = root().join(rel);
    if std::env::var_os("VIBEKE_UPDATE_DOCS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, want).unwrap();
    } else if std::fs::read_to_string(&path).ok().as_deref() != Some(want) {
        stale.push(rel.to_string());
    }
}

#[test]
fn generated_docs_are_up_to_date() {
    let cat = catalog();
    let mut stale = vec![];
    check_generated("docs/api/methods.json", &pretty(&cat), &mut stale);
    check_generated(
        "docs/api/vibeke-1.schema.json",
        &pretty(&vk_server::api_schema::bundle()),
        &mut stale,
    );
    let md = api_markdown(&cat);
    check_generated("docs/api/README.md", &md, &mut stale);
    check_generated(
        "docs/site/src/reference/api.md",
        &md.replace("(methods.json)", "(../../../api/methods.json)")
            .replace(
                "(vibeke-1.frozen.json)",
                "(../../../api/vibeke-1.frozen.json)",
            )
            .replace(
                "(vibeke-1.schema.json)",
                "(../../../api/vibeke-1.schema.json)",
            )
            .replace("(../../clients/", "(../../../../clients/"),
        &mut stale,
    );
    check_generated(
        "docs/site/src/reference/cli.md",
        &cli_markdown(),
        &mut stale,
    );
    check_generated(
        "docs/site/src/reference/config.md",
        &config_markdown(),
        &mut stale,
    );
    assert!(
        stale.is_empty(),
        "generated docs are stale: {stale:?}; run VIBEKE_UPDATE_DOCS=1 cargo test -p vibeke --test api_docs"
    );
}

#[test]
fn catalog_covers_every_table_and_is_consistent() {
    let cat = catalog();
    let names: Vec<&str> = cat["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, names, "methods are unique and sorted");
    for (table, rows) in tables() {
        for (n, _) in rows {
            assert!(
                names.contains(n),
                "{n} from {table} missing from the catalog"
            );
        }
    }
    for m in [
        "pane.send_text",
        "task.review.accept",
        "server.stop",
        "events.read",
    ] {
        assert!(names.contains(&m), "{m} should be in the catalog");
    }
    let by = |n: &str| {
        cat["methods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(by("server.stop")["scope"], "full");
    assert_eq!(by("pane.read")["pane_scope"], "open");
    assert_eq!(by("pane.send_text")["pane_scope"], "own_target");
    for m in [
        "preview.mirror",
        "preview.unmirror",
        "preview.profile.reset",
    ] {
        assert_eq!(
            by(m)["scope"],
            "full",
            "{m}: its handler refuses pane scope"
        );
        assert_eq!(by(m)["pane_scope"], "forbidden", "{m}");
    }
    assert_eq!(by("pane.send_text")["gateway"], "actor_required");
    assert_eq!(by("pane.list")["mutating"], false);
    // Spec-derived signatures are attached where spec 07 §2 tabulates a method.
    assert!(
        by("workspace.create")["spec"]
            .as_str()
            .unwrap()
            .contains("→")
    );
    assert_eq!(by("workspace.create")["milestone"], "M1");
    assert!(
        spec_rows().len() > 100,
        "spec 07 §2 parsing found too few rows"
    );
}

#[test]
fn spec_parser_handles_pipes_in_code_and_combined_rows() {
    let rows = spec_rows();
    // `client.hello` carries `tui|cli|plugin|gateway` inside its params cell.
    assert!(rows["client.hello"].1.contains("tui|cli|plugin|gateway"));
    // Combined first cell: `theme.get` / `theme.set_mode`.
    assert_eq!(rows["theme.get"].1, rows["theme.set_mode"].1);
}

fn diff_against_freeze(frozen: &Value, cat: &Value) -> Vec<String> {
    let mut problems = vec![];
    let cur: BTreeMap<&str, &Value> = cat["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m))
        .collect();
    for (name, f) in frozen["methods"].as_object().unwrap() {
        match cur.get(name.as_str()) {
            None => problems.push(format!("{name}: removed")),
            Some(m) => {
                if m["mutating"] != f["mutating"] {
                    problems.push(format!(
                        "{name}: mutating changed {} -> {}",
                        f["mutating"], m["mutating"]
                    ));
                }
                if m["scope"] != f["scope"] {
                    problems.push(format!(
                        "{name}: scope changed {} -> {}",
                        f["scope"], m["scope"]
                    ));
                }
                // own_target / open / forbidden: every frozen entry records it.
                if f.get("pane_scope").is_none() {
                    problems.push(format!("{name}: frozen entry lacks pane_scope"));
                } else if m["pane_scope"] != f["pane_scope"] {
                    problems.push(format!(
                        "{name}: pane_scope changed {} -> {}",
                        f["pane_scope"], m["pane_scope"]
                    ));
                }
            }
        }
    }
    if frozen["api"] != cat["api"] {
        problems.push(format!(
            "api string changed {} -> {}",
            frozen["api"], cat["api"]
        ));
    }
    // A frozen holder protocol must stay reachable: the server must still speak it.
    let (fp, cmin) = (
        frozen["holder_protocol"]["proto"].as_u64().unwrap(),
        cat["holder_protocol"]["min"].as_u64().unwrap(),
    );
    if cmin > fp {
        problems.push(format!(
            "holder protocol minimum {cmin} no longer reaches frozen {fp}"
        ));
    }
    if cat["render_protocol"].as_u64() < frozen["render_protocol"].as_u64() {
        problems.push("render protocol went backwards".into());
    }
    problems
}

#[test]
fn vibeke_1_freeze_holds() {
    let cat = catalog();
    let path = root().join("docs/api/vibeke-1.frozen.json");
    if std::env::var_os("VIBEKE_UPDATE_API_FREEZE").is_some() {
        let mut next = freeze_doc(&cat);
        if let Ok(old) = std::fs::read_to_string(&path) {
            let old: Value = serde_json::from_str(&old).unwrap();
            let problems = diff_against_freeze(&old, &cat);
            if std::env::var_os("VIBEKE_API_FREEZE_ALLOW_BREAK").is_none() {
                assert!(
                    problems.is_empty(),
                    "refusing to rewrite the freeze with breaking changes (set VIBEKE_API_FREEZE_ALLOW_BREAK=1): {problems:?}"
                );
            }
            // Keep the recorded status if the snapshot already exists.
            next["status"] = old["status"].clone();
        }
        std::fs::write(&path, pretty(&next)).unwrap();
        return;
    }
    let frozen: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("docs/api/vibeke-1.frozen.json"),
    )
    .unwrap();
    let problems = diff_against_freeze(&frozen, &cat);
    assert!(problems.is_empty(), "vibeke/1 freeze broken: {problems:?}");
}

#[test]
fn freeze_check_detects_breaks_and_allows_additions() {
    let cat = catalog();
    let frozen = freeze_doc(&cat);
    assert!(diff_against_freeze(&frozen, &cat).is_empty());

    // Addition: a frozen snapshot that lacks a current method still passes.
    let mut fewer = frozen.clone();
    fewer["methods"]
        .as_object_mut()
        .unwrap()
        .remove("pane.list");
    assert!(diff_against_freeze(&fewer, &cat).is_empty());

    // Removal: a frozen method that no longer exists fails.
    let mut more = frozen.clone();
    more["methods"]["no.such_method"] =
        json!({"mutating": false, "scope": "pane", "pane_scope": "open"});
    assert_eq!(
        diff_against_freeze(&more, &cat),
        vec!["no.such_method: removed"]
    );

    // Flag changes fail.
    let mut flipped = frozen.clone();
    flipped["methods"]["pane.list"]["mutating"] = json!(true);
    flipped["methods"]["server.stop"]["scope"] = json!("pane");
    let p = diff_against_freeze(&flipped, &cat);
    assert_eq!(p.len(), 2, "{p:?}");

    // own_target <-> open regressions keep `scope` at "pane" but still fail.
    let mut widened = frozen.clone();
    assert_eq!(
        widened["methods"]["pane.send_text"]["pane_scope"],
        "own_target"
    );
    widened["methods"]["pane.send_text"]["pane_scope"] = json!("open");
    widened["methods"]["pane.read"]["pane_scope"] = json!("own_target");
    assert_eq!(
        diff_against_freeze(&widened, &cat),
        vec![
            "pane.read: pane_scope changed \"own_target\" -> \"open\"",
            "pane.send_text: pane_scope changed \"open\" -> \"own_target\"",
        ]
    );
    // A frozen entry without pane_scope is rejected (every entry must record it).
    let mut bare = frozen.clone();
    bare["methods"]["pane.read"]
        .as_object_mut()
        .unwrap()
        .remove("pane_scope");
    assert_eq!(
        diff_against_freeze(&bare, &cat),
        vec!["pane.read: frozen entry lacks pane_scope"]
    );
    // forbidden -> open shows up in both columns.
    let mut opened = frozen.clone();
    opened["methods"]["preview.mirror"]["scope"] = json!("pane");
    opened["methods"]["preview.mirror"]["pane_scope"] = json!("open");
    assert_eq!(diff_against_freeze(&opened, &cat).len(), 2);

    // Protocol and api string.
    let mut proto = frozen.clone();
    proto["api"] = json!("vibeke/0");
    proto["holder_protocol"]["proto"] = json!(0);
    proto["render_protocol"] = json!(99);
    assert_eq!(diff_against_freeze(&proto, &cat).len(), 3);
}

#[test]
fn frozen_snapshot_is_marked_draft() {
    let frozen: Value =
        serde_json::from_str(&read("docs/api/vibeke-1.frozen.json")).expect("freeze snapshot");
    assert_eq!(frozen["status"], "draft until 1.0");
    assert!(read("spec/11-milestones.md").contains("draft until 1.0"));
}

#[test]
fn site_summary_links_resolve() {
    let src = root().join("docs/site/src");
    let summary = std::fs::read_to_string(src.join("SUMMARY.md")).expect("SUMMARY.md");
    let mut n = 0;
    for line in summary.lines() {
        if let (Some(a), Some(b)) = (line.find("]("), line.rfind(')')) {
            let target = &line[a + 2..b];
            assert!(
                src.join(target).is_file(),
                "SUMMARY.md links to missing {target}"
            );
            n += 1;
        }
    }
    assert!(n >= 12, "expected the full chapter list, found {n}");
}
