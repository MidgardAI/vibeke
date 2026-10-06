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
