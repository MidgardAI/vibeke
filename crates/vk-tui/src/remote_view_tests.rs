use super::*;
use crate::app::test_app;
use tokio::sync::mpsc;

fn acks(rx: &mut mpsc::UnboundedReceiver<ClientFrame>) -> Vec<(String, u64)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        if let ClientFrame::Ack { pane, rev, .. } = f {
            v.push((pane, rev));
        }
    }
    v
}

#[test]
fn frame_rate_follows_rtt() {
    assert_eq!(target_hz(60, None), 60);
    assert_eq!(target_hz(60, Some(1)), 60); // 1000 / 8 = 125 > 60
    assert_eq!(target_hz(60, Some(23)), 52); // 1000 / (11 + 8)
    assert_eq!(target_hz(60, Some(100)), 17); // 1000 / 58
    assert_eq!(target_hz(60, Some(400)), 4); // 1000 / 208
    assert_eq!(target_hz(60, Some(10_000)), 1);
    for _ in 0..100 {
        let j = jitter();
        assert!((0.8..1.2).contains(&j), "{j}");
    }
}

#[test]
fn unfocused_remote_pane_is_paced_to_4hz() {
    let (mut app, mut rxs) = test_app(2);
    app.cur = 0; // nothing on machine 1 is focused
    // A burst of 10 frames: the first ack goes at once, the rest are owed.
    for rev in 1..=10 {
        ack(&mut app, 1, "p1".into(), 1, rev);
    }
    assert_eq!(acks(&mut rxs[1]), vec![("p1".into(), 1)]);
    assert_eq!(owed(&app, 1, "p1"), 9);
    // Nothing more before the interval…
    release_due(&mut app, Instant::now());
    assert!(acks(&mut rxs[1]).is_empty());
    // …then exactly one ack per interval.
    let later = Instant::now() + unfocused_ack_interval();
    release_due(&mut app, later);
    assert_eq!(acks(&mut rxs[1]), vec![("p1".into(), 2)]);
    release_due(&mut app, later);
    assert!(acks(&mut rxs[1]).is_empty(), "one per interval");
    // The deadline is armed for the next release.
    let d = app.deadlines(Instant::now());
    assert!(d.next().is_some());
    // Over one simulated second at most 4 acks go out (4 Hz steady state).
    let mut t = later;
    let mut sent = 0;
    for _ in 0..40 {
        t += Duration::from_millis(25);
        release_due(&mut app, t);
        sent += acks(&mut rxs[1]).len();
    }
    assert!(sent <= 4, "{sent} acks in one second");
}

#[test]
fn focused_and_local_panes_ack_immediately_and_focus_flushes() {
    let (mut app, mut rxs) = test_app(2);
    // Local machine: never paced.
    for rev in 1..=5 {
        ack(&mut app, 0, "p2".into(), 1, rev);
    }
    assert_eq!(acks(&mut rxs[0]).len(), 5);
    // Remote, unfocused: paced; focusing it releases everything owed.
    app.cur = 0;
    for rev in 1..=4 {
        ack(&mut app, 1, "p1".into(), 1, rev);
    }
    assert_eq!(acks(&mut rxs[1]).len(), 1);
    app.cur = 1;
    app.machines[1].focus.pane = Some("p1".into());
    release_due(&mut app, Instant::now());
    assert_eq!(
        acks(&mut rxs[1]),
        vec![("p1".into(), 2), ("p1".into(), 3), ("p1".into(), 4)]
    );
    assert_eq!(owed(&app, 1, "p1"), 0);
    // Focused remote pane: immediate.
    ack(&mut app, 1, "p1".into(), 1, 5);
    assert_eq!(acks(&mut rxs[1]), vec![("p1".into(), 5)]);
}

#[test]
fn disconnect_drops_owed_acks() {
    let (mut app, mut rxs) = test_app(2);
    app.cur = 0;
    for rev in 1..=3 {
        ack(&mut app, 1, "p1".into(), 1, rev);
    }
    acks(&mut rxs[1]);
    app.machines[1].tx = None;
    app.on_disconnected(1);
    assert_eq!(owed(&app, 1, "p1"), 0);
}

#[test]
fn sidebar_suffix_shows_rtt_degraded_and_last_seen() {
    let (mut app, _rxs) = test_app(2);
    let info = std::sync::Arc::new(std::sync::Mutex::new(LinkInfo {
        state: "connected".into(),
        rtt_ms: Some(23),
        last_seen_ms: Some(now_ms()),
    }));
    let i2 = info.clone();
    app.machines[1].link = Some(Arc::new(move || i2.lock().unwrap().clone()));
    assert_eq!(status_suffix(&app, 1), (" 23ms".to_string(), false));
    {
        let mut g = info.lock().unwrap();
        g.state = "degraded".into();
        g.rtt_ms = Some(512);
    }
    assert_eq!(
        status_suffix(&app, 1),
        (" degraded 512ms".to_string(), true)
    );
    {
        let mut g = info.lock().unwrap();
        g.state = "offline".into();
        g.last_seen_ms = Some(now_ms() - 4 * 60_000);
    }
    app.machines[1].tx = None;
    app.machines[1].status = "offline".into();
    assert_eq!(
        status_suffix(&app, 1).0,
        " offline · last seen 4m ago".to_string()
    );
    // Without a probe the time of the drop is used.
    app.machines[1].link = None;
    app.remote.lost_at_ms.insert(1, now_ms() - 42_000);
    assert_eq!(
        status_suffix(&app, 1).0,
        " offline · last seen 42s ago".to_string()
    );
}

fn ev(kind: &str, pane: &str, data: Value) -> Value {
    json!({"seq": 1, "type": kind, "subject": {"pane": pane, "run": format!("r-{pane}")}, "data": data})
}

#[test]
fn away_summary_collapses_per_pane() {
    let events = json!([
        ev(
            "agent.state_changed",
            "a",
            json!({"from": "idle", "to": "working"})
        ),
        ev(
            "agent.state_changed",
            "a",
            json!({"from": "working", "to": "idle"})
        ),
        ev(
            "agent.state_changed",
            "b",
            json!({"from": "working", "to": "idle"})
        ),
        // c finished, then asked for approval: the latest state wins.
        ev(
            "agent.state_changed",
            "c",
            json!({"from": "working", "to": "idle"})
        ),
        ev("interaction.opened", "c", json!({"kind": "approval"})),
        // d is still working: not news.
        ev(
            "agent.state_changed",
            "d",
            json!({"from": "idle", "to": "working"})
        ),
    ]);
    assert_eq!(
        summarize(&events).as_deref(),
        Some("1 needs approval, 2 agents finished")
    );
    assert_eq!(summarize(&json!([])), None);
    assert_eq!(
        summarize(&json!([ev(
            "agent.state_changed",
            "x",
            json!({"from": "idle", "to": "working"})
        )])),
        None
    );
    let two = json!([
        ev("interaction.opened", "a", json!({"kind": "approval"})),
        ev("interaction.opened", "b", json!({"kind": "approval"})),
        ev("interaction.opened", "e", json!({"kind": "question"})),
        ev(
            "agent.state_changed",
            "f",
            json!({"from": "working", "to": "error"})
        ),
    ]);
    assert_eq!(
        summarize(&two).as_deref(),
        Some("2 need approval, 1 needs an answer, 1 agent failed")
    );
}

fn commands(rx: &mut mpsc::UnboundedReceiver<ClientFrame>) -> Vec<(u64, Value)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        if let ClientFrame::Command { req, json } = f {
            v.push((req, serde_json::from_str(&json).unwrap()));
        }
    }
    v
}

fn reply(app: &mut App, mi: usize, req: u64, result: Value) {
    app.on_frame(
        mi,
        vk_proto::render::ServerFrame::CommandResult {
            req,
            json: json!({"jsonrpc": "2.0", "id": req, "result": result}).to_string(),
        },
    );
}

#[test]
fn reconnect_replays_what_happened_while_away() {
    let (mut app, mut rxs) = test_app(2);
    // First connect: only the head is asked for.
    on_connected(&mut app, 1);
    let c = commands(&mut rxs[1]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].1["method"], "server.status");
    reply(&mut app, 1, c[0].0, json!({"event_seq": 40}));
    // Link drops; later it comes back.
    app.machines[1].tx = None;
    app.on_disconnected(1);
    let (tx, rx) = mpsc::unbounded_channel();
    app.machines[1].tx = Some(tx);
    rxs[1] = rx;
    on_connected(&mut app, 1);
    let c = commands(&mut rxs[1]);
    assert_eq!(c[0].1["method"], "events.read");
    assert_eq!(c[0].1["params"]["after"], 40);
    assert_eq!(c[1].1["method"], "server.status");
    let before = app.toasts.len();
    reply(
        &mut app,
        1,
        c[0].0,
        json!({"events": [
            ev("agent.state_changed", "a", json!({"from": "working", "to": "idle"})),
            ev("interaction.opened", "b", json!({"kind": "approval"})),
        ]}),
    );
    let t = &app.toasts[before..];
    assert!(
        t.iter()
            .any(|t| t.text == "[m1] While you were away: 1 needs approval, 1 agent finished"),
        "{:?}",
        t.iter().map(|t| &t.text).collect::<Vec<_>>()
    );
    // Local machines never replay.
    on_connected(&mut app, 0);
    assert!(commands(&mut rxs[0]).is_empty());
}
