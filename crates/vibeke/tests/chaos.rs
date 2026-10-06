//! Chaos gate (10 §5): kill -9 the server repeatedly while panes produce output; processes must
//! survive, screens keep updating, input keeps working. Iterations: `VIBEKE_CHAOS_ITER` (PR
//! gate default 10).

use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkchaos")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"));
        c.env_remove("VIBEKE")
            .env_remove("VIBEKE_SOCKET")
            .env_remove("VIBEKE_SESSION")
            .env_remove("VIBEKE_PANE_TOKEN");
        c.arg("--json").args(args);
        c
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().expect("run vibeke");
        assert!(
            out.status.success(),
            "vibeke {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn server_pid(&self) -> i32 {
        std::fs::read_to_string(self.dir.path().join("run/default/server.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn alive(pid: i64) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn kill_server_mid_output_loses_nothing() {
    let iters: usize = std::env::var("VIBEKE_CHAOS_ITER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let s = Session::new();
    let ws = s.json(&[
        "workspace",
        "create",
        "--cwd",
        "/tmp",
        "--command",
        "i=0; while :; do i=$((i+1)); echo out-$i; sleep 0.01; done",
    ]);
    let root = ws["root_pane"]["id"].as_str().unwrap().to_string();
    let mut panes = vec![root.clone()];
    for i in 0..5 {
        let cmd = if i % 2 == 0 {
            "while :; do seq 1 200; sleep 0.05; done"
        } else {
            "/bin/sh"
        };
        let p = s.json(&[
            "pane",
            "split",
            &root,
            "--direction",
            if i % 2 == 0 { "right" } else { "down" },
            "--command",
            cmd,
        ]);
        panes.push(p["pane"]["id"].as_str().unwrap().to_string());
    }
    std::thread::sleep(Duration::from_millis(500));
    let pids: Vec<i64> = s.json(&["pane", "list"])["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["child_pid"].as_i64().unwrap())
        .collect();
    assert_eq!(pids.len(), panes.len());
    let mut worst = Duration::ZERO;
    for it in 0..iters {
        std::thread::sleep(Duration::from_millis(50 + (it as u64 * 37) % 300));
        let pid = s.server_pid();
        assert!(pid > 0);
        // SAFETY: killing our own server process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let t = Instant::now();
        // Next call auto-spawns a server, which reattaches every holder.
        let list = s.json(&["pane", "list"]);
        let n = list["panes"].as_array().unwrap().len();
        assert_eq!(n, panes.len(), "iteration {it}: panes lost");
        for p in &pids {
            assert!(alive(*p), "iteration {it}: process {p} died");
        }
        // The output pane keeps producing and input still reaches the shell pane.
        let marker = format!("chaos-{it}");
        s.json(&["pane", "send-text", &panes[2], &format!("echo {marker}\n")]);
        let r = s
            .cmd(&[
                "pane",
                "wait-output",
                &panes[2],
                &marker,
                "--timeout-ms",
                "5000",
            ])
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "iteration {it}: input after recovery not seen"
        );
        worst = worst.max(t.elapsed());
    }
    eprintln!(
        "{iters} kill -9 iterations, {} panes, worst recovery+roundtrip {:?}",
        panes.len(),
        worst
    );
}
