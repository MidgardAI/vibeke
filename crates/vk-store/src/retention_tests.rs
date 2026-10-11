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

fn put(s: &Store, kind: &str, id: &str, closed: bool, json: Value, age_days: i64) {
    s.conn
        .execute(
            "INSERT INTO entities (kind, id, json, closed, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                kind,
                id,
                json.to_string(),
                closed as i64,
                now_ms() - age_days * 86_400_000
            ],
        )
        .unwrap();
}

fn ids(s: &Store, kind: &str) -> Vec<String> {
    let mut st = s
        .conn
        .prepare("SELECT id FROM entities WHERE kind=?1 ORDER BY id")
        .unwrap();
    st.query_map([kind], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn closed_history_ages_out_and_open_rows_stay() {
    use serde_json::json;
    let s = Store::open_in_memory().unwrap();
    put(&s, "task", "t-open", false, json!({}), 100);
    // Old closed runs go, except the one of a task that is still open.
    put(&s, "run", "r-old", true, json!({}), 100);
    put(&s, "run", "r-parked", true, json!({"task": "t-open"}), 100);
    put(&s, "run", "r-recent", true, json!({}), 5);
    put(&s, "run", "r-live", false, json!({}), 100);
    put(&s, "interaction", "i-old", true, json!({}), 100);
    put(&s, "interaction", "i-open", false, json!({}), 100);
    put(&s, "pane", "p-old", true, json!({}), 100);
    // Records of the removed run go with it; those of kept runs stay.
    put(
        &s,
        "turn_usage",
        "u-old",
        false,
        json!({"run": "r-old"}),
        100,
    );
    put(
        &s,
        "turn_usage",
        "u-kept",
        false,
        json!({"run": "r-parked"}),
        100,
    );
    put(
        &s,
        "tool_item",
        "x-old",
        true,
        json!({"run_id": "r-old"}),
        100,
    );
    put(
        &s,
        "tool_item",
        "x-live",
        true,
        json!({"run_id": "r-live"}),
        100,
    );
    let removed = s.prune_closed(now_ms() - 30 * 86_400_000).unwrap();
    assert_eq!(removed, 5);
    assert_eq!(ids(&s, "run"), ["r-live", "r-parked", "r-recent"]);
    assert_eq!(ids(&s, "interaction"), ["i-open"]);
    assert!(ids(&s, "pane").is_empty());
    assert_eq!(ids(&s, "turn_usage"), ["u-kept"]);
    assert_eq!(ids(&s, "tool_item"), ["x-live"]);
}

#[test]
fn ended_turns_go_with_their_items() {
    use serde_json::json;
    let s = Store::open_in_memory().unwrap();
    put(
        &s,
        "stream_turn",
        "old",
        false,
        json!({"ended_at_ms": 10}),
        0,
    );
    put(
        &s,
        "stream_turn",
        "open",
        false,
        json!({"ended_at_ms": null}),
        0,
    );
    put(&s, "stream_item", "a", false, json!({"turn_id": "old"}), 0);
    put(&s, "stream_item", "b", false, json!({"turn_id": "old"}), 0);
    put(&s, "stream_item", "c", false, json!({"turn_id": "open"}), 0);
    assert_eq!(
        s.prune_ended_turns("stream_turn", "stream_item", 5)
            .unwrap(),
        0
    );
    assert_eq!(
        s.prune_ended_turns("stream_turn", "stream_item", 11)
            .unwrap(),
        3
    );
    assert_eq!(ids(&s, "stream_turn"), ["open"]);
    assert_eq!(ids(&s, "stream_item"), ["c"]);
    // The prune query uses the index rather than a scan per turn.
    let plan: Vec<String> = {
        let mut st = s
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM entities WHERE kind='stream_item' AND json_extract(json, '$.turn_id')='x'",
            )
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert!(plan.iter().any(|p| p.contains("entities_turn")), "{plan:?}");
}
