//! Unit tests of `cloud_move.rs`: job states, destinations, records and the pane refusal.

use super::*;
use crate::api::dispatch;
use crate::hardening::testkit::{pane_ctx, sample_pane, server, user};

#[test]
fn transitions() {
    for (from, to, ok) in [
        ("queued", "waiting_turn", true),
        ("queued", "exporting", true),
        ("queued", "done", false),
        ("queued", "queued", false),
        ("queued", "failed", true),
        ("queued", "cancelled", true),
        ("waiting_turn", "creating", true),
        ("creating", "bootstrapping", true),
        ("bootstrapping", "exporting", true),
        ("exporting", "uploading", true),
        ("exporting", "waiting_turn", false),
        ("uploading", "importing", true),
        ("importing", "resuming", true),
        ("resuming", "done", true),
        ("importing", "done", true),
        ("resuming", "importing", false),
        ("done", "failed", false),
        ("failed", "queued", false),
        ("cancelled", "exporting", false),
        ("bogus", "failed", false),
        ("queued", "bogus", false),
    ] {
        assert_eq!(transition_ok(from, to), ok, "{from} -> {to}");
    }
    assert_eq!(STATES.iter().filter(|s| terminal(s)).count(), 3);
    // Every unfinished state can fail or be cancelled.
    for st in STATES.iter().filter(|s| !terminal(s)) {
        assert!(
            transition_ok(st, "failed") && transition_ok(st, "cancelled"),
            "{st}"
        );
    }
}

#[test]
fn destinations() {
    assert_eq!(
        Dest::parse(&json!({"kind": "cloud", "provider": "sprites"})).unwrap(),
        Dest::Cloud {
            provider: Some("sprites".into()),
            box_id: None
        }
    );
    assert_eq!(Dest::parse(&json!("local")).unwrap(), Dest::Local);
    assert_eq!(
        Dest::parse(&json!({"kind": "peer", "peer": "laptop"})).unwrap(),
        Dest::Peer("laptop".into())
    );
    assert!(Dest::parse(&json!({"kind": "peer"})).is_err());
    assert!(Dest::parse(&json!({"kind": "mars"})).is_err());
    assert!(Dest::parse(&Value::Null).is_err());
    let d = Dest::Cloud {
        provider: Some("e2b".into()),
        box_id: Some("e2b/x".into()),
    };
    assert_eq!(Dest::parse(&d.json()).unwrap(), d);
    assert_eq!(d.direction(), "send");
    assert_eq!(Dest::Local.direction(), "bring_back");
}

#[test]
fn job_records_are_additive_json() {
    let j: Job = serde_json::from_value(json!({
        "id": "j1", "direction": "send", "from": {"kind": "local"}, "to": {"kind": "cloud"},
        "state": "queued", "created_at": 1, "updated_at": 1, "future_field": true
    }))
    .unwrap();
    assert_eq!(j.box_id, None);
    assert!(!j.interrupt);
    let v = job_json(&Job {
        box_id: Some("sprites/b1".into()),
        ..j
    });
    assert_eq!(v["box"], "sprites/b1");
    assert!(v.get("box_id").is_none());
    assert!(v.get("error").is_none());
}

#[test]
fn quoting_and_tails() {
    assert_eq!(sh_quote("/workspace/a b"), "'/workspace/a b'");
    assert_eq!(sh_quote("it's"), r"'it'\''s'");
    assert_eq!(tail(b"  boom\x1b[0m \n"), "boom[0m");
    assert!(tail(&[b'x'; 5000]).len() <= MAX_ERROR);
}

fn record(s: &Server, job: &Job) {
    let mut c = s.core.lock().unwrap();
    let mut tx = Tx::new();
    put(&mut tx, job);
    s.commit(&mut c, tx).unwrap();
}

fn sample(id: &str, state: &str) -> Job {
    Job {
        id: id.into(),
        direction: "send".into(),
        pane: Some("p1".into()),
        run: None,
        box_id: None,
        from: json!({"kind": "local"}),
        to: json!({"kind": "cloud"}),
        state: state.into(),
        progress: None,
        error: None,
        result: None,
        interrupt: false,
        source_after: None,
        task: None,
        by: None,
        created_at: now_s(),
        updated_at: now_s(),
    }
}

#[tokio::test]
async fn steps_cancel_and_finish() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "cmove1");
    record(&s, &sample("j1", "queued"));
    assert_eq!(
        step(&s, "j1", "waiting_turn").unwrap().state,
        "waiting_turn"
    );
    assert_eq!(step(&s, "j1", "exporting").unwrap().state, "exporting");
    // Backwards is refused; the same state is a no-op.
    assert!(step(&s, "j1", "creating").is_err());
    assert_eq!(step(&s, "j1", "exporting").unwrap().state, "exporting");
    set_progress(&s, "j1", 5, 10);
    note(&s, "j1", json!({"task": "t1", "box": "fake/b1"}));
    let j = get(&s, "j1").unwrap();
    assert_eq!(j.progress, Some(Progress { done: 5, total: 10 }));
    assert_eq!(j.task.as_deref(), Some("t1"));
    assert_eq!(j.box_id.as_deref(), Some("fake/b1"));

    // Cancelled: the runner's next step stops, and the outcome does not overwrite it.
    let r = cancel(&s, &json!({"id": "j1"})).unwrap();
    assert_eq!(r["job"]["state"], "cancelled");
    let e = step(&s, "j1", "uploading").unwrap_err();
    assert_eq!(e.data.details["reason"], "cancelled");
    assert!(check_cancel(&s, "j1").is_err());
    finish(&s, "j1", Ok(json!({"pane": "p9"})));
    assert_eq!(get(&s, "j1").unwrap().state, "cancelled");
    // Cancelling again is a no-op; a finished job can't be cancelled.
    cancel(&s, &json!({"id": "j1"})).unwrap();
    record(&s, &sample("j2", "resuming"));
    finish(&s, "j2", Ok(json!({"pane": "p9"})));
    let j2 = get(&s, "j2").unwrap();
    assert_eq!(j2.state, "done");
    assert_eq!(j2.result.unwrap()["pane"], "p9");
    assert!(cancel(&s, &json!({"id": "j2"})).is_err());
    record(&s, &sample("j3", "importing"));
    finish(
        &s,
        "j3",
        Err(err(ErrorKind::Conflict, "the import in the box failed")),
    );
    let j3 = get(&s, "j3").unwrap();
    assert_eq!(j3.state, "failed");
    assert_eq!(j3.error.unwrap()["kind"], "conflict");
    assert!(cancel(&s, &json!({"id": "nope"})).is_err());

    // Every change was announced with the whole record.
    let states: Vec<String> = s
        .with_core(|c| {
            c.store
                .events_after(0, 10_000, &["cloud.job".to_string()])
                .unwrap()
        })
        .into_iter()
        .filter(|e| e.data["id"] == "j1")
        .map(|e| e.data["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        states.first().map(String::as_str),
        Some("queued"),
        "{states:?}"
    );
    assert_eq!(states.last().map(String::as_str), Some("cancelled"));
}

#[tokio::test]
async fn leftover_jobs_fail_once_and_panes_cannot_move() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "cmove2");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("p1", "w1"));
        s.commit(&mut c, tx).unwrap();
    }
    record(&s, &sample("old", "uploading"));
    let r = dispatch(&s, &user(), "cloud.jobs", &json!({}))
        .await
        .unwrap();
    let jobs = r["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["state"], "failed");
    assert!(
        jobs[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("restarted")
    );
    // Every cloud method is the user's.
    for m in ["cloud.move", "cloud.jobs", "cloud.cancel"] {
        let e = dispatch(
            &s,
            &pane_ctx("p1"),
            m,
            &json!({"id": "old", "to": {"kind": "local"}}),
        )
        .await
        .unwrap_err();
        assert_eq!(e.data.kind, "permission_denied", "{m}");
    }
    // Nor can a pane point a new task at an existing cloud box.
    let e = dispatch(
        &s,
        &pane_ctx("p1"),
        "task.create",
        &json!({"title": "t", "isolate": "cloud", "box": "fake/other"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "permission_denied", "{e:?}");
    assert!(e.message.contains("cloud box"), "{}", e.message);
    // A host pane can't be brought back; bad destinations are refused before anything runs.
    let e = dispatch(
        &s,
        &user(),
        "cloud.move",
        &json!({"pane": "p1", "to": {"kind": "local"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "invalid_params");
    let e = dispatch(
        &s,
        &user(),
        "cloud.move",
        &json!({"pane": "p1", "to": {"kind": "cloud"}, "source_after": "explode"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "invalid_params");
}

#[test]
fn bring_back_marks_only_what_moved() {
    assert_eq!(keep_leftovers_reason(0, 0), None);
    assert!(
        keep_leftovers_reason(1, 0)
            .unwrap()
            .contains("not exported")
    );
    assert!(keep_leftovers_reason(0, 2).unwrap().contains("not written"));
    assert!(keep_leftovers_reason(1, 2).is_some());
    let m = crate::sandbox::cloud::mark_synced_script("/workspace", "abc");
    assert!(m.contains(crate::sandbox::cloud::SYNCED_MARK), "{m}");
    assert!(m.contains("abc"), "{m}");
}
