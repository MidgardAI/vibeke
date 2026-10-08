//! Native plugin runtime tests (fake plugins only: `sh` scripts in temp dirs; no network, no
//! real configuration, the registry lives in the test's temp dir).

use super::tokens::{TokenInfo, TokenKind};
use super::*;
use crate::ServerOpts;
use crate::api::dispatch;
use crate::paths::Paths;
use std::path::Path;
use std::time::Duration;
use vk_compat::native::caps::Capabilities;

fn server() -> (tempfile::TempDir, Arc<Server>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let opts = ServerOpts {
        session: "t".into(),
        machine: "m".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
        gateway: None,
    };
    let s = Server::new(paths, opts).unwrap();
    set_dirs(
        &s,
        PluginDirs {
            registry: root.join("cfg/plugins.json"),
            checkouts: root.join("pstate/checkouts"),
            config: root.join("cfg/plugins"),
            state: root.join("pstate/state"),
        },
    );
    (dir, s)
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

/// Write a plugin dir with `manifest_rest` after the id/version lines and link + consent it.
fn linked(s: &Server, dir: &Path, manifest_rest: &str, files: &[(&str, &str)]) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("vibeke-plugin.toml"),
        format!("id = \"acme.fake\"\nversion = \"1.0.0\"\n{manifest_rest}"),
    )
    .unwrap();
    for (n, body) in files {
        std::fs::write(dir.join(n), body).unwrap();
    }
    let d = dirs(s);
    Registry::update(&d, |r| r.native_link(dir).map(|_| ())).unwrap();
    Registry::update(&d, |r| r.native_consent("acme.fake", None).map(|_| ())).unwrap();
    registry_changed(s);
    "acme.fake".into()
}

fn token_ctx(s: &Server, caps: Capabilities, kind: TokenKind) -> Ctx {
    let consent = registry(s).native["acme.fake"]
        .consent
        .as_ref()
        .unwrap()
        .consent_id
        .clone();
    let (_, k) = tokens::issue(
        s,
        TokenInfo {
            plugin: "acme.fake".into(),
            consent_id: consent,
            caps,
            expires: None,
            kind,
            invocation: "test".into(),
        },
    );
    plugin_ctx("acme.fake", &k)
}

fn events(s: &Server, ty: &str) -> Vec<vk_store::Event> {
    s.with_core(|c| c.store.events_after(0, 10_000, &[]))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == ty)
        .collect()
}

async fn until(mut f: impl FnMut() -> bool, what: &str) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn capabilities_are_enforced_before_dispatch() {
    let (t, s) = server();
    linked(
        &s,
        &t.path().join("p"),
        "[capabilities]\nstorage = true\nevents_read = [\"notes.*\"]\n",
        &[],
    );
    let caps = registry(&s).native["acme.fake"]
        .consent
        .clone()
        .unwrap()
        .capabilities;
    let ctx = token_ctx(&s, caps, TokenKind::Process);
    // Read-only structure is open, writes need their capability.
    assert!(
        dispatch(&s, &ctx, "workspace.list", &json!({}))
            .await
            .is_ok()
    );
    let e = dispatch(
        &s,
        &ctx,
        "pane.send_text",
        &json!({"pane": "x", "text": "hi"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "permission_denied");
    assert!(e.message.contains("panes_write"), "{}", e.message);
    assert_eq!(events(&s, "plugin.capability_violation").len(), 1);
    // Administrative methods are never available.
    for m in ["server.stop", "plugin.install", "config.set", "policy.add"] {
        assert!(dispatch(&s, &ctx, m, &json!({})).await.is_err(), "{m}");
    }
    // Events need explicit types within events_read.
    assert!(dispatch(&s, &ctx, "events.read", &json!({})).await.is_err());
    assert!(
        dispatch(
            &s,
            &ctx,
            "events.read",
            &json!({"types": ["notes.updated"]})
        )
        .await
        .is_ok()
    );
    // KV works and is namespaced; mutating calls are audited as plugin.api_call.
    dispatch(
        &s,
        &ctx,
        "plugin.kv.set",
        &json!({"key": "k", "value": {"a": 1}}),
    )
    .await
    .unwrap();
    let v = dispatch(&s, &ctx, "plugin.kv.get", &json!({"key": "k"}))
        .await
        .unwrap();
    assert_eq!(v["value"]["a"], 1);
    assert!(!events(&s, "plugin.api_call").is_empty());
    // A user (no plugin identity) cannot use plugin-only methods.
    assert!(
        dispatch(&s, &user(), "plugin.kv.get", &json!({"key": "k"}))
            .await
            .is_err()
    );
    // Disabling the plugin cuts the token off at once.
    let d = dirs(&s);
    Registry::update(&d, |r| r.native_set_enabled("acme.fake", false).map(|_| ())).unwrap();
    registry_changed(&s);
    let e = dispatch(&s, &ctx, "workspace.list", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "permission_denied");
}

#[tokio::test]
async fn kv_quota_and_value_limit() {
    let (t, s) = server();
    linked(
        &s,
        &t.path().join("p"),
        "[capabilities]\nstorage = true\n",
        &[],
    );
    let ctx = token_ctx(
        &s,
        Capabilities {
            storage: true,
            ..Default::default()
        },
        TokenKind::Action,
    );
    let big = "x".repeat(vk_store::PLUGIN_KV_MAX_VALUE + 10);
    let e = dispatch(
        &s,
        &ctx,
        "plugin.kv.set",
        &json!({"key": "big", "value": big}),
    )
    .await
    .unwrap_err();
    assert!(e.message.contains("1 MiB"), "{}", e.message);
    for k in ["a/1", "a/2", "b/1"] {
        dispatch(
            &s,
            &ctx,
            "plugin.kv.set",
            &json!({"key": k, "value_b64": "AAEC"}),
        )
        .await
        .unwrap();
    }
    let l = dispatch(
        &s,
        &ctx,
        "plugin.kv.list",
        &json!({"prefix": "a/", "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(l["keys"].as_array().unwrap().len(), 1);
    assert_eq!(l["next"], "a/1");
    let g = dispatch(&s, &ctx, "plugin.kv.get", &json!({"key": "b/1"}))
        .await
        .unwrap();
    assert_eq!(g["value_b64"], "AAEC");
    let d = dispatch(&s, &ctx, "plugin.kv.delete", &json!({"key": "b/1"}))
        .await
        .unwrap();
    assert_eq!(d["deleted"], true);
}

#[tokio::test]
async fn argv_action_waits_and_failure_notifies() {
    let (t, s) = server();
    let dir = t.path().join("p");
    linked(
        &s,
        &dir,
        "[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"go.sh\"]\n[[actions]]\nid = \"bad\"\ntitle = \"Bad\"\ncommand = [\"sh\", \"-c\", \"echo oops >&2; exit 4\"]\n",
        &[(
            "go.sh",
            "echo \"id=$VIBEKE_PLUGIN_ID\"; case \"$VIBEKE_PLUGIN_TOKEN\" in vkp_*) echo token-ok;; esac; test -z \"$VIBEKE_PANE_TOKEN\" && echo no-pane-token\n",
        )],
    );
    let r = dispatch(
        &s,
        &user(),
        "plugin.action",
        &json!({"plugin": "acme.fake", "action": "go"}),
    )
    .await
    .unwrap();
    assert_eq!(r["exit_code"], 0, "{r}");
    let out = r["stdout_tail"].as_str().unwrap();
    assert!(
        out.contains("id=acme.fake") && out.contains("token-ok") && out.contains("no-pane-token"),
        "{out}"
    );
    let r = dispatch(
        &s,
        &user(),
        "plugin.action",
        &json!({"action": "acme.fake.bad"}),
    )
    .await
    .unwrap();
    assert_eq!(r["exit_code"], 4);
    assert!(r["log"]["stderr_tail"].as_str().unwrap().contains("oops"));
    assert!(s.with_core(|c| c.notifications.iter().any(|n| n.kind == "plugin")));
    assert_eq!(events(&s, "plugin.action_invoked").len(), 2);
    assert_eq!(events(&s, "plugin.command_finished").len(), 2);
    // Records are persisted per session and listed with the Herdr ones.
    let logs = dispatch(
        &s,
        &user(),
        "plugin.log.list",
        &json!({"plugin": "acme.fake"}),
    )
    .await
    .unwrap();
    assert!(logs["logs"].as_array().unwrap().len() >= 2);
    assert_eq!(
        s.with_core(|c| c.store.plugin_commands(Some("acme.fake"), 10))
            .unwrap()
            .len(),
        2
    );
    // Native actions appear in the palette list; a pane may not run them.
    let l = dispatch(&s, &user(), "plugin.action.list", &json!({}))
        .await
        .unwrap();
    assert!(
        l["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["qualified_id"] == "acme.fake.go")
    );
    let pane = Ctx {
        pane_scope: Some("p1".into()),
        ..user()
    };
    assert!(
        dispatch(
            &s,
            &pane,
            "plugin.action",
            &json!({"plugin": "acme.fake", "action": "go"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn hooks_get_the_event_on_stdin() {
    let (t, s) = server();
    let dir = t.path().join("p");
    linked(
        &s,
        &dir,
        "[[on]]\nevent = \"notes.updated\"\ncommand = [\"sh\", \"-c\", \"cat > \\\"$VIBEKE_PLUGIN_DATA_DIR/ev.json\\\"\"]\n[capabilities]\nevents_read = [\"notes.*\"]\n",
        &[],
    );
    let srv = s.clone();
    tokio::spawn(async move { actions::hook_dispatcher(srv).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = crate::core::Tx::new();
        tx.event(
            "notes.updated",
            json!({"workspace": "w"}),
            json!({"rev": 7, "bytes": 0}),
        );
        s.commit(&mut c, tx).unwrap();
    }
    let f = dirs(&s).state_dir("acme.fake").join("ev.json");
    until(
        || std::fs::read_to_string(&f).is_ok_and(|t| t.contains("\"rev\":7")),
        "hook output",
    )
    .await;
}

const FAKE_PROCESS: &str = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"contributions":[{"kind":"status_segment","id":"ci","text":"CI ok\u001b[31m"},{"kind":"palette_command","id":"p","title":"Ping","action":"ping"}]}}'
printf '%s\n' '{"jsonrpc":"2.0","id":"k1","method":"plugin.kv.set","params":{"key":"a","value":1}}'
while read l; do
  case "$l" in
    *plugin.shutdown*) exit 0;;
    *'"id":"k1"'*) printf '%s\n' "$l" > "$VIBEKE_PLUGIN_DATA_DIR/kv-reply";;
    *'"method":"plugin.action"'*) id=$(printf '%s' "$l" | sed 's/.*"id":\([0-9]*\).*/\1/'); printf '{"jsonrpc":"2.0","id":%s,"result":{"pong":true}}\n' "$id";;
  esac
done
"#;

#[tokio::test]
async fn process_plugin_lifecycle_contributions_and_stdio_api() {
    let (t, s) = server();
    let dir = t.path().join("p");
    linked(
        &s,
        &dir,
        "[[actions]]\nid = \"ping\"\ntitle = \"Ping\"\n[process]\ncommand = [\"sh\", \"main.sh\"]\nrestart = \"never\"\n[capabilities]\nstorage = true\nui = [\"status_segment\", \"palette\"]\n",
        &[("main.sh", FAKE_PROCESS)],
    );
    process::ensure(&s);
    until(
        || {
            ui::list(&s)["contributions"]
                .as_array()
                .is_some_and(|a| a.len() == 2)
        },
        "initialize contributions",
    )
    .await;
    let c = ui::list(&s);
    let seg = c["contributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "status_segment")
        .unwrap()
        .clone();
    assert_eq!(seg["text"], "CI ok[31m", "escape codes stripped");
    // The plugin's own API call over stdio went through its identity.
    let reply = dirs(&s).state_dir("acme.fake").join("kv-reply");
    until(|| reply.is_file(), "kv reply").await;
    assert!(
        std::fs::read_to_string(&reply)
            .unwrap()
            .contains("\"result\"")
    );
    let ctx = token_ctx(
        &s,
        Capabilities {
            storage: true,
            ..Default::default()
        },
        TokenKind::Action,
    );
    let v = dispatch(&s, &ctx, "plugin.kv.get", &json!({"key": "a"}))
        .await
        .unwrap();
    assert_eq!(v["value"], 1);
    // An action without a command is sent to the process; contributed palette commands run it.
    let r = dispatch(
        &s,
        &user(),
        "plugin.action",
        &json!({"plugin": "acme.fake", "action": "p"}),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "completed", "{r}");
    let l = dispatch(&s, &user(), "plugin.list", &json!({}))
        .await
        .unwrap();
    let me = l["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "acme.fake")
        .unwrap()
        .clone();
    assert_eq!(me["process"]["state"], "running");
    assert_eq!(events(&s, "plugin.process_started").len(), 1);
    // Stop: graceful shutdown, contributions gone.
    process::stop(&s, "acme.fake", "test").await;
    assert!(ui::list(&s)["contributions"].as_array().unwrap().is_empty());
    assert_eq!(events(&s, "plugin.process_stopped").len(), 1);
}

#[tokio::test]
async fn crash_without_restart_is_reported() {
    let (t, s) = server();
    linked(
        &s,
        &t.path().join("p"),
        "[process]\ncommand = [\"sh\", \"-c\", \"echo dying >&2; exit 3\"]\nrestart = \"never\"\n",
        &[],
    );
    process::ensure(&s);
    until(|| !events(&s, "plugin.crashed").is_empty(), "crash event").await;
    let ev = &events(&s, "plugin.crashed")[0];
    assert_eq!(ev.data["exit_code"], 3);
    assert_eq!(ev.data["restart"], false);
    assert!(ev.data["stderr_tail"].as_str().unwrap().contains("dying"));
    // Crash budget: the sixth crash in the window disables.
    for i in 1..=5 {
        assert!(!process::record_crash(&s, "x.y").1, "crash {i}");
    }
    assert!(process::record_crash(&s, "x.y").1);
}

#[tokio::test]
async fn contributions_need_ui_capability_and_known_actions() {
    let (t, s) = server();
    linked(
        &s,
        &t.path().join("p"),
        "[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"true\"]\n[capabilities]\nui = [\"status_segment\"]\n",
        &[],
    );
    let caps = Capabilities {
        ui: vec!["status_segment".into()],
        ..Default::default()
    };
    let ctx = token_ctx(&s, caps, TokenKind::Process);
    let ok = dispatch(
        &s,
        &ctx,
        "ui.contribute",
        &json!({"contributions": [{"kind": "status_segment", "id": "a", "text": "x", "on_click": "go"}]}),
    )
    .await;
    assert!(ok.is_ok(), "{ok:?}");
    let e = dispatch(
        &s,
        &ctx,
        "ui.contribute",
        &json!({"contributions": [{"kind": "sidebar_section", "id": "s", "title": "S"}]}),
    )
    .await
    .unwrap_err();
    assert!(e.message.contains("capability_violation"), "{}", e.message);
    let e = dispatch(
        &s,
        &ctx,
        "ui.contribute",
        &json!({"contributions": [{"kind": "status_segment", "id": "b", "text": "x", "on_click": "nope"}]}),
    )
    .await
    .unwrap_err();
    assert!(e.message.contains("no action"), "{}", e.message);
    // Debounced announcements: a burst produces at most two events quickly.
    for i in 0..20 {
        dispatch(
            &s,
            &ctx,
            "ui.contribute",
            &json!({"contributions": [{"kind": "status_segment", "id": "a", "text": format!("n{i}")}]}),
        )
        .await
        .unwrap();
    }
    let n = events(&s, "ui.contributions_changed").len();
    assert!(n <= 3, "{n} events for a burst");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let last = ui::list(&s)["contributions"][0]["text"].clone();
    assert_eq!(last, "n19");
    // The merged UI state carries contributions for clients.
    let st = dispatch(&s, &user(), "compat.ui.state", &json!({}))
        .await
        .unwrap();
    assert!(st["contributions"].is_array());
}

#[tokio::test]
async fn install_over_the_api_needs_accepted_capabilities() {
    let (t, s) = server();
    let src = t.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("vibeke-plugin.toml"),
        "id = \"acme.inst\"\nversion = \"1.0.0\"\n[capabilities]\npanes_write = true\n",
    )
    .unwrap();
    let e = dispatch(&s, &user(), "plugin.install", &json!({"source": src}))
        .await
        .unwrap_err();
    assert!(e.message.contains("capabilities_not_accepted"));
    assert!(registry(&s).native.is_empty(), "nothing registered");
    let items = e.data.details["requested_capabilities"].clone();
    assert_eq!(items[0]["risk"], "high");
    let r = dispatch(
        &s,
        &user(),
        "plugin.install",
        &json!({"source": src, "accept_capabilities": ["panes_write"]}),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "active", "{r}");
    // Pane callers cannot install; removal works.
    let pane = Ctx {
        pane_scope: Some("p1".into()),
        ..user()
    };
    assert!(
        dispatch(&s, &pane, "plugin.install", &json!({"source": src}))
            .await
            .is_err()
    );
    dispatch(
        &s,
        &user(),
        "plugin.remove",
        &json!({"plugin": "acme.inst"}),
    )
    .await
    .unwrap();
    assert!(registry(&s).native.is_empty());
}

#[tokio::test]
async fn registry_observation_emits_plugin_events() {
    let (t, s) = server();
    observe::tick(&s).await;
    linked(&s, &t.path().join("p"), "", &[]);
    observe::tick(&s).await;
    assert_eq!(events(&s, "plugin.linked").len(), 1);
    assert!(!events(&s, "plugin.registry_observed").is_empty());
    let d = dirs(&s);
    Registry::update(&d, |r| r.native_set_enabled("acme.fake", false).map(|_| ())).unwrap();
    registry_changed(&s);
    observe::tick(&s).await;
    assert_eq!(events(&s, "plugin.disabled").len(), 1);
    Registry::update(&d, |r| r.native_revoke("acme.fake").map(|_| ())).unwrap();
    registry_changed(&s);
    observe::tick(&s).await;
    assert_eq!(events(&s, "plugin.trust_changed").len(), 1);
    let d2 = d.clone();
    Registry::update(&d, |r| r.native_remove(&d2, "acme.fake").map(|_| ())).unwrap();
    registry_changed(&s);
    observe::tick(&s).await;
    assert_eq!(events(&s, "plugin.unlinked").len(), 1);
}

#[test]
fn dev_fingerprint_follows_watched_files() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    std::fs::create_dir_all(r.join("dist")).unwrap();
    std::fs::write(r.join("vibeke-plugin.toml"), "x").unwrap();
    std::fs::write(r.join("dist/a.js"), "1").unwrap();
    std::fs::write(r.join("other.txt"), "1").unwrap();
    let watch = vec!["dist/*.js".to_string()];
    let a = observe::fingerprint(r, &[], &watch);
    std::fs::write(r.join("other.txt"), "22").unwrap();
    assert_eq!(a, observe::fingerprint(r, &[], &watch), "unwatched file");
    std::fs::write(r.join("dist/a.js"), "22").unwrap();
    assert_ne!(a, observe::fingerprint(r, &[], &watch));
}

// ---- connection identity: tokenless plugin connections, render attach ------------------------

/// A process plugin that answers `plugin.initialize`, starts a child in its process tree and
/// records the child's pid, then idles.
const IDLE_PROCESS: &str = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"contributions":[]}}'
sleep 60 &
echo $! > "$VIBEKE_PLUGIN_DATA_DIR/child.pid"
while read l; do
  case "$l" in
    *plugin.shutdown*) kill $! 2>/dev/null; exit 0;;
  esac
done
"#;

/// A raw token of `acme.fake` with `caps` (consent of the linked registration).
fn raw_token(s: &Server, caps: Capabilities) -> (String, String) {
    let consent = registry(s).native["acme.fake"]
        .consent
        .as_ref()
        .unwrap()
        .consent_id
        .clone();
    tokens::issue(
        s,
        TokenInfo {
            plugin: "acme.fake".into(),
            consent_id: consent,
            caps,
            expires: None,
            kind: TokenKind::Process,
            invocation: "test".into(),
        },
    )
}

/// One socket connection as `peer_pid`, line-oriented.
struct Conn {
    rd: tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    wr: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Conn {
    fn open(s: &Arc<Server>, peer_pid: Option<i32>) -> Conn {
        let (client, server_end) = tokio::io::duplex(1 << 20);
        let task = tokio::spawn(crate::run::connection(s.clone(), server_end, peer_pid));
        let (rd, wr) = tokio::io::split(client);
        Conn {
            rd: tokio::io::BufReader::new(rd),
            wr,
            task,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        self.wr
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.rd.read_line(&mut line))
            .await
            .expect("reply")
            .unwrap();
        serde_json::from_str(&line).unwrap_or(Value::Null)
    }
}

fn denied_kind(v: &Value) -> (&str, &str) {
    (
        v["error"]["data"]["kind"].as_str().unwrap_or(""),
        v["error"]["message"].as_str().unwrap_or(""),
    )
}

/// A plugin (sandboxed where the OS sandbox works) with no write capabilities: a fresh
/// connection from its process tree without its token gets nothing — not `pane.run`, not
/// `interaction.answer`, not even reads — and holds no pane or elevated identity. With its token
/// it has exactly its capabilities, revocation closes that connection, and afterwards a
/// tokenless connection is still refused.
#[tokio::test(flavor = "multi_thread")]
async fn tokenless_connection_from_plugin_tree_is_refused() {
    let (t, s) = server();
    let sandbox = cfg!(target_os = "macos") && vk_sandbox::seatbelt::available();
    linked(
        &s,
        &t.path().join("p"),
        &format!(
            "sandbox = {sandbox}\n[process]\ncommand = [\"sh\", \"main.sh\"]\nrestart = \"never\"\n[capabilities]\nstorage = true\n"
        ),
        &[("main.sh", IDLE_PROCESS)],
    );
    process::ensure(&s);
    let pidfile = dirs(&s).state_dir("acme.fake").join("child.pid");
    for _ in 0..200 {
        if std::fs::read_to_string(&pidfile).is_ok_and(|p| p.trim().parse::<i32>().is_ok()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ev: Vec<_> = events(&s, "plugin.crashed")
        .into_iter()
        .chain(events(&s, "plugin.launch_failed"))
        .map(|e| e.data)
        .collect();
    assert!(pidfile.is_file(), "plugin child pid: {ev:?}");
    let child: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let plugin_pid = state(&s).procs.lock().unwrap()["acme.fake"]
        .pid
        .lock()
        .unwrap()
        .unwrap() as i32;
    assert_eq!(
        peer_plugin(&s, Some(child)).as_deref(),
        Some("acme.fake"),
        "child of the plugin process"
    );
    assert_eq!(
        peer_plugin(&s, Some(plugin_pid)).as_deref(),
        Some("acme.fake")
    );
    assert_eq!(peer_plugin(&s, Some(std::process::id() as i32)), None);

    for peer in [plugin_pid, child] {
        let mut c = Conn::open(&s, Some(peer));
        for (m, p) in [
            (
                "pane.run",
                json!({"pane": "x", "command": "touch /tmp/pwned"}),
            ),
            (
                "interaction.answer",
                json!({"interaction": "x", "answer": "yes"}),
            ),
            ("workspace.list", json!({})),
        ] {
            let v = c.call(m, p).await;
            let (kind, msg) = denied_kind(&v);
            assert_eq!(kind, "permission_denied", "{m}: {v}");
            assert!(msg.contains("plugin_token_required"), "{m}: {msg}");
        }
        // No other identity: an unknown token is refused at hello.
        let v = c.call("client.hello", json!({"token": "vkp_nope"})).await;
        assert_eq!(denied_kind(&v).0, "permission_denied", "{v}");
        drop(c);
    }

    // With its token: its own capabilities only, and revocation closes the connection.
    let (tok, _) = raw_token(
        &s,
        Capabilities {
            storage: true,
            ..Default::default()
        },
    );
    let mut c = Conn::open(&s, Some(child));
    let v = c.call("client.hello", json!({"token": tok})).await;
    assert!(v.get("result").is_some(), "{v}");
    let v = c.call("workspace.list", json!({})).await;
    assert!(v.get("result").is_some(), "{v}");
    let v = c
        .call("pane.run", json!({"pane": "x", "command": "true"}))
        .await;
    let (kind, msg) = denied_kind(&v);
    assert_eq!(kind, "permission_denied");
    assert!(msg.contains("panes_write"), "{msg}");
    tokens::revoke_plugin(&s, "acme.fake");
    let ended = tokio::time::timeout(Duration::from_secs(5), &mut c.task).await;
    assert!(ended.is_ok(), "revocation closes the connection");

    // After revocation a fresh tokenless connection from the tree is still refused.
    let mut c = Conn::open(&s, Some(child));
    let v = c
        .call("pane.run", json!({"pane": "x", "command": "true"}))
        .await;
    assert_eq!(denied_kind(&v).0, "permission_denied", "{v}");
    let v = c.call("client.hello", json!({"token": tok})).await;
    assert_eq!(denied_kind(&v).0, "permission_denied", "revoked token: {v}");
    drop(c);
    process::stop(&s, "acme.fake", "test").await;
}

#[test]
fn plugin_identity_in_the_environment_marks_a_plugin_peer() {
    let e = |kv: &[(&str, &str)]| {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(peer_plugin_env(&e(&[("HOME", "/h")])), None);
    assert_eq!(
        peer_plugin_env(&e(&[("VIBEKE_PLUGIN_ID", "acme.x")])).as_deref(),
        Some("acme.x")
    );
    assert_eq!(
        peer_plugin_env(&e(&[("VIBEKE_PLUGIN_TOKEN", "vkp_1")])).as_deref(),
        Some("")
    );
    assert!(kind_matches_peer("native-plugin:acme.x#ab", "acme.x"));
    assert!(kind_matches_peer("native-plugin:acme.x#ab", ""));
    assert!(!kind_matches_peer("native-plugin:acme.x#ab", "acme.y"));
}

/// `render.attach` with a plugin token is refused without the explicit `render_attach`
/// capability; with it the session keeps the plugin identity (never "tui") and revoking the
/// token ends the established stream.
#[tokio::test(flavor = "multi_thread")]
async fn render_attach_needs_capability_and_ends_on_revocation() {
    use tokio::io::AsyncReadExt;
    let (t, s) = server();
    linked(
        &s,
        &t.path().join("p"),
        "[capabilities]\npanes_read = true\nrender_attach = true\n",
        &[],
    );
    let attach = json!({"client_id": "plug", "protocol": vk_proto::render::PROTOCOL});
    // Restricted token (no render_attach): refused, recorded as a violation.
    let (tok, _) = raw_token(
        &s,
        Capabilities {
            panes_read: true,
            ..Default::default()
        },
    );
    let mut c = Conn::open(&s, None);
    let v = c.call("client.hello", json!({"token": tok})).await;
    assert!(v.get("result").is_some(), "{v}");
    let v = c.call("render.attach", attach.clone()).await;
    let (kind, msg) = denied_kind(&v);
    assert_eq!(kind, "permission_denied", "{v}");
    assert!(msg.contains("render_attach"), "{msg}");
    assert!(!events(&s, "plugin.capability_violation").is_empty());
    let _ = tokio::time::timeout(Duration::from_secs(5), c.task).await;

    // Granted: attached as the plugin; revocation ends the stream.
    let (tok, kind) = raw_token(
        &s,
        Capabilities {
            render_attach: true,
            ..Default::default()
        },
    );
    let mut c = Conn::open(&s, None);
    let v = c.call("client.hello", json!({"token": tok})).await;
    assert!(v.get("result").is_some(), "{v}");
    let v = c.call("render.attach", attach).await;
    assert!(v.get("result").is_some(), "{v}");
    let _hello: vk_proto::render::ServerFrame = vk_proto::frame::asyncio::read_frame(&mut c.rd)
        .await
        .unwrap();
    until(
        || {
            s.clients
                .lock()
                .unwrap()
                .get("plug")
                .is_some_and(|st| st.kind == kind)
        },
        "plugin identity on the render client",
    )
    .await;
    tokens::revoke_plugin(&s, "acme.fake");
    let drained = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = vec![0u8; 1 << 16];
        while c.rd.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    })
    .await;
    assert!(drained.is_ok(), "revocation ends the render stream");
    let _ = tokio::time::timeout(Duration::from_secs(5), c.task).await;
}
