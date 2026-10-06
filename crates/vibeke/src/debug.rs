//! `vibeke debug ptyshot`: run a command in a PTY, send scripted input, render the output with
//! the VT engine and print the final screen. Used to test the TUI end to end.
//!
//! `--keys` script: `text` is typed as-is; `{...}` is a key in the key grammar (`{ctrl+b}`,
//! `{enter}`), `{sleep:300}` waits, `{wait:TEXT}` waits until TEXT is on screen (≤ 10 s).

use std::time::{Duration, Instant};
use vk_term::Engine;

pub fn ptyshot(args: &[String]) -> i32 {
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let cols: u16 = get("--cols").and_then(|v| v.parse().ok()).unwrap_or(120);
    let rows: u16 = get("--rows").and_then(|v| v.parse().ok()).unwrap_or(36);
    let settle: u64 = get("--settle").and_then(|v| v.parse().ok()).unwrap_or(600);
    let keys = get("--keys").unwrap_or_default();
    let Some(sep) = args.iter().position(|a| a == "--") else {
        eprintln!(
            "vibeke debug ptyshot [--cols N --rows N --keys SCRIPT --settle MS] -- cmd args…"
        );
        return 2;
    };
    let argv: Vec<String> = args[sep + 1..].to_vec();
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k != "VIBEKE" && k != "VIBEKE_SESSION")
        .chain([("TERM".into(), "xterm-256color".into())])
        .collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let (pty, mut child) = match vk_hold::pty::spawn(&argv, &cwd, &env, cols, rows) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("spawn: {e:#}");
            return 1;
        }
    };
    let mut engine = Engine::new(cols, rows, 1000);
    let mut fx = Vec::new();
    let pump = |engine: &mut Engine, fx: &mut Vec<vk_term::Effect>, dur: Duration| {
        let end = Instant::now() + dur;
        let mut buf = [0u8; 65536];
        while Instant::now() < end {
            match rustix::io::read(&pty.master, &mut buf) {
                Ok(n) if n > 0 => {
                    engine.feed(&buf[..n], fx);
                    // Answer terminal queries like a real host would.
                    for e in fx.drain(..) {
                        match e {
                            vk_term::Effect::Reply(b) => {
                                let _ = rustix::io::write(&pty.master, &b);
                            }
                            vk_term::Effect::Clipboard { data, .. } => {
                                eprintln!(
                                    "[host clipboard set: {:?}]",
                                    String::from_utf8_lossy(&data)
                                );
                            }
                            _ => {}
                        }
                    }
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    };
    pump(&mut engine, &mut fx, Duration::from_millis(settle));
    let mut chars = keys.chars().peekable();
    let mut buf = String::new();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut tok = String::new();
            for d in chars.by_ref() {
                if d == '}' {
                    break;
                }
                tok.push(d);
            }
            if !buf.is_empty() {
                let _ = rustix::io::write(&pty.master, buf.as_bytes());
                buf.clear();
                pump(&mut engine, &mut fx, Duration::from_millis(50));
            }
            if let Some(text) = tok.strip_prefix("paste:") {
                // What a host terminal sends for a drop/paste when bracketed paste is on.
                let b = format!("\x1b[200~{text}\x1b[201~");
                let _ = rustix::io::write(&pty.master, b.as_bytes());
                pump(&mut engine, &mut fx, Duration::from_millis(100));
            } else if let Some(ms) = tok.strip_prefix("sleep:") {
                pump(
                    &mut engine,
                    &mut fx,
                    Duration::from_millis(ms.parse().unwrap_or(100)),
                );
            } else if let Some(t) = tok.strip_prefix("wait:") {
                let end = Instant::now() + Duration::from_secs(10);
                while !engine.screen_text().contains(t) && Instant::now() < end {
                    pump(&mut engine, &mut fx, Duration::from_millis(50));
                }
            } else {
                match vk_term::keygrammar::parse_key(&tok) {
                    Ok(ev) => {
                        let bytes = vk_term::encode::encode_key(&ev, &engine.input_modes());
                        let _ = rustix::io::write(&pty.master, &bytes);
                        pump(&mut engine, &mut fx, Duration::from_millis(80));
                    }
                    Err(e) => eprintln!("bad key {tok}: {e}"),
                }
            }
        } else {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        let _ = rustix::io::write(&pty.master, buf.as_bytes());
    }
    pump(&mut engine, &mut fx, Duration::from_millis(settle));
    println!("{}", engine.screen_text());
    let _ = child.kill();
    let _ = child.wait();
    0
}

/// `vibeke debug fake-chromium [chromium args…]`: a fake Chromium speaking CDP on fds 3/4
/// (`--remote-debugging-pipe`), for browser-pane tests without a browser
/// (`VIBEKE_CHROMIUM=<script exec'ing this>`). `VIBEKE_FAKE_CHROMIUM_LOG` receives one JSON
/// line per CDP command.
pub fn fake_chromium(args: &[String]) -> i32 {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    // SAFETY: the launcher hands us fds 3 (commands) and 4 (responses) for our lifetime.
    let (r, w) = unsafe { (std::fs::File::from_raw_fd(3), std::fs::File::from_raw_fd(4)) };
    let state = std::sync::Arc::new(std::sync::Mutex::new(
        vk_browser::fake_chromium::State::default(),
    ));
    let log = std::env::var_os("VIBEKE_FAKE_CHROMIUM_LOG");
    let st = state.clone();
    let args_line = serde_json::json!({"argv": args}).to_string();
    if let Some(p) = &log
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    {
        let _ = writeln!(f, "{args_line}");
    }
    let logger = log.map(|p| {
        std::thread::spawn(move || {
            let mut seen = 0;
            loop {
                std::thread::sleep(Duration::from_millis(20));
                let (lines, closed): (Vec<String>, bool) = {
                    let s = st.lock().unwrap();
                    let v = s.log[seen..]
                        .iter()
                        .map(|(m, p, sid)| {
                            serde_json::json!({"method": m, "params": p, "session": sid})
                                .to_string()
                        })
                        .collect();
                    seen = s.log.len();
                    (v, s.closed)
                };
                if !lines.is_empty()
                    && let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&p)
                {
                    for l in lines {
                        let _ = writeln!(f, "{l}");
                    }
                }
                if closed {
                    return;
                }
            }
        })
    });
    vk_browser::fake_chromium::serve(r, w, vk_browser::fake_chromium::dpr_from_args(args), state);
    if let Some(h) = logger {
        let _ = h.join();
    }
    0
}

/// `vibeke debug latency [--n 500]`: added keystroke→screen latency of the render path vs a bare
/// PTY echo (10 §1.1). Uses an isolated pane running `cat` in the current session.
pub async fn latency(g: &vk_cli::Global, args: &[String]) -> i32 {
    use serde_json::json;
    use std::time::Instant;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use vk_proto::frame::asyncio;
    use vk_proto::render::{ClientFrame, PaneRect, ServerFrame};
    let n: usize = args
        .iter()
        .position(|a| a == "--n")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    // 1) Bare PTY: write a byte to `cat` (raw, -echo off so the tty echoes) and wait for it.
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let env: Vec<(String, String)> = std::env::vars().collect();
    let (pty, mut child) =
        vk_hold::pty::spawn(&["/bin/cat".to_string()], &cwd, &env, 80, 24).expect("pty");
    std::thread::sleep(Duration::from_millis(200));
    let mut bare = Vec::new();
    let mut buf = [0u8; 4096];
    for i in 0..n {
        let c = b'a' + (i % 26) as u8;
        let t = Instant::now();
        let _ = rustix::io::write(&pty.master, &[c]);
        loop {
            match rustix::io::read(&pty.master, &mut buf) {
                Ok(k) if k > 0 && buf[..k].contains(&c) => break,
                _ => std::hint::spin_loop(),
            }
            if t.elapsed() > Duration::from_secs(1) {
                break;
            }
        }
        bare.push(t.elapsed());
    }
    let _ = child.kill();
    let _ = child.wait();

    // 2) Through Vibeke: a pane running `cat`, keys as logical events, wait for the diff.
    let socket = vk_cli::client::socket_path(&g.session, g.socket.as_deref());
    let s = match vk_cli::client::connect_or_spawn(&g.session, &socket, false).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e:#}");
            return 1;
        }
    };
    let mut c = vk_cli::client::Client::new(s);
    let _ = c.hello("cli").await;
    let ws = c
        .call(
            "workspace.create",
            json!({"cwd": "/tmp", "name": "latency-probe", "command": ["/bin/cat"]}),
        )
        .await
        .expect("workspace");
    let pane = ws["root_pane"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let ws_id = ws["workspace"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let s2 = vk_cli::client::connect(&socket)
        .await
        .expect("render socket");
    let (rd, mut wr) = tokio::io::split(s2);
    let mut rd = BufReader::new(rd);
    let req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach","params":{"client_id":"latency","caps":{"max_fps":1000}}});
    wr.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    let mut line = String::new();
    rd.read_line(&mut line).await.unwrap();
    let mut w = tokio::io::BufWriter::new(wr);
    send_frame(
        &mut w,
        ClientFrame::ViewHint {
            panes: vec![PaneRect {
                pane: pane.clone(),
                cols: 80,
                rows: 24,
            }],
            active: true,
        },
    )
    .await;
    send_frame(&mut w, ClientFrame::Focus { pane: pane.clone() }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Drain the initial frames.
    let drain_until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < drain_until {
        if tokio::time::timeout(Duration::from_millis(50), asyncio::read_body(&mut rd))
            .await
            .is_err()
        {
            break;
        }
    }
    let mut via = Vec::new();
    for i in 0..n {
        let ch = (b'a' + (i % 26) as u8) as char;
        let t = Instant::now();
        send_frame(
            &mut w,
            ClientFrame::Key {
                input_id: i as u64 + 1,
                pane: pane.clone(),
                key: vk_proto::input::KeyEvent::ch(ch),
            },
        )
        .await;
        loop {
            let Ok(Ok(body)) =
                tokio::time::timeout(Duration::from_secs(1), asyncio::read_body(&mut rd)).await
            else {
                break;
            };
            let f: ServerFrame = match vk_proto::frame::decode(&body) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if let ServerFrame::PaneDiff {
                pane: p,
                epoch,
                rev,
                ops,
                ..
            } = &f
                && p == &pane
            {
                send_frame(
                    &mut w,
                    ClientFrame::Ack {
                        pane: pane.clone(),
                        epoch: *epoch,
                        rev: *rev,
                    },
                )
                .await;
                if format!("{ops:?}").contains(ch) {
                    break;
                }
            }
            if let ServerFrame::PaneFull {
                pane: p,
                epoch,
                rev,
                ..
            } = &f
                && p == &pane
            {
                send_frame(
                    &mut w,
                    ClientFrame::Ack {
                        pane: pane.clone(),
                        epoch: *epoch,
                        rev: *rev,
                    },
                )
                .await;
            }
        }
        via.push(t.elapsed());
        // Newline every 60 chars keeps rows short.
        if i % 60 == 59 {
            send_frame(
                &mut w,
                ClientFrame::Key {
                    input_id: 100_000 + i as u64,
                    pane: pane.clone(),
                    key: vk_proto::input::KeyEvent::named(vk_proto::input::NamedKey::Enter),
                },
            )
            .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let _ = c.call("workspace.close", json!({"workspace": ws_id})).await;
    let pct = |v: &mut Vec<Duration>, p: f64| {
        v.sort();
        v[((v.len() as f64 - 1.0) * p) as usize]
    };
    let (b50, b99) = (pct(&mut bare, 0.5), pct(&mut bare, 0.99));
    let (v50, v99) = (pct(&mut via, 0.5), pct(&mut via, 0.99));
    println!(
        "bare pty echo:     p50 {:>8.3} ms  p99 {:>8.3} ms",
        b50.as_secs_f64() * 1e3,
        b99.as_secs_f64() * 1e3
    );
    println!(
        "through vibeke:    p50 {:>8.3} ms  p99 {:>8.3} ms",
        v50.as_secs_f64() * 1e3,
        v99.as_secs_f64() * 1e3
    );
    println!(
        "added:             p50 {:>8.3} ms  p99 {:>8.3} ms   (budget p50 ≤ 1 ms, p99 ≤ 3 ms)",
        (v50.saturating_sub(b50)).as_secs_f64() * 1e3,
        (v99.saturating_sub(b99)).as_secs_f64() * 1e3
    );
    0
}

async fn send_frame<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, f: vk_proto::render::ClientFrame) {
    use tokio::io::AsyncWriteExt;
    let bytes = vk_proto::frame::encode(&f).unwrap();
    let _ = w.write_all(&bytes).await;
    let _ = w.flush().await;
}

/// `vibeke debug bandwidth --machine M [--seconds 20]`: bytes on the SSH link for the remote
/// bandwidth budgets (06 A7, 10 §1.5): idle panes, an unfocused spinner, a focused spinner.
pub async fn bandwidth(g: &vk_cli::Global, args: &[String]) -> i32 {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use vk_proto::frame::asyncio;
    use vk_proto::render::{ClientFrame, PaneRect, ServerFrame};
    let secs: u64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let Some(machine) = g.machine.clone() else {
        eprintln!("--machine required");
        return 2;
    };
    let cfg = crate::commands::load_config();
    let label = machine.clone();
    let target = cfg
        .remote
        .machine
        .iter()
        .find(|m| m.label == label)
        .map(|m| vk_remote::Target::parse(&m.label, &m.address))
        .unwrap_or_else(|| vk_remote::Target::parse(&label, &label));
    let (mux, _child) = target
        .bridge(vk_remote::bootstrap::REMOTE_BIN, &g.session)
        .await
        .expect("bridge");
    let stats = mux.stats();
    let ctl = mux.open("socket").await.expect("control");
    let mut c = vk_cli::client::Client::new(ctl);
    let _ = c.hello("cli").await;
    let spinner = "while :; do for c in '|' / - '\\\\'; do printf '\\r%s working on it' \"$c\"; sleep 0.1; done; done";
    let a = c
        .call(
            "workspace.create",
            json!({"name": "bw-probe", "command": ["/bin/sh", "-c", "sleep 100000"]}),
        )
        .await
        .expect("ws");
    let ws_id = a["workspace"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let pa = a["root_pane"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let b = c
        .call(
            "pane.split",
            json!({"pane": pa, "direction": "right", "command": ["/bin/sh", "-c", spinner]}),
        )
        .await
        .expect("split");
    let pb = b["pane"]["id"].as_str().unwrap_or_default().to_string();
    let r = mux.open("socket").await.expect("render");
    let (rd, wr) = tokio::io::split(r);
    let mut rd = BufReader::new(rd);
    let mut w = tokio::io::BufWriter::new(wr);
    let req = json!({"jsonrpc":"2.0","id":1,"method":"render.attach","params":{"client_id":"bw","remote":true,"caps":{"max_fps":60}}});
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    w.flush().await.unwrap();
    let mut line = String::new();
    rd.read_line(&mut line).await.unwrap();
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<ClientFrame>();
    tokio::spawn(async move {
        while let Ok(body) = asyncio::read_body(&mut rd).await {
            if let Ok(
                ServerFrame::PaneDiff {
                    pane, epoch, rev, ..
                }
                | ServerFrame::PaneFull {
                    pane, epoch, rev, ..
                },
            ) = vk_proto::frame::decode::<ServerFrame>(&body)
            {
                let _ = ack_tx.send(ClientFrame::Ack { pane, epoch, rev });
            }
        }
    });
    let hint = ClientFrame::ViewHint {
        panes: vec![
            PaneRect {
                pane: pa.clone(),
                cols: 80,
                rows: 24,
            },
            PaneRect {
                pane: pb.clone(),
                cols: 80,
                rows: 24,
            },
        ],
        active: true,
    };
    send_frame(&mut w, hint).await;
    for (label, focus) in [
        ("unfocused spinner (06 A7 budget ≤ 2 KiB/s)", pa.clone()),
        ("focused spinner (budget ≤ 8 KiB/s)", pb.clone()),
    ] {
        send_frame(&mut w, ClientFrame::Focus { pane: focus }).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let start = stats.bytes_in.load(std::sync::atomic::Ordering::Relaxed);
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            match tokio::time::timeout(Duration::from_millis(100), ack_rx.recv()).await {
                Ok(Some(f)) => send_frame(&mut w, f).await,
                Ok(None) => break,
                Err(_) => {}
            }
        }
        let bytes = stats.bytes_in.load(std::sync::atomic::Ordering::Relaxed) - start;
        println!(
            "{label:<48} {:>8.2} KiB/s  ({bytes} bytes in {secs}s)",
            bytes as f64 / 1024.0 / secs as f64
        );
    }
    // Idle: stop the spinner.
    let _ = c.call("pane.close", json!({"pane": pb})).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    while let Ok(f) = ack_rx.try_recv() {
        send_frame(&mut w, f).await;
    }
    let start = stats.bytes_in.load(std::sync::atomic::Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let bytes = stats.bytes_in.load(std::sync::atomic::Ordering::Relaxed) - start;
    println!(
        "{:<48} {:>8.2} KiB/s  ({bytes} bytes in {secs}s; keepalive pings included)",
        "idle (budget 0 B/s + keepalive)",
        bytes as f64 / 1024.0 / secs as f64
    );
    println!(
        "rtt: {:.2} ms",
        stats.rtt_us.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0
    );
    let _ = c.call("workspace.close", json!({"workspace": ws_id})).await;
    0
}
