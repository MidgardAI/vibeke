//! Pull-request fixtures for Herdr plugin calls (spec 18 §13).
//!
//! Each file in `tests/compat/herdr/0.9.3/plugins/fixtures/` is one call that real plugins
//! make, with the state it needs and the shape of the response:
//!
//! ```toml
//! plugin = "fixture.caller"          # a synthetic plugin id
//! authority = "legacy"               # the plugin's trust (only legacy for now)
//! [initial]                          # created before the call
//! workspaces = 1                     # placeholders {workspace} {tab} {pane}
//! tabs = 1                           # extra tabs in the first workspace: {tab2}
//! panes = 1                          # extra panes split from {pane}: {pane2}
//! [call]
//! method = "pane.split"              # the Herdr method
//! params = { pane_id = "{pane}" }
//! [transport]
//! kind = "socket"                    # the call as one JSON line on the plugin's broker
//! # kind = "cli"; args = ["pane", "split", "{pane}"]   # the `herdr` CLI inside the plugin
//! [expect]
//! ok = true                          # or false with `code`
//! type = "pane_info"                 # `result.type`
//! keys = { "pane.pane_id" = "string" }   # dotted paths: string|number|bool|array|object|null
//! ```
//!
//! The test installs a tiny synthetic plugin from a `file://` repository, trusts it legacy, and
//! makes each call from inside a real invocation of that plugin, so the broker and the plugin's
//! own authority stay in the path. These fixtures catch regressions; they do not prove that
//! Herdr answers the same way.

use crate::support::Session;
use crate::support::plugins::{git, read, session_with_env, write};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/compat/herdr/0.9.3/plugins/fixtures")
}

struct Fixture {
    name: String,
    plugin: String,
    initial: (u64, u64, u64),
    method: String,
    params: Value,
    transport: String,
    args: Vec<String>,
    expect: Value,
}

fn load(path: &Path) -> Result<Fixture, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: Value = toml::from_str(&text).map_err(|e| e.to_string())?;
    let known = [
        "plugin",
        "authority",
        "initial",
        "call",
        "transport",
        "expect",
    ];
    for k in v.as_object().ok_or("not a table")?.keys() {
        if !known.contains(&k.as_str()) {
            return Err(format!("unknown key `{k}`"));
        }
    }
    if v["authority"] != "legacy" {
        return Err("authority must be \"legacy\"".into());
    }
    let n = |k: &str| v["initial"][k].as_u64().unwrap_or(0);
    let fx = Fixture {
        name: path.file_stem().unwrap().to_string_lossy().into_owned(),
        plugin: v["plugin"].as_str().ok_or("plugin is required")?.into(),
        initial: (n("workspaces").max(1), n("tabs"), n("panes")),
        method: v["call"]["method"]
            .as_str()
            .ok_or("call.method is required")?
            .into(),
        params: v["call"].get("params").cloned().unwrap_or(json!({})),
        transport: v["transport"]["kind"]
            .as_str()
            .ok_or("transport.kind is required")?
            .into(),
        args: v["transport"]["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        expect: v["expect"].clone(),
    };
    match fx.transport.as_str() {
        "socket" => {}
        "cli" => {
            // The CLI spelling must be the same call the fixture names.
            match vk_compat::herdr::cli::parse(&fx.args) {
                vk_compat::herdr::cli::Parsed::Call { method, .. } if method == fx.method => {}
                other => return Err(format!("cli args do not parse to {}: {other:?}", fx.method)),
            }
        }
        other => return Err(format!("unknown transport `{other}`")),
    }
    if !fx.expect["ok"].is_boolean() {
        return Err("expect.ok is required".into());
    }
    Ok(fx)
}

fn load_all() -> Vec<Fixture> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| load(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .collect()
}

/// The action makes the call in `$HERDR_PLUGIN_STATE_DIR/request` (socket) or `argv` (CLI)
/// and leaves the answer in `response` or `stdout`, `stderr` and `exit`.
const CALL_SH: &str = r#"out="$HERDR_PLUGIN_STATE_DIR"
rm -f "$out/stdout" "$out/stderr" "$out/exit" "$out/response"
case "$(cat "$out/transport")" in
cli)
  set --
  while IFS= read -r a; do set -- "$@" "$a"; done < "$out/argv"
  herdr "$@" > "$out/stdout" 2> "$out/stderr"
  echo $? > "$out/exit.tmp"
  ;;
socket)
  /usr/bin/perl "$HERDR_PLUGIN_ROOT/bin/sock.pl" "$HERDR_SOCKET_PATH" "$out/request" > "$out/response" 2> "$out/stderr"
  echo $? > "$out/exit.tmp"
  ;;
esac
mv "$out/exit.tmp" "$out/exit"
"#;

/// A raw socket client, like plugins that speak Herdr's wire protocol directly.
const SOCK_PL: &str = r#"use strict;
use IO::Socket::UNIX;
my ($path, $file) = @ARGV;
open(my $f, '<', $file) or die "request: $!";
my $req = <$f>;
my $s = IO::Socket::UNIX->new(Peer => $path) or die "connect $path: $!";
print $s $req;
my $line = <$s>;
print $line;
"#;

/// Publish the synthetic plugin `id` as `fixtures/<repo>` under `srv`; returns the repo name.
fn publish(base: &Path, id: &str) -> String {
    let repo = id.replace('.', "-");
    let work = base.join("work").join(&repo);
    write(
        &work.join("herdr-plugin.toml"),
        &format!(
            "id = \"{id}\"\nversion = \"1\"\n[[actions]]\nid = \"call\"\ntitle = \"Call\"\ncommand = [\"sh\", \"bin/call.sh\"]\n"
        ),
    );
    write(&work.join("bin/call.sh"), CALL_SH);
    write(&work.join("bin/sock.pl"), SOCK_PL);
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "fixture plugin"]);
    let bare = base.join("srv/fixtures").join(&repo);
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        base,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    repo
}

/// Create the fixture's initial state; returns its placeholders.
fn initial(s_: &Session, fx: &Fixture, cwd: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    let (ws, tabs, panes) = fx.initial;
    for i in 0..ws {
        let w = s_.herdr(&["workspace", "create", "--cwd", cwd]);
        if i == 0 {
            vars.insert("workspace", w["workspace"]["workspace_id"].clone());
            vars.insert("tab", w["tab"]["tab_id"].clone());
            vars.insert("pane", w["root_pane"]["pane_id"].clone());
        }
    }
    let ws_id = vars["workspace"].as_str().unwrap().to_string();
    for i in 0..tabs {
        let t = s_.herdr(&["tab", "create", "--workspace-id", &ws_id]);
        vars.insert(
            if i == 0 { "tab2" } else { "tab3" },
            t["tab"]["tab_id"].clone(),
        );
    }
    let root = vars["pane"].as_str().unwrap().to_string();
    for i in 0..panes {
        let p = s_.herdr(&["pane", "split", &root, "--direction", "right"]);
        vars.insert(
            if i == 0 { "pane2" } else { "pane3" },
            p["pane"]["pane_id"].clone(),
        );
    }
    vars.into_iter()
        .map(|(k, v)| (k.to_string(), v.as_str().unwrap_or_default().to_string()))
        .collect()
}

fn fill(text: &str, vars: &BTreeMap<String, String>) -> String {
    vars.iter().fold(text.to_string(), |t, (k, v)| {
        t.replace(&format!("{{{k}}}"), v)
    })
}

/// The fixture's answer as `{ok, result | error}`, from either transport.
fn answer(fx: &Fixture, state: &Path) -> Result<Value, String> {
    let exit: i32 = read(&state.join("exit")).trim().parse().unwrap_or(-1);
    let first_json = |t: &str| {
        t.lines()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .unwrap_or(Value::Null)
    };
    if fx.transport == "cli" {
        return Ok(if exit == 0 {
            json!({"ok": true, "result": first_json(&read(&state.join("stdout")))})
        } else {
            json!({"ok": false, "error": first_json(&read(&state.join("stderr")))["error"]})
        });
    }
    let r = first_json(&read(&state.join("response")));
    if exit != 0 || r.is_null() {
        return Err(format!(
            "socket client failed ({exit}): {}",
            read(&state.join("stderr"))
        ));
    }
    Ok(match r.get("error") {
        Some(e) if !e.is_null() => json!({"ok": false, "error": e}),
        _ => json!({"ok": true, "result": r["result"]}),
    })
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn at<'v>(v: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.')
        .try_fold(v, |cur, seg| match seg.parse::<usize>() {
            Ok(i) if cur.is_array() => cur.get(i),
            _ => cur.get(seg),
        })
}

/// Differences between an answer and the fixture's `[expect]`.
fn shape_errors(expect: &Value, got: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if got["ok"] != expect["ok"] {
        out.push(format!("ok: expected {}, got {got}", expect["ok"]));
        return out;
    }
    if got["ok"] == true {
        let r = &got["result"];
        if let Some(t) = expect["type"].as_str()
            && r["type"] != t
        {
            out.push(format!("type: expected {t}, got {}", r["type"]));
        }
        for (path, want) in expect["keys"].as_object().into_iter().flatten() {
            match at(r, path) {
                None => out.push(format!("{path}: missing")),
                Some(x) if Some(type_name(x)) != want.as_str() => {
                    out.push(format!("{path}: expected {want}, got {}", type_name(x)))
                }
                Some(_) => {}
            }
        }
    } else if let Some(c) = expect["code"].as_str()
        && got["error"]["code"] != c
    {
        out.push(format!("error code: expected {c}, got {}", got["error"]));
    }
    out
}

#[test]
fn shape_checks_catch_real_differences() {
    let expect = json!({"ok": true, "type": "pane_info", "keys": {"pane.pane_id": "string", "panes.0.x": "number"}});
    let good = json!({"ok": true, "result": {"type": "pane_info", "pane": {"pane_id": "w1:p1"}, "panes": [{"x": 1}]}});
    assert!(shape_errors(&expect, &good).is_empty());
    let bad =
        json!({"ok": true, "result": {"type": "pane_list", "pane": {"pane_id": 3}, "panes": []}});
    assert_eq!(shape_errors(&expect, &bad).len(), 3);
    let err = json!({"ok": false, "error": {"code": "pane_not_found"}});
    assert_eq!(shape_errors(&expect, &err).len(), 1);
    let want_err = json!({"ok": false, "code": "pane_not_found"});
    assert!(shape_errors(&want_err, &err).is_empty());
    let other = json!({"ok": false, "error": {"code": "invalid_params"}});
    assert_eq!(shape_errors(&want_err, &other).len(), 1);
}

#[test]
fn fixtures_are_well_formed() {
    let all = load_all();
    assert!(all.len() >= 6, "spec 18 §13 fixtures are missing");
    assert!(all.iter().any(|f| f.transport == "socket"));
    assert!(all.iter().any(|f| f.transport == "cli"));
    assert!(all.iter().any(|f| f.expect["ok"] == false));
}

#[test]
fn herdr_calls_from_a_plugin_keep_their_shape() {
    let fixtures = load_all();
    let base = tempfile::Builder::new()
        .prefix("vkfx")
        .tempdir_in("/tmp")
        .unwrap();
    let b = base.path().canonicalize().unwrap();
    let mut repos = BTreeMap::new();
    for fx in &fixtures {
        if !repos.contains_key(&fx.plugin) {
            repos.insert(fx.plugin.clone(), publish(&b, &fx.plugin));
        }
    }
    let base_url = format!("file://{}", b.join("srv").display());
    let s_ = session_with_env(
        "",
        &[
            ("VIBEKE_TEST_HOOKS", "1"),
            ("VIBEKE_PLUGIN_GIT_BASE", &base_url),
        ],
    );
    for (id, repo) in &repos {
        s_.json(&["plugin", "install", &format!("fixtures/{repo}")]);
        s_.json(&["plugin", "trust", id, "--legacy"]);
        assert_eq!(s_.plugin(id)["status"], "active");
    }
    let cwd = s_.path("");
    let cwd = cwd.to_str().unwrap();
    let mut failures = Vec::new();
    for fx in &fixtures {
        let vars = initial(&s_, fx, cwd);
        let state = s_.path(&format!("state/plugins/state/{}", fx.plugin));
        std::fs::create_dir_all(&state).unwrap();
        write(&state.join("transport"), &fx.transport);
        let request = json!({
            "id": fx.name,
            "method": fx.method,
            "params": serde_json::from_str::<Value>(&fill(&fx.params.to_string(), &vars)).unwrap(),
        });
        write(&state.join("request"), &format!("{request}\n"));
        let argv: String = fx
            .args
            .iter()
            .map(|a| format!("{}\n", fill(a, &vars)))
            .collect();
        write(&state.join("argv"), &argv);
        s_.run(&fx.plugin, "call");
        let logs = s_.settle(&fx.plugin);
        let last = logs.last().unwrap();
        if last["status"] != "succeeded" {
            failures.push(format!("{}: the invocation failed: {last}", fx.name));
            continue;
        }
        match answer(fx, &state) {
            Err(e) => failures.push(format!("{}: {e}", fx.name)),
            Ok(got) => {
                for e in shape_errors(&fx.expect, &got) {
                    failures.push(format!("{} ({}): {e}\n  {got}", fx.name, fx.transport));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
