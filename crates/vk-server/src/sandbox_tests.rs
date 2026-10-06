//! In-process tests for the server side of spec 13: egress Interactions, the per-pane broker,
//! credential projection staying out of events, spawn wrapping and paste visibility. No holders
//! or harnesses are started; panes are model records only.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::process::Command;
use std::sync::Once;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Same values as the review tests' init_env: whichever module runs first, every test in
        // this binary sees one consistent runtime/state root.
        let base = std::env::temp_dir().join(format!("vk-review-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: identical values to the only other writer; set before servers read them.
        unsafe {
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

const FAKE_CLAUDE: &str = "sk-ant-oat01-FAKE-claude-token-for-tests";
const FAKE_CODEX: &str = "FAKE-codex-access-token-for-tests";

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
    root: PathBuf,
    checkout: PathBuf,
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl Env {
    fn new() -> Env {
        init_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths {
            session: "t".into(),
            runtime: root.join("run"),
            state: root.join("state"),
        };
        let opts = ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("CLAUDE_CODE_OAUTH_TOKEN".into(), FAKE_CLAUDE.into()),
                ("AWS_SECRET_ACCESS_KEY".into(), "FAKE-aws".into()),
            ],
            shims: false,
        };
        let server = Server::new(paths, opts).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/auth.json"),
            format!("{{\"tokens\":{{\"access_token\":\"{FAKE_CODEX}\"}}}}"),
        )
        .unwrap();
        std::fs::create_dir_all(home.join("Desktop")).unwrap();
        std::fs::write(home.join("Desktop/shot.png"), "png").unwrap();
        server.sandbox.set_home(home);
        let checkout = root.join("repo");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "-q", "-b", "main"]);
        std::fs::write(checkout.join("README"), "hi\n").unwrap();
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-q", "-m", "base"]);
        Env {
            _dir: dir,
            server,
            root,
            checkout,
        }
    }

    /// Task + workspace + one (holder-less) pane record, like `task.create` would leave them.
    fn task_with_pane(&self, task: &str, iso: Isolation) -> String {
        let pane = format!("{task}-pane");
        let ws = format!("{task}-ws");
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.ws(Workspace {
            id: ws.clone(),
            handle: "w9".into(),
            name: None,
            auto_name: "t".into(),
            root_path: self.checkout.to_string_lossy().into_owned(),
            task: Some(task.into()),
            order: 1.0,
            branch: None,
        });
        tx.pane(Pane {
            id: pane.clone(),
            handle: "w9:p1".into(),
            tab: "tab".into(),
            workspace: ws.clone(),
            title: None,
            auto_title: "zsh".into(),
            cwd: Some(self.checkout.to_string_lossy().into_owned()),
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            recovered: None,
            isolation: iso.clone(),
            browser: None,
        });
        tx.task(Task {
            id: task.into(),
            handle: "k9".into(),
            title: "t".into(),
            slug: "t".into(),
            workspace: Some(ws),
            repo_root: self.checkout.to_string_lossy().into_owned(),
            worktree_path: Some(self.checkout.to_string_lossy().into_owned()),
            status: "active".into(),
            isolation: iso,
            ..Default::default()
        });
        self.server.commit(&mut c, tx).unwrap();
        pane
    }

    fn user_ctx(&self) -> Ctx {
        Ctx {
            client_id: "user-1".into(),
            kind: "tui".into(),
            pane_scope: None,
            remote: false,
        }
    }

    fn all_events_json(&self) -> String {
        self.server.with_core(|c| {
            serde_json::to_string(&c.store.events_after(0, 10_000, &[]).unwrap()).unwrap()
        })
    }
}

fn req(level: IsolationLevel, network: NetworkProfile, harnesses: &[&str]) -> IsoRequest {
    IsoRequest {
        level,
        network,
        yolo: true,
        harnesses: harnesses.iter().map(|s| s.to_string()).collect(),
        local_ports: vec![],
        image: None,
        proxy_port: None,
        ..Default::default()
    }
}

async fn wait_open_interaction(server: &Server) -> Interaction {
    for _ in 0..200 {
        if let Some(it) = server.with_core(|c| {
            c.model
                .interactions
                .iter()
                .find(|i| i.status == InteractionStatus::Open)
                .cloned()
        }) {
            return it;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no open interaction");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn egress_interaction_allow_for_task_and_deny() {
    let e = Env::new();
    let b = prepare_box(
        &e.server,
        "task-eg",
        Some("task-eg"),
        &e.checkout,
        req(IsolationLevel::Sandbox, NetworkProfile::HarnessApis, &[]),
    )
    .await
    .unwrap();
    let iso = b.isolation.clone();
    let pane = e.task_with_pane("task-eg", iso);
    assert!(b.proxy.is_some());

    // Two concurrent attempts to the same host share one Interaction.
    let s1 = e.server.clone();
    let a1 = tokio::spawn(async move { egress_ask(&s1, "task-eg", "npmjs.org".into(), 443).await });
    let it = wait_open_interaction(&e.server).await;
    let s2 = e.server.clone();
    let a2 = tokio::spawn(async move { egress_ask(&s2, "task-eg", "npmjs.org".into(), 443).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let open = e.server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open)
            .count()
    });
    assert_eq!(open, 1);
    assert_eq!(it.pane, pane);
    assert_eq!(it.kind, InteractionKind::Approval);
    assert!(it.title.contains("npmjs.org:443"));
    assert_eq!(
        it.native_ref.as_deref(),
        Some("egress:task-eg:npmjs.org:443")
    );

    // The sandboxed pane itself may not answer (and its broker could not even ask).
    let agent_ctx = Ctx {
        client_id: "a".into(),
        kind: "agent".into(),
        pane_scope: Some(pane.clone()),
        remote: false,
    };
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "interaction.answer", "params": {"interaction": it.id, "decision": "allow_always"}}).to_string();
    let r = crate::api::handle_line(&e.server, &agent_ctx, &line).await;
    assert!(
        r.contains("self_answer_forbidden") || r.contains("permission"),
        "{r}"
    );

    // The user answers "allow always" = for this task.
    let r = crate::api::handle_line(&e.server, &e.user_ctx(), &line).await;
    assert!(r.contains("\"result\""), "{r}");
    assert_eq!(a1.await.unwrap(), AskDecision::AllowTask);
    assert_eq!(a2.await.unwrap(), AskDecision::AllowTask);
    let pol = b.proxy.as_ref().unwrap().policy.read().unwrap().clone();
    assert!(pol.task_allow.contains("npmjs.org:443"));
    assert!(!pol.task_allow.contains("npmjs.org"));
    // Closed interactions leave the live model; the outbox records the delivery.
    assert!(e.server.with_core(|c| c.interaction(&it.id).is_none()));
    let events = e.all_events_json();
    assert!(events.contains("interaction.decided") && events.contains("interaction.delivered"));

    // Deny is remembered: the next attempt is refused without a new Interaction.
    let s3 = e.server.clone();
    let a3 = tokio::spawn(async move { egress_ask(&s3, "task-eg", "evil.test".into(), 443).await });
    let it = wait_open_interaction(&e.server).await;
    let line = json!({"jsonrpc": "2.0", "id": 2, "method": "interaction.answer", "params": {"interaction": it.id, "decision": "deny"}}).to_string();
    crate::api::handle_line(&e.server, &e.user_ctx(), &line).await;
    assert_eq!(a3.await.unwrap(), AskDecision::Deny);
    assert_eq!(
        egress_ask(&e.server, "task-eg", "evil.test".into(), 443).await,
        AskDecision::Deny
    );
    let n = e.server.with_core(|c| {
        c.store
            .events_after(0, 10_000, &[])
            .unwrap()
            .iter()
            .filter(|ev| {
                ev.kind == "interaction.opened" && ev.data["egress"]["host"] == "evil.test"
            })
            .count()
    });
    assert_eq!(n, 1);
    teardown(&e.server, "task-eg");
    assert!(e.server.sandbox.get("task-eg").is_none());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn credentials_are_projected_but_never_in_events() {
    let e = Env::new();
    let b = prepare_box(
        &e.server,
        "task-cr",
        Some("task-cr"),
        &e.checkout,
        req(
            IsolationLevel::Sandbox,
            NetworkProfile::Dev,
            &["claude", "codex"],
        ),
    )
    .await
    .unwrap();
    e.task_with_pane("task-cr", b.isolation.clone());
    let (argv, env, iso) = wrap_spawn(
        &e.server,
        "pane-cr-01",
        &e.checkout.to_string_lossy(),
        &["/bin/zsh".to_string(), "-l".to_string()],
        vec![
            ("PATH".into(), "/usr/bin".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "FAKE-aws".into()),
            ("VIBEKE_PANE_TOKEN".into(), "tok".into()),
        ],
        Some("task-cr"),
    )
    .unwrap();
    assert_eq!(argv[0], vk_sandbox::seatbelt::SANDBOX_EXEC);
    assert_eq!(iso.level, IsolationLevel::Sandbox);
    assert!(iso.visible_roots.iter().any(|r| r.ends_with("/inbox")));
    let get = |k: &str| env.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
    assert_eq!(get("CLAUDE_CODE_OAUTH_TOKEN").as_deref(), Some(FAKE_CLAUDE));
    assert!(get("AWS_SECRET_ACCESS_KEY").is_none());
    assert!(get("VIBEKE_PANE_TOKEN").is_some());
    let codex_home = PathBuf::from(get("CODEX_HOME").unwrap());
    assert!(
        std::fs::read_to_string(codex_home.join("auth.json"))
            .unwrap()
            .contains(FAKE_CODEX)
    );
    assert!(get("VIBEKE_SOCKET").unwrap().ends_with("/b.sock"));
    // Secrets appear in no event, no kv record and not in the generated profile.
    let events = e.all_events_json();
    assert!(events.contains("sandbox.created"));
    assert!(events.contains("env:CLAUDE_CODE_OAUTH_TOKEN"));
    assert!(events.contains("file:.codex/auth.json"));
    for secret in [FAKE_CLAUDE, FAKE_CODEX, "FAKE-aws"] {
        assert!(!events.contains(secret), "{secret} leaked into events");
    }
    let kv = e
        .server
        .with_core(|c| c.store.kv_get("sandbox", "task-cr").unwrap())
        .unwrap();
    assert!(!kv.contains(FAKE_CLAUDE) && !kv.contains(FAKE_CODEX));
    let profile = std::fs::read_to_string(&argv[2]).unwrap();
    assert!(!profile.contains(FAKE_CLAUDE) && !profile.contains(FAKE_CODEX));
    // The projected credential file is in the profile's final write-deny block.
    assert!(profile.contains(&codex_home.join("auth.json").to_string_lossy().to_string()));
    let list = api(&e.server, &e.user_ctx(), "sandbox.list", &json!({}))
        .await
        .unwrap()
        .unwrap()
        .to_string();
    assert!(!list.contains(FAKE_CLAUDE) && !list.contains(FAKE_CODEX));
    teardown(&e.server, "task-cr");
}

#[tokio::test]
async fn broker_serves_only_pane_scoped_methods() {
    let e = Env::new();
    let pane = e.task_with_pane("task-br", Isolation::default());
    let (client, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(broker_connection(
        e.server.clone(),
        server_side,
        pane.clone(),
    ));
    let (rd, mut wr) = tokio::io::split(client);
    let mut rd = BufReader::new(rd);
    let mut call = async |id: u64, method: &str, params: Value| -> Value {
        let l = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        wr.write_all(format!("{l}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        rd.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let hello = call(1, "client.hello", json!({"token": "forged"})).await;
    assert_eq!(hello["result"]["capabilities"], json!(["pane"]));
    for (i, m) in [
        "interaction.answer",
        "pane.send_keys",
        "pane.send_text",
        "workspace.create",
        "task.create",
        "server.stop",
        "events.subscribe",
        "sandbox.allow",
    ]
    .iter()
    .enumerate()
    {
        let r = call(10 + i as u64, m, json!({"pane": pane})).await;
        assert_eq!(r["error"]["data"]["kind"], "permission_denied", "{m}: {r}");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn paste_visibility_follows_the_profile() {
    let e = Env::new();
    let b = prepare_box(
        &e.server,
        "task-pv",
        Some("task-pv"),
        &e.checkout,
        req(IsolationLevel::Sandbox, NetworkProfile::None, &[]),
    )
    .await
    .unwrap();
    assert!(b.proxy.is_none(), "network none starts no proxy");
    let pane = e.task_with_pane("task-pv", b.isolation.clone());
    let inbox = Paths::inbox().join("abc/x.png");
    let desktop = e.root.join("home/Desktop/shot.png");
    assert_eq!(
        can_see(
            &e.server,
            &pane,
            &e.checkout.join("README").to_string_lossy()
        ),
        Some(true)
    );
    assert_eq!(
        can_see(&e.server, &pane, &inbox.to_string_lossy()),
        Some(true)
    );
    assert_eq!(
        can_see(&e.server, &pane, &desktop.to_string_lossy()),
        Some(false)
    );
    // Through the API: the Desktop file exists but the sandboxed pane can't see it.
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "pane.can_see_paths", "params": {"pane": pane, "paths": [desktop, e.checkout.join("README")]}}).to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert_eq!(r["result"]["visible"], json!([false, true]));
    teardown(&e.server, "task-pv");
}

#[test]
fn iso_request_params() {
    let cfg = IsolationConfig::default();
    let r = IsoRequest::from_params(&json!({"yolo": true}), &cfg).unwrap();
    assert_eq!(
        r.level,
        IsolationLevel::Sandbox,
        "--yolo applies yolo_default"
    );
    let r = IsoRequest::from_params(&json!({"yolo": true, "isolate": "host"}), &cfg).unwrap();
    assert_eq!(r.level, IsolationLevel::Host);
    assert!(r.yolo);
    let r = IsoRequest::from_params(&json!({}), &cfg).unwrap();
    assert_eq!(r.level, IsolationLevel::Host);
    assert_eq!(r.network, NetworkProfile::Dev);
    assert!(IsoRequest::from_params(&json!({"isolate": "jail"}), &cfg).is_err());
    assert!(IsoRequest::from_params(&json!({"network": "lan"}), &cfg).is_err());
    // Container code isolation (13 §6).
    let r = IsoRequest::from_params(
        &json!({"isolate": "container", "code": "clone", "image": "alpine:3.20", "devcontainer": ".devcontainer/x.json", "build": true}),
        &cfg,
    )
    .unwrap();
    assert_eq!(r.code.as_deref(), Some("clone"));
    assert_eq!(r.image.as_deref(), Some("alpine:3.20"));
    assert_eq!(r.devcontainer.as_deref(), Some(".devcontainer/x.json"));
    assert!(r.build);
    assert!(
        IsoRequest::from_params(&json!({"isolate": "sandbox", "checkout": "clone"}), &cfg).is_err()
    );
    assert!(IsoRequest::from_params(&json!({"isolate": "container", "code": "jj"}), &cfg).is_err());
    assert_eq!(yolo_args("claude"), ["--dangerously-skip-permissions"]);
    assert!(yolo_args("pi").is_empty());
}

async fn answer(e: &Env, id: &str, decision: &str) {
    let line = json!({"jsonrpc": "2.0", "id": 9, "method": "interaction.answer", "params": {"interaction": id, "decision": decision}}).to_string();
    let r = crate::api::handle_line(&e.server, &e.user_ctx(), &line).await;
    assert!(r.contains("\"result\""), "{r}");
}

async fn wait_open_interaction_for(server: &Server, needle: &str) -> Interaction {
    for _ in 0..300 {
        if let Some(it) = server.with_core(|c| {
            c.model
                .interactions
                .iter()
                .find(|i| i.status == InteractionStatus::Open && i.title.contains(needle))
                .cloned()
        }) {
            return it;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no open interaction for {needle}");
}

/// Review finding 12: an approval covers exactly the endpoint it displayed, and "allow" covers
/// one connection.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn egress_approvals_are_per_endpoint_and_single_attempt() {
    let e = Env::new();
    let b = prepare_box(
        &e.server,
        "task-ep",
        Some("task-ep"),
        &e.checkout,
        req(IsolationLevel::Sandbox, NetworkProfile::HarnessApis, &[]),
    )
    .await
    .unwrap();
    e.task_with_pane("task-ep", b.isolation.clone());
    // The attacker opens example.com:443 and queues example.com:22 while it is pending.
    let s1 = e.server.clone();
    let https =
        tokio::spawn(async move { egress_ask(&s1, "task-ep", "example.com".into(), 443).await });
    let it443 = wait_open_interaction_for(&e.server, "example.com:443").await;
    let s2 = e.server.clone();
    let ssh =
        tokio::spawn(async move { egress_ask(&s2, "task-ep", "example.com".into(), 22).await });
    let it22 = wait_open_interaction_for(&e.server, "example.com:22").await;
    assert_ne!(it443.id, it22.id, "each port gets its own Interaction");
    // Approving the displayed HTTPS destination authorizes that endpoint only.
    answer(&e, &it443.id, "allow_always").await;
    assert_eq!(https.await.unwrap(), AskDecision::AllowTask);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !ssh.is_finished(),
        "port 22 rode along with the 443 approval"
    );
    let pol = b.proxy.as_ref().unwrap().policy.read().unwrap().clone();
    assert!(pol.task_allow.contains("example.com:443"));
    assert!(matches!(
        pol.check_host("example.com", 22),
        vk_sandbox::net::HostVerdict::Ask
    ));
    answer(&e, &it22.id, "deny").await;
    assert_eq!(ssh.await.unwrap(), AskDecision::Deny);

    // "allow" = this connection: a second attempt that joined the pending Interaction is asked
    // again instead of being admitted too.
    let s3 = e.server.clone();
    let first =
        tokio::spawn(async move { egress_ask(&s3, "task-ep", "once.test".into(), 443).await });
    let it = wait_open_interaction_for(&e.server, "once.test:443").await;
    let s4 = e.server.clone();
    let second =
        tokio::spawn(async move { egress_ask(&s4, "task-ep", "once.test".into(), 443).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    answer(&e, &it.id, "allow").await;
    assert_eq!(first.await.unwrap(), AskDecision::AllowOnce);
    let again = wait_open_interaction_for(&e.server, "once.test:443").await;
    assert_ne!(again.id, it.id);
    assert!(!second.is_finished());
    answer(&e, &again.id, "deny").await;
    assert_eq!(second.await.unwrap(), AskDecision::Deny);
    teardown(&e.server, "task-ep");
}

/// Review finding 6: a contained task whose context can't be restored never gets host panes.
#[tokio::test]
async fn failed_restore_keeps_the_task_contained() {
    let e = Env::new();
    let iso = Isolation {
        level: IsolationLevel::Sandbox,
        provider: "seatbelt".into(),
        network: "dev".into(),
        yolo: true,
        scope: "pane".into(),
        visible_roots: vec![],
    };
    e.task_with_pane("task-rf", iso.clone());
    {
        // A record whose context can't be built here (vm is unavailable), as after a runtime
        // or projection failure.
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv(
            "sandbox",
            "task-rf",
            Some(
                json!({"task": "task-rf", "checkout": e.checkout, "request": req(IsolationLevel::Vm, NetworkProfile::Dev, &[])})
                    .to_string(),
            ),
        );
        e.server.commit(&mut c, tx).unwrap();
    }
    restore(&e.server).await;
    assert!(e.server.sandbox.get("task-rf").is_none());
    assert!(e.server.sandbox.failure("task-rf").is_some());
    let argv = ["/bin/zsh".to_string(), "-l".to_string()];
    // A split/respawn in the task's workspace …
    let r = wrap_spawn(
        &e.server,
        "pane-rf-2",
        &e.checkout.to_string_lossy(),
        &argv,
        vec![],
        Some("task-rf"),
    );
    let msg = r.expect_err("host fallback").to_string();
    assert!(msg.contains("sandbox is unavailable"), "{msg}");
    // … and a pane elsewhere whose cwd is the task checkout both fail closed.
    let r = wrap_spawn(
        &e.server,
        "pane-rf-3",
        &e.checkout.join("src").to_string_lossy(),
        &argv,
        vec![],
        None,
    );
    assert!(r.is_err());
    // Unrelated host panes are unaffected.
    let r = wrap_spawn(&e.server, "pane-x", "/", &argv, vec![], None).unwrap();
    assert_eq!(r.0, argv);
    assert!(e.all_events_json().contains("sandbox.unavailable"));

    // A contained task without any readable record fails closed too.
    let e2 = Env::new();
    e2.task_with_pane("task-nr", iso);
    {
        let mut c = e2.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv("sandbox", "task-nr", Some("{not json".into()));
        e2.server.commit(&mut c, tx).unwrap();
    }
    restore(&e2.server).await;
    assert!(
        wrap_spawn(
            &e2.server,
            "pane-nr-2",
            &e2.checkout.to_string_lossy(),
            &argv,
            vec![],
            Some("task-nr"),
        )
        .is_err()
    );
}

/// Review finding 8: a contained process never gets `$HOME`, `/` or a directory holding
/// protected state as its writable checkout.
#[tokio::test]
async fn isolation_refuses_home_root_and_protected_checkouts() {
    let e = Env::new();
    let home = e.root.join("home");
    for (co, level) in [
        (home.clone(), IsolationLevel::Sandbox),
        (e.root.clone(), IsolationLevel::Sandbox),
        (PathBuf::from("/"), IsolationLevel::Sandbox),
        (home.join(".config"), IsolationLevel::Sandbox),
        (home.clone(), IsolationLevel::Container),
    ] {
        let r = prepare_box(
            &e.server,
            "task-home",
            Some("task-home"),
            &co,
            req(level, NetworkProfile::None, &[]),
        )
        .await;
        let er = r
            .err()
            .unwrap_or_else(|| panic!("{} accepted", co.display()));
        assert_eq!(er.data.kind, "permission_denied", "{}", er.message);
        assert!(er.message.contains("refusing"), "{}", er.message);
    }
    assert!(e.server.sandbox.get("task-home").is_none());
}

/// Review finding 10: the broker serves only allowlisted methods *and* only for its own pane:
/// foreign pane/task/preview targets are refused.
#[tokio::test]
async fn broker_enforces_ownership_of_explicit_targets() {
    let e = Env::new();
    let mine = e.task_with_pane("task-ba", Isolation::default());
    let theirs = e.task_with_pane("task-bb", Isolation::default());
    // The other task already has a preview on port 41999.
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "preview.declare", "params": {"port": 41999, "pane": theirs}}).to_string();
    let r = crate::api::handle_line(&e.server, &e.user_ctx(), &line).await;
    assert!(r.contains("\"result\""), "{r}");
    let (client, server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(broker_connection(
        e.server.clone(),
        server_side,
        mine.clone(),
    ));
    let (rd, mut wr) = tokio::io::split(client);
    let mut rd = BufReader::new(rd);
    let mut call = async |id: u64, method: &str, params: Value| -> Value {
        let l = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        wr.write_all(format!("{l}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        rd.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    };
    for (i, (m, params)) in [
        ("agent.get", json!({"target": theirs})),
        ("agent.get", json!({"target": "01NOSUCHRUN"})),
        ("agent.report", json!({"pane": theirs, "state": "idle"})),
        ("preview.declare", json!({"port": 41998, "pane": theirs})),
        ("preview.declare", json!({"port": 41998, "task": "task-bb"})),
        ("preview.declare", json!({"port": 41999})),
    ]
    .into_iter()
    .enumerate()
    {
        let r = call(10 + i as u64, m, params.clone()).await;
        assert_eq!(
            r["error"]["data"]["kind"], "permission_denied",
            "{m} {params}: {r}"
        );
    }
    // The other pane's preview kept its owner.
    let owner = e.server.with_core(|c| {
        c.model
            .previews
            .iter()
            .find(|p| p.port == 41999)
            .and_then(|p| p.pane.clone())
    });
    assert_eq!(owner.as_deref(), Some(theirs.as_str()));
    // Its own pane is fine.
    let r = call(30, "agent.report", json!({"pane": mine, "state": "idle"})).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = call(31, "preview.declare", json!({"port": 41997})).await;
    assert!(r.get("error").is_none(), "{r}");
    let r = call(
        32,
        "preview.declare",
        json!({"port": 41996, "task": "task-ba"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
}

#[path = "sandbox_container_tests.rs"]
mod container_tests;

#[path = "sandbox_extras_tests.rs"]
mod extras_tests;
