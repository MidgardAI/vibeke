//! Control connections: `events.subscribe` / `events.unsubscribe` (07 §2.13).

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn server() -> (tempfile::TempDir, Arc<Server>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let opts = ServerOpts {
        session: "t".into(),
        machine: "m".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
        gateway: None,
    };
    (dir, Server::new(paths, opts).unwrap())
}

fn emit(server: &Server, n: u32) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event(
        "notes.updated",
        json!({"workspace": "w"}),
        json!({"rev": n, "bytes": 0}),
    );
    server.commit(&mut c, tx).unwrap();
}

/// Read lines until the response with `id`, collecting `events.event` pushes on the way.
async fn until_response<R: tokio::io::AsyncBufRead + Unpin>(
    rd: &mut R,
    id: u64,
    pushes: &mut Vec<Value>,
) -> Value {
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), rd.read_line(&mut line))
            .await
            .expect("response in time")
            .unwrap();
        assert!(n > 0, "connection closed");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == json!(id) {
            return v;
        }
        if v["method"] == "events.event" {
            pushes.push(v);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unsubscribe_stops_delivery_and_is_idempotent() {
    let (_d, srv) = server();
    let (client, server_end) = tokio::io::duplex(1 << 20);
    let conn = tokio::spawn(connection(srv.clone(), server_end, None));
    let (rd, mut wr) = tokio::io::split(client);
    let mut rd = tokio::io::BufReader::new(rd);
    let mut send = async |id: u64, method: &str, params: Value| {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        wr.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    };
    let mut pushes = vec![];
    send(1, "events.subscribe", json!({"types": ["notes.*"]})).await;
    let r = until_response(&mut rd, 1, &mut pushes).await;
    let sid = r["result"]["subscription_id"].as_str().unwrap().to_string();
    // Delivery works before unsubscribing.
    emit(&srv, 1);
    send(2, "server.status", json!({})).await;
    let _ = until_response(&mut rd, 2, &mut pushes).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(3, "events.unsubscribe", json!({"subscription_id": sid})).await;
    let r = until_response(&mut rd, 3, &mut pushes).await;
    assert_eq!(r["result"]["unsubscribed"], true, "{r}");
    assert!(
        pushes
            .iter()
            .any(|p| p["params"]["subscription_id"] == json!(sid)),
        "an event before unsubscribing: {pushes:?}"
    );
    pushes.clear();
    // After unsubscribing nothing more arrives for that id.
    for n in 2..50 {
        emit(&srv, n);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(4, "server.status", json!({})).await;
    let _ = until_response(&mut rd, 4, &mut pushes).await;
    assert!(pushes.is_empty(), "events after unsubscribe: {pushes:?}");
    // Idempotent: a second call, or an unknown id, answers false.
    send(5, "events.unsubscribe", json!({"subscription_id": sid})).await;
    let r = until_response(&mut rd, 5, &mut pushes).await;
    assert_eq!(r["result"]["unsubscribed"], false, "{r}");
    send(6, "events.unsubscribe", json!({"subscription_id": "nope"})).await;
    let r = until_response(&mut rd, 6, &mut pushes).await;
    assert_eq!(r["result"]["unsubscribed"], false, "{r}");
    send(7, "events.unsubscribe", json!({})).await;
    let r = until_response(&mut rd, 7, &mut pushes).await;
    assert_eq!(r["error"]["data"]["kind"], "invalid_params", "{r}");
    drop(wr);
    drop(rd);
    let _ = tokio::time::timeout(Duration::from_secs(5), conn).await;
}

/// Outside a control connection (e.g. through `api::dispatch`) the method is refused like
/// `events.subscribe`.
#[tokio::test]
async fn unsubscribe_needs_a_control_connection() {
    let (_d, srv) = server();
    let ctx = Ctx {
        client_id: "c".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    };
    let e = api::dispatch(
        &srv,
        &ctx,
        "events.unsubscribe",
        &json!({"subscription_id": "s"}),
    )
    .await
    .unwrap_err();
    assert!(e.message.contains("control connection"), "{}", e.message);
    assert_eq!(
        api::pane_scope_of("events.unsubscribe").as_str(),
        "open",
        "pane-scoped callers may unsubscribe their own connection's subscriptions"
    );
}
