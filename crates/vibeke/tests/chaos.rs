//! Chaos gate (10 §5): kill -9 the server repeatedly while panes produce output; processes must
//! survive, screens keep updating, input keeps working. Iterations: `VIBEKE_CHAOS_ITER` (PR
//! gate default 10).

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vk_proto::frame::{read_frame, write_frame};
use vk_proto::render::{AckStatus, ClientFrame, ServerFrame};

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
        // Right after a kill -9 the CLI auto-starts a new server; on Linux the first connection
        // can be reset while that server comes up (`io` error, ECONNRESET). Clients reconnect
        // (the TUI does), so retry transient I/O errors a few times; anything else fails at once.
        // The server-side cause on Linux is still open (spec 10 chaos notes).
        let mut tries = 0;
        loop {
            let out = self.cmd(args).output().expect("run vibeke");
            if out.status.success() {
                return serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            }
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            tries += 1;
            if tries < 4 && err.contains("\"kind\":\"io\"") {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            panic!("vibeke {args:?}: {err}");
        }
    }
    fn server_pid(&self) -> i32 {
        std::fs::read_to_string(self.dir.path().join("run/default/server.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
    fn kill_server(&self) {
        let pid = self.server_pid();
        assert!(pid > 0);
        // SAFETY: killing our own server process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid as i64) {
            assert!(Instant::now() < deadline, "server {pid} did not die");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("run/default/vibeke.sock")
    }
    fn pane(&self, id: &str) -> Value {
        self.json(&["pane", "list"])["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
    fn read(&self, pane: &str, source: &str, lines: &str) -> String {
        self.json(&["pane", "read", pane, "--source", source, "--lines", lines])["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
    fn wait_output(&self, pane: &str, pat: &str) {
        let r = self
            .cmd(&["pane", "wait-output", pane, pat, "--timeout-ms", "5000"])
            .output()
            .unwrap();
        assert!(r.status.success(), "{pat:?} not seen in {pane}");
    }
}

/// The holder is the parent of the pane's child process.
fn parent_pid(pid: i64) -> i32 {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// A render-protocol client driven directly (07 §3): `render.attach`, then binary frames. A
/// reader thread records every `InputAck` (and keeps draining frames so the server never
/// blocks on us).
struct RenderClient {
    w: UnixStream,
    acks: Arc<Mutex<HashMap<u64, AckStatus>>>,
}

impl RenderClient {
    fn attach(socket: &std::path::Path, client_id: &str) -> Self {
        let mut s = UnixStream::connect(socket).expect("connect render");
        let req = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"render.attach",
            "params":{"client_id": client_id, "protocol": vk_proto::render::PROTOCOL, "caps": {"max_fps": 30}}});
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        // Read the JSON reply byte by byte so no binary frame is buffered away.
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while b[0] != b'\n' {
            s.read_exact(&mut b).unwrap();
            line.push(b[0]);
        }
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert!(v.get("error").is_none(), "render.attach: {v}");
        let acks = Arc::new(Mutex::new(HashMap::new()));
        let mut r = s.try_clone().unwrap();
        let a = acks.clone();
        std::thread::spawn(move || {
            while let Ok(f) = read_frame::<_, ServerFrame>(&mut r) {
                if let ServerFrame::InputAck { input_id, status } = f {
                    a.lock().unwrap().insert(input_id, status);
                }
            }
        });
        RenderClient { w: s, acks }
    }
    fn paste(&mut self, pane: &str, input_id: u64, text: &str) -> bool {
        write_frame(
            &mut self.w,
            &ClientFrame::Paste {
                input_id,
                pane: pane.to_string(),
                text: text.to_string(),
            },
        )
        .is_ok()
    }
    fn acked(&self) -> HashMap<u64, AckStatus> {
        self.acks.lock().unwrap().clone()
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

/// 01 §1.2 "No input applied twice": a client streams numbered lines over the render
/// protocol, the server is killed -9 mid-stream (several times), and after each restart the
/// client resends every input it has no ack for, with the same client id and input ids. The
/// holder dedupes by `holder_input_id(client, input_id)`, so every line must arrive exactly
/// once — both in what the program received and on the recovered screen.
#[test]
fn input_not_duplicated_across_server_kill() {
    const N: u64 = 200;
    const KILL_EVERY: u64 = 50;
    let s = Session::new();
    let out = s.dir.path().join("received.txt");
    let script = format!(
        "stty -echo; while IFS= read -r l; do printf '%s\\n' \"$l\" >> '{}'; echo \"got:$l\"; done",
        out.display()
    );
    let ws = s.json(&["workspace", "create", "--cwd", "/tmp", "--command", &script]);
    let pane = ws["root_pane"]["id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(500));
    let client_id = "chaos-input-client";
    let mut acked: BTreeMap<u64, AckStatus> = BTreeMap::new();
    let mut c = RenderClient::attach(&s.socket(), client_id);
    let mut kills = 0;
    let mut resent = 0;
    let mut next = 1;
    while next <= N {
        // A batch of new lines, streamed without waiting for acks.
        let end = (next + KILL_EVERY - 1).min(N);
        for i in next..=end {
            if !c.paste(&pane, i, &format!("L{i}:\n")) {
                break;
            }
        }
        next = end + 1;
        if next > N {
            break;
        }
        // kill -9 while (some of) those inputs are in flight.
        std::thread::sleep(Duration::from_millis([0, 30, 5][kills % 3]));
        s.kill_server();
        kills += 1;
        acked.extend(c.acked());
        // Next CLI call auto-spawns a server, which reattaches the holder.
        assert_eq!(s.pane(&pane)["id"], pane.as_str());
        c = RenderClient::attach(&s.socket(), client_id);
        let batch = next - KILL_EVERY..next;
        for i in 1..next {
            // Unacked inputs are resent, as a client's ledger does. So is every 5th input of
            // the interrupted batch that *was* acked: that is the "written, but the ack died
            // with the server" case, made deterministic, which only holder dedupe can absorb.
            let ack_lost = batch.contains(&i) && i % 5 == 0;
            if !acked.contains_key(&i) || ack_lost {
                assert!(c.paste(&pane, i, &format!("L{i}:\n")));
                resent += 1;
            }
        }
    }
    // Every input is eventually acknowledged.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let now = c.acked();
        let missing: Vec<u64> = (1..=N)
            .filter(|i| !acked.contains_key(i) && !now.contains_key(i))
            .collect();
        if missing.is_empty() {
            acked.extend(now);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no ack for {} inputs: {missing:?}",
            missing.len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        acked.values().all(|st| *st == AckStatus::Written),
        "{acked:?}"
    );
    // Let the program drain everything.
    s.wait_output(&pane, &format!("got:L{N}:"));
    std::thread::sleep(Duration::from_millis(300));
    let count = |text: &str| -> (usize, usize) {
        let mut n: HashMap<u64, usize> = HashMap::new();
        for l in text.lines() {
            if let Some(i) = l
                .trim()
                .trim_start_matches("got:")
                .strip_prefix('L')
                .and_then(|r| r.strip_suffix(':'))
                .and_then(|r| r.parse().ok())
            {
                *n.entry(i).or_default() += 1;
            }
        }
        let dups = n.values().map(|c| c.saturating_sub(1)).sum();
        let lost = (1..=N).filter(|i| !n.contains_key(i)).count();
        (dups, lost)
    };
    let received = std::fs::read_to_string(&out).unwrap();
    let (dups, lost) = count(&received);
    let screen = s.read(&pane, "recent", "1000");
    let screen_lines: String = screen
        .lines()
        .filter(|l| l.trim().starts_with("got:"))
        .collect::<Vec<_>>()
        .join("\n");
    let (sdups, slost) = count(&screen_lines);
    eprintln!(
        "{N} inputs, {kills} server kills, {resent} resent: program received {dups} duplicates, \
         {lost} lost; pane scrollback shows {sdups} duplicates, {slost} missing"
    );
    assert_eq!(dups, 0, "duplicated input reached the program");
    assert_eq!(lost, 0, "unacked inputs were resent, none may be lost");
    assert_eq!(sdups, 0, "duplicated lines on the recovered screen");
    assert_eq!(slost, 0, "lines missing from the recovered scrollback");
}

/// Review finding 6: a dropped holder connection while the holder lives must reconnect, not
/// respawn the pane (which used to fail on the occupied socket and drop the pane, orphaning
/// its process). SIGUSR1 makes the holder drop its server connections.
#[test]
fn holder_connection_loss_reconnects_instead_of_respawning() {
    let s = Session::new();
    let ws = s.json(&[
        "workspace",
        "create",
        "--cwd",
        "/tmp",
        "--command",
        "/bin/sh",
    ]);
    let pane = ws["root_pane"]["id"].as_str().unwrap().to_string();
    s.json(&["pane", "send-text", &pane, "echo before-$((6*7))-drop\n"]);
    s.wait_output(&pane, "before-42-drop");
    let before = s.pane(&pane);
    let child = before["child_pid"].as_i64().unwrap();
    let holder = parent_pid(child);
    let server = s.server_pid();
    let occurrences = |t: &str| t.matches("before-42-drop").count();
    let n_before = occurrences(&s.read(&pane, "recent", "200"));
    assert_eq!(n_before, 1);
    for round in 0..3 {
        // SAFETY: signalling our own holder process.
        unsafe { libc::kill(holder, libc::SIGUSR1) };
        std::thread::sleep(Duration::from_millis(400));
        let p = s.pane(&pane);
        assert_eq!(p["id"], pane.as_str(), "round {round}: pane removed");
        assert_eq!(
            p["child_pid"].as_i64(),
            Some(child),
            "round {round}: respawned"
        );
        assert_ne!(p["recovered"], "lost", "round {round}");
        assert!(alive(child), "round {round}: process died");
        assert_eq!(s.server_pid(), server, "server restarted");
        let marker = format!("after-drop-{round}");
        s.json(&[
            "pane",
            "send-text",
            &pane,
            &format!("echo after-drop-$((0+{round}))\n"),
        ]);
        s.wait_output(&pane, &marker);
        // Reconnect replays only the missed bytes: nothing is applied to the screen twice.
        assert_eq!(occurrences(&s.read(&pane, "recent", "200")), n_before);
    }
}

/// Review finding 6 (snapshots): after a pane's holder is replaced, the old holder's VT
/// snapshot must never be restored over the new shell's screen.
#[test]
fn recovery_after_respawn_does_not_restore_the_old_screen() {
    let s = Session::new();
    std::fs::write(
        s.dir.path().join("config.toml"),
        "[terminal]\ndefault_shell = \"/bin/sh\"\n",
    )
    .unwrap();
    let ws = s.json(&[
        "workspace",
        "create",
        "--cwd",
        "/tmp",
        "--command",
        "/bin/sh",
    ]);
    let pane = ws["root_pane"]["id"].as_str().unwrap().to_string();
    // Plenty of output so the old snapshot's offset lies past the new holder's ring end.
    s.json(&[
        "pane",
        "send-text",
        &pane,
        "seq 1 300; echo OLD-SCREEN-$((2+2))\n",
    ]);
    s.wait_output(&pane, "OLD-SCREEN-4");
    // Idle > 2 s: the server snapshots the old screen.
    std::thread::sleep(Duration::from_millis(3000));
    let child = s.pane(&pane)["child_pid"].as_i64().unwrap();
    let holder = parent_pid(child);
    // SAFETY: killing our own holder (its child loses the PTY and exits too).
    unsafe { libc::kill(holder, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let p = s.pane(&pane);
        assert_eq!(p["id"], pane.as_str(), "pane removed after holder loss");
        if p["child_pid"].as_i64().is_some_and(|c| c != child) {
            break;
        }
        assert!(Instant::now() < deadline, "pane not respawned");
        std::thread::sleep(Duration::from_millis(100));
    }
    s.json(&["pane", "send-text", &pane, "echo NEW-$((1+1))-MARK\n"]);
    s.wait_output(&pane, "NEW-2-MARK");
    // Restart before the new holder's first snapshot is taken.
    s.kill_server();
    let p = s.pane(&pane);
    assert_eq!(p["id"], pane.as_str());
    let visible = s.read(&pane, "visible", "200");
    assert!(
        !visible.contains("OLD-SCREEN"),
        "old screen restored:\n{visible}"
    );
    assert!(
        visible.contains("NEW-2-MARK"),
        "new screen lost:\n{visible}"
    );
}
