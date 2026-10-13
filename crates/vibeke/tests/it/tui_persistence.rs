//! Graceful shutdown must retain archive rows that no longer fit in a pane snapshot.

use crate::support::{Session, alive};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use vk_store::archive::Archive;

const SCROLLBACK: usize = 2_048;
const ROWS_PER_BURST: usize = 6_000;
const BURSTS: usize = 3;

fn wait_until(label: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal_stop(s: &Session) {
    let pid = s.server_pid();
    assert!(pid > 0, "missing isolated server pid");
    // SAFETY: this pid belongs to the server started in our isolated session.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
}

fn wait_stopped(pid: i32) {
    wait_until("graceful server exit", || !alive(pid.into()));
}

fn release(s: &Session, burst: usize) {
    std::fs::write(s.dir.path().join(format!("go-{burst}")), "").unwrap();
}

fn wait_burst(s: &Session, pane: &str, burst: usize) {
    s.wait_output(pane, &format!("VKDONE_{burst}"), 30_000);
}

/// Run only after the server exits, so both the database and archive are stable. A holder
/// checkpoint must never cover rows that exist only in the dead server's archive buffer.
fn assert_snapshot_archive_prefix(s: &Session, pane: &str) {
    let state = s.dir.path().join("state/default");
    let store = vk_store::Store::open(&state.join("state.db")).unwrap();
    let snapshot = store
        .snapshot_for(pane)
        .unwrap()
        .expect("shutdown snapshot");
    let engine = vk_term::engine::Engine::restore(&snapshot.blob, SCROLLBACK).unwrap();
    let upto = engine.scrolled_total();
    assert!(upto > SCROLLBACK as u64, "snapshot must cover evicted rows");
    let rows = Archive::new(&state.join("scrollback"))
        .read(pane, 0, upto)
        .unwrap();
    assert_eq!(rows.len() as u64, upto, "snapshot exceeds durable archive");
    for (expected, row) in rows.iter().enumerate() {
        assert_eq!(
            row.n, expected as u64,
            "durable archive has a gap or duplicate"
        );
    }
}

#[test]
fn graceful_stop_and_restart_preserve_evicted_archive_rows() {
    let s = Session::builder()
        .prefix("vkpersist")
        .config(&format!("[terminal]\nscrollback_lines = {SCROLLBACK}\n"))
        .build();
    let script = s.dir.path().join("producer.sh");
    // Each output frame is at most 64 KiB. These 64-column rows keep a full frame smaller
    // than SCROLLBACK, while each complete burst is much larger than the memory window.
    // File barriers keep the same child alive and emitting across both kinds of restart.
    std::fs::write(
        &script,
        format!(
            r#"set -eu
root=$1
burst=1
i=1
while [ "$burst" -le {BURSTS} ]; do
    while [ ! -e "$root/go-$burst" ]; do sleep 0.01; done
    end=$((burst * {ROWS_PER_BURST}))
    while [ "$i" -le "$end" ]; do
        printf 'VKROW_%08d:abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuv\n' "$i"
        i=$((i + 1))
    done
    # Move the last numbered row from the visible screen into archived history.
    padding=0
    while [ "$padding" -lt 100 ]; do
        printf 'padding\n'
        padding=$((padding + 1))
    done
    printf 'VKDONE_%s\n' "$burst"
    : > "$root/done-$burst"
    burst=$((burst + 1))
done
while [ ! -e "$root/exit" ]; do sleep 0.01; done
"#
        ),
    )
    .unwrap();
    let params = json!({
        "cwd": "/tmp",
        "command": ["/bin/sh", script, s.dir.path()],
    });
    let workspace = s.json(&["api", "call", "workspace.create", &params.to_string()]);
    let pane = workspace["root_pane"]["id"].as_str().unwrap();
    release(&s, 1);
    wait_burst(&s, pane, 1);
    let child = s.pane(pane)["child_pid"].as_i64().unwrap();
    assert!(
        !s.read(pane, "recent", "10000").contains("VKROW_00000001:"),
        "the first rows must already be evicted from memory"
    );

    // Output continues in the holder while SIGTERM snapshots, flushes and exits. Do not
    // issue CLI calls during this interval: they could auto-start the next server early.
    let pid = s.server_pid();
    release(&s, 2);
    signal_stop(&s);
    wait_stopped(pid);
    wait_until("second burst emitted by surviving holder", || {
        s.dir.path().join("done-2").exists()
    });
    assert!(alive(child), "SIGTERM must preserve the pane process");
    assert_snapshot_archive_prefix(&s, pane);

    // The next CLI call starts a new server and replays everything after its snapshot.
    wait_burst(&s, pane, 2);
    wait_until("snapshot recovery", || {
        s.pane(pane)["recovered"] == "snapshot+replay"
    });
    assert_eq!(s.pane(pane)["child_pid"].as_i64(), Some(child));

    let before = s.json(&["server", "status"]);
    s.json(&["server", "restart"]);
    release(&s, 3);
    wait_until("exec restart", || {
        let output = s.cmd(&["server", "status"]).output().unwrap();
        let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        assert!(status["restart_error"].is_null(), "{status}");
        status["boot_id"].as_str().is_some_and(|id| {
            if Some(id) == before["boot_id"].as_str() {
                return false;
            }
            assert_eq!(status["pid"], before["pid"], "restart must exec in place");
            true
        })
    });
    wait_burst(&s, pane, 3);
    wait_until("snapshot recovery after exec", || {
        s.pane(pane)["recovered"] == "snapshot+replay"
    });
    assert_eq!(s.pane(pane)["child_pid"].as_i64(), Some(child));

    let pid = s.server_pid();
    signal_stop(&s);
    wait_stopped(pid);
    assert_snapshot_archive_prefix(&s, pane);
    let archived = Archive::new(&s.dir.path().join("state/default/scrollback"))
        .read(pane, 0, u64::MAX)
        .unwrap();
    let numbered: Vec<usize> = archived
        .iter()
        .filter_map(|row| row.t.strip_prefix("VKROW_"))
        .map(|row| row.split_once(':').unwrap().0.parse().unwrap())
        .collect();
    assert_eq!(
        numbered.len(),
        ROWS_PER_BURST * BURSTS,
        "all numbered output must survive in the durable archive"
    );
    for (index, number) in numbered.iter().enumerate() {
        assert_eq!(
            *number,
            index + 1,
            "archive row is missing, repeated or reordered"
        );
    }
}

#[test]
fn failed_archive_flush_is_reported_and_a_second_signal_forces_stop() {
    let s = Session::builder().prefix("vkstop").build();
    let script = s.dir.path().join("producer.sh");
    std::fs::write(
        &script,
        r#"set -eu
root=$1
for phase in initial outage; do
    if [ "$phase" = outage ]; then
        while [ ! -e "$root/go" ]; do sleep 0.01; done
    fi
    i=0
    while [ "$i" -lt 100 ]; do
        printf 'retained %s %s\n' "$phase" "$i"
        i=$((i + 1))
    done
    printf 'READY_%s\n' "$phase"
done
while [ ! -e "$root/exit" ]; do sleep 0.01; done
"#,
    )
    .unwrap();
    let params = json!({"cwd": "/tmp", "command": ["/bin/sh", script, s.dir.path()]});
    let workspace = s.json(&["api", "call", "workspace.create", &params.to_string()]);
    let pane = workspace["root_pane"]["id"].as_str().unwrap();
    s.wait_output(pane, "READY_initial", 30_000);
    let child = s.pane(pane)["child_pid"].as_i64().unwrap();
    let state = s.dir.path().join("state/default");
    let root = state.join("scrollback");
    let saved = state.join("scrollback.saved");
    // Unlike permissions, replacing the directory fails writes even when tests run as root.
    std::fs::rename(&root, &saved).unwrap();
    std::fs::write(&root, "not a directory").unwrap();
    std::fs::write(s.dir.path().join("go"), "").unwrap();
    s.wait_output(pane, "READY_outage", 30_000);
    let stop = s.cmd(&["server", "stop"]).output().unwrap();
    assert!(
        !stop.status.success(),
        "failed flush must not report successful stop"
    );
    let error = format!(
        "{}{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(error.contains("could not flush"), "{error}");
    let pid = s.server_pid();
    assert!(alive(pid.into()), "failed stop leaves the server serving");
    signal_stop(&s);
    wait_until("first signal's failed flush", || {
        std::fs::read_to_string(state.join("logs/server.log"))
            .unwrap_or_default()
            .contains("shutdown flush failed; retrying")
    });
    assert!(alive(pid.into()));
    signal_stop(&s);
    wait_stopped(pid);
    assert!(
        alive(child),
        "forced stop still retains the holder and child"
    );
    let store = vk_store::Store::open(&state.join("state.db")).unwrap();
    let events = store
        .events_after(0, 100, &["session.shutdown_unflushed".into()])
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "forced stop must record possible archive loss"
    );
    assert_eq!(events[0].data["reason"], "second_signal");
    // Restore storage before Session's normal holder cleanup.
    std::fs::remove_file(&root).unwrap();
    std::fs::rename(&saved, &root).unwrap();
}
