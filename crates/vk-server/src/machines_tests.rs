//! Machine and Session entities and their events (02 §1.1, §2.2; 3D).

use super::*;
use crate::api::dispatch;
use crate::hardening::testkit::{pane_ctx, sample_pane, server, user};

fn events(s: &Server, kind: &str) -> Vec<vk_store::Event> {
    s.with_core(|c| {
        c.store
            .events_after(0, 10_000, &[kind.to_string()])
            .unwrap()
    })
}

async fn call(
    s: &std::sync::Arc<Server>,
    m: &str,
    p: Value,
) -> Result<Value, vk_proto::rpc::RpcError> {
    dispatch(s, &user(), m, &p).await
}

#[tokio::test]
async fn start_registers_the_session_and_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "start");
    start(&s);
    let started = events(&s, "session.started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].data["pid"], std::process::id());
    assert_eq!(started[0].data["fresh"], true);
    assert_eq!(started[0].data["prev_pid"], Value::Null);
    assert_eq!(events(&s, "machine.added").len(), 1);

    let info = call(&s, "session.info", json!({})).await.unwrap();
    assert_eq!(info["session"]["name"], "start");
    assert_eq!(info["session"]["server_pid"], std::process::id());
    assert_eq!(info["session"]["server_version"], vk_proto::VERSION);
    let machine_id = info["session"]["machine_id"].as_str().unwrap();
    assert_eq!(info["machine"]["id"], machine_id);
    assert_eq!(info["machine"]["kind"], "local");
    assert_eq!(info["machine"]["status"], "connected");
    assert_eq!(info["cursor"]["machine_uuid"], machine_id);

    let list = call(&s, "machine.list", json!({})).await.unwrap();
    assert_eq!(list["local"], machine_id);
    assert_eq!(list["machines"].as_array().unwrap().len(), 1);
    assert_eq!(list["machines"][0]["os"], std::env::consts::OS);
    assert_eq!(list["machines"][0]["arch"], std::env::consts::ARCH);

    // A second start (same database) is not "fresh", names the previous pid and does not add the
    // machine again.
    start(&s);
    let started = events(&s, "session.started");
    assert_eq!(started.len(), 2);
    assert_eq!(started[1].data["fresh"], false);
    assert_eq!(started[1].data["prev_pid"], std::process::id());
    assert_eq!(events(&s, "machine.added").len(), 1);
    let created = |s: &Server| {
        s.with_core(|c| {
            c.store
                .get::<SessionInfo>(K_SESSION, &c.store.session_uuid)
                .unwrap()
                .unwrap()
                .created_at_ms
        })
    };
    let first = created(&s);
    start(&s);
    assert_eq!(created(&s), first, "created_at is kept across starts");
}

#[tokio::test]
async fn stopped_emits_session_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "stop");
    stopped(&s, "api");
    let e = events(&s, "session.stopped");
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].data["reason"], "api");
}

#[tokio::test]
async fn remote_machines_are_registered_and_their_status_changes_are_events() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "remote");
    start(&s);
    let r = call(
        &s,
        "machine.upsert",
        json!({"label": "devbox", "address": "me@devbox.tailnet", "os": "linux", "arch": "x86_64", "vibeke_version": "0.1.0", "status": "connecting"}),
    )
    .await
    .unwrap();
    assert_eq!(r["created"], true);
    assert_eq!(r["machine"]["kind"], "ssh");
    assert_eq!(r["machine"]["status"], "connecting");
    assert_eq!(r["machine"]["last_seen_ms"], 0, "never seen connected yet");
    let id = r["machine"]["id"].as_str().unwrap().to_string();
    assert_eq!(id, remote_id("devbox"));
    assert_eq!(events(&s, "machine.added").len(), 2, "local + devbox");
    assert!(
        events(&s, "machine.connected").is_empty(),
        "connecting is not an event"
    );

    call(
        &s,
        "machine.upsert",
        json!({"label": "devbox", "status": "connected"}),
    )
    .await
    .unwrap();
    let con = events(&s, "machine.connected");
    assert_eq!(con.len(), 1);
    assert_eq!(con[0].subject["machine"], id);

    // The same status again is not a transition.
    let again = call(
        &s,
        "machine.upsert",
        json!({"label": "devbox", "status": "connected"}),
    )
    .await
    .unwrap();
    assert_eq!(again["created"], false);
    assert_eq!(events(&s, "machine.connected").len(), 1);
    assert!(again["machine"]["last_seen_ms"].as_i64().unwrap() > 0);

    call(
        &s,
        "machine.upsert",
        json!({"label": "devbox", "status": "degraded", "reason": "rtt 900ms"}),
    )
    .await
    .unwrap();
    assert_eq!(
        events(&s, "machine.degraded")[0].data["reason"],
        "rtt 900ms"
    );
    call(
        &s,
        "machine.upsert",
        json!({"label": "devbox", "status": "offline", "reason": "ssh exited"}),
    )
    .await
    .unwrap();
    assert_eq!(
        events(&s, "machine.disconnected")[0].data["reason"],
        "ssh exited"
    );

    // Fields survive a status-only update.
    let got = call(&s, "machine.get", json!({"machine": "devbox"}))
        .await
        .unwrap();
    assert_eq!(got["machine"]["address"], "me@devbox.tailnet");
    assert_eq!(got["machine"]["os"], "linux");
    assert_eq!(got["machine"]["status"], "offline");
    assert_eq!(
        call(&s, "machine.get", json!({"machine": id}))
            .await
            .unwrap()["machine"]["label"],
        "devbox"
    );

    // Listing puts this machine first, then the remotes by label.
    call(
        &s,
        "machine.upsert",
        json!({"label": "alpha", "kind": "quic"}),
    )
    .await
    .unwrap();
    let list = call(&s, "machine.list", json!({})).await.unwrap();
    let labels: Vec<&str> = list["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["m", "alpha", "devbox"]);

    let rm = call(&s, "machine.remove", json!({"machine": "devbox"}))
        .await
        .unwrap();
    assert_eq!(rm["removed"], id);
    assert_eq!(events(&s, "machine.removed").len(), 1);
    assert_eq!(
        call(&s, "machine.get", json!({"machine": "devbox"}))
            .await
            .unwrap_err()
            .data
            .kind,
        "not_found"
    );
}

#[tokio::test]
async fn this_machine_is_not_editable_and_inputs_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "guard");
    start(&s);
    let local = call(&s, "machine.list", json!({})).await.unwrap()["local"]
        .as_str()
        .unwrap()
        .to_string();
    for p in [json!({"label": "m"}), json!({"label": "x", "id": local})] {
        let e = call(&s, "machine.upsert", p).await.unwrap_err();
        assert_eq!(e.data.kind, "conflict");
    }
    assert_eq!(
        call(&s, "machine.remove", json!({"machine": "m"}))
            .await
            .unwrap_err()
            .data
            .kind,
        "conflict"
    );
    for p in [
        json!({"label": ""}),
        json!({"label": "has space"}),
        json!({"label": "x", "kind": "local"}),
        json!({"label": "x", "kind": "carrier-pigeon"}),
        json!({"label": "x", "status": "sleepy"}),
        json!({}),
    ] {
        let e = call(&s, "machine.upsert", p.clone()).await.unwrap_err();
        assert_eq!(e.data.kind, "invalid_params", "{p}");
    }
}

#[tokio::test]
async fn pane_tokens_may_read_but_not_change_machines() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), "scope");
    start(&s);
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = crate::core::Tx::new();
        tx.pane(sample_pane("pa", "wa"));
        s.commit(&mut c, tx).unwrap();
    }
    for m in ["session.info", "machine.list"] {
        assert!(
            dispatch(&s, &pane_ctx("pa"), m, &json!({})).await.is_ok(),
            "{m}"
        );
    }
    for (m, p) in [
        ("machine.upsert", json!({"label": "evil", "address": "x"})),
        ("machine.remove", json!({"machine": "evil"})),
    ] {
        let e = dispatch(&s, &pane_ctx("pa"), m, &p).await.unwrap_err();
        assert_eq!(e.data.kind, "permission_denied", "{m}");
    }
}

#[test]
fn remote_ids_are_stable_per_label() {
    assert_eq!(remote_id("devbox"), remote_id("devbox"));
    assert_ne!(remote_id("devbox"), remote_id("devbox2"));
    assert!(remote_id("devbox").starts_with("m-"));
}
