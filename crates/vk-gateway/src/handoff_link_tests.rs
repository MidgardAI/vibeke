//! Continue links end to end against the fake account server (`vk_account::fake`) and a fake
//! Vibeke server: create → link → redeem → an incoming handoff from a teammate; a link whose
//! checksum does not match; a revoked blob; the account errors; a restart.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use vk_account::fake::FakeServer;

use super::*;
use crate::api::{Call, kind_allows, required_scope};
use crate::server;
use crate::state::{Scope, StateDir};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

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

/// A fake Vibeke server: one idle Claude pane in `cwd`; `handoff.incoming.add` keeps a copy of
/// the bundle at `<root>/received-<n>.tar.zst` and leaves the handoff pending. Every call is
/// recorded.
fn fake_vibeke(sock: PathBuf, root: PathBuf, cwd: PathBuf, calls: Calls) {
    let listener = UnixListener::bind(&sock).unwrap();
    let n = Arc::new(Mutex::new(0usize));
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (calls, root, cwd, n) = (calls.clone(), root.clone(), cwd.clone(), n.clone());
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap().to_string();
                    let p = req["params"].clone();
                    calls.lock().unwrap().push((method.clone(), p.clone()));
                    let result = match method.as_str() {
                        "client.hello" => {
                            json!({"server_version": "test", "capabilities": ["*"]})
                        }
                        "events.subscribe" => json!({"subscription_id": "s", "at": {"seq": 1}}),
                        "pane.get" => json!({
                            "pane": {"id": "p1", "workspace": "w1", "title": "Login form"},
                            "cwd": cwd,
                            "run": {"id": "r1", "harness": "claude",
                                    "execution": {"value": "idle"},
                                    "last_message": "Done. The login form validates."},
                        }),
                        "handoff.incoming.add" => {
                            let mut n = n.lock().unwrap();
                            *n += 1;
                            let from = PathBuf::from(p["path"].as_str().unwrap());
                            std::fs::copy(&from, root.join(format!("received-{n}.tar.zst")))
                                .unwrap();
                            json!({"incoming": {"id": format!("in{n}"), "state": "pending",
                                                "result": null}})
                        }
                        _ => json!({}),
                    };
                    let line = json!({"jsonrpc": "2.0", "id": req["id"], "result": result})
                        .to_string()
                        + "\n";
                    if w.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn owner() -> Device {
    Device {
        id: "own".into(),
        name: "owner's laptop".into(),
        platform: "test".into(),
        public: "pub-own".into(),
        scope: Scope::Full,
        paired_at: 0,
        vapid_private: None,
        push: vec![],
        prefs: Default::default(),
        push_failures: 0,
        kind: "device".into(),
        expires_at: None,
        limit: None,
        peer: None,
        supports_clear: false,
        github: None,
        invitation: None,
        review: None,
        team_check: None,
    }
}

struct Env {
    _t: tempfile::TempDir,
    root: PathBuf,
    gw: Arc<Gateway>,
    dev: Device,
    acct: FakeServer,
    calls: Calls,
}

impl Env {
    fn call(&self) -> Call<'_> {
        Call {
            gw: &self.gw,
            device: &self.dev,
        }
    }

    async fn dispatch(&self, method: &str, p: Value) -> ApiResult {
        self.call().dispatch(method, p).await
    }

    /// Poll a job until it finishes.
    async fn wait(&self, job: &str) -> Value {
        for _ in 0..600 {
            let v = self
                .dispatch("handoff.link.status", json!({"job": job}))
                .await
                .unwrap();
            if matches!(v["state"].as_str(), Some("done" | "failed")) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {job} did not finish");
    }

    async fn create(&self, p: Value) -> Value {
        let r = self.dispatch("handoff.link.create", p).await.unwrap();
        let v = self.wait(r["job"].as_str().unwrap()).await;
        assert_eq!(v["state"], "done", "{v:#}");
        v
    }

    async fn redeem(&self, link: &str) -> Value {
        let r = self
            .dispatch("handoff.link.redeem", json!({"link": link}))
            .await
            .unwrap();
        self.wait(r["job"].as_str().unwrap()).await
    }

    fn calls_of(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

/// A gateway with an account server (`account_url`) and, with `signed_in`, a stored credential.
async fn env(signed_in: bool, account_server: bool) -> Env {
    // Short: the server socket path must stay under 104 bytes.
    let t = tempfile::Builder::new()
        .prefix("vkhl")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().canonicalize().unwrap();
    let src = root.join("src");
    std::fs::create_dir_all(src.join("app")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("app/a.txt"), "a\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "base"]);
    std::fs::write(src.join("app/a.txt"), "a\nuncommitted\n").unwrap();

    let calls = Calls::default();
    let sock = root.join("s.sock");
    fake_vibeke(sock.clone(), root.clone(), src.join("app"), calls.clone());

    let acct = FakeServer::start().await;
    acct.with(|s| {
        s.access = Some("a1".into());
        s.refresh = Some("r1".into());
        s.issued = 1;
    });
    let state = StateDir::open(root.join("gw")).unwrap();
    let mut cfg = state.config().unwrap();
    cfg.account_url = account_server.then(|| acct.url.clone());
    cfg.app_url = Some("http://app.example".into());
    cfg.host_name = Some("alpha".into());
    cfg.local_socket = false;
    state.save_config(&cfg).unwrap();
    if signed_in {
        let a = crate::account::account(&acct.url, &state.dir).unwrap();
        a.save(&vk_account::Credential {
            server: a.server().to_string(),
            login: "octo".into(),
            refresh_token: "r1".into(),
            access_token: Some("a1".into()),
            access_exp: Some(now_s() + 3600),
            saved_at: now_s(),
        })
        .unwrap();
    }
    let gw = Gateway::new(state, server::Server::new(sock)).unwrap();
    let dev = owner();
    gw.add_device(dev.clone()).unwrap();
    Env {
        _t: t,
        root,
        gw,
        dev,
        acct,
        calls,
    }
}

#[test]
fn owner_methods_only() {
    for m in [
        "handoff.link.create",
        "handoff.link.status",
        "handoff.link.list",
        "handoff.link.revoke",
        "handoff.link.redeem",
        "handoff.link.cancel",
    ] {
        assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
        assert!(kind_allows("device", m), "{m}");
        for k in ["peer", "share", "handoff"] {
            assert!(!kind_allows(k, m), "{k} {m}");
        }
        assert!(crate::api::host_wide(m), "{m}: never for limited devices");
    }
    assert!(!crate::api::is_mutating("handoff.link.status"));
    assert!(!crate::api::is_mutating("handoff.link.list"));
    assert!(crate::api::is_mutating("handoff.link.create"));
    assert!(crate::api::is_mutating("handoff.link.redeem"));
}

#[test]
fn snippet_and_dates() {
    assert_eq!(day_month(1_800_000_000), "15 Jan");
    assert_eq!(day_month(1_709_251_200), "1 Mar");
    let b = Brief {
        repo: "app".into(),
        branch: Some("feat/login".into()),
        harness: Some("claude".into()),
        from: "alice".into(),
        ..Default::default()
    };
    assert_eq!(
        markdown(&b, "https://app.example/#/continue?d=x", 1_800_000_000),
        "**Continue this session**: Claude Code on `feat/login`, sent by @alice, link valid until 15 Jan.\n[Open in Vibeke](https://app.example/#/continue?d=x)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_then_redeem_as_a_teammates_handoff() {
    let e = env(true, true).await;
    let v = e
        .create(json!({"pane": "p1", "access": "link", "ttl_s": 3600}))
        .await;
    let url = v["link"].as_str().unwrap();
    assert!(url.starts_with("http://app.example/#/continue?d="), "{url}");
    let blob = v["blob"].as_str().unwrap().to_string();
    let md = v["markdown"].as_str().unwrap();
    assert!(md.contains("Claude Code on `main`, sent by @octo"), "{md}");
    assert!(md.contains(url), "{md}");

    // The link carries the brief and names the uploaded blob; the server only has ciphertext.
    let link = ContinueLink::parse(url).unwrap();
    assert_eq!(link.blob, blob);
    assert_eq!(link.brief.repo, "src");
    assert_eq!(link.brief.branch.as_deref(), Some("main"));
    assert_eq!(link.brief.harness.as_deref(), Some("claude"));
    assert_eq!(link.brief.title.as_deref(), Some("Login form"));
    assert_eq!(
        link.brief.last.as_deref(),
        Some("Done. The login form validates.")
    );
    assert_eq!(link.brief.from, "octo");
    let stored = e.acct.blobs.with(|s| s.blobs[&blob].clone());
    let data = stored.data.unwrap();
    assert_eq!(&data[..4], b"VKC1");
    assert_eq!(data.len() as u64, stream::cipher_len(link.size));
    assert!(
        stored.access.is_none(),
        "access link: any signed-in account"
    );
    // No temporary file is left behind.
    let left: Vec<_> = std::fs::read_dir(dir(&e.gw).unwrap())
        .unwrap()
        .flatten()
        .map(|d| d.file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");

    let list = e.dispatch("handoff.link.list", json!({})).await.unwrap();
    let links = list["links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["blob"], blob.as_str());
    assert_eq!(links[0]["pane"], "p1");

    // Redeemed (here by the same account): delivered as a teammate's handoff, never imported.
    let r = e.redeem(url).await;
    assert_eq!(r["state"], "done", "{r:#}");
    assert_eq!(r["kind"], "redeem");
    assert_eq!(r["incoming"], "in1");
    assert_eq!(r["from"]["login"], "octo");
    let adds = e.calls_of("handoff.incoming.add");
    assert_eq!(adds.len(), 1);
    let from = &adds[0]["from"];
    assert_eq!(from["owner"], "teammate");
    assert_eq!(from["host"], format!("link:{blob}"));
    assert_eq!(from["name"], "octo");
    assert_eq!(from["account"], json!({"id": 4242, "login": "octo"}));
    assert_eq!(adds[0]["sha256"], link.sha256.as_str());
    assert_eq!(adds[0]["manifest"]["repo_name"], "src");
    let received = e.root.join("received-1.tar.zst");
    assert_eq!(
        vk_handoff::hash_file(&received).unwrap(),
        (link.size, link.sha256.clone())
    );
    assert!(e.calls_of("handoff.accept").is_empty());
    assert!(e.calls_of("agent.start").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checksum_mismatch_or_damaged_blob_is_refused() {
    let e = env(true, true).await;
    let v = e.create(json!({"pane": "p1", "access": "link"})).await;
    let link = ContinueLink::parse(v["link"].as_str().unwrap()).unwrap();

    // The link names another plaintext.
    let mut other = link.clone();
    other.sha256 = "0".repeat(64);
    let r = e.redeem(&other.to_url("http://app.example")).await;
    assert_eq!(r["state"], "failed");
    assert_eq!(r["error_kind"], "conflict");
    assert!(r["error"].as_str().unwrap().contains("checksum"), "{r:#}");

    // Another key does not open it.
    let mut other = link.clone();
    other.k = ContinueLink::new(
        "http://x",
        "b",
        &[9; 32],
        &link.sha256,
        1,
        1,
        Brief::default(),
    )
    .k;
    let r = e.redeem(&other.to_url("http://app.example")).await;
    assert_eq!(r["state"], "failed");
    assert!(
        r["error"].as_str().unwrap().contains("does not open"),
        "{r:#}"
    );

    // The ciphertext was changed on the server.
    e.acct.blobs.with(|s| {
        let d = s.blobs.get_mut(&link.blob).unwrap().data.as_mut().unwrap();
        let n = d.len();
        d[n - 1] ^= 1;
    });
    let r = e.redeem(&link.to_url("http://app.example")).await;
    assert_eq!(r["state"], "failed");
    assert_eq!(r["error_kind"], "conflict");

    assert!(e.calls_of("handoff.incoming.add").is_empty());
    let left: Vec<_> = std::fs::read_dir(dir(&e.gw).unwrap())
        .unwrap()
        .flatten()
        .map(|d| d.file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_link_cannot_be_redeemed() {
    let e = env(true, true).await;
    let v = e.create(json!({"pane": "p1", "access": "link"})).await;
    let blob = v["blob"].as_str().unwrap();
    e.dispatch("handoff.link.revoke", json!({"blob": blob}))
        .await
        .unwrap();
    assert!(e.acct.blobs.with(|s| s.blobs.is_empty()));
    let list = e.dispatch("handoff.link.list", json!({})).await.unwrap();
    assert_eq!(list["links"], json!([]));
    let st = e
        .dispatch("handoff.link.status", json!({"job": v["job"]}))
        .await
        .unwrap();
    assert_eq!(st["revoked"], true);

    let r = e.redeem(v["link"].as_str().unwrap()).await;
    assert_eq!(r["state"], "failed");
    assert_eq!(r["error_kind"], "not_found", "{r:#}");
    assert!(e.calls_of("handoff.incoming.add").is_empty());

    // Revoking again: already gone, still fine for a known blob; an unknown one is not found.
    e.dispatch("handoff.link.revoke", json!({"blob": blob}))
        .await
        .unwrap();
    let x = e
        .dispatch("handoff.link.revoke", json!({"blob": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(x.kind, "not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_for_other_github_accounts_is_forbidden() {
    let e = env(true, true).await;
    let v = e.create(json!({"pane": "p1", "access": "link"})).await;
    let link = ContinueLink::parse(v["link"].as_str().unwrap()).unwrap();
    // Another signed-in account (the creator itself may always read its blob).
    e.acct.blobs.with(|s| {
        s.blobs.get_mut(&link.blob).unwrap().access = Some(vec![1]);
        s.github = (5, "eve".into());
    });
    let r = e.redeem(v["link"].as_str().unwrap()).await;
    assert_eq!(r["state"], "failed");
    assert_eq!(r["error_kind"], "forbidden", "{r:#}");
}

const ACME: &str = "0b5c3f7e-1d2a-4c8b-9e6f-3a1b2c3d4e5f";
const BETA: &str = "11111111-2222-4333-8444-555555555555";

/// Continue links for teams (spec 20 §2.5): `teams` by name or id go to the account server as
/// team ids; without `github` and `teams` the account page's people and ticked teams are used.
#[tokio::test(flavor = "multi_thread")]
async fn continue_links_for_teams() {
    use vk_account::fake::FakeTeam;
    let e = env(true, true).await;
    e.acct.with(|s| {
        s.teams = vec![
            FakeTeam::new(ACME, "Acme", &[4242, 5]),
            FakeTeam::new(BETA, "Beta", &[4242]),
        ]
    });
    let blob_of = |v: &Value| v["blob"].as_str().unwrap().to_string();
    let teams_of = |b: &str| e.acct.blobs.with(|s| s.blobs[b].teams.clone());
    let v = e
        .create(json!({"pane": "p1", "access": "github", "teams": ["acme", BETA.to_uppercase()]}))
        .await;
    let b = blob_of(&v);
    assert_eq!(teams_of(&b), [ACME, BETA]);
    assert_eq!(
        e.acct.blobs.with(|s| s.blobs[&b].access.clone()),
        Some(vec![])
    );
    // With people too.
    let v = e
        .create(json!({"pane": "p1", "access": "github", "github": ["alice"], "teams": ["Beta"]}))
        .await;
    let b = blob_of(&v);
    assert_eq!(teams_of(&b), [BETA]);
    assert_eq!(
        e.acct.blobs.with(|s| s.blobs[&b].access.clone()),
        Some(vec![2])
    );
    // A member of Acme (eve, 5) can fetch an Acme link; she is not in Beta.
    let acme = e
        .create(json!({"pane": "p1", "access": "github", "teams": [ACME]}))
        .await;
    let beta = e
        .create(json!({"pane": "p1", "access": "github", "teams": [BETA]}))
        .await;
    e.acct.blobs.with(|s| s.github = (5, "eve".into()));
    let r = e.redeem(acme["link"].as_str().unwrap()).await;
    assert_ne!(r["error_kind"], "forbidden", "{r:#}");
    let r = e.redeem(beta["link"].as_str().unwrap()).await;
    assert_eq!(r["error_kind"], "forbidden", "{r:#}");
    e.acct.blobs.with(|s| s.github = (4242, "octo".into()));
    // Default: the account page's people and ticked teams.
    e.acct.with(|s| {
        s.sharing = vec![(2, "alice".into())];
        s.sharing_teams = vec![ACME.into()];
    });
    let v = e.create(json!({"pane": "p1", "access": "github"})).await;
    let b = blob_of(&v);
    assert_eq!(teams_of(&b), [ACME]);
    assert_eq!(
        e.acct.blobs.with(|s| s.blobs[&b].access.clone()),
        Some(vec![2])
    );
    // Teams only on the account page is enough.
    e.acct.with(|s| s.sharing = vec![]);
    let v = e.create(json!({"pane": "p1", "access": "github"})).await;
    assert_eq!(teams_of(&blob_of(&v)), [ACME]);
    // Refusals before any job starts.
    let x = e
        .dispatch(
            "handoff.link.create",
            json!({"pane": "p1", "access": "github", "teams": ["Gamma"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (x.kind.as_str(), &x.details),
        ("unknown_team", &json!({"team": "Gamma"}))
    );
    for p in [
        json!({"pane": "p1", "access": "link", "teams": ["Acme"]}),
        json!({"pane": "p1", "access": "github", "teams": []}),
        json!({"pane": "p1", "access": "github", "teams": "Acme"}),
        json!({"pane": "p1", "access": "github", "github": ["*"], "teams": ["Acme"]}),
    ] {
        let x = e
            .dispatch("handoff.link.create", p.clone())
            .await
            .unwrap_err();
        assert_eq!(x.kind, "invalid_params", "{p}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn account_errors_and_bad_params() {
    // No account server at all.
    let e = env(true, false).await;
    let x = e
        .dispatch(
            "handoff.link.create",
            json!({"pane": "p1", "access": "link"}),
        )
        .await
        .unwrap_err();
    assert_eq!(x.kind, "unavailable");

    // An account server, but this host never signed in.
    let e = env(false, true).await;
    for (m, p) in [
        (
            "handoff.link.create",
            json!({"pane": "p1", "access": "link"}),
        ),
        ("handoff.link.revoke", json!({"blob": "blob1"})),
    ] {
        let x = e.dispatch(m, p).await.unwrap_err();
        assert_eq!(x.kind, "account_required", "{m}");
    }

    let e = env(true, true).await;
    for p in [
        json!({"access": "link"}),
        json!({"pane": "p1"}),
        json!({"pane": "p1", "access": "everyone"}),
        json!({"pane": "p1", "access": "github"}),
        json!({"pane": "p1", "access": "github", "github": []}),
        json!({"pane": "p1", "access": "link", "ttl_s": 10}),
        json!({"pane": "p1", "access": "link", "ttl_s": MAX_TTL_S + 1}),
    ] {
        let x = e
            .dispatch("handoff.link.create", p.clone())
            .await
            .unwrap_err();
        assert_eq!(x.kind, "invalid_params", "{p}");
    }
    // `["*"]`: any signed-in GitHub account.
    let v = e
        .create(json!({"pane": "p1", "access": "github", "github": ["*"]}))
        .await;
    let blob = v["blob"].as_str().unwrap().to_string();
    assert!(e.acct.blobs.with(|s| s.blobs[&blob].access.is_none()));

    // Links for another account server never get this host's token.
    let mut link = ContinueLink::parse(v["link"].as_str().unwrap()).unwrap();
    link.server = "https://elsewhere.example".into();
    let x = e
        .dispatch(
            "handoff.link.redeem",
            json!({"link": link.to_url("http://app.example")}),
        )
        .await
        .unwrap_err();
    assert_eq!(x.kind, "forbidden");
    // Expired and malformed links.
    let mut link = ContinueLink::parse(v["link"].as_str().unwrap()).unwrap();
    link.exp = 1;
    let x = e
        .dispatch("handoff.link.redeem", json!({"link": link.encode()}))
        .await
        .unwrap_err();
    assert_eq!(x.kind, "expired");
    let x = e
        .dispatch(
            "handoff.link.redeem",
            json!({"link": "https://app.example/#/pair?d=x"}),
        )
        .await
        .unwrap_err();
    assert_eq!(x.kind, "invalid_params");
    let x = e
        .dispatch("handoff.link.status", json!({"job": "hl-nope"}))
        .await
        .unwrap_err();
    assert_eq!(x.kind, "not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_jobs_fail_and_cancel_answers_for_link_jobs() {
    let e = env(true, true).await;
    // A job a previous gateway process left running.
    let job = LinkJob {
        id: "hl-old".into(),
        kind: "create".into(),
        state: "uploading".into(),
        created_at: now_s(),
        updated_at: now_s(),
        ..Default::default()
    };
    crate::state::write_json(&job_file(&e.gw), &[job]).unwrap();
    let v = e
        .dispatch("handoff.link.status", json!({"job": "hl-old"}))
        .await
        .unwrap();
    assert_eq!(v["state"], "failed");
    assert_eq!(v["error"], "interrupted");

    // `handoff.cancel` with a link job id is answered here; a finished job stays as it is.
    let done = e.create(json!({"pane": "p1", "access": "link"})).await;
    let c = e
        .dispatch("handoff.cancel", json!({"id": done["job"]}))
        .await
        .unwrap();
    assert_eq!(c["job"]["state"], "done");
    // Other ids still go to the server's send jobs.
    assert!(
        dispatch(&e.gw, &e.dev, "handoff.cancel", &json!({"id": "01hzxjob"}))
            .await
            .is_none()
    );
    assert!(
        dispatch(&e.gw, &e.dev, "handoff.jobs", &json!({}))
            .await
            .is_none()
    );

    // Cancelling a running job stops it (unless it finished first).
    let r = e
        .dispatch(
            "handoff.link.create",
            json!({"pane": "p1", "access": "link"}),
        )
        .await
        .unwrap();
    let id = r["job"].as_str().unwrap();
    let c = e
        .dispatch("handoff.cancel", json!({"id": id}))
        .await
        .unwrap();
    let v = e.wait(id).await;
    assert_eq!(v["state"], c["job"]["state"], "{v:#}");
    if v["state"] == "failed" {
        assert_eq!(v["error"], "cancelled");
        assert!(v.get("link").is_none());
    }
}
