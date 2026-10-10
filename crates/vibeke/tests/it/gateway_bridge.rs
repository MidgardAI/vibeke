//! The server-to-gateway bridge (spec 16 §15.5) against a real server and a fake gateway:
//! `gateway.call` reaches the gateway as a `gateway.request` event and returns its
//! `gateway.reply`; without a gateway, on a timeout or when the gateway goes away it is
//! `remote_unavailable`; `gateway.reply` is for gateway clients only; and only the allow-listed
//! methods go through.

use crate::support::{Rpc, Session};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn kind(e: &Value) -> String {
    e.pointer("/error/kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn api(s: &Session, method: &str, p: Value) -> Result<Value, Value> {
    let out = s
        .cmd(&["api", "call", method, &p.to_string()])
        .output()
        .unwrap();
    if out.status.success() {
        Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
    } else {
        Err(serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&out.stderr).to_string()})))
    }
}

/// The gateway's event stream: `client.hello {kind: gateway}` and `events.subscribe`.
struct Stream {
    rd: BufReader<UnixStream>,
    _wr: UnixStream,
}

impl Stream {
    fn connect(s: &Session) -> Stream {
        let sock = UnixStream::connect(s.socket()).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut wr = sock.try_clone().unwrap();
        let mut rd = BufReader::new(sock);
        let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "client.hello",
            "params": {"client": "test-gateway", "version": "0", "api": "vibeke/1", "kind": "gateway"}});
        let sub = json!({"jsonrpc": "2.0", "id": 2, "method": "events.subscribe", "params": {}});
        writeln!(wr, "{hello}\n{sub}").unwrap();
        let mut seen = 0;
        while seen < 2 {
            let mut line = String::new();
            assert!(rd.read_line(&mut line).unwrap() > 0, "closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == 1 || v["id"] == 2 {
                assert!(v.get("error").is_none_or(Value::is_null), "{v}");
                seen += 1;
            }
        }
        Stream { rd, _wr: wr }
    }

    /// The data of the next `gateway.request` event.
    fn request(&mut self) -> Value {
        loop {
            let mut line = String::new();
            assert!(self.rd.read_line(&mut line).unwrap() > 0, "closed");
            let v: Value = serde_json::from_str(&line).unwrap();
            let ev = &v["params"]["event"];
            if v["method"] == "events.event" && ev["type"] == "gateway.request" {
                return ev["data"].clone();
            }
        }
    }
}

/// A gateway-kind connection for the replies (the real gateway calls on its own connection).
fn gateway_rpc(s: &Session) -> Rpc {
    let mut rpc = Rpc::connect(&s.socket());
    rpc.call(
        "client.hello",
        json!({"client": "test-gateway", "version": "0", "api": "vibeke/1", "kind": "gateway"}),
    )
    .unwrap();
    rpc
}

#[test]
fn without_a_gateway_the_call_is_unavailable() {
    let s = Session::new();
    let st = api(&s, "gateway.status", json!({})).unwrap();
    assert_eq!(st["connected"], false, "{st}");
    let e = api(&s, "gateway.call", json!({"method": "peer.list"})).unwrap_err();
    assert_eq!(kind(&e), "remote_unavailable", "{e}");
    let msg = e.pointer("/error/message").and_then(Value::as_str).unwrap();
    assert!(msg.contains("vibeke gateway on"), "{e}");
}

#[test]
fn the_gateway_answers_a_call() {
    let s = Session::new();
    // The first call starts the server; the raw stream needs its socket.
    let st = api(&s, "gateway.status", json!({})).unwrap();
    assert_eq!(st["connected"], false, "{st}");
    let mut stream = Stream::connect(&s);
    let mut gw = gateway_rpc(&s);
    let st = api(&s, "gateway.status", json!({})).unwrap();
    assert_eq!(st["connected"], true, "{st}");

    std::thread::scope(|sc| {
        // A result comes back as the call's result.
        let call = sc.spawn(|| {
            api(
                &s,
                "gateway.call",
                json!({"method": "peer.invite", "params": {"ttl_s": 120}}),
            )
        });
        let req = stream.request();
        assert_eq!(req["method"], "peer.invite", "{req}");
        assert_eq!(req["params"], json!({"ttl_s": 120}));
        let id = req["id"].as_str().unwrap();
        gw.call(
            "gateway.reply",
            json!({"id": id, "result": {"link": "vibeke://pair?d=x", "pid": "p1"}}),
        )
        .unwrap();
        let r = call.join().unwrap().unwrap();
        assert_eq!(r["pid"], "p1", "{r}");
        // The request is settled: a second reply has nothing to answer.
        let e = gw
            .call("gateway.reply", json!({"id": id, "result": {}}))
            .unwrap_err();
        assert_eq!(e["data"]["kind"], "not_found", "{e}");

        // An error keeps its kind.
        let call = sc.spawn(|| api(&s, "gateway.call", json!({"method": "share.list"})));
        let req = stream.request();
        assert_eq!(req["method"], "share.list");
        gw.call(
            "gateway.reply",
            json!({"id": req["id"], "error": {"kind": "forbidden", "message": "no"}}),
        )
        .unwrap();
        let e = call.join().unwrap().unwrap_err();
        assert_eq!(kind(&e), "permission_denied", "{e}");

        // A gateway that stays silent: the call times out.
        let call = sc.spawn(|| {
            api(
                &s,
                "gateway.call",
                json!({"method": "peer.list", "timeout_ms": 300}),
            )
        });
        stream.request();
        let e = call.join().unwrap().unwrap_err();
        assert_eq!(kind(&e), "remote_unavailable", "{e}");

        // The gateway going away fails the waiting call at once.
        let call = sc.spawn(|| {
            api(
                &s,
                "gateway.call",
                json!({"method": "peer.list", "timeout_ms": 60000}),
            )
        });
        stream.request();
        drop(stream);
        let e = call.join().unwrap().unwrap_err();
        assert_eq!(kind(&e), "remote_unavailable", "{e}");
    });
    // Eventually the server notices the closed stream.
    for _ in 0..50 {
        let st = api(&s, "gateway.status", json!({})).unwrap();
        if st["connected"] == false {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the gateway still counts as connected");
}

#[test]
fn only_a_gateway_may_reply() {
    let s = Session::new();
    // An ordinary client is refused, even with a plausible id.
    let e = api(&s, "gateway.reply", json!({"id": "gr1", "result": {}})).unwrap_err();
    assert_eq!(kind(&e), "permission_denied", "{e}");
    // A gateway replying to nothing: not_found.
    let mut gw = gateway_rpc(&s);
    let e = gw
        .call("gateway.reply", json!({"id": "gr1", "result": {}}))
        .unwrap_err();
    assert_eq!(e["data"]["kind"], "not_found", "{e}");
    // The reply needs a result or an error.
    let e = gw.call("gateway.reply", json!({"id": "gr1"})).unwrap_err();
    assert_eq!(e["data"]["kind"], "invalid_params", "{e}");
}

#[test]
fn methods_outside_the_allow_list_are_refused() {
    let s = Session::new();
    for (method, params) in [
        ("handoff.send", json!({})),
        ("pane.send_text", json!({"text": "x"})),
        ("share.create", json!({"kind": "device", "workspace": "w"})),
        ("share.create", json!({})),
    ] {
        let e = api(
            &s,
            "gateway.call",
            json!({"method": method, "params": params}),
        )
        .unwrap_err();
        assert_eq!(kind(&e), "invalid_params", "{method}: {e}");
    }
    // Device management, pairing and account login pass the check (and then find no gateway).
    for method in [
        "devices.list",
        "devices.revoke",
        "pair.create",
        "pair.status",
        "account.status",
        "account.login.start",
        "account.login.status",
        "account.login.cancel",
        "account.logout",
    ] {
        let e = api(&s, "gateway.call", json!({"method": method, "params": {}})).unwrap_err();
        assert_eq!(kind(&e), "remote_unavailable", "{method}: {e}");
    }
    // The kinds the bridge carries pass the check (and then find no gateway).
    for kind_ in ["handoff", "peer", "share"] {
        let e = api(
            &s,
            "gateway.call",
            json!({"method": "share.create", "params": {"kind": kind_}}),
        )
        .unwrap_err();
        assert_eq!(kind(&e), "remote_unavailable", "{kind_}: {e}");
    }
}

#[test]
fn reported_devices_last_as_long_as_the_gateway_connection() {
    let s = Session::new();
    let none = api(&s, "client.list", json!({})).unwrap();
    assert_eq!(none["devices"], json!([]), "{none}");
    let mut gw = gateway_rpc(&s);
    gw.call(
        "client.devices",
        json!({"devices": [{"id": "d1", "name": "Pixel", "platform": "android", "kind": "device"}]}),
    )
    .unwrap();
    // Gateways talk plain RPC (no render attach); their report still counts.
    let list = api(&s, "client.list", json!({})).unwrap();
    let devices = list["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1, "{list}");
    assert_eq!(devices[0]["name"], "Pixel", "{list}");
    // Only gateways report devices.
    let e = api(&s, "client.devices", json!({"devices": []})).unwrap_err();
    assert_eq!(kind(&e), "permission_denied", "{e}");
    // The gateway goes away: so do the devices it reached.
    drop(gw);
    for _ in 0..100 {
        let list = api(&s, "client.list", json!({})).unwrap();
        if list["devices"] == json!([]) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the devices outlived their gateway");
}
