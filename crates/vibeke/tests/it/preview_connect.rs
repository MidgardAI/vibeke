//! `preview.connect` end to end (06 B3.5): a user client's control connection becomes a raw
//! byte stream to a loopback port on this machine. Bytes pipelined right after the request
//! line reach the destination, half-close is passed on, and non-loopback hosts, bad ports and
//! closed ports are refused with the documented errors. A port that is no preview is refused
//! with `port_not_allowed` until the client asks the user and sends `allow_port: true`; the
//! server then remembers it.

use crate::support::{Rpc, Session};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// A loopback TCP echo server on an ephemeral port: echoes until the peer half-closes, then
/// closes its side.
fn echo_listener() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { return };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if c.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = c.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

/// A raw connection to the session socket that has sent `client.hello` as the gateway does.
fn hello(s: &Session) -> (BufReader<UnixStream>, UnixStream) {
    let sock = UnixStream::connect(s.socket()).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let rd = BufReader::new(sock.try_clone().unwrap());
    let mut wr = sock;
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "client.hello",
                     "params": {"kind": "gateway-tunnel", "remote": true}});
    writeln!(wr, "{req}").unwrap();
    let mut rd = rd;
    let v = read_json(&mut rd);
    assert_eq!(v["id"], 1, "{v}");
    assert!(v.get("error").is_none_or(Value::is_null), "{v}");
    (rd, wr)
}

fn read_json(rd: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    assert!(rd.read_line(&mut line).unwrap() > 0, "connection closed");
    serde_json::from_str(&line).unwrap()
}

fn accepted(s: &Session) -> u64 {
    let st = Rpc::connect(&s.socket())
        .call("preview.status", json!({}))
        .expect("preview.status");
    st["accepted"].as_u64().unwrap_or(0)
}

/// Send `preview.connect` and return its response line.
fn connect(s: &Session, params: Value) -> Value {
    let (mut rd, mut wr) = hello(s);
    let req = json!({"jsonrpc": "2.0", "id": 2, "method": "preview.connect", "params": params});
    writeln!(wr, "{req}").unwrap();
    read_json(&mut rd)
}

#[test]
fn preview_connect_streams_bytes_to_a_loopback_port() {
    let s = Session::new();
    s.json(&["server", "status"]);
    let port = echo_listener();
    let before = accepted(&s);

    // No preview uses the port: the client must ask the user first.
    let v = connect(&s, json!({"host": "localhost", "port": port}));
    assert_eq!(v["error"]["data"]["kind"], "permission_denied", "{v}");
    assert_eq!(
        v["error"]["data"]["details"]["reason"], "port_not_allowed",
        "{v}"
    );
    assert_eq!(v["error"]["data"]["details"]["port"], port, "{v}");
    // The user allowed it: the port is remembered.
    let v = connect(
        &s,
        json!({"host": "localhost", "port": port, "allow_port": true}),
    );
    assert_eq!(v["result"]["connected"], true, "{v}");

    let (mut rd, mut wr) = hello(&s);
    // The request line and the first stream bytes in one write: bytes that arrive before the
    // upgrade must not be lost.
    let req = json!({"jsonrpc": "2.0", "id": 2, "method": "preview.connect",
                     "params": {"host": "localhost", "port": port}});
    let mut first = format!("{req}\n").into_bytes();
    first.extend_from_slice(b"early bytes|");
    wr.write_all(&first).unwrap();
    let v = read_json(&mut rd);
    assert_eq!(v["id"], 2, "{v}");
    assert_eq!(v["result"]["connected"], true, "{v}");

    let mut got = vec![0u8; b"early bytes|".len()];
    rd.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"early bytes|");

    // A larger payload, then half-close: the echo ends and the stream reaches EOF.
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let w = {
        let mut wr = wr.try_clone().unwrap();
        let big = big.clone();
        std::thread::spawn(move || {
            wr.write_all(&big).unwrap();
            wr.shutdown(Shutdown::Write).unwrap();
        })
    };
    let mut echoed = Vec::new();
    rd.read_to_end(&mut echoed).unwrap();
    w.join().unwrap();
    assert_eq!(echoed.len(), big.len());
    assert!(echoed == big, "echoed bytes differ");

    let after = accepted(&s);
    assert!(after > before, "accepted {before} -> {after}");
}

#[test]
fn preview_connect_refuses_bad_destinations() {
    let s = Session::new();
    s.json(&["server", "status"]);

    // Not loopback.
    let v = connect(&s, json!({"host": "example.com", "port": 80}));
    assert_eq!(v["error"]["data"]["kind"], "invalid_params", "{v}");
    let v = connect(&s, json!({"host": "10.0.0.1", "port": 80}));
    assert_eq!(v["error"]["data"]["kind"], "invalid_params", "{v}");
    // Port out of range.
    let v = connect(&s, json!({"host": "127.0.0.1", "port": 0}));
    assert_eq!(v["error"]["data"]["kind"], "invalid_params", "{v}");
    let v = connect(&s, json!({"host": "127.0.0.1", "port": 70000}));
    assert_eq!(v["error"]["data"]["kind"], "invalid_params", "{v}");

    // Nothing listening: a port that was just free.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let v = connect(&s, json!({"host": "127.0.0.1", "port": closed}));
    assert_eq!(
        v["error"]["data"]["details"]["reason"], "port_not_allowed",
        "{v}"
    );
    let v = connect(
        &s,
        json!({"host": "127.0.0.1", "port": closed, "allow_port": true}),
    );
    assert_eq!(v["error"]["data"]["kind"], "remote_unavailable", "{v}");
    assert_eq!(v["error"]["data"]["details"]["reason"], "refused", "{v}");
}

#[test]
fn preview_connect_reaches_declared_previews_without_asking() {
    let s = Session::new();
    s.json(&["server", "status"]);
    let port = echo_listener();
    Rpc::connect(&s.socket())
        .call("preview.declare", json!({"port": port}))
        .expect("preview.declare");
    let v = connect(&s, json!({"host": "localhost", "port": port}));
    assert_eq!(v["result"]["connected"], true, "{v}");
}
