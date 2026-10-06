//! Event push client: subscription on capable servers, routing, the polling fallback for
//! older servers, lag catch-up and resubscription with a cursor.

use super::*;
use crate::app::test_app;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

fn drain(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<ClientFrame> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        v.push(f);
    }
    v
}

fn methods(frames: &[ClientFrame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|f| match f {
            ClientFrame::Command { json, .. } => serde_json::from_str::<Value>(json)
                .ok()
                .and_then(|v| v["method"].as_str().map(str::to_string)),
            _ => None,
        })
        .collect()
}

/// Answer every pending `events.read` on machine `i` with an empty page.
fn answer_events(app: &mut App, i: usize, frames: &[ClientFrame]) {
    for f in frames {
        if let ClientFrame::Command { req, json } = f
            && json.contains("events.read")
        {
            let resp = json!({"jsonrpc": "2.0", "id": req, "result": {"events": [], "next": 0}});
            app.on_frame(
                i,
                vk_proto::render::ServerFrame::CommandResult {
                    req: *req,
                    json: resp.to_string(),
                },
            );
        }
    }
}

fn ev(seq: i64, kind: &str, subject: Value, data: Value) -> PushedEvent {
    PushedEvent {
        seq,
        kind: kind.into(),
        json: json!({"seq": seq, "ts": now_ms(), "type": kind, "subject": subject, "data": data})
            .to_string(),
    }
}

#[test]
fn subscribes_on_capable_servers_and_polls_older_ones() {
    let (mut app, mut rxs) = test_app(2);
    app.machines[0].features = vec!["event_push".into()];
    app.on_connected(0);
    app.on_connected(1);
    let f0 = drain(&mut rxs[0]);
    let sub = f0
        .iter()
        .find_map(|f| match f {
            ClientFrame::Subscribe { types, after } => Some((types.clone(), *after)),
            _ => None,
        })
        .expect("Subscribe to the capable server");
    assert!(sub.0.contains(&"client.confirm_*".to_string()));
    assert!(sub.0.contains(&"interaction.*".to_string()));
    assert_eq!(sub.1, None, "nothing seen yet");
    assert!(
        drain(&mut rxs[1])
            .iter()
            .all(|f| !matches!(f, ClientFrame::Subscribe { .. })),
        "never sent to an older server (it would drop the connection)"
    );
    assert!(app.push.active(0) && !app.push.active(1));
    // Both get one catch-up events.read.
    crate::gateway::tick(&mut app);
    let (f0, f1) = (drain(&mut rxs[0]), drain(&mut rxs[1]));
    assert!(methods(&f0).contains(&"events.read".to_string()));
    assert!(methods(&f1).contains(&"events.read".to_string()));
    answer_events(&mut app, 0, &f0);
    answer_events(&mut app, 1, &f1);
    assert!(!crate::gateway::polling(&mut app, 0), "push: caught up");
    assert!(
        crate::gateway::polling(&mut app, 1),
        "older server: still polled"
    );
    std::thread::sleep(Duration::from_millis(1050));
    crate::gateway::tick(&mut app);
    assert!(
        !methods(&drain(&mut rxs[0])).contains(&"events.read".to_string()),
        "no polling with push"
    );
    assert!(
        methods(&drain(&mut rxs[1])).contains(&"events.read".to_string()),
        "fallback keeps polling every second"
    );
}

#[test]
fn pushed_events_are_routed() {
    let (mut app, mut rxs) = test_app(1);
    app.machines[0].features = vec!["event_push".into()];
    app.on_connected(0);
    drain(&mut rxs[0]);
    // A confirm request opens the overlay; its resolution closes it.
    on_events(
        &mut app,
        0,
        vec![ev(
            7,
            "client.confirm_requested",
            json!({"confirm": "c1"}),
            json!({"title": "Pair?", "timeout_ms": 60000,
                   "options": [{"id": "ok", "label": "OK"}]}),
        )],
        false,
    );
    assert!(app.gateway.modal());
    on_events(
        &mut app,
        0,
        vec![ev(
            8,
            "client.confirm_resolved",
            json!({"confirm": "c1"}),
            json!({}),
        )],
        false,
    );
    assert!(!app.gateway.modal());
    // Client attach/detach refreshes the devices list; browser events the session list.
    on_events(
        &mut app,
        0,
        vec![
            ev(9, "client.attached", json!({"client": "gw"}), json!({})),
            ev(
                10,
                "browser.session_opened",
                json!({"browser_session": "b1"}),
                json!({}),
            ),
        ],
        false,
    );
    let m = methods(&drain(&mut rxs[0]));
    assert!(m.contains(&"client.list".to_string()), "{m:?}");
    assert!(m.contains(&"browser.list".to_string()), "{m:?}");
    assert_eq!(app.push.per[0].cursor, 10);
    // A screenshot event shows a toast.
    on_events(
        &mut app,
        0,
        vec![ev(
            11,
            "screenshot.captured",
            json!({"browser_session": "b1"}),
            json!({"url": "http://localhost:5173/"}),
        )],
        false,
    );
    assert!(app.toasts.iter().any(|t| {
        t.text
            .contains("screenshot captured http://localhost:5173/")
    }));
    // Lagged: one catch-up poll.
    crate::gateway::tick(&mut app);
    answer_events(&mut app, 0, &drain(&mut rxs[0]));
    assert!(!crate::gateway::polling(&mut app, 0));
    on_events(&mut app, 0, vec![], true);
    assert!(crate::gateway::polling(&mut app, 0));
    // After a reconnect the subscription replays from the cursor.
    app.on_connected(0);
    let after = drain(&mut rxs[0]).into_iter().find_map(|f| match f {
        ClientFrame::Subscribe { after, .. } => Some(after),
        _ => None,
    });
    assert_eq!(after, Some(Some(11)));
}
