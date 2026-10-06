//! Native plugins end to end (07 §7.1–7.6, 09 §6; lane 3B) against an isolated session: CLI
//! install with consent, a fake process plugin (`sh`) speaking JSON-RPC over stdio, argv actions
//! calling back with their capability-scoped token, KV, UI contributions, the registry methods
//! over the API, search against a `file://` index, export/import. Fakes only: no network, no
//! real configuration.

mod support;

use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};
use support::{Rpc, Session};

const PROCESS: &str = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"contributions":[{"kind":"status_segment","id":"ci","text":"CI ok","on_click":"ping"},{"kind":"pane","id":"tail","title":"Tail","command":["sh","-c","sleep 30"]}]}}'
while read l; do
  case "$l" in
    *plugin.shutdown*) exit 0;;
    *'"method":"plugin.action"'*) id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"jsonrpc":"2.0","id":%s,"result":{"pong":true}}\n' "$id";;
  esac
done
"#;

fn plugin_dir(
    s: &Session,
    name: &str,
    manifest: &str,
    files: &[(&str, &str)],
) -> std::path::PathBuf {
    let d = s.dir.path().join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("vibeke-plugin.toml"), manifest).unwrap();
    for (n, b) in files {
        std::fs::write(d.join(n), b).unwrap();
    }
    d
}

fn wait(mut f: impl FnMut() -> bool, what: &str) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(20) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out: {what}");
}

fn native<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["plugin_id"] == id && p["native"] == true)
        .unwrap_or(&Value::Null)
}

#[test]
fn process_plugin_actions_kv_and_ui_end_to_end() {
    let s = Session::new();
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let dir = plugin_dir(
        &s,
        "acme",
        "id = \"acme.e2e\"\nversion = \"1.0.0\"\n\
         [[actions]]\nid = \"ping\"\ntitle = \"Ping\"\n\
         [[actions]]\nid = \"store\"\ntitle = \"Store\"\ncommand = [\"sh\", \"store.sh\"]\n\
         [[actions]]\nid = \"escalate\"\ntitle = \"Escalate\"\ncommand = [\"sh\", \"escalate.sh\"]\n\
         [process]\ncommand = [\"sh\", \"main.sh\"]\n\
         [capabilities]\nstorage = true\nui = [\"status_segment\", \"pane\"]\n",
        &[
            ("main.sh", PROCESS),
            (
                "store.sh",
                &format!(
                    "{bin} --json api call plugin.kv.set '{{\"key\":\"greeting\",\"value\":\"hi\"}}' && {bin} --json api call plugin.kv.get '{{\"key\":\"greeting\"}}' && {bin} --json api call plugin.kv.list '{{}}' && {bin} --json api call plugin.kv.delete '{{\"key\":\"greeting\"}}'\n"
                ),
            ),
            (
                "escalate.sh",
                &format!(
                    "{bin} --json api call pane.send_text '{{\"pane\":\"x\",\"text\":\"rm -rf\"}}'\n"
                ),
            ),
        ],
    );
    // Install with consent (non-interactive --yes after review).
    let r = s.json(&["plugin", "install", dir.to_str().unwrap(), "--yes"]);
    assert_eq!(r["kind"], "native", "{r}");
    let l = s.json(&["plugin", "list"]);
    assert!(l.to_string().contains("acme.e2e"));
    // The server starts the process; its initialize contributions show up.
    let _ = s.json(&["server", "status"]);
    let mut rpc = Rpc::connect(&s.socket());
    wait(
        || {
            rpc.call("ui.contributions", json!({}))
                .is_ok_and(|v| v["contributions"].as_array().is_some_and(|a| a.len() == 2))
        },
        "contributions",
    );
    let list = rpc.call("plugin.list", json!({})).unwrap();
    assert_eq!(native(&list, "acme.e2e")["process"]["state"], "running");
    // An action without a command goes to the process.
    let r = rpc
        .call(
            "plugin.action",
            json!({"plugin": "acme.e2e", "action": "ping"}),
        )
        .unwrap();
    assert_eq!(r["status"], "completed", "{r}");
    // An argv action calls back with its token: KV works…
    let r = rpc
        .call(
            "plugin.action",
            json!({"plugin": "acme.e2e", "action": "store"}),
        )
        .unwrap();
    assert_eq!(r["exit_code"], 0, "{r}");
    assert!(r["stdout_tail"].as_str().unwrap().contains("\"hi\""), "{r}");
    // …but anything beyond its capabilities is refused and recorded.
    let r = rpc
        .call(
            "plugin.action",
            json!({"plugin": "acme.e2e", "action": "escalate"}),
        )
        .unwrap();
    assert_ne!(r["exit_code"], 0, "{r}");
    let ev = rpc
        .call(
            "events.read",
            json!({"types": ["plugin.capability_violation"], "after": 0}),
        )
        .unwrap();
    assert!(!ev["events"].as_array().unwrap().is_empty(), "{ev}");
    // A user client cannot use plugin-only methods.
    assert!(
        rpc.call("plugin.kv.get", json!({"key": "greeting"}))
            .is_err()
    );
    assert!(
        rpc.call("ui.contribute", json!({"contributions": []}))
            .is_err()
    );
    // The contributed pane opens next to a pane.
    let _ = s.workspace("sh");
    let r = rpc.call(
        "ui.pane.open",
        json!({"plugin": "acme.e2e", "pane": "tail"}),
    );
    assert!(r.is_ok(), "{r:?}");
    // Restart, disable (process stops, contributions go), enable, remove.
    rpc.call("plugin.restart", json!({"plugin": "acme.e2e"}))
        .unwrap();
    rpc.call("plugin.disable", json!({"plugin": "acme.e2e"}))
        .unwrap();
    wait(
        || {
            rpc.call("ui.contributions", json!({}))
                .is_ok_and(|v| v["contributions"].as_array().is_some_and(|a| a.is_empty()))
        },
        "contributions cleared",
    );
    rpc.call("plugin.enable", json!({"plugin": "acme.e2e"}))
        .unwrap();
    rpc.call("plugin.remove", json!({"plugin": "acme.e2e"}))
        .unwrap();
    s.kill_server();
}

#[test]
fn registry_over_the_api_consent_link_and_cli_tools() {
    let s = Session::new();
    let src = plugin_dir(
        &s,
        "risky",
        "id = \"acme.risky\"\nversion = \"1.0.0\"\n[capabilities]\npanes_write = true\n",
        &[],
    );
    let _ = s.json(&["server", "status"]);
    let mut rpc = Rpc::connect(&s.socket());
    let e = rpc
        .call("plugin.install", json!({"source": src}))
        .unwrap_err();
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("capabilities_not_accepted"),
        "{e}"
    );
    rpc.call(
        "plugin.install",
        json!({"source": src, "accept_capabilities": ["panes_write"]}),
    )
    .unwrap();
    // A widening edit of a linked plugin needs consent again.
    let dev = plugin_dir(&s, "dev", "id = \"acme.dev\"\nversion = \"0.1.0\"\n", &[]);
    rpc.call("plugin.link", json!({"path": dev})).unwrap();
    rpc.call(
        "plugin.consent",
        json!({"plugin": "acme.dev", "accept_capabilities": ["*"]}),
    )
    .unwrap();
    std::fs::write(
        dev.join("vibeke-plugin.toml"),
        "id = \"acme.dev\"\nversion = \"0.1.1\"\n[capabilities]\nagents_control = true\n",
    )
    .unwrap();
    let l = rpc.call("plugin.list", json!({})).unwrap();
    assert_eq!(native(&l, "acme.dev")["status"], "reconsent", "{l}");
    rpc.call(
        "plugin.consent",
        json!({"plugin": "acme.dev", "accept_capabilities": ["agents_control"]}),
    )
    .unwrap();
    let l = rpc.call("plugin.list", json!({})).unwrap();
    assert_eq!(native(&l, "acme.dev")["status"], "active");
    // Search reads a local index mirror.
    let idx = s.dir.path().join("index.json");
    std::fs::write(
        &idx,
        r#"{"plugins":[{"id":"acme.ci","repo":"acme/ci","description":"CI status","stars":5}]}"#,
    )
    .unwrap();
    let r = s.json(&[
        "plugin",
        "search",
        "ci",
        "--index",
        &format!("file://{}", idx.display()),
    ]);
    assert_eq!(r["results"][0]["id"], "acme.ci");
    // Export and import into another configuration.
    let out = s.dir.path().join("backup");
    let r = s.json(&["plugin", "export", out.to_str().unwrap()]);
    assert_eq!(r["plugins"].as_array().unwrap().len(), 2);
    assert!(Path::new(&out).join("plugins.json").is_file());
    let r = s.json(&["plugin", "import", out.to_str().unwrap(), "--dry-run"]);
    assert_eq!(
        r["conflicts"].as_array().unwrap().len(),
        2,
        "already registered here"
    );
    s.kill_server();
}
