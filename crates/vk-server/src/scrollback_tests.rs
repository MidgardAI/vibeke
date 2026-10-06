//! In-process tests for archive retention wiring and `scrollback.forget` (02 "Archive search as
//! implemented", 09 §9.3). Runs against a throwaway server; no holders are started.

use crate::api::{Ctx, dispatch};
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use serde_json::{Value, json};
use std::sync::{Arc, Once};
use vk_store::archive::ArchivedRow;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-scrollback-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            std::env::set_var("VIBEKE_NO_OPEN", "1");
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

fn ctx_full() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
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
            env: vec![],
            shims: false,
        };
        Env {
            _dir: dir,
            server: Server::new(paths, opts).unwrap(),
        }
    }

    /// Archive `chunks` x 1000 rows for `pane` (rotates at ~1 MiB) and flush archive + FTS.
    fn fill(&self, pane: &str, chunks: u64) {
        for chunk in 0..chunks {
            let rows = (0..1000)
                .map(|i| ArchivedRow {
                    n: chunk * 1000 + i,
                    t: format!("needle {pane} {} {}", chunk * 1000 + i, "x".repeat(40)),
                    w: false,
                })
                .collect();
            self.server.archive_rows(pane, rows);
            self.server.housekeeping();
        }
    }

    fn fts_rows(&self, pane: &str) -> usize {
        self.server.with_core(|c| {
            c.store
                .fts_search("needle", Some(pane), 1_000_000)
                .unwrap()
                .len()
        })
    }

    async fn call(&self, method: &str, p: Value) -> crate::api::R {
        dispatch(&self.server, &ctx_full(), method, &p).await
    }
}

#[test]
fn retention_removes_segments_and_fts_rows_together() {
    let e = Env::new();
    e.fill("p1", 30);
    e.fill("p2", 1);
    assert_eq!(e.fts_rows("p1"), 30_000);
    e.server.archive_retention_with(1, 0);
    let left = e.server.archive.lock().unwrap().first_line("p1").unwrap();
    let left = left.expect("newest segment survives");
    assert!(left > 0, "oldest segments are gone");
    let rows = e.fts_rows("p1");
    assert_eq!(
        rows as u64,
        30_000 - left,
        "FTS rows match the segments on disk"
    );
    assert_eq!(e.fts_rows("p2"), 1000);
}

#[tokio::test]
async fn forget_scopes_dry_run_and_idempotence() {
    let e = Env::new();
    e.fill("p1", 2);
    e.fill("p2", 1);
    e.server.with_core(|c| {
        c.store
            .fts_register_panes(&[
                (
                    "p1".into(),
                    "w1".into(),
                    "t".into(),
                    "w1:p1".into(),
                    "a".into(),
                ),
                (
                    "p2".into(),
                    "w2".into(),
                    "t".into(),
                    "w2:p2".into(),
                    "b".into(),
                ),
            ])
            .unwrap();
    });
    // Exactly one scope.
    assert!(e.call("scrollback.forget", json!({})).await.is_err());
    assert!(
        e.call("scrollback.forget", json!({"all": true, "pane": "p1"}))
            .await
            .is_err()
    );
    // Dry run changes nothing.
    let d = e
        .call(
            "scrollback.forget",
            json!({"workspace": "w1", "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(d["fts_rows_deleted"], 2000);
    assert_eq!(e.fts_rows("p1"), 2000);
    // Workspace scope: only p1.
    let r = e
        .call("scrollback.forget", json!({"workspace": "w1"}))
        .await
        .unwrap();
    assert_eq!(r["fts_rows_deleted"], 2000);
    assert_eq!(r["archive_panes_dropped"], 1);
    assert_eq!(e.fts_rows("p1"), 0);
    assert_eq!(e.fts_rows("p2"), 1000);
    assert!(
        e.server
            .archive
            .lock()
            .unwrap()
            .read("p1", 0, 10)
            .unwrap()
            .is_empty()
    );
    // The event carries counts, not text.
    let ev = e.server.with_core(|c| c.store.last_seq().unwrap());
    assert!(ev > 0);
    // Idempotent.
    let again = e
        .call("scrollback.forget", json!({"workspace": "w1"}))
        .await
        .unwrap();
    assert_eq!(again["segments_deleted"], 0);
    assert_eq!(again["fts_rows_deleted"], 0);
    // All.
    let r = e
        .call("scrollback.forget", json!({"all": true}))
        .await
        .unwrap();
    assert_eq!(r["fts_rows_deleted"], 1000);
    assert_eq!(e.fts_rows("p2"), 0);
    // Unknown pane and path-like ids are refused.
    assert!(
        e.call("scrollback.forget", json!({"pane": "nope"}))
            .await
            .is_err()
    );
    assert!(
        e.call("scrollback.forget", json!({"pane": "../x"}))
            .await
            .is_err()
    );
}

/// Review finding 7: a housekeeping pass paused right after draining the FTS buffer must not
/// re-insert its batch after a `forget` of that pane: the forget waits for the batch (both run
/// under the archive lock), then deletes the batch's rows with the rest.
#[test]
fn in_flight_fts_batch_cannot_resurrect_forgotten_text() {
    use std::sync::mpsc;
    let e = Env::new();
    e.fill("p1", 1);
    e.fill("p2", 1);
    // A batch that is archived but not indexed yet.
    let rows = (1000..1100)
        .map(|n| ArchivedRow {
            n,
            t: format!("needle p1 secret {n}"),
            w: false,
        })
        .collect();
    e.server.archive_rows("p1", rows);
    let (drained_tx, drained_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    *e.server.after_fts_drain.lock().unwrap() = Some(Box::new(move || {
        let _ = drained_tx.send(());
        let _ = go_rx.lock().unwrap().recv();
    }));
    let srv = e.server.clone();
    let hk = std::thread::spawn(move || srv.housekeeping());
    drained_rx.recv().unwrap();
    // Housekeeping holds its private batch. Forget the pane meanwhile.
    let srv = e.server.clone();
    let fg = std::thread::spawn(move || {
        crate::search::forget(&srv, &ctx_full(), &json!({"pane": "p1"}))
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !fg.is_finished(),
        "forget must wait for the in-flight FTS batch"
    );
    go_tx.send(()).unwrap();
    hk.join().unwrap();
    let r = fg.join().unwrap().unwrap();
    *e.server.after_fts_drain.lock().unwrap() = None;
    assert_eq!(r["fts_rows_deleted"], 1100, "{r}");
    // Nothing of p1 is searchable, now or after the next flush; p2 is untouched.
    e.server.housekeeping();
    assert_eq!(e.fts_rows("p1"), 0);
    assert_eq!(e.fts_rows("p2"), 1000);
}

fn put_pane(e: &Env, id: &str, ws: &str) {
    let p = vk_proto::model::Pane {
        id: id.into(),
        handle: id.into(),
        tab: "tab".into(),
        workspace: ws.into(),
        title: None,
        auto_title: String::new(),
        cwd: None,
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
        isolation: Default::default(),
        browser: None,
        jj: None,
    };
    let mut c = e.server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.pane(p);
    e.server.commit(&mut c, tx).unwrap();
}

fn focus(e: &Env, pane: &str) {
    e.server.clients.lock().unwrap().insert(
        "tui1".into(),
        crate::ClientState {
            kind: "tui".into(),
            focus: crate::ClientFocus {
                pane: Some(pane.into()),
                ..Default::default()
            },
            last_active: Some(std::time::Instant::now()),
            ..Default::default()
        },
    );
}

/// Review finding 6: the dry run returns the canonical plan (resolved ids, absolute cutoff,
/// digest) and the confirmed call executes exactly that. Focus moving from A to B between the
/// dry run and the confirmation deletes A only; resending `@focused` with the old plan is
/// refused; a workspace that gained a pane, or a relative cutoff, no longer matches either.
#[tokio::test]
async fn forget_executes_the_confirmed_plan_after_a_focus_change() {
    let e = Env::new();
    put_pane(&e, "pa", "w1");
    put_pane(&e, "pb", "w1");
    e.fill("pa", 1);
    e.fill("pb", 1);
    focus(&e, "pa");
    let plan = e
        .call(
            "scrollback.forget",
            json!({"pane": "@focused", "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(plan["scope"], json!({"pane": "pa"}));
    assert_eq!(plan["pane_ids"], json!(["pa"]));
    let digest = plan["plan"].as_str().unwrap().to_string();
    assert!(digest.starts_with("fp1-"));
    // The user is looking at the prompt; focus moves to B.
    focus(&e, "pb");
    // The original parameters with the old plan no longer resolve the same way: refused.
    let err = e
        .call(
            "scrollback.forget",
            json!({"pane": "@focused", "plan": digest}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "conflict");
    assert_eq!(e.fts_rows("pa"), 1000);
    assert_eq!(e.fts_rows("pb"), 1000);
    // The canonical plan deletes exactly what was shown.
    let mut confirmed = plan["scope"].clone();
    confirmed["plan"] = json!(digest);
    let r = e.call("scrollback.forget", confirmed).await.unwrap();
    assert_eq!(r["fts_rows_deleted"], 1000);
    assert_eq!(e.fts_rows("pa"), 0);
    assert_eq!(e.fts_rows("pb"), 1000);

    // Workspace: a pane added after the dry run changes the plan.
    let plan = e
        .call(
            "scrollback.forget",
            json!({"workspace": "w1", "dry_run": true}),
        )
        .await
        .unwrap();
    put_pane(&e, "pc", "w1");
    let mut confirmed = plan["scope"].clone();
    confirmed["plan"] = plan["plan"].clone();
    let err = e.call("scrollback.forget", confirmed).await.unwrap_err();
    assert_eq!(err.data.kind, "conflict");
    assert_eq!(e.fts_rows("pb"), 1000);

    // `before`: the plan carries an absolute cutoff; a relative one re-evaluated later doesn't
    // match it.
    let plan = e
        .call(
            "scrollback.forget",
            json!({"before": "1d", "dry_run": true}),
        )
        .await
        .unwrap();
    let cutoff = plan["scope"]["before"].as_i64().unwrap();
    assert!(cutoff > 0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let err = e
        .call(
            "scrollback.forget",
            json!({"before": "1d", "plan": plan["plan"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "conflict");
    let r = e
        .call(
            "scrollback.forget",
            json!({"before": cutoff, "plan": plan["plan"]}),
        )
        .await
        .unwrap();
    assert_eq!(r["scope"]["before"], cutoff);
}

#[tokio::test]
async fn forget_is_not_available_to_pane_tokens() {
    let e = Env::new();
    let ctx = Ctx {
        client_id: "c-pane".into(),
        kind: "cli".into(),
        pane_scope: Some("pane-a".into()),
        remote: false,
    };
    let r = dispatch(&e.server, &ctx, "scrollback.forget", &json!({"all": true})).await;
    assert!(r.is_err());
}
