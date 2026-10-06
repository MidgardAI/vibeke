//! Handoff end to end (spec 16 §15.2): export from one clone, carry the bundle, import into another
//! clone of the same origin, resume a (fake) Claude session there.

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

/// Fake server: answers the calls handoff makes; records agent.start params.
fn fake_server(
    path: PathBuf,
    src: PathBuf,
    dst: PathBuf,
    transcript: PathBuf,
    started: Arc<Mutex<Option<Value>>>,
) {
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (src, dst, transcript, started) = (
                src.clone(),
                dst.clone(),
                transcript.clone(),
                started.clone(),
            );
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let p = req["params"].clone();
                    let result = match req["method"].as_str().unwrap() {
                        "client.hello" => json!({"capabilities": ["*"]}),
                        "pane.get" => {
                            json!({"pane": {"id": "p1", "workspace": "w1"}, "cwd": src.join("app"),
                            "run": {"id": "r1", "harness": "claude", "execution": {"value": "Idle"},
                                    "harness_session_id": "sess1", "transcript_path": transcript,
                                    "resume_argv": ["claude", "--resume", "sess1"], "last_message": "done"}})
                        }
                        "session.snapshot" => {
                            json!({"workspaces": [{"id": "w9", "root_path": dst}]})
                        }
                        "workspace.create" => {
                            json!({"workspace": {"id": "w2"}, "root_pane": {"id": "p2"}})
                        }
                        "agent.start" => {
                            assert!(p.get("actor").is_some(), "gateway mutations carry an actor");
                            *started.lock().unwrap() = Some(p.clone());
                            json!({"run": {"id": "r2"}})
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

#[tokio::test(flavor = "multi_thread")]
async fn export_carry_import_resume() {
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
    let started = Arc::new(Mutex::new(None));
    fake_server(
        sock.clone(),
        src.clone(),
        dst.clone(),
        transcript.clone(),
        started.clone(),
    );
    let gw = Gateway::new(
        StateDir::open(root.join("gw")).unwrap(),
        server::Server::new(sock),
    )
    .unwrap();
    let dev = Device {
        id: "d1".into(),
        name: "phone".into(),
        platform: "test".into(),
        public: "k".into(),
        scope: Scope::Full,
        paired_at: 0,
        vapid_private: None,
        push: vec![],
        prefs: Default::default(),
        push_failures: 0,
        kind: "device".into(),
        expires_at: None,
        limit: None,
    };
    gw.add_device(dev.clone()).unwrap();

    // Export.
    let ex = handoff::dispatch(&gw, &dev, "handoff.export", &json!({"pane": "p1"}))
        .await
        .unwrap();
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

    // Carry it in chunks, as the app does.
    let size = ex["size"].as_u64().unwrap();
    let begin = handoff::dispatch(
        &gw,
        &dev,
        "handoff.begin",
        &json!({"manifest": m, "size": size, "sha256": ex["sha256"]}),
    )
    .await
    .unwrap();
    let mut offset = 0u64;
    while offset < size {
        let chunk = handoff::dispatch(
            &gw,
            &dev,
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
            &gw,
            &dev,
            "handoff.write",
            &json!({"id": begin["id"], "offset": offset, "data_b64": data}),
        )
        .await
        .unwrap();
        offset += n;
    }

    // Import: the destination clone is found by origin URL.
    let r = handoff::dispatch(&gw, &dev, "handoff.finish", &json!({"id": begin["id"]}))
        .await
        .unwrap();
    let wt = PathBuf::from(r["worktree"].as_str().unwrap());
    assert_eq!(r["branch"], "handoff/feature");
    assert_eq!(r["resumed"], true);
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

    // The agent resumes with the original session.
    let started = started.lock().unwrap().clone().unwrap();
    assert_eq!(started["pane"], "p2");
    assert_eq!(started["harness"], "claude");
    assert_eq!(started["args"], json!(["--resume", "sess1"]));
    assert_eq!(started["actor"], "gateway:phone");

    // The source is untouched.
    assert_eq!(
        std::fs::read_to_string(src.join("app/main.txt")).unwrap(),
        "v1\nfeature\nuncommitted\n"
    );
}
