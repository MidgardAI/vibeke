use super::*;

fn store_with(rows: &[(&str, i64)]) -> Store {
    let s = Store::open_in_memory().unwrap();
    for (tier, age_days) in rows {
        s.conn
            .execute(
                "INSERT INTO events (ts, type, tier, data_json, v) VALUES (?1, 't', ?2, '{}', 1)",
                params![now_ms() - age_days * 86_400_000, tier],
            )
            .unwrap();
    }
    s
}

fn count_tier(s: &Store, tier: &str) -> i64 {
    s.conn
        .query_row("SELECT COUNT(*) FROM events WHERE tier=?1", [tier], |r| {
            r.get(0)
        })
        .unwrap()
}

#[test]
fn defaults_are_the_spec_values() {
    let r = Retention::default();
    assert_eq!(
        (r.sync_days, r.history_days, r.max_rows),
        (7, 365, 2_000_000)
    );
}

#[test]
fn age_limits_apply_per_tier_and_the_newest_row_stays() {
    // 8 days old sync goes, 6 days old sync stays, 400 days old history goes, 30 days history
    // stays; the last row (9 days old sync) is the newest seq and is never removed.
    let s = store_with(&[
        ("sync", 8),
        ("sync", 6),
        ("history", 400),
        ("history", 30),
        ("sync", 9),
    ]);
    let r = s.prune_with(&Retention::default()).unwrap();
    assert_eq!(r.aged, 2);
    assert_eq!(s.event_count().unwrap(), 3);
}

#[test]
fn configured_windows_are_honoured() {
    let s = store_with(&[("sync", 3), ("sync", 1), ("history", 20), ("history", 0)]);
    let r = s
        .prune_with(&Retention {
            sync_days: 2,
            history_days: 10,
            max_rows: 0,
        })
        .unwrap();
    assert_eq!(r.aged, 2);
}

#[test]
fn row_cap_removes_sync_before_history_oldest_first() {
    let s = store_with(&[
        ("history", 0),
        ("sync", 0),
        ("sync", 0),
        ("history", 0),
        ("sync", 0),
        ("history", 0),
    ]);
    let r = s
        .prune_with(&Retention {
            sync_days: 7,
            history_days: 365,
            max_rows: 4,
        })
        .unwrap();
    assert_eq!((r.aged, r.capped), (0, 2));
    assert_eq!(s.event_count().unwrap(), 4);
    assert_eq!(count_tier(&s, "sync"), 1, "the two oldest sync rows went");
}

#[test]
fn row_cap_reaches_into_history_only_when_sync_is_exhausted() {
    let s = store_with(&[("sync", 0), ("history", 0), ("history", 0), ("history", 0)]);
    let r = s
        .prune_with(&Retention {
            sync_days: 7,
            history_days: 365,
            max_rows: 2,
        })
        .unwrap();
    assert_eq!(r.capped, 2);
    assert_eq!(count_tier(&s, "sync"), 0);
    assert_eq!(s.event_count().unwrap(), 2);
}

#[test]
fn the_cap_never_empties_the_log_so_seq_never_restarts() {
    let s = store_with(&[("sync", 0), ("sync", 0), ("sync", 0)]);
    let last = s.last_seq().unwrap();
    s.prune_with(&Retention {
        sync_days: 0,
        history_days: 0,
        max_rows: 1,
    })
    .unwrap();
    assert_eq!(s.event_count().unwrap(), 1);
    assert_eq!(s.last_seq().unwrap(), last);
}

#[test]
fn legacy_prune_still_works() {
    let s = store_with(&[("sync", 10), ("sync", 0)]);
    assert_eq!(s.prune(7, 365).unwrap(), 1);
}
