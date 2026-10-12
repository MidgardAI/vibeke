//! A blocked prompt that the harness put back (spec 19 §4.3). When the `UserPromptSubmit` hook
//! blocks a person's leading mention, Claude Code shows "operation blocked by hook" and restores
//! the prompt into its input box; Codex does the same. That leftover draft would make every
//! later delivery into the pane wait (the box is not empty) and looks like an unsent prompt.
//!
//! So after a leading-mention handoff, [`spawn`] watches the input box for a few seconds. Only
//! when the box holds exactly the routed prompt (whitespace aside) does it clear the box with
//! the harness's line-kill key, under the pane's input hold, and re-read the box between
//! presses. Any other text is the person's: it is left alone.

use super::input_box::{self, InputBox};
use crate::Server;
use crate::agents::harness::{Family, Harness};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the harness may take to put the prompt back.
const WATCH: Duration = Duration::from_secs(3);
/// Between two reads of the box.
const POLL: Duration = Duration::from_millis(120);
/// After a key, before the box is read again.
const SETTLE: Duration = Duration::from_millis(90);
/// Most keys sent to clear one prompt.
const MAX_KEYS: usize = 20;
/// The time the clearing may take: within the input hold ([`crate::input_hold::MAX_HOLD`]).
const CLEAR_BUDGET: Duration = Duration::from_millis(1800);

/// Kill the current line (Claude Code and Codex: ctrl+u).
const LINE_KILL: &[u8] = b"\x15";
/// Join a now empty line with the one above (backspace).
const BACKSPACE: &[u8] = b"\x7f";

/// What the watcher does with what the input box shows now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The box holds exactly the routed prompt: clear it.
    Restored,
    /// Nothing there yet (or the box cannot be read): keep watching.
    Wait,
    /// Other text: the person's. Leave it alone (and keep watching, a frame may be partial).
    Other,
}

/// The next key while clearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The box is empty.
    Done,
    /// Send the line-kill key.
    Kill,
    /// The last key changed nothing (an empty last line): send backspace.
    Backspace,
    /// The box holds something that is not what is left of the prompt: stop.
    Stop,
}

/// Text without any whitespace: soft wraps and the box's indentation may split the prompt
/// anywhere, even inside a word.
pub(crate) fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether the harness of `h` restores a blocked prompt that Vibeke should clear.
pub(crate) fn applies(h: Harness) -> bool {
    matches!(h.family(), Family::Claude | Family::Codex)
}

/// What the box shows, compared with the routed prompt.
pub(crate) fn seen(found: &InputBox, prompt: &str) -> Seen {
    match found {
        InputBox::Draft(t) if !t.trim().is_empty() && compact(t) == compact(prompt) => {
            Seen::Restored
        }
        InputBox::Draft(_) => Seen::Other,
        InputBox::Empty | InputBox::Unknown => Seen::Wait,
    }
}

/// The next key, given what the box shows now and what it showed before the last key.
/// What is left of the prompt is always a non-empty start of it: the line-kill key removes the
/// last line.
pub(crate) fn step(found: &InputBox, prompt: &str, before: Option<&str>) -> Step {
    let t = match found {
        InputBox::Empty => return Step::Done,
        InputBox::Unknown => return Step::Stop,
        InputBox::Draft(t) => compact(t),
    };
    if t.is_empty() {
        return Step::Done;
    }
    if !compact(prompt).starts_with(&t) {
        return Step::Stop;
    }
    match before {
        Some(b) if b == t => Step::Backspace,
        _ => Step::Kill,
    }
}

/// After a leading mention from `pane` was sent as a message: clear the prompt the harness
/// restores into the input box, if (and only if) the box shows exactly that prompt.
pub(crate) fn spawn(server: &Arc<Server>, pane: &str, h: Harness, prompt: &str) {
    if !applies(h) {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let server = server.clone();
    let pane = pane.to_string();
    let prompt = prompt.to_string();
    rt.spawn(async move {
        clear_restored(&server, &pane, h, &prompt).await;
    });
}

async fn clear_restored(server: &Arc<Server>, pane: &str, h: Harness, prompt: &str) {
    let read = || input_box::of_pane(server, pane, Some(h));
    let deadline = Instant::now() + WATCH;
    loop {
        tokio::time::sleep(POLL).await;
        let Some(found) = read() else {
            return;
        };
        if seen(&found, prompt) == Seen::Restored {
            break;
        }
        if Instant::now() >= deadline {
            return;
        }
    }
    // People's keys wait while the box is cleared, so none is erased with it.
    let hold = server.input_hold.hold(pane);
    let started = Instant::now();
    // Re-read under the hold: the person may have typed since.
    let Some(found) = read() else {
        return;
    };
    if seen(&found, prompt) != Seen::Restored {
        return;
    }
    let mut current = found;
    let mut before: Option<String> = None;
    for _ in 0..MAX_KEYS {
        let key = match step(&current, prompt, before.as_deref()) {
            Step::Done => break,
            Step::Stop => {
                tracing::debug!(pane, "mentions: restored prompt changed while clearing");
                break;
            }
            Step::Kill => LINE_KILL,
            Step::Backspace => BACKSPACE,
        };
        if started.elapsed() >= CLEAR_BUDGET {
            break;
        }
        before = match &current {
            InputBox::Draft(t) => Some(compact(t)),
            _ => None,
        };
        let Some(prt) = server.pane_rt(pane) else {
            return;
        };
        // Written directly (not through the hold, which buffers people's input only), and
        // without touching the pane's last-input time: this is not the person typing.
        prt.send(crate::pane::PaneCmd::Input {
            id: server.next_internal_input_id(),
            bytes: key.to_vec(),
            ack: None,
        });
        tokio::time::sleep(SETTLE).await;
        let Some(next) = read() else {
            return;
        };
        current = next;
    }
    drop(hold);
    if current != InputBox::Empty {
        tracing::debug!(pane, "mentions: the restored prompt was not cleared");
    }
    server.mentions.poke();
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
