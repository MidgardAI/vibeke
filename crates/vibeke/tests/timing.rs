//! Startup and recovery timings (10 §1.4): cold start, attach, restart with 30 panes, reboot
//! restore. The bounds are deliberately generous so the PR run never flakes on a loaded
//! shared runner (and debug builds); `VIBEKE_PERF_STRICT=1` switches to the spec budgets for a
//! quiet reference machine. Every run prints its measurement (`--nocapture`) and, when
//! `VIBEKE_TIMING_REPORT` names a file, appends `name<TAB>milliseconds` lines to it for the
//! perf gate.

#[path = "it/support/mod.rs"]
mod support;

use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use support::{Session, alive, bound, parent_pid};

fn report(name: &str, d: Duration) {
    let ms = d.as_secs_f64() * 1000.0;
    eprintln!("timing {name}: {ms:.1} ms");
    if let Some(path) = std::env::var_os("VIBEKE_TIMING_REPORT")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{name}\t{ms:.1}");
    }
}

fn check(name: &str, took: Duration, limit: Duration) {
    report(name, took);
    assert!(
        took <= limit,
        "{name}: {took:?} exceeds {limit:?} (VIBEKE_PERF_STRICT={})",
        std::env::var_os("VIBEKE_PERF_STRICT").is_some()
    );
}

/// 30 panes in one workspace, each an idle `sleep` holder child; returns their ids.
fn thirty_panes(s: &Session) -> Vec<String> {
    let first = s.workspace("sleep 3600");
    let mut ids = vec![first.clone()];
    for _ in 1..30 {
        ids.push(s.split(&first, "sleep 3600"));
    }
    assert_eq!(s.panes().len(), 30);
    ids
}

/// Cold start <= 300 ms: from no server at all to the first answered call (CLI process
/// start, server spawn, state open, listen, reply).
#[test]
fn cold_start_to_first_answer() {
    let s = Session::new();
    // Warm the page cache and the binary once with an unrelated, separate session.
    {
        let w = Session::new();
        w.json(&["pane", "list"]);
    }
    let t = Instant::now();
    let v = s.json(&["pane", "list"]);
    let took = t.elapsed();
    assert!(v["panes"].as_array().unwrap().is_empty());
    check("cold_start", took, bound(300, 5_000));
}

/// Attach <= 50 ms: `render.attach` on a warm server with a workspace to the first reply.
#[test]
fn attach_latency() {
    let s = Session::new();
    s.workspace("/bin/sh");
    let mut samples = Vec::new();
    for i in 0..5 {
        let t = Instant::now();
        let mut sock = UnixStream::connect(s.socket()).unwrap();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach",
            "params":{"client_id": format!("timing-{i}"), "protocol": vk_proto::render::PROTOCOL, "caps": {"max_fps": 30}}});
        sock.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut b = [0u8; 1];
        loop {
            sock.read_exact(&mut b).unwrap();
            if b[0] == b'\n' {
                break;
            }
        }
        samples.push(t.elapsed());
    }
    // Median of five: one scheduler hiccup must not fail the run.
    samples.sort();
    check("attach", samples[2], bound(50, 2_000));
}

/// Restart <= 1 s with 30 panes: `kill -9` the server, then time until a call returns the
/// full pane list again (new server up, every holder reattached).
#[test]
fn restart_with_30_panes() {
    let s = Session::new();
    let ids = thirty_panes(&s);
    let before: Vec<i64> = s
        .panes()
        .iter()
        .map(|p| p["child_pid"].as_i64().unwrap())
        .collect();
    s.kill_server();
    let t = Instant::now();
    let after = s.panes();
    let took = t.elapsed();
    assert_eq!(after.len(), ids.len(), "panes lost");
    for p in &before {
        assert!(alive(*p), "process {p} died");
    }
    check("restart_30_panes", took, bound(1_000, 30_000));
}

/// Reboot restore <= 3 s: every holder and the server are killed (a reboot); the next call
/// brings the layout back with a fresh process in every slot.
#[test]
fn reboot_restore_with_30_panes() {
    let s = Session::new();
    let ids = thirty_panes(&s);
    let old: Vec<i64> = s
        .panes()
        .iter()
        .map(|p| p["child_pid"].as_i64().unwrap())
        .collect();
    let holders: Vec<i32> = old.iter().map(|c| parent_pid(*c)).collect();
    s.kill_server();
    for h in &holders {
        // SAFETY: killing our own holder processes (their children die with the pty).
        unsafe { libc::kill(*h, libc::SIGKILL) };
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while old.iter().any(|c| alive(*c)) {
        assert!(Instant::now() < deadline, "old processes still alive");
        std::thread::sleep(Duration::from_millis(20));
    }
    let t = Instant::now();
    let limit = bound(3_000, 60_000);
    loop {
        let panes = s.panes();
        let fresh = panes.len() == ids.len()
            && panes.iter().all(|p| {
                p["child_pid"]
                    .as_i64()
                    .is_some_and(|c| !old.contains(&c) && alive(c))
            });
        if fresh {
            break;
        }
        assert!(
            t.elapsed() < limit * 2,
            "layout not restored: {} panes after {:?}",
            panes.len(),
            t.elapsed()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    check("reboot_restore_30_panes", t.elapsed(), limit);
}
