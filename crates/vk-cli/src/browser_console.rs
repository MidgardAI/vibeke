//! `vibeke browser console --pane <p> [--follow] [--console | --network] [--errors]` (06 B3.2):
//! a browser pane's console and network capture, redacted by the server. With `--follow` it
//! keeps printing new entries; on a terminal the keys `c` (console on/off), `n` (network on/off),
//! `e` (errors only on/off), `a` (everything) and `q` (quit) change the filter. This is what the
//! console split under a browser pane runs (`browser.pane.console`).

use crate::client::{CallError, Client};
use crate::{EXIT_API, EXIT_OK, Global, exit_code_for, print_error};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::time::Duration;
use vk_browser::capture::{Filter, format_line};

/// Poll interval while following.
const POLL: Duration = Duration::from_millis(400);
/// Entries shown when following starts or the filter changes.
const BACKLOG: u64 = 50;

/// The filter a follower starts with: `--console` / `--network` pick one ring, `--errors`
/// (or `--level error`, `--failed`) keeps errors only.
pub fn initial_filter(p: &Value) -> Filter {
    let flag = |k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(false);
    let (console, network) = match (flag("console"), flag("network")) {
        (true, false) => (true, false),
        (false, true) => (false, true),
        _ => match p.get("kind").and_then(Value::as_str) {
            Some("console") => (true, false),
            Some("network") => (false, true),
            _ => (true, true),
        },
    };
    Filter {
        console,
        network,
        errors: flag("errors")
            || flag("failed")
            || p.get("level").and_then(Value::as_str) == Some("error"),
        ..Default::default()
    }
}

/// Apply a key to the filter. Returns `None` for quit, `Some(changed)` otherwise.
pub fn on_key(f: &mut Filter, key: u8) -> Option<bool> {
    match key {
        b'q' | b'Q' | 3 | 4 => return None,
        b'c' | b'C' => {
            f.console = !f.console;
            if !f.console && !f.network {
                f.network = true;
            }
        }
        b'n' | b'N' => {
            f.network = !f.network;
            if !f.console && !f.network {
                f.console = true;
            }
        }
        b'e' | b'E' => f.errors = !f.errors,
        b'a' | b'A' => {
            f.console = true;
            f.network = true;
            f.errors = false;
        }
        _ => return Some(false),
    }
    Some(true)
}

/// `console+network`, `console · errors only`, …
pub fn describe(f: &Filter) -> String {
    let what = match (f.console, f.network) {
        (true, true) => "console+network",
        (true, false) => "console",
        _ => "network",
    };
    if f.errors {
        format!("{what} · errors only")
    } else {
        what.to_string()
    }
}

fn params(pane: &str, f: &Filter, after: u64, limit: u64, since: Option<&Value>) -> Value {
    let kind = match (f.console, f.network) {
        (true, false) => "console",
        (false, true) => "network",
        _ => "all",
    };
    let mut p =
        json!({"pane": pane, "kind": kind, "errors": f.errors, "after": after, "limit": limit});
    if let Some(s) = since {
        p["since"] = s.clone();
    }
    p
}

fn local_offset_s() -> i64 {
    // SAFETY: localtime_r writes into the zeroed tm we own.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff
    }
}

/// Colour for an entry on a terminal (errors red, warnings yellow).
fn colour(e: &Value) -> &'static str {
    let err = if e["kind"] == "network" {
        !e["error"].is_null() || e["status"].as_u64().is_some_and(|s| s >= 400)
    } else {
        e["level"] == "error"
    };
    if err {
        "\x1b[31m"
    } else if e["level"] == "warn" {
        "\x1b[33m"
    } else if e["kind"] == "network" {
        "\x1b[2m"
    } else {
        ""
    }
}

/// The line the follower prints for an entry: JSON, or [`format_line`] (coloured on a
/// terminal). Page-controlled text never reaches the terminal raw: `format_line` escapes
/// control characters, and JSON escapes them as `\u001b`.
pub fn render(e: &Value, as_json: bool, tty: bool, local_offset_s: i64) -> String {
    if as_json {
        serde_json::to_string(e).unwrap_or_default()
    } else if tty {
        format!("{}{}\x1b[0m", colour(e), format_line(e, local_offset_s))
    } else {
        format_line(e, local_offset_s)
    }
}

/// Terminal state for single-key filter changes (restored on drop).
struct RawStdin(Option<libc::termios>);

impl RawStdin {
    fn enter() -> RawStdin {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a termios we own.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawStdin(None);
            }
            let saved = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return RawStdin(None);
            }
            RawStdin(Some(saved))
        }
    }
}

impl Drop for RawStdin {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // SAFETY: restoring the attributes read in `enter`.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

/// Run `browser console --pane`. Returns the exit code.
pub async fn run<S>(client: &mut Client<S>, g: &Global, mut p: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some(pane) = p.get("pane").map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }) else {
        eprintln!(
            "vibeke browser console --pane <browser pane> [--follow] [--console|--network] [--errors]"
        );
        return crate::EXIT_USAGE;
    };
    let follow = p
        .as_object_mut()
        .and_then(|o| o.remove("follow"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut f = initial_filter(&p);
    let since = p.get("since").cloned();
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(200);
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let tty = std::io::stdout().is_terminal();
    let as_json = g.json.unwrap_or(!tty);
    let off = local_offset_s();
    let print = |e: &Value| println!("{}", render(e, as_json, tty, off));
    if !follow {
        return match client
            .call(
                "browser.console",
                params(&pane, &f, 0, limit, since.as_ref()),
            )
            .await
        {
            Ok(v) => {
                if as_json && !g.quiet {
                    println!("{}", serde_json::to_string(&v).unwrap_or_default());
                } else if !g.quiet {
                    for e in v["entries"].as_array().into_iter().flatten() {
                        print(e);
                    }
                }
                EXIT_OK
            }
            Err(e) => {
                print_error(&e);
                exit_code_for(&e)
            }
        };
    }
    // Following: single keys change the filter when we own a terminal.
    let interactive = !as_json && tty && std::io::stdin().is_terminal();
    let _raw = interactive.then(RawStdin::enter);
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
    if interactive {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut b = [0u8; 1];
            while let Ok(1) = std::io::stdin().read(&mut b) {
                if key_tx.send(b[0]).is_err() {
                    break;
                }
            }
        });
    }
    let header = |f: &Filter| {
        if interactive {
            println!(
                "\x1b[7m {} of {pane} — c console · n network · e errors only · a all · q quit \x1b[0m",
                describe(f)
            );
        } else if !as_json {
            println!("── {} of {pane} ──", describe(f));
        }
        let _ = std::io::stdout().flush();
    };
    header(&f);
    let mut after = 0u64;
    let mut first = true;
    loop {
        let lim = if first { BACKLOG } else { 500 };
        match client
            .call(
                "browser.console",
                params(&pane, &f, after, lim, since.as_ref().filter(|_| first)),
            )
            .await
        {
            Ok(v) => {
                for e in v["entries"].as_array().into_iter().flatten() {
                    print(e);
                    after = after.max(e["seq"].as_u64().unwrap_or(0));
                }
                // After the backlog, only entries newer than everything captured so far.
                if first {
                    after = after.max(v["last_seq"].as_u64().unwrap_or(0));
                }
                first = false;
                let _ = std::io::stdout().flush();
            }
            Err(CallError::Rpc(r)) if r.data.kind == "not_found" => {
                println!("browser pane {pane} is gone");
                return EXIT_OK;
            }
            Err(e) => {
                print_error(&e);
                return if matches!(e, CallError::Io(_)) {
                    EXIT_API
                } else {
                    exit_code_for(&e)
                };
            }
        }
        let key = tokio::time::timeout(POLL, key_rx.recv()).await;
        if let Ok(Some(k)) = key {
            match on_key(&mut f, k) {
                None => return EXIT_OK,
                Some(true) => {
                    header(&f);
                    after = 0;
                    first = true;
                }
                Some(false) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_from_flags_and_keys() {
        let f = initial_filter(&json!({"pane": "p", "network": true}));
        assert!(!f.console && f.network && !f.errors);
        let f = initial_filter(&json!({"pane": "p", "level": "error"}));
        assert!(f.console && f.network && f.errors);
        let mut f = initial_filter(&json!({"pane": "p"}));
        assert_eq!(describe(&f), "console+network");
        assert_eq!(on_key(&mut f, b'c'), Some(true));
        assert_eq!(describe(&f), "network");
        // Turning the last ring off turns the other one on.
        assert_eq!(on_key(&mut f, b'n'), Some(true));
        assert_eq!(describe(&f), "console");
        assert_eq!(on_key(&mut f, b'e'), Some(true));
        assert_eq!(describe(&f), "console · errors only");
        assert_eq!(on_key(&mut f, b'a'), Some(true));
        assert_eq!(describe(&f), "console+network");
        assert_eq!(on_key(&mut f, b'x'), Some(false));
        assert_eq!(on_key(&mut f, b'q'), None);
        let p = params("p", &f, 7, 50, None);
        assert_eq!(
            p,
            json!({"pane": "p", "kind": "all", "errors": false, "after": 7, "limit": 50})
        );
    }

    /// A page logs terminal escapes (OSC 52 clipboard write, title, notification, screen
    /// clear): what the follower prints, fed to a terminal engine, shows them as visible text
    /// and has no effect.
    #[test]
    fn hostile_console_text_is_inert_in_the_split() {
        let hostile = "copy \x1b]52;c;YXR0YWNrZXI=\x07 \x1b]2;owned\x07 \x1b]777;notify;a;b\x07 \x1b[2J \u{9b}0m";
        let entries = [
            json!({"kind": "console", "ts": 0, "level": "error", "text": hostile, "url": hostile, "line": 0, "seq": 1}),
            json!({"kind": "network", "ts": 0, "method": hostile, "url": hostile, "status": 500, "type": hostile, "seq": 2}),
        ];
        for (as_json, tty) in [(false, true), (false, false), (true, false)] {
            let mut engine = vk_term::Engine::new(160, 12, 100);
            let mut fx = Vec::new();
            for e in &entries {
                let line = render(e, as_json, tty, 0);
                engine.feed(format!("{line}\r\n").as_bytes(), &mut fx);
            }
            assert!(fx.is_empty(), "effects ({as_json}, {tty}): {fx:?}");
            assert_eq!(engine.title(), "");
            let screen = engine.screen_text();
            assert!(
                screen.contains("]52;c;YXR0YWNrZXI="),
                "the attempt stays visible: {screen}"
            );
        }
    }
}
