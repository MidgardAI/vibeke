//! Lane 2E (spec 13 extras) against fakes only: a docker-compatible **fake runtime** (a shell
//! script keeping per-container state files; `exec` runs on the host in the box's host-side
//! workspace dir), an in-process box link, a local bare git remote and record-only panes. No
//! container, VM, harness or network service is started.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vk_sandbox::boxops::BoxStats;
use vk_sandbox::container::BoxState;

/// Multi-container fake: `run` records each box's `/workspace` bind source and labels, `exec`
/// maps `/workspace` to it, `ps`/`stats`/`logs`/`pause`/`commit`/`image inspect` are emulated.
const FAKE: &str = r##"#!/bin/sh
D=@DIR@
printf '%s\n' "$*" >> "$D/log"
cmd="$1"; shift
n=""; for a in "$@"; do n="$a"; done
mkdir -p "$D/c" "$D/img"
case "$cmd" in
  inspect)
    if [ -f "$D/c/$n.state" ]; then cat "$D/c/$n.state"; exit 0; fi
    echo "Error: No such object" >&2; exit 1;;
  run)
    name=""; ws=""; key=""; ses=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --name) name="$2"; shift 2;;
        --label) case "$2" in vibeke.key=*) key="${2#vibeke.key=}";; vibeke.session=*) ses="${2#vibeke.session=}";; esac; shift 2;;
        --volume) case "$2" in *:/workspace) ws="${2%:/workspace}";; esac; shift 2;;
        *) shift;;
      esac
    done
    echo running > "$D/c/$name.state"
    printf '%s' "$ws" > "$D/c/$name.ws"
    printf '%s\t%s' "$key" "$ses" > "$D/c/$name.meta"
    echo 0123abcd; exit 0;;
  start|unpause) [ -f "$D/c/$n.state" ] || exit 1; echo running > "$D/c/$n.state"; exit 0;;
  stop) [ -f "$D/c/$n.state" ] && echo exited > "$D/c/$n.state"; exit 0;;
  pause) [ -f "$D/c/$n.state" ] || exit 1; echo paused > "$D/c/$n.state"; exit 0;;
  rm|delete) rm -f "$D/c/$n.state" "$D/c/$n.ws" "$D/c/$n.meta"; exit 0;;
  stats) if [ -f "$D/c/$n.stats" ]; then cat "$D/c/$n.stats"; else echo '{"CPUPerc":"1.00%","MemUsage":"1MiB / 1GiB","PIDs":"1"}'; fi; exit 0;;
  logs) echo "fake output of $n"; exit 0;;
  commit) echo x > "$D/img/$(printf '%s' "$n" | tr ':/' '__')"; exit 0;;
  image) if [ -f "$D/img/$(printf '%s' "$n" | tr ':/' '__')" ]; then echo sha256:feed; exit 0; fi; exit 1;;
  ps)
    for s in "$D"/c/*.state; do
      [ -f "$s" ] || continue
      b=$(basename "$s" .state)
      printf '%s\t%s\t%s\n' "$b" "$(cat "$D/c/$b.meta" 2>/dev/null)" "$(cat "$s")"
    done; exit 0;;
  exec)
    while [ $# -gt 0 ]; do
      case "$1" in
        --interactive|--tty) shift;;
        --workdir|--user|--env) shift 2;;
        *) break;;
      esac
    done
    b="$1"; shift
    ws=$(cat "$D/c/$b.ws" 2>/dev/null)
    [ "$(cat "$D/c/$b.state" 2>/dev/null)" = running ] || { echo "container $b is not running" >&2; exit 1; }
    if [ "$1" = git ]; then exec git "$2" "$ws"; fi
    if [ "$1" = /bin/sh ] && [ "$2" = -c ]; then
      case "$3" in *proc/net/tcp*) cat "$D/c/$b.ports" 2>/dev/null; exit 0;; esac
      if [ -n "$ws" ]; then cd "$ws" 2>/dev/null || true; fi
      s=$(printf '%s' "$3" | sed "s#/workspace#$ws#g")
      exec /bin/sh -c "$s"
    fi
    exit 0;;
esac
exit 0
"##;

struct Fake {
    cli: PathBuf,
    dir: PathBuf,
}

impl Fake {
    fn new(root: &Path) -> Fake {
        let dir = root.join("fake2");
        std::fs::create_dir_all(dir.join("c")).unwrap();
        let cli = dir.join("docker");
        std::fs::write(&cli, FAKE.replace("@DIR@", &dir.to_string_lossy())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake { cli, dir }
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }
    fn state(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join("c").join(format!("{name}.state")))
            .ok()
            .map(|s| s.trim().to_string())
    }
    fn kill(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.join("c").join(format!("{name}.state")));
    }
    fn put(&self, name: &str, ext: &str, text: &str) {
        std::fs::write(self.dir.join("c").join(format!("{name}.{ext}")), text).unwrap();
    }
}

fn box_name(key: &str) -> String {
    format!("vk-{}", vk_sandbox::runner::short_id(key))
}

/// A task worktree on branch `u/<name>`.
fn wt(e: &Env, name: &str) -> PathBuf {
    let p = e.root.join(format!("wt-{name}"));
    git(
        &e.checkout,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &format!("u/{name}"),
            p.to_str().unwrap(),
            "main",
        ],
    );
    p.canonicalize().unwrap()
}

fn ctr_req() -> IsoRequest {
    IsoRequest {
        level: IsolationLevel::Container,
        network: NetworkProfile::None,
        yolo: false,
        harnesses: vec!["claude".into()],
        image: Some("alpine:3.20".into()),
        ..Default::default()
    }
}

fn ctr_iso() -> Isolation {
    Isolation {
        level: IsolationLevel::Container,
        provider: "docker".into(),
        network: "none".into(),
        yolo: false,
        scope: "pane".into(),
        visible_roots: vec![],
    }
}

/// A container task box on the fake runtime, plus its (record-only) pane.
async fn ctr_task(e: &Env, f: &Fake, task: &str, name: &str) -> (Arc<TaskBox>, String, PathBuf) {
    let w = wt(e, name);
    e.server.sandbox.set_container_runtime(f.cli.clone());
    let pane = e.task_with_pane(task, ctr_iso());
    let b = prepare_box(&e.server, task, Some(task), &w, ctr_req())
        .await
        .unwrap();
    (b, pane, w)
}

/// A live run record in `pane` (no process).
fn add_run(e: &Env, pane: &str, resume: bool, working: bool) -> AgentRun {
    let mut c = e.server.core.lock().unwrap();
    let mut r = crate::agents::new_run(
        &mut c,
        pane,
        crate::agents::Harness::Claude,
        "process",
        StateSource::Process,
        0.6,
    );
    if resume {
        r.resume_argv = vec!["claude".into(), "--resume".into(), "s1".into()];
    }
    if working {
        r.execution.value = Execution::Working;
    }
    let mut tx = Tx::new();
    tx.run(r.clone());
    e.server.commit(&mut c, tx).unwrap();
    r
}

fn events_of(e: &Env, kind: &str) -> Vec<Value> {
    e.server.with_core(|c| {
        c.store
            .events_after(0, 100_000, &[kind.to_string()])
            .unwrap()
            .into_iter()
            .map(|ev| json!({"subject": ev.subject, "data": ev.data}))
            .collect()
    })
}

async fn wait_event(e: &Env, kind: &str, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..500 {
        if let Some(v) = events_of(e, kind).into_iter().find(|v| pred(v)) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no {kind} event");
}

async fn call(e: &Env, method: &str, p: Value) -> R {
    crate::api::dispatch(&e.server, &e.user_ctx(), method, &p).await
}

#[tokio::test]
async fn confirm_host_yolo_once_per_workspace() {
    let e = Env::new();
    let r = extras::check_host_yolo(&e.server, &e.checkout, false).unwrap_err();
    assert_eq!(r.data.details["reason"], "confirm_host_yolo");
    assert!(r.message.contains("confirm_host_yolo"), "{}", r.message);
    // Confirming records it for the workspace (the repo root, also from a subdirectory).
    std::fs::create_dir_all(e.checkout.join("src")).unwrap();
    extras::check_host_yolo(&e.server, &e.checkout.join("src"), true).unwrap();
    extras::check_host_yolo(&e.server, &e.checkout, false).unwrap();
    assert_eq!(events_of(&e, "sandbox.host_yolo_confirmed").len(), 1);
    // Another workspace asks again.
    let other = e.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    assert!(extras::check_host_yolo(&e.server, &other, false).is_err());
    // task.create's hook only applies to yolo on the host.
    let mut req = IsoRequest {
        level: IsolationLevel::Host,
        yolo: true,
        ..Default::default()
    };
    assert!(extras::check_task_host_yolo(&e.server, &req, &other, &json!({})).is_err());
    req.level = IsolationLevel::Sandbox;
    extras::check_task_host_yolo(&e.server, &req, &other, &json!({})).unwrap();
    req.level = IsolationLevel::Host;
    req.yolo = false;
    extras::check_task_host_yolo(&e.server, &req, &other, &json!({})).unwrap();
    // `isolation.confirm_host_yolo = false` skips the confirmation.
    let cfg = IsolationConfig {
        confirm_host_yolo: false,
        ..Default::default()
    };
    extras::set_cfg(&e.server, cfg);
    extras::check_host_yolo(&e.server, &other, false).unwrap();
    // LaunchOpts carries the flag from params.
    let o = LaunchOpts::from_params(
        &json!({"yolo": true, "isolate": "host", "confirm_host_yolo": true}),
    )
    .unwrap();
    assert!(o.confirm_host_yolo && o.yolo);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn allow_always_with_scope_always_is_global_and_persisted() {
    let e = Env::new();
    let b = prepare_box(
        &e.server,
        "task-glob1",
        Some("task-glob1"),
        &e.checkout,
        req(IsolationLevel::Sandbox, NetworkProfile::HarnessApis, &[]),
    )
    .await
    .unwrap();
    e.task_with_pane("task-glob1", b.isolation.clone());
    let s1 = e.server.clone();
    let a =
        tokio::spawn(async move { egress_ask(&s1, "task-glob1", "pkg.example".into(), 443).await });
    let it = wait_open_interaction(&e.server).await;
    assert!(
        it.body_md
            .as_deref()
            .unwrap_or("")
            .contains("scope: always")
    );
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "interaction.answer", "params": {"interaction": it.id, "decision": "allow_always", "choices": {"scope": "always"}}}).to_string();
    let r = crate::api::handle_line(&e.server, &e.user_ctx(), &line).await;
    assert!(r.contains("\"result\""), "{r}");
    assert_eq!(a.await.unwrap(), AskDecision::AllowAlways);
    assert!(extras::global_entries(&e.server).contains(&"pkg.example:443".to_string()));
    let pol = b.proxy.as_ref().unwrap().policy.read().unwrap().clone();
    assert!(pol.global_allow.contains("pkg.example:443"));
    assert!(!pol.task_allow.contains("pkg.example:443"));
    // A box created later starts with it.
    let b2 = prepare_box(
        &e.server,
        "task-glob2",
        Some("task-glob2"),
        &e.checkout,
        req(IsolationLevel::Sandbox, NetworkProfile::HarnessApis, &[]),
    )
    .await
    .unwrap();
    let pol2 = b2.proxy.as_ref().unwrap().policy.read().unwrap().clone();
    assert!(matches!(
        pol2.check_host("pkg.example", 443),
        vk_sandbox::net::HostVerdict::Allow { rule } if rule.starts_with("global:")
    ));
    // Listed, added and removed by the user.
    let l = call(&e, "sandbox.list", json!({})).await.unwrap();
    assert!(
        l["global_allow"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "pkg.example:443")
    );
    call(
        &e,
        "sandbox.allow",
        json!({"host": "cdn.example", "global": true}),
    )
    .await
    .unwrap();
    assert!(
        b2.proxy
            .as_ref()
            .unwrap()
            .policy
            .read()
            .unwrap()
            .global_allow
            .contains("cdn.example")
    );
    let r = call(&e, "sandbox.disallow", json!({"host": "pkg.example:443"}))
        .await
        .unwrap();
    assert_eq!(r["removed"], true);
    assert!(
        !b.proxy
            .as_ref()
            .unwrap()
            .policy
            .read()
            .unwrap()
            .global_allow
            .contains("pkg.example:443")
    );
    assert!(!extras::global_entries(&e.server).contains(&"pkg.example:443".to_string()));
    // A pane may not change approvals.
    let agent = Ctx {
        client_id: "a".into(),
        kind: "agent".into(),
        pane_scope: Some("p".into()),
        remote: false,
    };
    let line = json!({"jsonrpc": "2.0", "id": 2, "method": "sandbox.disallow", "params": {"host": "cdn.example"}}).to_string();
    let r = crate::api::handle_line(&e.server, &agent, &line).await;
    assert!(r.contains("\"error\""), "{r}");
    teardown(&e.server, "task-glob1");
    teardown(&e.server, "task-glob2");
}

#[tokio::test]
async fn global_scope_needs_the_config_and_the_choice() {
    let e = Env::new();
    let mut it = Interaction {
        id: "i".into(),
        handle: "i1".into(),
        run: String::new(),
        pane: "p".into(),
        kind: InteractionKind::Approval,
        status: InteractionStatus::Open,
        title: String::new(),
        body_md: None,
        action: None,
        questions: vec![],
        plan_md: None,
        answer_channel: AnswerChannel::Native,
        native_ref: None,
        source: StateSource::Structured,
        confidence: 1.0,
        answerable: true,
        gate: true,
        decision_rev: 0,
        delivery: DeliveryState::None,
        delivery_error: None,
        answer: Some(Answer {
            decision: Some(Decision::AllowAlways),
            choices: vec![],
            text: None,
        }),
        answered_by: None,
        answer_key: None,
        opened_at_ms: 0,
        answered_at_ms: None,
        picker: None,
    };
    assert_eq!(decision_of(&it, true), AskDecision::AllowTask);
    it.answer.as_mut().unwrap().text = Some("always".into());
    assert_eq!(decision_of(&it, true), AskDecision::AllowAlways);
    assert_eq!(decision_of(&it, false), AskDecision::AllowTask);
    it.answer.as_mut().unwrap().decision = Some(Decision::Allow);
    assert_eq!(decision_of(&it, true), AskDecision::AllowOnce);
    drop(e);
}

#[tokio::test]
async fn manifests_declare_endpoints_and_custom_auth() {
    let e = Env::new();
    let home = e.server.sandbox.home();
    let (allow, _, _) = extras::manifest_needs(&home, &["claude".into(), "codex".into()]);
    assert!(allow.contains(&"api.anthropic.com".to_string()));
    assert!(allow.contains(&"api.openai.com".to_string()));
    let (_, read, write) = extras::manifest_needs(&home, &["omp".into()]);
    assert!(read.contains(&home.join(".omp")));
    assert!(write.contains(&home.join(".omp/agent/sessions")));
    // Built-in harnesses keep their coded projection rules.
    assert!(extras::declared_auth("claude").is_none());
    // OpenCode (manifest-only) projects what its `[auth]` declares.
    let a = extras::declared_auth("opencode").expect("opencode declares [auth]");
    assert!(a.env.contains(&"OPENAI_API_KEY".to_string()));
    assert!(extras::declared_auth("no-such-harness").is_none());
}

#[tokio::test]
async fn first_use_notice_and_credential_use_boundary_event() {
    let e = Env::new();
    let names = vec!["env:CLAUDE_CODE_OAUTH_TOKEN".to_string()];
    extras::credentials_projected(
        &e.server,
        "k1",
        Some("k1"),
        IsolationLevel::Container,
        "dev",
        &["claude".into()],
        &names,
    );
    extras::credentials_projected(
        &e.server,
        "k2",
        Some("k2"),
        IsolationLevel::Container,
        "dev",
        &["claude".into()],
        &names,
    );
    // An audit record per box, the notice once per harness and level.
    let ba = events_of(&e, "sandbox.boundary_action");
    assert_eq!(ba.len(), 2);
    assert_eq!(ba[0]["data"]["kind"], "credential_use");
    assert_eq!(ba[0]["data"]["outcome"], "applied");
    let n = events_of(&e, "sandbox.credentials_notice");
    assert_eq!(n.len(), 1);
    let msg = n[0]["data"]["message"].as_str().unwrap();
    assert!(
        msg.contains("Claude Code in container will receive your Claude subscription token"),
        "{msg}"
    );
    assert!(msg.contains("Network: dev profile"), "{msg}");
    // Values never appear.
    assert!(!e.all_events_json().contains(FAKE_CLAUDE));
}

#[tokio::test]
async fn runner_lost_ends_runs_cancels_interactions_and_recover_restarts() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASLOST000001";
    let (b, pane, _w) = ctr_task(&e, &f, task, "lost1").await;
    let name = box_name(task);
    assert_eq!(f.state(&name).as_deref(), Some("running"));
    let run = add_run(&e, &pane, false, true);
    // An open boundary request on the pane.
    let r = call(
        &e,
        "sandbox.request",
        json!({"pane": pane, "kind": "copy_out", "path": "README"}),
    )
    .await
    .unwrap();
    let interaction = r["interaction"].as_str().unwrap().to_string();
    extras::observe_state(&e.server, &b, BoxState::Running, None).await;
    // The box dies under the task.
    f.kill(&name);
    extras::observe_state(&e.server, &b, BoxState::Missing, None).await;
    let ended: AgentRun = e
        .server
        .with_core(|c| {
            c.run(&run.id)
                .cloned()
                .or_else(|| c.store.find("run", &run.id).ok().flatten())
        })
        .unwrap();
    assert!(ended.ended_at_ms.is_some());
    assert_eq!(ended.execution.value, Execution::Exited);
    assert_eq!(ended.execution.detail.as_deref(), Some("runner_lost"));
    assert!(
        events_of(&e, "agent.exited")
            .iter()
            .any(|v| v["data"]["reason"] == "runner_lost")
    );
    let lost = wait_event(&e, "sandbox.runner_lost", |_| true).await;
    assert_eq!(lost["data"]["state"], "missing");
    assert!(
        lost["data"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == &json!(run.id))
    );
    // Not resumable (no resume handle): nothing to restart later.
    assert!(lost["data"]["resumable"].as_array().unwrap().is_empty());
    let ba = wait_event(&e, "sandbox.boundary_action", |v| {
        v["data"]["interaction"] == json!(interaction)
    })
    .await;
    assert_eq!(ba["data"]["outcome"], "cancelled");
    assert!(e.server.with_core(|c| {
        !c.model
            .interactions
            .iter()
            .any(|i| i.status == InteractionStatus::Open)
    }));
    // Seeing it gone again is not a second crash.
    extras::observe_state(&e.server, &b, BoxState::Missing, None).await;
    assert_eq!(events_of(&e, "sandbox.runner_lost").len(), 1);
    // Recover: a fresh box (the clone dir survived host-side) and nothing to resume.
    let r = call(&e, "sandbox.recover", json!({"task": task}))
        .await
        .unwrap();
    assert_eq!(r["created"], true, "{r}");
    assert!(r["resumed"].as_array().unwrap().is_empty());
    assert_eq!(f.state(&name).as_deref(), Some("running"));
    wait_event(&e, "sandbox.recovered", |_| true).await;
    // Running again: a later disappearance is a new crash.
    extras::observe_state(&e.server, &b, BoxState::Running, None).await;
    teardown(&e.server, task);
}

#[tokio::test]
async fn a_box_vibeke_stopped_is_not_lost() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASSTOP000001";
    let (b, _pane, _w) = ctr_task(&e, &f, task, "stop1").await;
    extras::observe_state(&e.server, &b, BoxState::Running, None).await;
    call(&e, "sandbox.stop", json!({"task": task}))
        .await
        .unwrap();
    assert_eq!(f.state(&box_name(task)).as_deref(), Some("exited"));
    extras::observe_state(&e.server, &b, BoxState::Stopped, None).await;
    assert!(events_of(&e, "sandbox.runner_lost").is_empty());
    teardown(&e.server, task);
}

#[tokio::test]
async fn idle_boxes_pause_and_wake_on_a_new_pane() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASIDLE000001";
    let (b, pane, w) = ctr_task(&e, &f, task, "idle1").await;
    let name = box_name(task);
    let after = Some(Duration::from_millis(1));
    // A working run keeps the box awake.
    let run = add_run(&e, &pane, false, true);
    extras::observe_state(&e.server, &b, BoxState::Running, after).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    extras::observe_state(&e.server, &b, BoxState::Running, after).await;
    assert_eq!(f.state(&name).as_deref(), Some("running"));
    assert!(!extras::is_paused(&e.server, task));
    // Idle: paused after the idle period (first sighting starts the clock).
    e.server.agents.end_run(&e.server, &run.id, "exited");
    extras::observe_state(&e.server, &b, BoxState::Running, after).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    extras::observe_state(&e.server, &b, BoxState::Running, after).await;
    assert_eq!(f.state(&name).as_deref(), Some("paused"));
    assert!(extras::is_paused(&e.server, task));
    let s = wait_event(&e, "sandbox.suspended", |_| true).await;
    assert_eq!(s["data"]["reason"], "idle");
    // A paused box is not a crash.
    extras::observe_state(&e.server, &b, BoxState::Paused, after).await;
    assert!(events_of(&e, "sandbox.runner_lost").is_empty());
    // A new pane execs into it: it runs first.
    wrap_spawn(
        &e.server,
        "ctask01JEXTRASIDLE000001-pane2",
        w.to_str().unwrap(),
        &["/bin/zsh".into(), "-l".into()],
        vec![],
        Some(task),
    )
    .unwrap();
    assert_eq!(f.state(&name).as_deref(), Some("running"));
    assert!(!extras::is_paused(&e.server, task));
    let r = wait_event(&e, "sandbox.resumed", |_| true).await;
    assert_eq!(r["data"]["reason"], "spawn");
    // Idle suspend off: never paused.
    extras::observe_state(&e.server, &b, BoxState::Running, None).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    extras::observe_state(&e.server, &b, BoxState::Running, None).await;
    assert_eq!(f.state(&name).as_deref(), Some("running"));
    teardown(&e.server, task);
}

#[tokio::test]
async fn resource_pressure_events_are_rate_limited_and_usage_is_listed() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASPRES000001";
    let (b, _pane, _w) = ctr_task(&e, &f, task, "pres1").await;
    let s = BoxStats {
        cpu_percent: 12.0,
        mem_bytes: 1 << 20,
        mem_limit: 1 << 30,
        pids: 1000,
    };
    extras::observe_stats(&e.server, &b, &s, 0.9);
    extras::observe_stats(&e.server, &b, &s, 0.9);
    let ev = events_of(&e, "sandbox.resource_pressure");
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0]["data"]["resource"], "pids");
    assert_eq!(ev[0]["data"]["limit"], 1024.0);
    // Below the threshold: nothing.
    let calm = BoxStats { pids: 3, ..s };
    extras::observe_stats(&e.server, &b, &calm, 0.9);
    assert_eq!(events_of(&e, "sandbox.resource_pressure").len(), 1);
    // The last sample is in sandbox.list (sidebar hover).
    let l = call(&e, "sandbox.list", json!({})).await.unwrap();
    let me = l["sandboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["sandbox"] == task)
        .unwrap()
        .clone();
    assert_eq!(me["usage"]["pids"], 3);
    assert_eq!(me["usage"]["limits"]["pids"], 1024);
    assert_eq!(me["idle_suspended"], false);
    // The poll path reads `stats` from the runtime.
    f.put(
        &box_name(task),
        "stats",
        r#"{"CPUPerc":"5.00%","MemUsage":"10MiB / 2GiB","PIDs":"1020"}"#,
    );
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    let st = c.b().stats().unwrap();
    assert_eq!(st.pids, 1020);
    teardown(&e.server, task);
}

#[tokio::test]
async fn shell_logs_and_prune() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASSHLL000001";
    let (_b, _pane, _w) = ctr_task(&e, &f, task, "shell1").await;
    let name = box_name(task);
    let r = call(&e, "sandbox.shell", json!({"task": task}))
        .await
        .unwrap();
    let argv: Vec<String> = serde_json::from_value(r["argv"].clone()).unwrap();
    assert_eq!(argv[0], f.cli.to_string_lossy());
    assert!(argv.contains(&"--tty".to_string()) && argv.contains(&name));
    assert_eq!(&argv[argv.len() - 2..], ["/bin/sh", "-l"]);
    assert!(
        !r.to_string().contains(FAKE_CLAUDE),
        "no credentials in a debugging shell"
    );
    let l = call(&e, "sandbox.logs", json!({"task": task, "tail": 50}))
        .await
        .unwrap();
    assert!(
        l["container"]
            .as_str()
            .unwrap()
            .contains(&format!("fake output of {name}"))
    );
    assert!(
        l["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "sandbox.created")
    );
    // Orphans: a box of this session with no context, and one of another session.
    f.put("vk-orphan01", "state", "running");
    f.put("vk-orphan01", "meta", "pane:gone\tt");
    f.put("vk-elsewhere", "state", "running");
    f.put("vk-elsewhere", "meta", "x\tother-session");
    let p = call(&e, "sandbox.prune", json!({"dry_run": true}))
        .await
        .unwrap();
    let names: Vec<&str> = p["containers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["container"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"vk-orphan01"), "{p}");
    assert!(
        !names.contains(&"vk-elsewhere") && !names.contains(&name.as_str()),
        "{p}"
    );
    assert!(
        p["containers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["removed"] == false)
    );
    assert_eq!(f.state("vk-orphan01").as_deref(), Some("running"));
    // Pane tokens get none of this.
    let agent = Ctx {
        client_id: "a".into(),
        kind: "agent".into(),
        pane_scope: Some("p".into()),
        remote: false,
    };
    for m in [
        "sandbox.shell",
        "sandbox.logs",
        "sandbox.prune",
        "sandbox.recover",
        "sandbox.push",
    ] {
        let line =
            json!({"jsonrpc": "2.0", "id": 3, "method": m, "params": {"task": task}}).to_string();
        let r = crate::api::handle_line(&e.server, &agent, &line).await;
        assert!(r.contains("\"error\""), "{m}: {r}");
    }
    teardown(&e.server, task);
}

#[tokio::test]
async fn boundary_actions_copy_out_push_and_deny() {
    let e = Env::new();
    let remote = e.root.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare"]);
    git(
        &e.checkout,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASBNDR000001";
    let (_b, pane, _w) = ctr_task(&e, &f, task, "push1").await;
    let boxdir = sbx_root(task).join("workspace");
    // Work in the box: one commit on the task branch.
    std::fs::write(boxdir.join("out.txt"), "result\n").unwrap();
    git(&boxdir, &["add", "out.txt"]);
    git(&boxdir, &["commit", "-q", "-m", "box work"]);
    let box_head = String::from_utf8(
        Command::new("git")
            .args(["-C", boxdir.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    // Copy out (user): one regular file into the outbox.
    let r = call(
        &e,
        "sandbox.copy_out",
        json!({"task": task, "path": "out.txt"}),
    )
    .await
    .unwrap();
    let dst = PathBuf::from(r["detail"]["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "result\n");
    assert!(dst.starts_with(crate::paths::state_root().join("outbox")));
    // Traversal and symlinks are refused.
    assert!(
        call(
            &e,
            "sandbox.copy_out",
            json!({"task": task, "path": "../x"})
        )
        .await
        .is_err()
    );
    std::os::unix::fs::symlink("/etc/hosts", boxdir.join("hosts-link")).unwrap();
    assert!(
        call(
            &e,
            "sandbox.copy_out",
            json!({"task": task, "path": "hosts-link"})
        )
        .await
        .is_err()
    );
    // A contained run asks to push; the user allows; the host pushes the task branch.
    let r = call(&e, "sandbox.request", json!({"pane": pane, "kind": "push"}))
        .await
        .unwrap();
    let id = r["interaction"].as_str().unwrap().to_string();
    let it = e.server.with_core(|c| c.interaction(&id).cloned()).unwrap();
    assert!(
        it.title.contains("u/push1") && it.title.contains("origin"),
        "{}",
        it.title
    );
    answer(&e, &id, "allow").await;
    let ba = wait_event(&e, "sandbox.boundary_action", |v| {
        v["data"]["interaction"] == json!(id)
    })
    .await;
    assert_eq!(ba["data"]["kind"], "push");
    assert_eq!(ba["data"]["outcome"], "applied", "{ba}");
    let pushed = String::from_utf8(
        Command::new("git")
            .args([
                "-C",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/u/push1",
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(pushed, box_head);
    // Denied: nothing happens.
    let r = call(
        &e,
        "sandbox.request",
        json!({"pane": pane, "kind": "copy_out", "path": "out.txt"}),
    )
    .await
    .unwrap();
    let id = r["interaction"].as_str().unwrap().to_string();
    answer(&e, &id, "deny").await;
    let ba = wait_event(&e, "sandbox.boundary_action", |v| {
        v["data"]["interaction"] == json!(id)
    })
    .await;
    assert_eq!(ba["data"]["outcome"], "denied");
    // Unknown kinds and hostile remotes are refused up front.
    assert!(
        call(
            &e,
            "sandbox.request",
            json!({"pane": pane, "kind": "deploy"})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            &e,
            "sandbox.request",
            json!({"pane": pane, "kind": "push", "remote": "--upload-pack=x"})
        )
        .await
        .is_err()
    );
    teardown(&e.server, task);
}

#[tokio::test]
async fn box_ports_are_forwarded_over_the_link_and_become_previews() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASFWRD000001";
    let (b, pane, _w) = ctr_task(&e, &f, task, "fwd1").await;
    // An in-process box end of the link.
    let (a, z) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(a);
    let (zr, zw) = tokio::io::split(z);
    let host = vk_remote::Mux::start(
        ar,
        aw,
        "bridge",
        Some(vk_remote::boxlink::host_acceptor(
            None,
            e.root.join("run-fwd"),
        )),
    );
    let bdir = e.root.join("brokers-fwd");
    tokio::spawn(async move { vk_remote::boxlink::box_side(zr, zw, None, bdir).await });
    let link = Arc::new(container::BoxLink::default());
    link.attach_for_test(host);
    e.server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .links
        .insert(task.to_string(), (link, tokio::spawn(async {})));
    // A dev server "inside the box" (the box side connects to its own loopback).
    let dev = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = dev.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = dev.accept().await {
            tokio::spawn(async move {
                let mut b = [0u8; 3];
                if s.read_exact(&mut b).await.is_ok() {
                    let _ = s.write_all(b"pong").await;
                }
            });
        }
    });
    forward::observe_ports(&e.server, &b, &[port, 22, 3128]).await;
    let h = forward::host_port(&e.server, task, port).expect("forwarded");
    assert_ne!(
        h, port,
        "the box port is busy on the host here: an ephemeral one is used"
    );
    assert!(
        forward::host_port(&e.server, task, 22).is_none(),
        "privileged ports are not previews"
    );
    assert!(
        forward::host_port(&e.server, task, 3128).is_none(),
        "the link's own port is not a preview"
    );
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", h))
        .await
        .unwrap();
    s.write_all(b"pin").await.unwrap();
    let mut out = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&out, b"pong");
    let pv = e
        .server
        .with_core(|c| c.model.previews.iter().find(|p| p.port == h).cloned())
        .expect("preview declared");
    assert_eq!(pv.label.as_deref(), Some(format!("box:{port}").as_str()));
    assert_eq!(pv.task.as_deref(), Some(task));
    let fwd = wait_event(&e, "sandbox.port_forwarded", |_| true).await;
    assert_eq!(fwd["data"]["source"], "discovered");
    // The pane's broker declares the box port: the preview gets the host port.
    let q = forward::rewrite_declare(&e.server, &pane, json!({"port": port})).await;
    assert_eq!(q["port"], json!(h));
    // A discovered port that goes away is dropped; a declared one stays.
    let dev2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p2 = dev2.local_addr().unwrap().port();
    forward::observe_ports(&e.server, &b, &[port, p2]).await;
    assert!(forward::host_port(&e.server, task, p2).is_some());
    forward::observe_ports(&e.server, &b, &[]).await;
    assert!(forward::host_port(&e.server, task, p2).is_none());
    assert!(forward::host_port(&e.server, task, port).is_some());
    // Listed for `sandbox list`; gone with the box.
    let l = call(&e, "sandbox.list", json!({})).await.unwrap();
    let me = l["sandboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["sandbox"] == task)
        .unwrap()
        .clone();
    assert_eq!(me["forwards"][0]["box_port"], json!(port));
    // The discovery scan reads /proc/net/tcp through the runtime.
    f.put(
        &box_name(task),
        "ports",
        "  sl  local_address rem_address   st\n   0: 00000000:0BB8 00000000:0000 0A 0\n",
    );
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    assert_eq!(c.b().listening_ports(), [3000]);
    teardown(&e.server, task);
    assert!(forward::list(&e.server, task).is_empty());
}

#[tokio::test]
async fn templates_are_committed_after_setup_and_reused() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let task = "ctask01JEXTRASTMPL000001";
    let (b, _pane, _w) = ctr_task(&e, &f, task, "tmpl1").await;
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    let mut cfg = IsolationConfig::default();
    let provider = c.b().spec.provider.clone();
    let cli_env = c.b().cli_env.clone();
    let steps0 = vec![("setup".to_string(), "sh .vibeke/setup.sh".to_string())];
    // Off by default.
    let (mut image, mut steps) = ("alpine:3.20".to_string(), steps0.clone());
    assert!(
        pool::apply_template(&e.server, &cfg, &provider, &cli_env, &mut image, &mut steps)
            .is_none()
    );
    cfg.container.template = true;
    let t =
        pool::apply_template(&e.server, &cfg, &provider, &cli_env, &mut image, &mut steps).unwrap();
    assert!(!t.from_template);
    assert_eq!(image, "alpine:3.20");
    assert_eq!(steps, steps0);
    // Setup succeeded: the box is committed as the template.
    pool::after_setup(&e.server, task, Some(task), c.b(), &t.key);
    let tag = vk_sandbox::boxops::template_tag(&t.key);
    assert!(f.log().contains(&format!(
        "commit --change LABEL vibeke.template=1 {} {tag}",
        box_name(task)
    )));
    let ev = wait_event(&e, "sandbox.template_created", |_| true).await;
    assert_eq!(ev["data"]["image"], json!(tag));
    // The next box with the same setup starts from it and skips the steps.
    let t2 =
        pool::apply_template(&e.server, &cfg, &provider, &cli_env, &mut image, &mut steps).unwrap();
    assert!(t2.from_template);
    assert_eq!(image, tag);
    assert!(steps.is_empty());
    // Different steps: a different template, not built yet.
    let (mut image, mut steps) = (
        "alpine:3.20".to_string(),
        vec![("setup".to_string(), "make".to_string())],
    );
    let t3 =
        pool::apply_template(&e.server, &cfg, &provider, &cli_env, &mut image, &mut steps).unwrap();
    assert!(!t3.from_template && t3.key != t.key);
    teardown(&e.server, task);
}

#[tokio::test]
async fn warm_pool_prestarts_a_box_and_the_next_task_claims_it() {
    let e = Env::new();
    let f = Fake::new(&e.root);
    let cfg = IsolationConfig {
        container: vk_sandbox::config::ContainerConfig {
            warm_pool: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    extras::set_cfg(&e.server, cfg);
    let ta = "ctask01JEXTRASWRMA000001";
    let (_ba, _pa, _wa) = ctr_task(&e, &f, ta, "warma").await;
    let ready = wait_event(&e, "sandbox.warm_ready", |_| true).await;
    let slot = ready["subject"]["sandbox"].as_str().unwrap().to_string();
    assert!(slot.starts_with("pool:"));
    assert_eq!(f.state(&box_name(&slot)).as_deref(), Some("running"));
    // Warm boxes are never prune candidates.
    let p = call(&e, "sandbox.prune", json!({"dry_run": true}))
        .await
        .unwrap();
    assert!(!p.to_string().contains(&box_name(&slot)), "{p}");
    // Task B has the same inputs: it adopts the warm box.
    let tb = "ctask01JEXTRASWRMB000001";
    let wb = wt(&e, "warmb");
    let pane_b = e.task_with_pane(tb, ctr_iso());
    let b = prepare_box(&e.server, tb, Some(tb), &wb, ctr_req())
        .await
        .unwrap();
    assert_eq!(b.request.slot.as_deref(), Some(slot.as_str()));
    let BoxRunner::Container(c) = &b.runner else {
        panic!()
    };
    assert_eq!(c.b().spec.name, box_name(&slot));
    wait_event(&e, "sandbox.warm_claimed", |v| {
        v["data"]["slot"] == json!(slot)
    })
    .await;
    // Its private clone was made at claim time, at task B's base.
    let clone_dir = sbx_root(&slot).join("workspace");
    git(&clone_dir, &["status"]);
    let base = String::from_utf8(
        Command::new("git")
            .args(["-C", wb.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let head = String::from_utf8(
        Command::new("git")
            .args(["-C", clone_dir.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(head, base);
    // Panes of task B exec into the adopted box.
    let (argv, _, _) = wrap_spawn(
        &e.server,
        &pane_b,
        wb.to_str().unwrap(),
        &["/bin/zsh".into(), "-l".into()],
        vec![],
        Some(tb),
    )
    .unwrap();
    assert!(argv.contains(&box_name(&slot)));
    teardown(&e.server, ta);
    teardown(&e.server, tb);
}
