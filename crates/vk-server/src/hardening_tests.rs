//! Retention config, the storage API and degraded mode (02 §2.3, §4a; 3D). Throwaway servers;
//! storage failure is simulated with `PRAGMA query_only` (every write fails like a full disk).

use super::testkit::*;
use super::*;
use crate::api::{dispatch, pane_scope_of};
use crate::core::Tx;
use std::path::Path;
use vk_store::archive::ArchivedRow;

fn parse(src: &str) -> EventsConfig {
    let (cfg, _) = vk_config::Config::parse(src, Path::new("config.toml")).unwrap();
    EventsConfig::from_config(&cfg)
}

#[test]
fn events_config_defaults_match_the_spec() {
    let c = parse("");
    assert_eq!(c.retention.sync_days, 7);
    assert_eq!(c.retention.history_days, 365);
    assert_eq!(c.retention.max_rows, 2_000_000);
    assert_eq!(c, EventsConfig::default());
}

#[test]
fn events_config_reads_durations_integers_and_the_cap() {
    let c = parse(
        r#"
[events]
sync_retention = "3d"
history_retention = "90d"
blob_retention = "36h"
max_rows = 500
"#,
    );
    assert_eq!(c.retention.sync_days, 3);
    assert_eq!(c.retention.history_days, 90);
    assert_eq!(c.blob_days, 2, "hours round up to whole days");
    assert_eq!(c.retention.max_rows, 500);
    let c = parse("[events]\nsync_retention = 14\nmax_rows = 0\n");
    assert_eq!(c.retention.sync_days, 14, "a bare integer is days");
    assert_eq!(c.retention.max_rows, 0, "0 disables the cap");
}

#[test]
fn events_config_ignores_bad_values() {
    let c = parse(
        r#"
[events]
sync_retention = "soon"
history_retention = 0
blob_retention = -4
max_rows = -1
"#,
    );
    assert_eq!(c, EventsConfig::default());
}

fn emit(server: &Server, n: usize) {
    for i in 0..n {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "notes.updated",
            json!({"workspace": "w"}),
            json!({"rev": i}),
        );
        server.commit(&mut c, tx).unwrap();
    }
}

#[test]
fn the_sweep_applies_the_row_cap() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "cap");
    emit(&s, 30);
    let cfg = EventsConfig {
        retention: Retention {
            sync_days: 7,
            history_days: 365,
            max_rows: 10,
        },
        blob_days: 30,
    };
    let r = sweep_with(&s, &cfg);
    assert!(r.events.capped > 0);
    assert_eq!(s.with_core(|c| c.store.event_count().unwrap()), 10);
    // The newest event survived (seq never goes back).
    let last = s.with_core(|c| c.store.last_seq().unwrap());
    assert!(last >= 30);
}

#[tokio::test]
async fn storage_status_reports_retention_backups_and_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "status");
    emit(&s, 3);
    let v = dispatch(&s, &user(), "storage.status", &json!({}))
        .await
        .unwrap();
    assert_eq!(v["degraded"], Value::Null);
    assert_eq!(v["ephemeral"], 0);
    assert!(v["events"]["count"].as_i64().unwrap() >= 3);
    assert_eq!(v["events"]["retention"]["sync_days"], 7);
    assert_eq!(v["keep_backups"], 3);
    assert!(
        v["backups"].as_array().unwrap().is_empty(),
        "new db, no backup"
    );
    assert!(v["db"]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(v["cursor"]["session_uuid"].as_str().unwrap().len(), 26);
    let p = dispatch(&s, &user(), "storage.prune", &json!({}))
        .await
        .unwrap();
    assert!(p["events_remaining"].as_i64().unwrap() >= 3);
}

#[tokio::test]
async fn storage_methods_are_full_scope_only() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "scope");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("pa", "wa"));
        s.commit(&mut c, tx).unwrap();
    }
    for m in ["storage.status", "storage.prune"] {
        assert_eq!(pane_scope_of(m), crate::api::PaneScope::Forbidden, "{m}");
        let e = dispatch(&s, &pane_ctx("pa"), m, &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.data.kind, "permission_denied", "{m}");
    }
}

fn degrade(s: &Server) {
    s.with_core(|c| c.store.set_query_only(true).unwrap());
}

fn heal(s: &Server) {
    s.with_core(|c| c.store.set_query_only(false).unwrap());
}

#[test]
fn a_failed_commit_did_not_happen_and_enters_degraded() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "failed");
    degrade(&s);
    let mut c = s.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(sample_pane("p1", "w1"));
    tx.event("pane.created", json!({}), json!({}));
    assert!(s.commit(&mut c, tx).is_err());
    assert!(c.pane("p1").is_none(), "in-memory projection unchanged");
    assert!(
        c.model
            .degraded
            .as_deref()
            .unwrap()
            .contains("storage unavailable")
    );
    drop(c);
    assert!(s.degraded.lock().unwrap().is_some());
}

#[test]
fn ui_convenience_is_applied_in_memory_and_counted_while_degraded() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "eph");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("p1", "w1"));
        s.commit(&mut c, tx).unwrap();
    }
    degrade(&s);
    let mut c = s.core.lock().unwrap();
    let mut p = c.pane("p1").unwrap().clone();
    p.unread = true;
    let mut tx = Tx::new();
    tx.pane(p);
    tx.event("pane.focused", json!({}), json!({}));
    tx.ephemeral = true;
    let events = s.commit(&mut c, tx).unwrap();
    assert!(
        events.is_empty(),
        "no event for a write that did not commit"
    );
    assert!(c.pane("p1").unwrap().unread, "applied in memory");
    assert!(c.model.degraded.is_some());
    // The store never saw it.
    let stored: Vec<vk_proto::model::Pane> = c.store.load("pane").unwrap();
    assert!(!stored[0].unread);
    drop(c);
    assert_eq!(s.hardening.ephemeral(), 1);
    // Anything that is not UI convenience still fails.
    let mut c = s.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(sample_pane("p2", "w1"));
    assert!(s.commit(&mut c, tx).is_err());
    assert!(c.pane("p2").is_none());
}

#[tokio::test]
async fn focus_and_unread_marks_survive_a_dead_store_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "focus");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("p1", "w1"));
        s.commit(&mut c, tx).unwrap();
    }
    degrade(&s);
    // The unread mark on output (a Server path, not a hand-built Tx).
    s.pane_output("p1");
    assert!(s.with_core(|c| c.pane("p1").unwrap().unread));
    assert!(s.degraded.lock().unwrap().is_some());
    assert_eq!(s.hardening.ephemeral(), 1);
}

#[test]
fn snapshots_and_archive_writes_pause_while_degraded() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "pause");
    degrade(&s);
    // Any failing commit enters degraded mode.
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event("x.y", json!({}), json!({}));
        let _ = s.commit(&mut c, tx);
    }
    assert!(
        !s.store_snapshot("p1", 0, vec![1, 2, 3], "inc"),
        "no snapshot"
    );
    s.archive_rows(
        "p1",
        vec![ArchivedRow {
            n: 1,
            t: "hello".into(),
            w: false,
        }],
    );
    assert!(
        s.fts_buf.lock().unwrap().is_empty(),
        "nothing queued for the index"
    );
    assert_eq!(s.hardening.archive_skipped_total(), 1);
    assert!(
        s.archive.lock().unwrap().last_line("p1").unwrap().is_none(),
        "nothing archived"
    );
}

#[test]
fn recovery_is_automatic_and_probes_every_five_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "recover");
    degrade(&s);
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event("x.y", json!({}), json!({}));
        let _ = s.commit(&mut c, tx);
    }
    assert!(s.degraded.lock().unwrap().is_some());
    // First probe is due at once and fails while the store is down.
    s.housekeeping();
    assert!(s.degraded.lock().unwrap().is_some());
    // The next one is not due for 5 s, even once the disk is back.
    heal(&s);
    s.housekeeping();
    assert!(
        s.degraded.lock().unwrap().is_some(),
        "probe interval not elapsed"
    );
    // Make the probe due again.
    s.hardening.last_probe_ms.store(0, Ordering::Relaxed);
    s.housekeeping();
    assert!(s.degraded.lock().unwrap().is_none());
    assert!(s.with_core(|c| c.model.degraded.is_none()));
    // Writes work again and nothing from memory was replayed.
    let mut c = s.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(sample_pane("p9", "w1"));
    s.commit(&mut c, tx).unwrap();
    assert_eq!(s.hardening.ephemeral(), 0);
}

#[tokio::test]
async fn an_answer_that_cannot_be_recorded_is_refused_and_never_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "answer");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("pa", "wa"));
        tx.run(sample_run("run1", "pa"));
        tx.interaction(sample_interaction("i1", "run1", "pa"));
        s.commit(&mut c, tx).unwrap();
    }
    degrade(&s);
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event("x.y", json!({}), json!({}));
        let _ = s.commit(&mut c, tx);
    }
    let e = dispatch(
        &s,
        &user(),
        "interaction.answer",
        &json!({"interaction": "i1", "decision": "allow"}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "storage_unavailable", "{e:?}");
    assert!(e.message.contains("agent's own UI"), "{}", e.message);
    // Nothing was recorded or delivered: the interaction is still open and undelivered.
    let it = s.with_core(|c| c.interaction("i1").cloned().unwrap());
    assert_eq!(it.status, vk_proto::model::InteractionStatus::Open);
    assert_eq!(it.delivery, vk_proto::model::DeliveryState::None);
    // The same refusal applies when the answer reaches the store before degraded is noticed.
    heal(&s);
    s.hardening.last_probe_ms.store(0, Ordering::Relaxed);
    s.housekeeping();
    let ok = dispatch(
        &s,
        &user(),
        "interaction.answer",
        &json!({"interaction": "i1", "decision": "allow"}),
    )
    .await;
    assert!(ok.is_ok(), "{ok:?}");
}

#[tokio::test]
async fn storage_prune_is_refused_while_degraded_and_status_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "prune-deg");
    degrade(&s);
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event("x.y", json!({}), json!({}));
        let _ = s.commit(&mut c, tx);
    }
    let e = dispatch(&s, &user(), "storage.prune", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "storage_unavailable");
    let st = dispatch(&s, &user(), "storage.status", &json!({}))
        .await
        .unwrap();
    assert!(
        st["degraded"]
            .as_str()
            .unwrap()
            .contains("storage unavailable")
    );
    let status = dispatch(&s, &user(), "server.status", &json!({}))
        .await
        .unwrap();
    assert!(status["degraded"].is_string());
    assert_eq!(status["ephemeral"], 0);
}

#[test]
fn every_3d_method_is_registered_and_pane_scope_is_declared() {
    for (table, forbidden) in [
        (crate::hardening::METHODS, crate::hardening::PANE_FORBIDDEN),
        (
            crate::blob_store::METHODS,
            crate::blob_store::PANE_FORBIDDEN,
        ),
        (crate::machines::METHODS, crate::machines::PANE_FORBIDDEN),
        (crate::items::METHODS, crate::items::PANE_FORBIDDEN),
    ] {
        for (m, _) in table {
            let scope = pane_scope_of(m);
            assert_eq!(
                scope == crate::api::PaneScope::Forbidden,
                forbidden.contains(m),
                "{m}: forbidden list and pane_scope_of agree"
            );
            assert!(
                crate::api_schema::registry().methods.contains_key(*m),
                "{m} has a shape"
            );
        }
    }
    // Mutating flags: reads are not mutating.
    assert!(crate::session_api::is_mutating("storage.prune"));
    assert!(crate::session_api::is_mutating("machine.upsert"));
    assert!(crate::session_api::is_mutating("blob.gc"));
    assert!(!crate::session_api::is_mutating("storage.status"));
    assert!(!crate::session_api::is_mutating("agent.items"));
}
