//! Container boxes against a **fake** docker-compatible runtime (a shell script that logs its
//! argv and emulates `inspect/run/start/stop/rm/exec`): no container is ever started. `exec`
//! runs scripts and git services on the host against the box's host-side workspace dir, which
//! is exactly what the real bind mount presents at `/workspace`.

use super::*;

struct Fake {
    cli: PathBuf,
    log: PathBuf,
}

fn fake_runtime(root: &Path, boxdir: &Path) -> Fake {
    let dir = root.join("fake");
    std::fs::create_dir_all(&dir).unwrap();
    let (log, state) = (dir.join("log"), dir.join("state"));
    let b = boxdir.display();
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$*" >> {log}
cmd="$1"; shift
case "$cmd" in
  inspect) if [ -f {state} ]; then cat {state}; exit 0; fi; echo "Error: No such object" >&2; exit 1;;
  run) echo running > {state}; echo 0123abcd; exit 0;;
  start) echo running > {state}; exit 0;;
  stop) echo exited > {state}; exit 0;;
  rm) rm -f {state}; exit 0;;
  exec)
    while [ $# -gt 0 ]; do
      case "$1" in
        --interactive|--tty) shift;;
        --workdir|--user|--env) shift 2;;
        *) break;;
      esac
    done
    shift
    if [ "$1" = git ]; then exec git "$2" {b}; fi
    if [ "$1" = /bin/sh ] && [ "$2" = -c ]; then
      s=$(printf '%s' "$3" | sed "s#/workspace#{b}#g")
      exec /bin/sh -c "$s"
    fi
    exit 0;;
esac
exit 0
"#,
        log = log.display(),
        state = state.display(),
    );
    let cli = dir.join("docker");
    std::fs::write(&cli, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    Fake { cli, log }
}

fn log_of(f: &Fake) -> String {
    std::fs::read_to_string(&f.log).unwrap_or_default()
}

/// A task worktree on branch `u/<task>` next to the test repo.
fn worktree(e: &Env, task: &str) -> PathBuf {
    let wt = e.root.join(format!("wt-{task}"));
    git(
        &e.checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("u/{task}"),
            wt.to_str().unwrap(),
            "main",
        ],
    );
    wt.canonicalize().unwrap()
}

fn container_req(net: NetworkProfile) -> IsoRequest {
    IsoRequest {
        level: IsolationLevel::Container,
        network: net,
        yolo: true,
        harnesses: vec!["claude".into()],
        image: Some("alpine:3.20".into()),
        ..Default::default()
    }
}

fn container_iso() -> Isolation {
    Isolation {
        level: IsolationLevel::Container,
        provider: "docker".into(),
        network: "none".into(),
        yolo: true,
        scope: "pane".into(),
        visible_roots: vec![],
    }
}

#[tokio::test]
async fn container_task_box_clone_sync_and_teardown() {
    let e = Env::new();
    let task = "ctask01JABCDEFGHJKMNPQRS1";
    let wt = worktree(&e, "c1");
    let boxdir = sbx_root(task).join("workspace");
    let fake = fake_runtime(&e.root, &boxdir);
    e.server.sandbox.set_container_runtime(fake.cli.clone());
    let pane = e.task_with_pane(task, container_iso());

    let b = prepare_box(
        &e.server,
        task,
        Some(task),
        &wt,
        container_req(NetworkProfile::None),
    )
    .await
    .unwrap();
    assert_eq!(b.isolation.level, IsolationLevel::Container);
    let log = log_of(&fake);
    let run = log
        .lines()
        .find(|l| l.starts_with("run --detach"))
        .expect("box created");
    assert!(run.contains("--network none"), "{run}");
    assert!(run.contains("--cap-drop ALL --security-opt no-new-privileges"));
    assert!(run.contains(&format!(
        "--volume {}:/vibeke/inbox:ro",
        crate::paths::Paths::inbox().display()
    )));
    assert!(run.contains(&format!("--volume {}:/workspace", boxdir.display())));
    // The host objects are borrowed read-only; the host worktree itself is never mounted.
    let objects = e.checkout.canonicalize().unwrap().join(".git/objects");
    assert!(run.contains(&format!(
        "--volume {}:{}:ro",
        objects.display(),
        objects.display()
    )));
    assert!(!run.contains(&format!("{}:", wt.display())));
    assert!(run.contains("--label vibeke.key=ctask01JABCDEFGHJKMNPQRS1"));
    assert!(run.ends_with(
        "alpine:3.20 -c trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done"
    ));
    // Secrets never in argv.
    assert!(!log.contains(FAKE_CLAUDE));
    // The private clone exists (made by the in-box script) at the base commit.
    let base = String::from_utf8(
        Command::new("git")
            .args(["-C", wt.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    git(&boxdir, &["status"]);
    let head = Command::new("git")
        .args(["-C", boxdir.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap(), base);

    // A pane: `exec -it` into the box, secrets by name, broker socket in the run dir.
    let (argv, env, iso) = wrap_spawn(
        &e.server,
        &pane,
        wt.to_str().unwrap(),
        &["/bin/zsh".into(), "-l".into()],
        vec![
            ("VIBEKE_PANE_TOKEN".into(), "tok-secret".into()),
            ("VIBEKE_PANE_ID".into(), "w9:p1".into()),
        ],
        Some(task),
    )
    .unwrap();
    let j = argv.join(" ");
    assert_eq!(argv[0], fake.cli.to_string_lossy());
    assert!(
        j.contains("exec --interactive --tty --workdir /workspace"),
        "{j}"
    );
    assert!(j.contains("--env CLAUDE_CODE_OAUTH_TOKEN "));
    assert!(j.contains("--env CLAUDE_CONFIG_DIR=/vibeke/creds/home/claude"));
    assert!(j.contains("--env VIBEKE_PANE_ID=w9:p1"));
    assert!(j.ends_with("/bin/sh -l"));
    assert!(!j.contains(FAKE_CLAUDE) && !j.contains("tok-secret"));
    assert!(
        env.iter()
            .any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v == FAKE_CLAUDE)
    );
    assert!(!env.iter().any(|(k, _)| k == "AWS_SECRET_ACCESS_KEY"));
    assert_eq!(iso.level, IsolationLevel::Container);
    assert!(
        iso.visible_roots.is_empty(),
        "clone mode shows no host path"
    );
    let sock =
        container::run_dir(task).join(format!("{}.sock", vk_sandbox::runner::short_id(&pane)));
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(sock.exists(), "broker socket for the box pane");

    // Drops: the client put the file in the host inbox; the box sees /vibeke/inbox.
    let host = crate::paths::Paths::inbox().join("3f9a1c0b2e7d/shot.png");
    assert_eq!(
        paste_text(&e.server, &pane, host.to_string_lossy().into_owned()),
        "/vibeke/inbox/3f9a1c0b2e7d/shot.png"
    );

    // The agent commits in the box; `task sync` pulls it into the host worktree.
    std::fs::write(boxdir.join("from-box.txt"), "hi").unwrap();
    git(&boxdir, &["add", "-A"]);
    git(&boxdir, &["commit", "-q", "-m", "box work"]);
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "task.sync", "params": {"task": task}})
        .to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert_eq!(r["result"]["synced"][0]["status"], "fast_forwarded", "{r}");
    assert_eq!(r["result"]["synced"][0]["commits"], 1);
    assert!(wt.join("from-box.txt").is_file());
    assert!(
        log_of(&fake).contains("git upload-pack"),
        "upload-pack ran via exec"
    );
    assert!(e.all_events_json().contains("task.synced"));
    // A pane-scoped caller can't sync.
    let pctx = Ctx {
        client_id: "agent".into(),
        kind: "agent".into(),
        pane_scope: Some(pane.clone()),
        remote: false,
    };
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &pctx, &line).await).unwrap();
    assert!(r["error"].is_object(), "{r}");

    // Host fix pushed in: lands on the side ref and the box fast-forwards.
    std::fs::write(wt.join("host.txt"), "fix").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "host fix"]);
    let line = json!({"jsonrpc": "2.0", "id": 2, "method": "task.sync", "params": {"task": task, "direction": "push"}}).to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert_eq!(r["result"]["synced"][0]["status"], "pushed", "{r}");
    assert!(boxdir.join("host.txt").is_file());

    // sandbox.list shows the container.
    let line = json!({"jsonrpc": "2.0", "id": 3, "method": "sandbox.list"}).to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    let c = &r["result"]["sandboxes"][0]["container"];
    assert_eq!(c["state"], "running", "{r}");
    assert_eq!(c["code"], "clone");
    assert_eq!(c["clone"]["branch"], "u/c1");

    // Stop/start.
    let line =
        json!({"jsonrpc": "2.0", "id": 4, "method": "sandbox.stop", "params": {"task": task}})
            .to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert_eq!(r["result"]["state"], "stopped", "{r}");
    // Stopped box: sync falls back to the hardened host-side services.
    let line = json!({"jsonrpc": "2.0", "id": 5, "method": "task.sync", "params": {"task": task}})
        .to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert_eq!(r["result"]["synced"][0]["status"], "up_to_date", "{r}");
    let line =
        json!({"jsonrpc": "2.0", "id": 6, "method": "sandbox.start", "params": {"task": task}})
            .to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert_eq!(r["result"]["state"], "running", "{r}");

    // Finish: fully synced → removed.
    teardown(&e.server, task);
    for _ in 0..200 {
        if log_of(&fake).contains("rm --force") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(log_of(&fake).contains("rm --force"));
    for _ in 0..100 {
        if e.all_events_json().contains("\"removed\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(e.all_events_json().contains("sandbox.destroyed"));
}

#[tokio::test]
async fn unsynced_box_is_kept_on_finish() {
    let e = Env::new();
    let task = "ctask01JABCDEFGHJKMNPQRS2";
    let wt = worktree(&e, "c2");
    let boxdir = sbx_root(task).join("workspace");
    let fake = fake_runtime(&e.root, &boxdir);
    e.server.sandbox.set_container_runtime(fake.cli.clone());
    e.task_with_pane(task, container_iso());
    prepare_box(
        &e.server,
        task,
        Some(task),
        &wt,
        container_req(NetworkProfile::None),
    )
    .await
    .unwrap();
    // Box and host diverge: finishing must not destroy the box's commits.
    std::fs::write(boxdir.join("b.txt"), "b").unwrap();
    git(&boxdir, &["add", "-A"]);
    git(&boxdir, &["commit", "-q", "-m", "box"]);
    std::fs::write(wt.join("h.txt"), "h").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "host"]);
    teardown(&e.server, task);
    for _ in 0..200 {
        if log_of(&fake).contains("stop --time 5") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let log = log_of(&fake);
    assert!(log.contains("stop --time 5"), "{log}");
    assert!(!log.contains("rm --force"));
    assert!(boxdir.join("b.txt").is_file(), "box clone kept");
    for _ in 0..100 {
        if e.server.sandbox.get(task).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        e.server.sandbox.get(task).is_some(),
        "kept box stays listed"
    );
    assert!(e.all_events_json().contains("unsynced_kept"));
}

#[tokio::test]
async fn container_proxy_profile_and_in_box_binary() {
    let e = Env::new();
    let task = "ctask01JABCDEFGHJKMNPQRS3";
    let wt = worktree(&e, "c3");
    let fake = fake_runtime(&e.root, &sbx_root(task).join("workspace"));
    e.server.sandbox.set_container_runtime(fake.cli.clone());
    e.task_with_pane(task, container_iso());
    // No Linux vibeke: a proxy profile is refused (never silently open).
    let r = prepare_box(
        &e.server,
        task,
        Some(task),
        &wt,
        container_req(NetworkProfile::Dev),
    )
    .await;
    let msg = r.err().expect("refused").message;
    assert!(msg.contains("vibeke_linux"), "{msg}");
    // With the static binary in the release cache, the box gets `box-init` as PID 1 and a link
    // (`exec -i … vibeke sandbox bridge`) to the proxy.
    let home = e.root.join("home");
    let rel = home.join(".cache/vibeke/releases").join(vk_proto::VERSION);
    std::fs::create_dir_all(&rel).unwrap();
    let bin = rel.join(format!("vibeke-linux-{}", std::env::consts::ARCH));
    std::fs::write(&bin, "not really an ELF").unwrap();
    let b = prepare_box(
        &e.server,
        task,
        Some(task),
        &wt,
        container_req(NetworkProfile::Dev),
    )
    .await
    .unwrap();
    assert!(b.proxy.is_some());
    let run = log_of(&fake)
        .lines()
        .rev()
        .find(|l| l.starts_with("run --detach"))
        .unwrap()
        .to_string();
    assert!(run.contains("--network none"), "{run}");
    // No unix sockets cross a bind mount (VM-backed runtimes don't forward them).
    assert!(!run.contains("/vibeke/run"), "{run}");
    assert!(run.contains(&format!("--volume {}:/vibeke/bin/vibeke:ro", bin.display())));
    assert!(run.contains("--env HTTPS_PROXY=http://127.0.0.1:3128"));
    assert!(run.ends_with("--entrypoint /vibeke/bin/vibeke alpine:3.20 sandbox box-init"));
    // The link: `exec -i <box> vibeke sandbox bridge`, listening on the proxy port in the box.
    let want =
        "/vibeke/bin/vibeke sandbox bridge --brokers /tmp/vibeke-brokers --listen 127.0.0.1:3128";
    for _ in 0..200 {
        if log_of(&fake).contains(want) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(log_of(&fake).contains(want), "{}", log_of(&fake));
    assert!(link(&e.server, task).is_some());
    teardown(&e.server, task);
    assert!(link(&e.server, task).is_none());
}

#[tokio::test]
async fn devcontainer_configures_the_box_and_waits_for_trust() {
    let e = Env::new();
    let task = "ctask01JABCDEFGHJKMNPQRS4";
    std::fs::create_dir_all(e.checkout.join(".devcontainer")).unwrap();
    std::fs::write(
        e.checkout.join(".devcontainer/devcontainer.json"),
        r#"{
          // comment
          "image": "alpine:3.20",
          "remoteUser": "node",
          "containerEnv": {"FOO": "bar"},
          "postCreateCommand": "touch /workspace/.post-created",
          "mounts": ["source=vk-test-cache,target=/cache,type=volume",
                     "source=${localEnv:HOME}/.ssh,target=/root/.ssh,type=bind"],
          "runArgs": ["--privileged"],
        }"#,
    )
    .unwrap();
    git(&e.checkout, &["add", "-A"]);
    git(&e.checkout, &["commit", "-q", "-m", "devcontainer"]);
    let wt = worktree(&e, "c4");
    let boxdir = sbx_root(task).join("workspace");
    let fake = fake_runtime(&e.root, &boxdir);
    e.server.sandbox.set_container_runtime(fake.cli.clone());
    e.task_with_pane(task, container_iso());
    let mut req = container_req(NetworkProfile::None);
    req.image = None;
    let b = prepare_box(&e.server, task, Some(task), &wt, req)
        .await
        .unwrap();
    let run = log_of(&fake)
        .lines()
        .find(|l| l.starts_with("run --detach"))
        .unwrap()
        .to_string();
    assert!(run.contains("--user node"), "{run}");
    assert!(run.contains("--env FOO=bar"));
    assert!(run.contains("--volume vk-test-cache:/cache"));
    assert!(
        !run.contains(".ssh"),
        "host bind mounts from a repo file are refused"
    );
    assert!(!run.contains("privileged"));
    assert!(run.contains(" alpine:3.20 "));
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    assert!(c.lifecycle.is_empty(), "untrusted: no postCreateCommand");
    assert!(c.warnings.iter().any(|w| w.contains("not trusted")));
    assert!(c.warnings.iter().any(|w| w.contains("runArgs")));
    teardown(&e.server, task);
    for _ in 0..200 {
        if log_of(&fake).contains("rm --force") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // After `policy trust`, the next box runs the lifecycle command inside the box.
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "policy.trust", "params": {"path": e.checkout}}).to_string();
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&e.server, &e.user_ctx(), &line).await)
            .unwrap();
    assert!(r["result"]["devcontainer"]["digest"].is_string(), "{r}");
    let task2 = "ctask01JABCDEFGHJKMNPQRS5";
    let wt2 = worktree(&e, "c5");
    let boxdir2 = sbx_root(task2).join("workspace");
    let fake2 = fake_runtime(&e.root.join("f2"), &boxdir2);
    e.server.sandbox.set_container_runtime(fake2.cli.clone());
    e.task_with_pane(task2, container_iso());
    let mut req = container_req(NetworkProfile::None);
    req.image = None;
    let b = prepare_box(&e.server, task2, Some(task2), &wt2, req)
        .await
        .unwrap();
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    assert_eq!(c.lifecycle.len(), 1);
    for _ in 0..200 {
        if boxdir2.join(".post-created").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        boxdir2.join(".post-created").exists(),
        "postCreateCommand ran in the box"
    );
    teardown(&e.server, task2);
}

#[tokio::test]
async fn restore_rebuilds_without_starting() {
    let e = Env::new();
    let task = "ctask01JABCDEFGHJKMNPQRS6";
    let wt = worktree(&e, "c6");
    let fake = fake_runtime(&e.root, &sbx_root(task).join("workspace"));
    e.server.sandbox.set_container_runtime(fake.cli.clone());
    e.task_with_pane(task, container_iso());
    {
        // What task.create persisted.
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv(
            "sandbox",
            task,
            Some(
                json!({"task": task, "checkout": wt, "request": container_req(NetworkProfile::None)})
                    .to_string(),
            ),
        );
        e.server.commit(&mut c, tx).unwrap();
    }
    restore(&e.server).await;
    assert!(e.server.sandbox.get(task).is_some());
    assert!(
        !log_of(&fake).contains("run --detach"),
        "restore never creates/starts a box"
    );
    teardown(&e.server, task);
}

/// Real runtime, opt-in: `VIBEKE_CONTAINER_TESTS=1`. Needs a locally present image with git and
/// busybox (default `alpine/git:latest`, override `VIBEKE_CONTAINER_GIT_IMAGE`); never pulls.
/// With a static Linux vibeke (`VIBEKE_TEST_LINUX_BIN`, or the zigbuild output in `target/`)
/// it also checks the egress path: proxy socket reachable through the bind mount, no direct
/// route out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_container_clone_sync_and_egress_gated() {
    if std::env::var("VIBEKE_CONTAINER_TESTS").as_deref() != Ok("1") {
        return;
    }
    let Some(p) = vk_sandbox::container::select(None, NetworkProfile::Dev) else {
        eprintln!("no container runtime; skipping");
        return;
    };
    let image = std::env::var("VIBEKE_CONTAINER_GIT_IMAGE").unwrap_or("alpine/git:latest".into());
    let host_env: Vec<(String, String)> = std::env::vars().collect();
    let have = vk_sandbox::container::run_cmd(
        &[
            p.cli().to_string_lossy().into_owned(),
            "image".into(),
            "inspect".into(),
            image.clone(),
        ],
        &vk_sandbox::container::cli_env(&host_env),
        None,
        Duration::from_secs(20),
    )
    .map(|o| o.ok)
    .unwrap_or(false);
    if !have {
        eprintln!("{image} not present locally; not pulling; skipping");
        return;
    }
    let e = Env::new();
    let task = "rtask01JABCDEFGHJKMNPQRS7";
    let wt = worktree(&e, "r1");
    // The server's env reaches the runtime CLI (PATH/HOME/DOCKER_*).
    let mut opts_env = host_env.clone();
    opts_env.retain(|(k, _)| !k.contains("TOKEN") && !k.contains("KEY") && !k.contains("SECRET"));
    let server = {
        let paths = Paths {
            session: "t".into(),
            runtime: e.root.join("run2"),
            state: e.root.join("state2"),
        };
        Server::new(
            paths,
            ServerOpts {
                session: "t".into(),
                machine: "testbox".into(),
                bin: "/bin/false".into(),
                hold_args: vec![],
                default_shell: None,
                env: opts_env,
                shims: false,
            },
        )
        .unwrap()
    };
    let home = e.root.join("home");
    server.sandbox.set_home(home.clone());
    let linux_bin = std::env::var_os("VIBEKE_TEST_LINUX_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "../../target/{}-unknown-linux-musl/debug/vibeke",
                std::env::consts::ARCH.replace("arm64", "aarch64")
            ));
            p.is_file().then_some(p)
        });
    let net = if let Some(b) = &linux_bin {
        let rel = home.join(".cache/vibeke/releases").join(vk_proto::VERSION);
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::copy(
            b,
            rel.join(format!("vibeke-linux-{}", std::env::consts::ARCH)),
        )
        .unwrap();
        NetworkProfile::HarnessApis
    } else {
        eprintln!("no Linux vibeke; egress part skipped");
        NetworkProfile::None
    };
    struct Rm(PathBuf, String);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = Command::new(&self.0)
                .args(["rm", "--force", &self.1])
                .output();
        }
    }
    let _rm = Rm(
        p.cli().to_path_buf(),
        format!("vk-{}", vk_sandbox::runner::short_id(task)),
    );
    let req = IsoRequest {
        level: IsolationLevel::Container,
        network: net,
        image: Some(image),
        ..Default::default()
    };
    let b = prepare_box(&server, task, Some(task), &wt, req)
        .await
        .unwrap();
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    // The agent works in its private clone.
    let o = c
        .b()
        .exec_script(
            "echo hi > box.txt && git add -A && git -c user.name=a -c user.email=a@example.invalid commit -qm box && git rev-parse --abbrev-ref HEAD",
            None,
            Duration::from_secs(60),
        )
        .unwrap();
    assert!(o.ok, "{}", o.stderr);
    assert_eq!(o.stdout.trim(), "u/r1");
    // The user's worktree is not visible inside.
    let o = c
        .b()
        .exec_script(
            &format!("test -e {}", wt.display()),
            None,
            Duration::from_secs(30),
        )
        .unwrap();
    assert!(!o.ok, "host worktree visible in the box");
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "task.sync", "params": {"task": task}})
        .to_string();
    let ctx = Ctx {
        client_id: "u".into(),
        kind: "tui".into(),
        pane_scope: None,
        remote: false,
    };
    let r: Value =
        serde_json::from_str(&crate::api::handle_line(&server, &ctx, &line).await).unwrap();
    assert_eq!(r["result"]["synced"][0]["status"], "fast_forwarded", "{r}");
    assert!(wt.join("box.txt").is_file());
    if linux_bin.is_some() {
        let l = link(&server, task).expect("box link");
        for _ in 0..100 {
            if l.connected.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            l.connected.load(std::sync::atomic::Ordering::SeqCst),
            "link up"
        );
        // Through the proxy: a non-allowlisted host is refused by the host proxy (403), which
        // proves the exec-stdio link + in-box listener path works.
        let o = c
            .b()
            .exec_script(
                "wget -S -T 10 -O- http://example.com/ 2>&1; echo rc=$?; wget -S -T 10 -O- http://192.0.2.1/ 2>&1; echo rc=$?",
                None,
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(
            o.stdout.contains("403") || o.stdout.contains("Forbidden"),
            "{} {}",
            o.stdout,
            o.stderr
        );
        // A hook inside the box reaches its pane's broker on the host through the link, and the
        // broker still refuses everything outside pane scope.
        let pane = "01JPANE0000000000000BOXP1";
        let short = vk_sandbox::runner::short_id(pane);
        start_broker(&server, pane, &c.b().run_dir.join(format!("{short}.sock")));
        l.add_pane(pane);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let o = c
            .b()
            .exec_script(
                &format!(
                    "export VIBEKE_SESSION=t VIBEKE_SOCKET=/tmp/vibeke-brokers/{short}.sock; /vibeke/bin/vibeke api call interaction.answer '{{}}' 2>&1; echo rc=$?"
                ),
                None,
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(
            o.stdout.contains("not available inside a sandbox"),
            "{} {}",
            o.stdout,
            o.stderr
        );
        // No direct route.
        let o = c
            .b()
            .exec_script(
                "wget -q -T 5 -Y off -O- http://1.1.1.1/ 2>&1; echo rc=$?",
                None,
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(!o.stdout.contains("rc=0"), "{}", o.stdout);
    }
    teardown(&server, task);
    for _ in 0..200 {
        if c.b().state() == vk_sandbox::container::BoxState::Missing {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(c.b().state(), vk_sandbox::container::BoxState::Missing);
}
