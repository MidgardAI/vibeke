//! Chaos scenarios from 10 §5.1 that `chaos.rs` does not cover: ring overflow (`lost`),
//! holder crash, `log_epoch` identity across restarts, and a client that stops reading.
//! Each is quick enough for the PR run; `VIBEKE_CHAOS_ITER` repeats the loops for the nightly.

mod support;

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use support::{Rpc, Session, alive, parent_pid};

fn iters() -> usize {
    std::env::var("VIBEKE_CHAOS_ITER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// 10 §5.1 "Ring overflow": the server is down while the pane emits more than the holder's
/// 16 MiB ring holds. On restart the pane is recovered `lost` (replay cannot be
/// complete), the process is alive and the pane is usable.
#[test]
#[ignore = "slow in debug builds (replays 16 MiB); nightly runs it in release: cargo test --release -p vibeke --test chaos_gaps -- --ignored ring_overflow"]
fn ring_overflow_while_server_is_down_recovers_lost() {
    for round in 0..iters() {
        let s = Session::new();
        let pane = s.workspace("/bin/sh");
        let child = s.pane(&pane)["child_pid"].as_i64().unwrap();
        // 20 MB of output, over the 16 MiB ring; the shell prints it whether or not a server
        // is attached.
        s.json(&[
            "pane",
            "send-text",
            &pane,
            "head -c 20000000 /dev/zero | tr '\\000' x; printf '\\033[2J\\033[H'; echo RING-DONE-$((40+2))\n",
        ]);
        s.kill_server();
        // Let the holder absorb the whole burst while nobody is attached.
        std::thread::sleep(Duration::from_secs(5));
        // The next call auto-starts a server that reattaches the holder; replaying the
        // surviving 16 MiB takes a while in a debug build, so wait for `recovered`.
        let deadline = Instant::now() + Duration::from_secs(90);
        let p = loop {
            let p = s.pane(&pane);
            assert_eq!(p["id"], pane.as_str(), "round {round}: pane lost");
            if !p["recovered"].is_null() || Instant::now() > deadline {
                break p;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        assert_eq!(
            p["child_pid"].as_i64(),
            Some(child),
            "round {round}: process replaced"
        );
        assert!(alive(child), "round {round}: process died");
        assert_eq!(
            p["recovered"], "lost",
            "round {round}: recovery method; pane = {p}"
        );
        // Announced once, as an event.
        let mut rpc = Rpc::connect(&s.socket());
        let ev = rpc
            .call("events.read", json!({"types": ["pane.recovered"]}))
            .unwrap();
        let methods: Vec<&str> = ev["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["data"]["method"].as_str())
            .collect();
        assert_eq!(
            methods,
            ["lost"],
            "round {round}: pane.recovered events"
        );
        // Usable: the shell finished its burst and takes new input.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = s.read(&pane, "visible", "50");
            if screen.contains("RING-DONE-42") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "round {round}: tail of the burst missing:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        s.json(&["pane", "send-text", &pane, "echo after-ring-$((3*3))\n"]);
        s.wait_output(&pane, "after-ring-9", 5_000);
    }
}

/// 10 §5.1 "Holder crash": `kill -9` a holder. The pane keeps its layout slot with a fresh
/// shell (the lost process is gone, reported by the run ending `holder_lost`); every other
/// pane, its process and its input path are unaffected.
#[test]
fn holder_crash_loses_only_that_pane() {
    for round in 0..iters() {
        let s = Session::new();
        let a = s.workspace("/bin/sh");
        let b = s.split(&a, "/bin/sh");
        let child_a = s.pane(&a)["child_pid"].as_i64().unwrap();
        let child_b = s.pane(&b)["child_pid"].as_i64().unwrap();
        let holder_a = parent_pid(child_a);
        let holder_b = parent_pid(child_b);
        assert_ne!(holder_a, holder_b, "one holder per pane");
        let server = s.server_pid();

        // SAFETY: killing our own holder process.
        unsafe { libc::kill(holder_a, libc::SIGKILL) };

        let deadline = Instant::now() + Duration::from_secs(15);
        let new_a = loop {
            let p = s.pane(&a);
            assert_eq!(p["id"], a.as_str(), "round {round}: slot removed");
            if let Some(c) = p["child_pid"].as_i64()
                && c != child_a
            {
                break c;
            }
            assert!(
                Instant::now() < deadline,
                "round {round}: pane not respawned"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(alive(new_a), "replacement shell not running");
        // The unrelated pane: same process, same holder, still taking input.
        let pb = s.pane(&b);
        assert_eq!(
            pb["child_pid"].as_i64(),
            Some(child_b),
            "bystander replaced"
        );
        assert!(alive(child_b) && alive(holder_b as i64));
        assert_eq!(s.server_pid(), server, "server restarted");
        s.json(&["pane", "send-text", &b, "echo bystander-$((5+5))\n"]);
        s.wait_output(&b, "bystander-10", 5_000);
        // And the replacement works too.
        s.json(&["pane", "send-text", &a, "echo replacement-$((6+6))\n"]);
        s.wait_output(&a, "replacement-12", 5_000);
    }
}

/// 10 §5.1 "DB restore" (the part that exists): the event-log identity (`session_uuid`,
/// `log_epoch`) survives `kill -9`, so cursors stay valid across a server restart, and a
/// cursor from another log epoch is refused with `truncated` plus the current cursor so the
/// client can resnapshot, never silently misread.
#[test]
fn cursors_survive_restart_and_foreign_epochs_are_refused() {
    let s = Session::new();
    let pane = s.workspace("/bin/sh");
    let mut rpc = Rpc::connect(&s.socket());
    let first = rpc.call("events.read", json!({"limit": 5})).unwrap();
    let cur = first["next"].clone();
    let epoch = cur["log_epoch"]
        .as_str()
        .expect("cursor.log_epoch")
        .to_string();
    let session_uuid = cur["session_uuid"].as_str().unwrap().to_string();
    assert!(!epoch.is_empty());
    s.json(&["pane", "send-text", &pane, "echo one\n"]);
    drop(rpc);

    s.kill_server();
    // The next CLI call auto-starts the replacement server.
    s.panes();
    let mut rpc = Rpc::connect(&s.socket());
    let again = rpc.call("events.read", json!({"after": cur})).unwrap();
    assert_eq!(
        again["next"]["log_epoch"],
        epoch.as_str(),
        "epoch changed on restart"
    );
    assert_eq!(again["next"]["session_uuid"], session_uuid.as_str());
    let seq = |v: &Value| v["next"]["seq"].as_i64().unwrap();
    assert!(seq(&again) >= seq(&first), "seq went backwards");
    // `server_restarted` is in the log after the old cursor: nothing was lost.
    let types: Vec<&str> = again["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["type"].as_str())
        .collect();
    assert!(
        types.contains(&"session.server_restarted"),
        "no server_restarted after the cursor: {types:?}"
    );

    let mut foreign = cur.clone();
    foreign["log_epoch"] = json!("0000000000000000");
    let err = rpc
        .call("events.read", json!({"after": foreign}))
        .expect_err("a foreign epoch must be refused");
    assert_eq!(
        err["data"]["kind"].as_str().or(err["kind"].as_str()),
        Some("truncated"),
        "{err}"
    );
    let current = &err["data"]["details"]["current"];
    assert_eq!(
        current["log_epoch"],
        epoch.as_str(),
        "error must carry the current cursor: {err}"
    );
}

/// 10 §5.1 "DB restore": restoring `state.db` from a backup must rotate `log_epoch`.
/// NOT BUILT: the epoch is persisted in the db's `meta` table and nothing detects a restore,
/// so a restored db brings its old epoch back. Kept as an executable statement of the gap
/// (`cargo test -- --ignored restore_rotates`); un-ignore when the store rotates on restore.
#[test]
#[ignore = "10 5.1 DB restore: log_epoch rotation on restore is not built"]
fn restore_rotates_log_epoch() {
    let s = Session::new();
    s.workspace("/bin/sh");
    let epoch_of = |s: &Session| {
        let mut rpc = Rpc::connect(&s.socket());
        rpc.call("events.read", json!({"limit": 1})).unwrap()["next"]["log_epoch"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let before = epoch_of(&s);
    let db = s.dir.path().join("state/default/state.db");
    let backup = s.dir.path().join("backup.db");
    s.json(&["server", "stop"]);
    std::fs::copy(&db, &backup).expect("copy state.db");
    // A restore replaces the db file with the backup (stale -wal/-shm removed).
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
    }
    std::fs::copy(&backup, &db).unwrap();
    s.json(&["pane", "list"]);
    assert_ne!(epoch_of(&s), before, "restored db kept its old log_epoch");
}

/// 10 §5.1 "Client death", the half that is built: a render client that stops reading while
/// a pane floods must not slow the server or other panes. (The 30 s disconnect is the next
/// test.)
#[test]
fn stalled_render_client_does_not_hurt_the_server_or_other_panes() {
    let s = Session::new();
    let flood = s.workspace("while :; do seq 1 2000; done");
    let quiet = s.split(&flood, "/bin/sh");
    let stalled = attach_and_stall(&s);
    std::thread::sleep(Duration::from_secs(2));

    // The server keeps answering promptly and the quiet pane still takes input and shows
    // output while the stalled client holds an unread socket.
    let t = Instant::now();
    let n = s.panes().len();
    assert_eq!(n, 2);
    let list_time = t.elapsed();
    s.json(&["pane", "send-text", &quiet, "echo alive-$((8*8))\n"]);
    s.wait_output(&quiet, "alive-64", 10_000);
    assert!(
        list_time < support::bound(200, 5_000),
        "pane.list took {list_time:?} with a stalled client"
    );
    // The stalled socket is still ours to close; the server must survive it.
    let _ = stalled.shutdown(std::net::Shutdown::Both);
    drop(stalled);
    std::thread::sleep(Duration::from_millis(300));
    s.json(&["pane", "send-text", &quiet, "echo post-close-$((9*9))\n"]);
    s.wait_output(&quiet, "post-close-81", 10_000);
}

/// 10 §5.1 "stalled client disconnected after 30 s". NOT BUILT: the render writer applies
/// backpressure per connection but never drops a client for not reading. Executable statement
/// of the gap; run with `cargo test -- --ignored stalled_client_is_disconnected` (takes ~40 s).
#[test]
#[ignore = "10 5.1 client stall disconnect after 30 s is not built"]
fn stalled_client_is_disconnected_after_30_seconds() {
    let s = Session::new();
    s.workspace("while :; do seq 1 2000; done");
    let mut stalled = attach_and_stall(&s);
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut buf = [0u8; 65536];
    // Stay stalled for 30 s, then drain: a disconnected client sees EOF/ECONNRESET after the
    // already-buffered bytes, a connected one keeps receiving frames forever.
    std::thread::sleep(Duration::from_secs(32));
    loop {
        match stalled.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "server never disconnected the stalled client"
        );
    }
}

/// `render.attach`, then never read again.
fn attach_and_stall(s: &Session) -> UnixStream {
    let mut sock = UnixStream::connect(s.socket()).expect("connect render");
    let req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach",
        "params":{"client_id": "stalled", "protocol": vk_proto::render::PROTOCOL, "caps": {"max_fps": 30}}});
    sock.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while b[0] != b'\n' {
        sock.read_exact(&mut b).unwrap();
        line.push(b[0]);
    }
    let v: Value = serde_json::from_slice(&line).unwrap();
    assert!(v.get("error").is_none(), "render.attach: {v}");
    sock
}
