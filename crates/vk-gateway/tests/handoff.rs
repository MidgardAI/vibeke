//! Handoff end to end (spec 16 §15.2): export from one clone, carry the bundle, deliver it to the
//! destination's server as an incoming handoff, import it into another clone of the same origin
//! and check the (fake) Claude session would resume there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use vk_gateway::state::{Device, Scope, StateDir};
use vk_gateway::{Gateway, handoff, server};

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} in {}", dir.display());
}

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Fake server: answers the calls handoff makes and records every call. `handoff.incoming.add`
/// keeps a copy of the bundle (as the real server does) at `<root>/received-<n>.tar.zst`.
fn fake_server(path: PathBuf, root: PathBuf, src: PathBuf, transcript: PathBuf, calls: Calls) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (root, src, transcript, calls) =
                (root.clone(), src.clone(), transcript.clone(), calls.clone());
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let p = req["params"].clone();
                    let method = req["method"].as_str().unwrap().to_string();
                    let n = {
                        let mut c = calls.lock().unwrap();
                        c.push((method.clone(), p.clone()));
                        c.len()
                    };
                    let result = match method.as_str() {
                        "client.hello" => json!({"capabilities": ["*"]}),
                        "pane.get" => {
                            json!({"pane": {"id": "p1", "workspace": "w1"}, "cwd": src.join("app"),
                            "run": {"id": "r1", "harness": "claude", "execution": {"value": "Idle"},
                                    "harness_session_id": "sess1", "transcript_path": transcript,
                                    "resume_argv": ["claude", "--resume", "sess1"], "last_message": "done"}})
                        }
                        "handoff.incoming.add" => {
                            assert!(p.get("actor").is_some(), "gateway mutations carry an actor");
                            let from = PathBuf::from(p["path"].as_str().unwrap());
                            std::fs::copy(&from, root.join(format!("received-{n}.tar.zst")))
                                .unwrap();
                            json!({"incoming": {"id": format!("in{n}"), "state": "pending", "result": null}})
                        }
                        "handoff.accept" => {
                            assert!(p.get("actor").is_some(), "gateway mutations carry an actor");
                            json!({"incoming": {"id": p["id"], "state": "imported",
                                   "result": {"worktree": "/x/repo-handoff-feature", "branch": "handoff/feature"}}})
                        }
                        _ => json!({}),
                    };
                    let out = json!({"jsonrpc": "2.0", "id": req["id"], "result": result})
                        .to_string()
                        + "\n";
                    if w.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn device(id: &str, name: &str, kind: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
        platform: "test".into(),
        public: format!("k-{id}"),
        scope: Scope::Full,
        paired_at: 0,
        vapid_private: None,
        push: vec![],
        prefs: Default::default(),
        push_failures: 0,
        kind: kind.into(),
        expires_at: None,
        limit: None,
    }
}

/// Export the pane and carry the bundle to `to` in chunks, as the app does. Returns the export and
/// the destination's handoff id.
async fn carry(gw: &Arc<Gateway>, from: &Device, to: &Device) -> (Value, Value) {
    let ex = handoff::dispatch(gw, from, "handoff.export", &json!({"pane": "p1"}))
        .await
        .unwrap();
    let size = ex["size"].as_u64().unwrap();
    let begin = handoff::dispatch(
        gw,
        to,
        "handoff.begin",
        &json!({"manifest": ex["manifest"], "size": size, "sha256": ex["sha256"]}),
    )
    .await
    .unwrap();
    let mut offset = 0u64;
    while offset < size {
        let chunk = handoff::dispatch(
            gw,
            from,
            "handoff.read",
            &json!({"id": ex["id"], "offset": offset, "len": 1000}),
        )
        .await
        .unwrap();
        let data = chunk["data_b64"].as_str().unwrap().to_string();
        let n = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &data)
            .unwrap()
            .len() as u64;
        handoff::dispatch(
            gw,
            to,
            "handoff.write",
            &json!({"id": begin["id"], "offset": offset, "data_b64": data}),
        )
        .await
        .unwrap();
        offset += n;
    }
    (ex, begin["id"].clone())
}

fn calls_of(calls: &Calls, method: &str) -> Vec<Value> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == method)
        .map(|(_, p)| p.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn export_carry_deliver_import_resume() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let origin = root.join("origin.git");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);

    // Source clone: a pushed main, a local feature branch with one unpushed commit, uncommitted
    // changes, an untracked file and a secret.
    let src = root.join("src");
    git(&root, &["clone", "-q", origin.to_str().unwrap(), "src"]);
    std::fs::create_dir_all(src.join("app")).unwrap();
    std::fs::write(src.join("app/main.txt"), "v1\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "base"]);
    git(&src, &["push", "-q", "origin", "HEAD:main"]);
    git(&src, &["checkout", "-qb", "feature"]);
    std::fs::write(src.join("app/main.txt"), "v1\nfeature\n").unwrap();
    git(&src, &["commit", "-qam", "feature work"]);
    std::fs::write(src.join("app/main.txt"), "v1\nfeature\nuncommitted\n").unwrap();
    std::fs::write(src.join("app/notes.md"), "untracked notes\n").unwrap();
    std::fs::write(src.join(".env"), "TOKEN=hunter2\n").unwrap();

    // Destination clone (only knows main).
    let dst = root.join("dst");
    git(&root, &["clone", "-q", origin.to_str().unwrap(), "dst"]);

    // A Claude transcript mentioning the source path and a secret.
    let claude_src = root.join("claude-src/projects/-src-app");
    std::fs::create_dir_all(&claude_src).unwrap();
    let transcript = claude_src.join("sess1.jsonl");
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            json!({"type": "user", "cwd": src.join("app"), "message": "hi"}),
            json!({"type": "assistant", "message": "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz0123456789"})
        ),
    )
    .unwrap();
    let claude_dst = root.join("claude-dst");
    // SAFETY: single-threaded setup before the gateway runs; only this test reads it.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &claude_dst) };

    let sock = root.join("s.sock");
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    fake_server(
        sock.clone(),
        root.clone(),
        src.clone(),
        transcript.clone(),
        calls.clone(),
    );
    let gw = Gateway::new(
        StateDir::open(root.join("gw")).unwrap(),
        server::Server::new(sock),
    )
    .unwrap();
    let dev = device("d1", "phone", "device");
    gw.add_device(dev.clone()).unwrap();

    // Export, carry, finish: the bundle is delivered to the server as a pending incoming handoff.
    let (ex, dest_id) = carry(&gw, &dev, &dev).await;
    let m = &ex["manifest"];
    assert_eq!(m["branch"], "feature");
    assert_eq!(m["bundle"], "thin");
    assert_eq!(m["cwd_rel"], "app");
    assert_eq!(m["untracked"], json!(["app/notes.md"]));
    assert!(
        m["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == ".env" && s["reason"] == "secret")
    );
    assert_eq!(m["redactions"], 1);
    let r = handoff::dispatch(&gw, &dev, "handoff.finish", &json!({"id": dest_id}))
        .await
        .unwrap();
    assert_eq!(r["state"], "pending");
    let incoming = r["incoming"].as_str().unwrap().to_string();
    let adds = calls_of(&calls, "handoff.incoming.add");
    assert_eq!(adds.len(), 1);
    let add = &adds[0];
    assert_eq!(add["sha256"], ex["sha256"]);
    assert_eq!(&add["manifest"], m);
    assert_eq!(add["from"]["owner"], "self");
    assert_eq!(add["from"]["host"], m["source_host"]);
    assert_eq!(add["actor"], "gateway:phone");
    assert!(
        !Path::new(add["path"].as_str().unwrap()).exists(),
        "the gateway drops its copy once the server has the bundle"
    );
    assert!(calls_of(&calls, "handoff.accept").is_empty());
    assert!(
        calls_of(&calls, "agent.start").is_empty(),
        "the gateway no longer imports or starts agents"
    );
    // The finished transfer is gone.
    assert!(
        handoff::dispatch(&gw, &dev, "handoff.finish", &json!({"id": dest_id}))
            .await
            .is_err()
    );

    // The server's copy imports into the destination clone (what `handoff.accept` runs).
    let n: usize = incoming.trim_start_matches("in").parse().unwrap();
    let received = root.join(format!("received-{n}.tar.zst"));
    let work = root.join("work");
    std::fs::create_dir(&work).unwrap();
    let packed = vk_handoff::unpack(&received, &work).unwrap();
    let reviewed: vk_handoff::Manifest = serde_json::from_value(m.clone()).unwrap();
    vk_handoff::verify(&packed, &reviewed).unwrap();
    let imported = vk_handoff::import(&work, &packed, &dst, None, None)
        .await
        .unwrap();
    let wt = imported.worktree.clone();
    assert_eq!(imported.branch, "handoff/feature");
    assert!(imported.resumed);
    assert_eq!(
        imported.resume_args,
        Some(vec!["--resume".to_string(), "sess1".to_string()])
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("app/main.txt")).unwrap(),
        "v1\nfeature\nuncommitted\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("app/notes.md")).unwrap(),
        "untracked notes\n"
    );
    assert!(!wt.join(".env").exists(), "secrets never travel");

    // Transcript installed for the new cwd, paths rewritten, secret redacted.
    let new_cwd = wt.join("app");
    let installed = claude_dst
        .join("projects")
        .join(handoff::claude_project_dir(&new_cwd))
        .join("sess1.jsonl");
    let text = std::fs::read_to_string(&installed).unwrap();
    assert!(text.contains(&new_cwd.display().to_string()));
    assert!(!text.contains(&src.join("app").display().to_string()));
    assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxyz"));

    // The source is untouched.
    assert_eq!(
        std::fs::read_to_string(src.join("app/main.txt")).unwrap(),
        "v1\nfeature\nuncommitted\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn teammates_deliver_and_own_devices_may_place() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let src = root.join("src");
    std::fs::create_dir_all(src.join("app")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("app/a.txt"), "a\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "base"]);
    let dst = root.join("dst");
    std::fs::create_dir(&dst).unwrap();

    let sock = root.join("s.sock");
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    fake_server(
        sock.clone(),
        root.clone(),
        src.clone(),
        root.join("no-transcript.jsonl"),
        calls.clone(),
    );
    let gw = Gateway::new(
        StateDir::open(root.join("gw")).unwrap(),
        server::Server::new(sock),
    )
    .unwrap();
    let me = device("d1", "laptop", "device");
    let mate = device("d2", "invite", "handoff");
    gw.add_device(me.clone()).unwrap();
    gw.add_device(mate.clone()).unwrap();

    // A teammate's invitation never chooses where work lands, and never needs to: the handoff
    // waits as pending on this host.
    let (_, id) = carry(&gw, &me, &mate).await;
    let e = handoff::dispatch(
        &gw,
        &mate,
        "handoff.finish",
        &json!({"id": id, "repo_path": dst}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, "forbidden");
    let r = handoff::dispatch(&gw, &mate, "handoff.finish", &json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(r["state"], "pending");
    let adds = calls_of(&calls, "handoff.incoming.add");
    assert_eq!(adds.last().unwrap()["from"]["owner"], "teammate");
    assert_eq!(adds.last().unwrap()["actor"], "gateway:invite");
    assert!(calls_of(&calls, "handoff.accept").is_empty());

    // The owner's own device may still place it right away: the gateway accepts on its behalf.
    let (_, id) = carry(&gw, &me, &me).await;
    let r = handoff::dispatch(
        &gw,
        &me,
        "handoff.finish",
        &json!({"id": id, "repo_path": dst, "branch": "me/topic", "start_agent": false}),
    )
    .await
    .unwrap();
    assert_eq!(r["state"], "imported");
    assert_eq!(r["result"]["branch"], "handoff/feature");
    let accepts = calls_of(&calls, "handoff.accept");
    assert_eq!(accepts.len(), 1);
    let a = &accepts[0];
    assert_eq!(a["id"], r["incoming"]);
    assert_eq!(a["repo"]["path"], json!(dst));
    assert_eq!(a["branch"], "me/topic");
    assert_eq!(a["start_agent"], false);
    assert_eq!(a["actor"], "gateway:laptop");

    // Incoming handoffs are forwarded to the server for full devices.
    handoff::dispatch(&gw, &me, "handoff.incoming.list", &json!({"junk": 1}))
        .await
        .unwrap();
    handoff::dispatch(
        &gw,
        &me,
        "handoff.decline",
        &json!({"id": "in9", "extra": true}),
    )
    .await
    .unwrap();
    let lists = calls_of(&calls, "handoff.incoming.list");
    assert_eq!(
        lists.last().unwrap(),
        &json!({}),
        "only known params travel"
    );
    let declines = calls_of(&calls, "handoff.decline");
    assert_eq!(declines[0]["id"], "in9");
    assert!(declines[0].get("extra").is_none());
    assert_eq!(declines[0]["actor"], "gateway:laptop");
}
