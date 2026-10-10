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

#[test]
fn gateway_render_stream_keeps_remote_authorization_and_actor_requirements() {
    use vk_proto::frame;
    use vk_proto::render::{ClientFrame, ServerFrame};
    let s = Session::new();
    let pane = s.workspace("/bin/sh");
    let sock = UnixStream::connect(s.socket()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut wr = sock.try_clone().unwrap();
    let mut rd = BufReader::new(sock);
    for (id, method, params) in [
        (
            1,
            "client.hello",
            json!({"client":"browser-tui-test", "kind":"gateway-tui", "remote":true, "api":"vibeke/1"}),
        ),
        (
            2,
            "render.attach",
            json!({"client_id":"browser-tui-test", "protocol":vk_proto::render::PROTOCOL, "remote":true}),
        ),
    ] {
        writeln!(
            wr,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("error").is_none(), "{v}");
    }
    let mut command = |req, method: &str, params: Value| {
        frame::write_frame(
            &mut wr,
            &ClientFrame::Command {
                req,
                json: json!({"jsonrpc":"2.0","id":req,"method":method,"params":params}).to_string(),
            },
        )
        .unwrap();
        loop {
            if let ServerFrame::CommandResult { req: r, json } = frame::read_frame(&mut rd).unwrap()
                && r == req
            {
                break serde_json::from_str::<Value>(&json).unwrap();
            }
        }
    };
    let denied = command(
        10,
        "client.confirm_answer",
        json!({"confirm":"none","choice":"yes","actor":"gateway:Browser"}),
    );
    assert!(
        denied["error"]["message"]
            .as_str()
            .unwrap()
            .contains("confirmations are answered from the TUI"),
        "{denied}"
    );
    for (req, method) in [
        (20, "handoff.peers.set"),
        (21, "handoff.job.update"),
        (22, "gateway.reply"),
        (23, "client.devices"),
        (24, "handoff.incoming.add"),
    ] {
        let denied = command(req, method, json!({"actor":"gateway:Browser"}));
        assert_eq!(
            denied["error"]["data"]["kind"], "permission_denied",
            "{method}: {denied}"
        );
    }
    let missing_actor = command(
        11,
        "pane.rename",
        json!({"pane":pane,"title":"Remote name"}),
    );
    assert!(
        missing_actor["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must pass `actor`"),
        "{missing_actor}"
    );
    let renamed = command(
        12,
        "pane.rename",
        json!({"pane":pane,"title":"Remote name","actor":"gateway:Browser"}),
    );
    assert!(renamed.get("error").is_none_or(Value::is_null), "{renamed}");
    assert_eq!(s.pane(&pane)["title"], "Remote name");
}

/// Test malicious wire frames against the real host, not just the browser's disabled controls.
#[test]
fn shared_render_filters_model_input_history_commands_and_geometry() {
    use vk_proto::frame;
    use vk_proto::render::{AckStatus, ClientFrame, PaneRect, ServerFrame};
    let s = Session::new();
    let pane = s.workspace("/bin/sh");
    let sibling = s.json(&[
        "pane",
        "split",
        &pane,
        "--direction",
        "right",
        "--command",
        "/bin/sh",
    ])["pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let other = s.workspace("/bin/sh");
    let mut owner = Rpc::connect(&s.socket());
    owner
        .call("client.hello", json!({"kind":"tui","api":"vibeke/1"}))
        .unwrap();
    owner.call("pane.focus", json!({"pane":other})).unwrap();
    owner
        .call(
            "render.attach",
            json!({"protocol":vk_proto::render::PROTOCOL}),
        )
        .unwrap();
    let original = s.pane(&pane);
    for (scope, workspace_share) in [
        ("view", false),
        ("approve", false),
        ("full", false),
        ("view", true),
        ("approve", true),
        ("full", true),
    ] {
        let limit = if workspace_share {
            json!({"pane":null,"workspace":original["workspace"],"scope":scope,"expires_at":u64::MAX})
        } else {
            json!({"pane":pane,"workspace":original["workspace"],"scope":scope,"expires_at":u64::MAX})
        };
        let sock = UnixStream::connect(s.socket()).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut wr = sock.try_clone().unwrap();
        let mut rd = BufReader::new(sock);
        for (id, method, params) in [
            (
                1,
                "client.hello",
                json!({"kind":"gateway-tui","remote":true,"api":"vibeke/1"}),
            ),
            (
                2,
                "render.attach",
                json!({"client_id":format!("share-{scope}-{workspace_share}"),"protocol":vk_proto::render::PROTOCOL,"remote":true,
                "share":limit}),
            ),
        ] {
            writeln!(
                wr,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            )
            .unwrap();
            let mut line = String::new();
            rd.read_line(&mut line).unwrap();
            assert!(
                serde_json::from_str::<Value>(&line)
                    .unwrap()
                    .get("error")
                    .is_none(),
                "{line}"
            );
        }
        loop {
            if let ServerFrame::Model { model, focus, seen } = frame::read_frame(&mut rd).unwrap() {
                assert_eq!(model.panes.len(), if workspace_share { 2 } else { 1 });
                assert_eq!(model.panes[0].id, pane);
                assert_eq!(model.workspaces.len(), 1);
                assert_eq!(model.tabs.len(), 1);
                if !workspace_share {
                    assert_eq!(model.tabs[0].layout.panes(), vec![pane.clone()]);
                }
                assert_eq!(focus.pane.as_deref(), Some(pane.as_str()));
                assert!(
                    model.groups.is_empty()
                        && model.tasks.is_empty()
                        && model.previews.is_empty()
                        && seen.is_empty()
                );
                assert!(!serde_json::to_string(&model).unwrap().contains(&other));
                if !workspace_share {
                    assert!(!serde_json::to_string(&model).unwrap().contains(&sibling));
                }
                break;
            }
        }
        for f in [
            ClientFrame::ViewHint {
                panes: vec![
                    PaneRect {
                        pane: pane.clone(),
                        cols: 20,
                        rows: 5,
                    },
                    PaneRect {
                        pane: other.clone(),
                        cols: 20,
                        rows: 5,
                    },
                ],
                active: true,
            },
            ClientFrame::FetchHistory {
                req: 1,
                pane: other.clone(),
                start: 0,
                count: 100,
            },
            ClientFrame::Subscribe {
                types: vec!["*".into()],
                after: Some(0),
            },
            ClientFrame::RawInput {
                input_id: 1,
                pane: other.clone(),
                bytes: b"echo UNSHARED\n".to_vec(),
            },
            ClientFrame::RawInput {
                input_id: 2,
                pane: pane.clone(),
                bytes: vec![],
            },
            ClientFrame::Command {
                req: 3,
                json: json!({"method":"session.snapshot","params":{}}).to_string(),
            },
            ClientFrame::Focus {
                pane: other.clone(),
            },
            ClientFrame::Focus { pane: pane.clone() },
            ClientFrame::Ping { nonce: 99 },
        ] {
            frame::write_frame(&mut wr, &f).unwrap();
        }
        let mut acks = Vec::new();
        let mut denied = false;
        loop {
            match frame::read_frame::<_, ServerFrame>(&mut rd).unwrap() {
                ServerFrame::InputAck { input_id, status } => acks.push((input_id, status)),
                ServerFrame::CommandResult { req: 3, json } => {
                    assert_eq!(
                        serde_json::from_str::<Value>(&json).unwrap()["error"]["data"]["kind"],
                        "forbidden"
                    );
                    denied = true;
                }
                ServerFrame::PaneFull { pane: p, .. } | ServerFrame::PaneDiff { pane: p, .. } => {
                    assert_eq!(p, pane)
                }
                ServerFrame::History { .. } | ServerFrame::Events { .. } => {
                    panic!("private history/events leaked")
                }
                ServerFrame::Pong { nonce: 99, .. } => break,
                _ => {}
            }
        }
        assert!(denied);
        assert!(acks.contains(&(1, AckStatus::Rejected)));
        assert!(acks.contains(&(
            2,
            if scope == "full" {
                AckStatus::Written
            } else {
                AckStatus::Rejected
            }
        )));
        let clients = api(&s, "client.list", json!({})).unwrap();
        let guest_id = format!("share-{scope}-{workspace_share}");
        let guest = clients["clients"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == guest_id)
            .unwrap();
        assert_eq!(guest["kind"], "share");
        assert_eq!(s.json(&["pane", "get", "@focused"])["pane"]["id"], other);
        let after = s.pane(&pane);
        assert_eq!(
            (after["cols"].clone(), after["rows"].clone()),
            (original["cols"].clone(), original["rows"].clone())
        );
    }
}

#[test]
fn shared_render_rejects_untrusted_and_invalid_grants() {
    let s = Session::new();
    let pane = s.workspace("/bin/sh");
    for (kind, scope) in [("cli", "view"), ("gateway-tui", "owner")] {
        let mut rpc = Rpc::connect(&s.socket());
        rpc.call("client.hello", json!({"kind":kind,"api":"vibeke/1"}))
            .unwrap();
        let error = rpc
            .call(
                "render.attach",
                json!({
                    "protocol":vk_proto::render::PROTOCOL,
                    "share":{"pane":pane,"scope":scope,"expires_at":u64::MAX}
                }),
            )
            .unwrap_err();
        assert_eq!(error["code"], -32602, "{error}");
    }
}
