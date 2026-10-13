//! Messages that wait inside a harness, and "Send now".
//!
//! `agent.prompt` presses Enter even while the agent works. The harness then holds the message
//! and delivers it later: Claude Code queues it until a tool boundary or the next turn, Codex
//! keeps it as a pending steer until the next tool call. This module knows what waits and makes
//! the harness deliver it right away.
//!
//! - **Claude.** The transcript records the queue (`queue-operation` lines; see
//!   [`vk_agents::transcript::Parser::queued_inputs`]). The tailer hands the parser's queue to
//!   [`from_transcript`]. Entries older than the harness process are left out: an old process
//!   wrote them and nobody will deliver them. Send now presses the user's `chat:sendNow` key from
//!   `~/.claude/keybindings.json` (default `ctrl+x ctrl+s`, which works over any terminal).
//! - **Codex.** Rollouts do not record pending input. The screen does: Codex lists pending steers
//!   under "Messages to be submitted after next tool call" above its input box.
//!   [`from_codex_screen`] reads that block on every screen evaluation. Send now presses Esc,
//!   which interrupts the turn and submits the pending steers. A bare Esc only interrupts, so it
//!   is sent only while the run works and the screen shows the block at that moment.
//!
//! The lists live in memory: both sources rebuild them (a transcript replay, the next screen).
//! A change emits `agent.queued_changed` with the count and ids, never the text.

use super::*;
use std::sync::LazyLock;
use vk_agents::transcript::QueuedInput;

/// One waiting message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// Stable while the message waits.
    pub id: String,
    pub text: String,
    pub since_ms: i64,
    /// `transcript` (Claude) or `screen` (Codex).
    pub source: &'static str,
}

impl Queued {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "text": vk_redact::redact(&self.text),
            "since_ms": self.since_ms,
            "source": self.source,
        })
    }
}

static QUEUES: LazyLock<Mutex<HashMap<String, Vec<Queued>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lines at the bottom of a Codex screen that may hold the pending-input block.
const CODEX_SCAN_ROWS: usize = 24;
const CODEX_STEER_HEADER: &str = "to be submitted after next tool call";
/// Claude's default `chat:sendNow` chord that works over any PTY (`ctrl+enter` needs a terminal
/// that reports modified Enter).
const CLAUDE_SEND_NOW_DEFAULT: &[&str] = &["ctrl+x ctrl+s", "ctrl+enter"];
const CLAUDE_SEND_NOW: &str = "chat:sendNow";
/// Pause between the strokes of a chord, so the harness reads them as separate keys.
const CHORD_GAP: Duration = Duration::from_millis(40);

/// The run's waiting messages, oldest first. Ended runs have none.
pub fn list(run: &AgentRun) -> Vec<Queued> {
    if run.ended_at_ms.is_some() {
        return vec![];
    }
    QUEUES
        .lock()
        .unwrap()
        .get(&run.id)
        .cloned()
        .unwrap_or_default()
}

/// Drop what is known about `run` (it ended or its tail went away).
pub fn forget(run: &str) {
    QUEUES.lock().unwrap().remove(run);
}

/// FNV-1a: ids that stay the same while the same message waits.
fn short_hash(parts: &[&[u8]]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in *p {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

/// Replace the run's list and emit `agent.queued_changed` when the ids changed.
fn store(server: &Server, run: &AgentRun, next: Vec<Queued>) {
    let changed = {
        let mut q = QUEUES.lock().unwrap();
        let prev = q.get(&run.id).map(Vec::as_slice).unwrap_or(&[]);
        let same = prev.len() == next.len() && prev.iter().zip(&next).all(|(a, b)| a.id == b.id);
        if same {
            false
        } else {
            if next.is_empty() {
                q.remove(&run.id);
            } else {
                q.insert(run.id.clone(), next.clone());
            }
            true
        }
    };
    if !changed {
        return;
    }
    let ids: Vec<&str> = next.iter().map(|q| q.id.as_str()).collect();
    server.with_core(|c| {
        let mut tx = Tx::new();
        tx.event(
            "agent.queued_changed",
            json!({"run": run.id, "pane": run.pane}),
            json!({"count": next.len(), "ids": ids}),
        );
        let _ = server.commit(c, tx);
    });
}

/// Claude's queue as its transcript describes it.
pub fn from_transcript(server: &Server, run_id: &str, items: Vec<QueuedInput>) {
    let Some(run) = server.with_core(|c| c.run(run_id).cloned()) else {
        forget(run_id);
        return;
    };
    if run.ended_at_ms.is_some() || headless::is_headless(&run) {
        forget(run_id);
        return;
    }
    let now = now_ms();
    let mut next = Vec::with_capacity(items.len());
    for (i, it) in items.into_iter().enumerate() {
        // A replayed entry an earlier process left behind is not waiting any more.
        if let Some(ts) = it.ts_ms
            && super::bg::predates_process(server, &run, ts)
        {
            continue;
        }
        let ts = it.ts_ms.unwrap_or(now);
        let id = short_hash(&[
            it.text.as_bytes(),
            ts.to_string().as_bytes(),
            i.to_string().as_bytes(),
        ]);
        next.push(Queued {
            id: format!("q{id}"),
            text: it.text,
            since_ms: ts,
            source: "transcript",
        });
    }
    store(server, &run, next);
}

/// The pending steers on a Codex screen, oldest first. `None` when the block is not shown;
/// an entry may be empty when Codex drew the header but the text is cut off.
pub fn codex_pending(lines: &[String]) -> Option<Vec<String>> {
    let from = lines.len().saturating_sub(CODEX_SCAN_ROWS);
    let tail = &lines[from..];
    let header = tail.iter().rposition(|l| {
        let t = l.trim_start();
        let lower = t.to_lowercase();
        lower.contains(CODEX_STEER_HEADER) && (t.starts_with('•') || lower.starts_with("messages"))
    })?;
    let mut items: Vec<String> = vec![];
    for l in &tail[header + 1..] {
        let t = l.trim();
        if t.is_empty() || t.starts_with('•') || t.starts_with('›') {
            break;
        }
        if let Some(rest) = t.strip_prefix('↳') {
            items.push(rest.trim().to_string());
            continue;
        }
        // A key hint (`⌥ + ↑ edit`) closes the block.
        if t.starts_with('⌥') || t.to_lowercase().starts_with("alt +") {
            break;
        }
        // A line that starts at the left edge is not part of the indented block.
        if !l.starts_with(char::is_whitespace) {
            break;
        }
        match items.last_mut() {
            Some(last) => {
                if !last.is_empty() {
                    last.push('\n');
                }
                last.push_str(t);
            }
            None => items.push(t.to_string()),
        }
    }
    if items.is_empty() {
        items.push(String::new());
    }
    Some(items)
}

/// Codex's pending steers as its screen shows them (called on each screen evaluation).
pub fn from_codex_screen(server: &Server, run: &AgentRun, lines: &[String]) {
    let texts = codex_pending(lines).unwrap_or_default();
    let prev = list(run);
    let now = now_ms();
    let mut next = Vec::with_capacity(texts.len());
    for (i, text) in texts.into_iter().enumerate() {
        let id = format!(
            "s{}",
            short_hash(&[text.as_bytes(), i.to_string().as_bytes()])
        );
        // Keep the time a message was first seen.
        let since_ms = prev
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.since_ms)
            .unwrap_or(now);
        next.push(Queued {
            id,
            text,
            since_ms,
            source: "screen",
        });
    }
    store(server, run, next);
}

/// One `chat:sendNow` binding: its keystrokes, and whether every stroke reaches the harness
/// over a plain PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub keys: Vec<String>,
    pub pty_safe: bool,
}

fn binding_of(chord: &str) -> Option<Binding> {
    let keys: Vec<String> = chord.split_whitespace().map(str::to_lowercase).collect();
    if keys.is_empty() {
        return None;
    }
    let mut pty_safe = true;
    for k in &keys {
        let ev = vk_term::keygrammar::parse_key(k).ok()?;
        use vk_proto::input::{Key, Mods, NamedKey};
        if ev.mods.0 & (Mods::SUPER.0 | Mods::HYPER.0) != 0 {
            pty_safe = false;
        }
        // Legacy encoding drops ctrl/shift on these keys (`ctrl+enter` is a plain CR).
        if ev.mods.0 & (Mods::CTRL.0 | Mods::SHIFT.0) != 0
            && matches!(
                ev.key,
                Key::Named(
                    NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace | NamedKey::Escape
                )
            )
        {
            pty_safe = false;
        }
    }
    Some(Binding { keys, pty_safe })
}

/// Normalized chord text, for comparing a user's key with a default.
fn norm(chord: &str) -> String {
    chord
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The chord that triggers Claude's `chat:sendNow`, from the text of `keybindings.json`
/// (`None` when the file does not exist). User bindings in the `Chat` or `Global` context win;
/// a default stays usable unless the user unbound it (`null`) or gave its key another action.
/// `Err` when no usable binding is left. An unreadable file falls back to the defaults.
pub fn claude_send_now_keys(file: Option<&str>) -> Result<Binding, String> {
    let mut user: Vec<Binding> = vec![];
    let mut taken: Vec<String> = vec![];
    if let Some(text) = file
        && let Ok(v) = serde_json::from_str::<Value>(text)
    {
        let blocks = v
            .get("bindings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for b in blocks {
            let ctx = b.get("context").and_then(Value::as_str).unwrap_or("Chat");
            if !matches!(ctx, "Chat" | "Global") {
                continue;
            }
            let Some(map) = b.get("bindings").and_then(Value::as_object) else {
                continue;
            };
            for (key, action) in map {
                match action.as_str() {
                    Some(CLAUDE_SEND_NOW) => {
                        if let Some(b) = binding_of(key) {
                            user.push(b);
                        }
                    }
                    // Unbound, or the key now does something else.
                    _ => taken.push(norm(key)),
                }
            }
        }
    }
    let defaults = CLAUDE_SEND_NOW_DEFAULT
        .iter()
        .filter(|d| !taken.contains(&norm(d)))
        .filter_map(|d| binding_of(d));
    let all: Vec<Binding> = user.into_iter().chain(defaults).collect();
    all.iter()
        .find(|b| b.pty_safe)
        .or(all.first())
        .cloned()
        .ok_or_else(|| {
            "Claude Code has no key for chat:sendNow: bind one in ~/.claude/keybindings.json \
             (for example \"ctrl+x ctrl+s\": \"chat:sendNow\" in the Chat context)"
                .to_string()
        })
}

fn claude_keybindings() -> Option<String> {
    let path = vk_agents::install::Dirs::from_env()
        .claude
        .join("keybindings.json");
    std::fs::read_to_string(path).ok()
}

/// `agent.queued`.
async fn queued(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "target"))?;
    let items: Vec<Value> = list(&run).iter().map(Queued::to_json).collect();
    Ok(json!({"run": run.id, "items": items}))
}

/// `agent.send_now`: make the harness deliver its waiting messages now.
async fn send_now(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "target"))?;
    if headless::is_headless(&run) {
        return Err(err(
            ErrorKind::Unsupported,
            "a headless run has no harness queue; send the message with agent.prompt mode steer",
        )
        .details(json!({"reason": "headless", "run": run.handle})));
    }
    let nothing = || {
        err(ErrorKind::Conflict, "nothing is queued").details(json!({"reason": "nothing_queued"}))
    };
    if dialog_on_pane(server, &run).is_some() {
        return Err(err(
            ErrorKind::Conflict,
            "the agent is showing a dialog; answer or cancel it first",
        )
        .details(json!({"reason": "dialog_open", "run": run.handle})));
    }
    let family = Harness::from_id(&run.harness).map(|h| h.family());
    let keys: Vec<String> = match family {
        Some(harness::Family::Claude) => {
            if list(&run).is_empty() {
                return Err(nothing());
            }
            claude_send_now_keys(claude_keybindings().as_deref())
                .map_err(|m| {
                    err(ErrorKind::Unsupported, m).details(json!({"reason": "no_send_now_key"}))
                })?
                .keys
        }
        Some(harness::Family::Codex) => {
            // Esc without a pending steer only interrupts: check the screen right now.
            if run.execution.value != Execution::Working {
                return Err(nothing());
            }
            let rt = server.pane_rt(&run.pane).ok_or_else(nothing)?;
            let lines = {
                let sc = rt.screen.lock().unwrap();
                screen::snapshot_of(&sc.engine).lines
            };
            if codex_pending(&lines).is_none() {
                from_codex_screen(server, &run, &lines);
                return Err(nothing());
            }
            vec!["esc".to_string()]
        }
        _ => {
            return Err(err(
                ErrorKind::Unsupported,
                "Send now works with Claude Code and Codex",
            )
            .details(json!({"reason": "harness", "harness": run.harness})));
        }
    };
    let count = list(&run).len();
    let modes = crate::render::input_modes(server, &run.pane);
    let mut strokes: Vec<Vec<u8>> = vec![];
    for k in &keys {
        let ev = vk_term::keygrammar::parse_key(k)
            .map_err(|e| err(ErrorKind::InvalidKey, e.to_string()).details(json!({"key": k})))?;
        strokes.push(vk_term::encode::encode_key(&ev, &modes));
    }
    // Keys a person types meanwhile wait until the chord is complete.
    let hold = server.input_hold.hold(&run.pane);
    for (i, bytes) in strokes.into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(CHORD_GAP).await;
        }
        crate::render::write_and_ack(server, &run.pane, server.next_internal_input_id(), bytes)
            .await;
    }
    drop(hold);
    // The screen block goes away once Codex takes the steers: look again soon.
    if family == Some(harness::Family::Codex) {
        let (srv, pane) = (server.clone(), run.pane.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            srv.agents.on_screen(&srv, &pane);
        });
    }
    let r = server.with_core(|c| c.run(&run.id).map(|r| run_json(c, r)));
    Ok(json!({"run": r, "sent": count, "keys": keys}))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "agent.queued" => queued(server, ctx, p).await,
        "agent.send_now" => send_now(server, ctx, p).await,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn codex_pending_reads_the_steer_block() {
        let lines = screen(
            "• Ran cargo test\n  └ ok\n\n• Working (12s • esc to interrupt)\n\n\
             • Messages to be submitted after next tool call (press esc to interrupt and send immediately)\n  \
             ↳ also update the docs\n  ↳ and the tests,\n    please\n\n\
             › Ask Codex to do anything\n\n  ? for shortcuts",
        );
        let p = codex_pending(&lines).unwrap();
        assert_eq!(p, vec!["also update the docs", "and the tests,\nplease"]);
    }

    #[test]
    fn codex_pending_ignores_follow_ups_and_missing_block() {
        let lines = screen(
            "• Working (3s • esc to interrupt)\n\n• Queued follow-up messages\n  ↳ later\n    ⌥ + ↑ edit\n\n› ",
        );
        assert_eq!(codex_pending(&lines), None);
        let lines = screen(
            "• Messages to be submitted after next tool call\n  ↳ now\n• Queued follow-up messages\n  ↳ later\n› ",
        );
        assert_eq!(codex_pending(&lines).unwrap(), vec!["now"]);
    }

    #[test]
    fn codex_pending_only_reads_the_bottom_of_the_screen() {
        let mut s = String::from(
            "• Messages to be submitted after next tool call\n  ↳ old text in the conversation\n",
        );
        for _ in 0..CODEX_SCAN_ROWS {
            s.push_str("output line\n");
        }
        assert_eq!(codex_pending(&screen(&s)), None);
    }

    #[test]
    fn claude_send_now_defaults_to_the_ctrl_x_chord() {
        let b = claude_send_now_keys(None).unwrap();
        assert_eq!(b.keys, vec!["ctrl+x", "ctrl+s"]);
        assert!(b.pty_safe);
        // A file that is not JSON falls back to the defaults.
        assert_eq!(claude_send_now_keys(Some("{ nope")).unwrap().keys, b.keys);
    }

    #[test]
    fn claude_send_now_uses_the_users_binding() {
        let f = r#"{"$schema":"x","bindings":[
            {"context":"Chat","bindings":{"ctrl+x ctrl+s":null,"alt+n":"chat:sendNow","ctrl+e":"chat:externalEditor"}}
        ]}"#;
        let b = claude_send_now_keys(Some(f)).unwrap();
        assert_eq!(b.keys, vec!["alt+n"]);
        // Another context is not the chat input.
        let f = r#"{"bindings":[{"context":"Transcript","bindings":{"alt+n":"chat:sendNow"}}]}"#;
        assert_eq!(
            claude_send_now_keys(Some(f)).unwrap().keys,
            vec!["ctrl+x", "ctrl+s"]
        );
    }

    #[test]
    fn claude_send_now_prefers_keys_that_survive_a_pty() {
        let f = r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+s":null}}]}"#;
        let b = claude_send_now_keys(Some(f)).unwrap();
        assert_eq!(b.keys, vec!["ctrl+enter"]);
        assert!(!b.pty_safe);
        let f = r#"{"bindings":[{"context":"Chat","bindings":{"cmd+s":"chat:sendNow","ctrl+k ctrl+n":"chat:sendNow"}}]}"#;
        assert_eq!(
            claude_send_now_keys(Some(f)).unwrap().keys,
            vec!["ctrl+k", "ctrl+n"]
        );
    }

    #[test]
    fn claude_send_now_unbound_is_an_error() {
        let f = r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+s":null,"ctrl+enter":"chat:submit"}}]}"#;
        let e = claude_send_now_keys(Some(f)).unwrap_err();
        assert!(e.contains("chat:sendNow"), "{e}");
    }

    #[test]
    fn ids_are_stable() {
        assert_eq!(short_hash(&[b"a", b"1"]), short_hash(&[b"a", b"1"]));
        assert_ne!(short_hash(&[b"a", b"1"]), short_hash(&[b"a1"]));
    }
}
