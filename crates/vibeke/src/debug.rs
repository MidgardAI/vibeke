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
                        if let vk_term::Effect::Reply(b) = e {
                            let _ = rustix::io::write(&pty.master, &b);
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
