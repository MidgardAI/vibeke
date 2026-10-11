//! `vibeke gateway …` inside a Vibeke pane. A pane may run an agent, and an agent must not hand
//! out lasting access to this host on its own: `pair`, `share`, `peer invite` and `peer add` ask
//! the user through the pane's server (`auth.approve` of `gateway.call`, decided in the TUI) and
//! the gateway runs them once approved. The user's own shell in a pane keeps working: they approve
//! their own request in Vibeke. `--no-confirm` does not apply there, and a new device is always
//! confirmed in the TUI, out of the pane's reach.
//!
//! Links are shown on the terminal's alternate screen when it is one, so they don't stay in the
//! pane's scrollback.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use vk_proto::rpc::Request;

/// How long the CLI waits for the user's decision (the server's maximum).
const WAIT_MS: u64 = 600_000;

/// The message printed while waiting.
pub const WAITING: &str = "Waiting for your approval in Vibeke (the approval notice in the tab bar, or the approvals view)…";

/// Inside a pane without an elevation (`vibeke auth elevate`): access changes ask first.
pub fn in_pane() -> bool {
    in_pane_with(|k| std::env::var(k).ok())
}

fn in_pane_with(get: impl Fn(&str) -> Option<String>) -> bool {
    let set = |k: &str| get(k).is_some_and(|v| !v.is_empty());
    set("VIBEKE_PANE_TOKEN") && !set("VIBEKE_ELEVATED_TOKEN")
}

/// Why a request failed, in words for the terminal.
fn failure(e: &Value) -> String {
    let msg = e["message"].as_str().unwrap_or("the request failed");
    let kind = e
        .pointer("/data/kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if msg.starts_with("approval_denied") {
        "denied: the request was not approved in Vibeke; nothing was created".into()
    } else if msg.starts_with("approval_withdrawn") {
        "the request was withdrawn before a decision; nothing was created".into()
    } else if kind == "timeout" {
        "no decision in time; the request was withdrawn and nothing was created".into()
    } else {
        msg.to_string()
    }
}

/// Ask the user to run `gateway.call {method, params}` and wait for the decision. Returns the
/// gateway's result. Ctrl-C closes the connection, which withdraws the request.
pub async fn ask(method: &str, params: Value) -> Result<Value> {
    let session = std::env::var("VIBEKE_SESSION").unwrap_or_else(|_| "default".into());
    let path = crate::server::socket_path(None, &session);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to the Vibeke server at {}", path.display()))?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let token = std::env::var("VIBEKE_PANE_TOKEN").unwrap_or_default();
    let hello = json!({"client": "vibeke-gateway-cli", "version": env!("CARGO_PKG_VERSION"),
                       "api": "vibeke/1", "kind": "cli", "token": token});
    let ask = json!({"method": "gateway.call", "timeout_ms": WAIT_MS,
                     "params": {"method": method, "params": params}});
    for (id, m, p) in [(1, "client.hello", hello), (2, "auth.approve", ask)] {
        let line = serde_json::to_string(&Request::new(id, m, p))?;
        w.write_all(format!("{line}\n").as_bytes()).await?;
        if id == 2 {
            eprintln!("{WAITING}");
        }
        let resp = tokio::select! {
            r = read_response(&mut lines, id) => r?,
            _ = tokio::signal::ctrl_c() => bail!("withdrawn: nothing was created"),
        };
        if let Some(e) = resp.get("error") {
            bail!("{}", failure(e));
        }
        if id == 2 {
            return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
        }
    }
    unreachable!("the loop returns on the second response")
}

/// The response with `id`; notifications and other ids are skipped.
async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    id: u64,
) -> Result<Value> {
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(v);
        }
    }
    bail!("the Vibeke server closed the connection")
}

/// The terminal's alternate screen while it lives (when stdout is a terminal): what is shown
/// there leaves no scrollback once it is dropped.
pub struct AltScreen(bool);

impl AltScreen {
    pub fn enter() -> AltScreen {
        let tty = std::io::stdout().is_terminal();
        if tty {
            print!("\x1b[?1049h\x1b[H\x1b[2J");
            let _ = std::io::stdout().flush();
        }
        AltScreen(tty)
    }

    /// Whether the screen is a terminal's alternate screen (else output stays in the log).
    pub fn active(&self) -> bool {
        self.0
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        if self.0 {
            print!("\x1b[?1049l");
            let _ = std::io::stdout().flush();
        }
    }
}

/// Show `text` on the alternate screen until the user presses Enter (or Ctrl-C), then leave it.
/// Without a terminal it is printed as it is.
pub async fn show_until_enter(text: &str) {
    let screen = AltScreen::enter();
    println!("{text}");
    if !screen.active() {
        return;
    }
    println!("\nPress Enter when you are done. The link then disappears from this pane.");
    let _ = std::io::stdout().flush();
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    });
    tokio::select! {
        _ = read => {}
        _ = tokio::signal::ctrl_c() => {}
        // Never hold the pane forever.
        _ = tokio::time::sleep(Duration::from_secs(30 * 60)) => {}
    }
    drop(screen);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_are_recognized_by_their_token() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(in_pane_with(env(&[("VIBEKE_PANE_TOKEN", "t")])));
        assert!(!in_pane_with(env(&[])));
        assert!(!in_pane_with(env(&[("VIBEKE_PANE_TOKEN", "")])));
        // An approved elevation acts as the user.
        assert!(!in_pane_with(env(&[
            ("VIBEKE_PANE_TOKEN", "t"),
            ("VIBEKE_ELEVATED_TOKEN", "e")
        ])));
    }

    #[test]
    fn failures_read_as_sentences() {
        let denied =
            json!({"message": "approval_denied: no", "data": {"kind": "permission_denied"}});
        assert!(failure(&denied).starts_with("denied"));
        let timeout = json!({"message": "x", "data": {"kind": "timeout"}});
        assert!(failure(&timeout).starts_with("no decision in time"));
        let other =
            json!({"message": "the gateway isn't running", "data": {"kind": "remote_unavailable"}});
        assert_eq!(failure(&other), "the gateway isn't running");
    }
}
