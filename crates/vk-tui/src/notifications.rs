//! Notifications on the client (08 §7): toasts for `Notify` frames with coalesced counts, the
//! host-terminal OSC 9 forward (skipped when the server already delivered a native OS
//! notification, `Notify.delivered` contains `native`), and the host identity sent in
//! `render.attach` so the server can raise this terminal on click-to-focus.

use crate::app::{App, Toast};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct State {
    /// (machine, pane or `title:<title>`) → (toast text without the count, count, last at).
    pub groups: HashMap<(usize, String), (String, u32, Instant)>,
    /// Tests: OSC forwards land here instead of the host terminal.
    pub osc_sink: Vec<String>,
}

/// `{bundle_id, term_program}` from the given values; `None` when neither is known.
pub fn host_from(bundle_id: Option<String>, term_program: Option<String>) -> Option<Value> {
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (b, t) = (clean(bundle_id), clean(term_program));
    if b.is_none() && t.is_none() {
        return None;
    }
    Some(json!({"bundle_id": b, "term_program": t}))
}

/// This process's host terminal (`$__CFBundleIdentifier`, `$TERM_PROGRAM`).
pub fn host_meta() -> Option<Value> {
    host_from(
        std::env::var("__CFBundleIdentifier").ok(),
        std::env::var("TERM_PROGRAM").ok(),
    )
}

const TOAST_TTL: Duration = Duration::from_secs(6);

/// Make pane-controlled text safe to embed in an escape sequence written to the host terminal:
/// drops every control character (C0, DEL, C1 incl. U+009C/U+009D/U+009B, CR/LF, ...) and bidi
/// override/isolate/mark characters, then caps the result at `max` characters.
pub(crate) fn host_safe(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c as u32,
                    0x202A..=0x202E | 0x2066..=0x2069 | 0x200E | 0x200F | 0x061C)
        })
        .take(max)
        .collect()
}

/// A `Notify` frame from machine `mi`.
pub fn on_notify(
    app: &mut App,
    mi: usize,
    title: String,
    body: String,
    pane: Option<String>,
    delivered: Vec<String>,
) {
    let label = if app.machines.len() > 1 {
        format!("[{}] ", app.machines[mi].label)
    } else {
        String::new()
    };
    let text = if body.is_empty() {
        format!("{label}{title}")
    } else {
        format!("{label}{title}: {body}")
    };
    let focused_here =
        pane.is_some() && pane == app.focused_pane() && app.cur == mi && app.host_focused;
    if focused_here && app.config.notifications.suppress_when_focused {
        return;
    }
    let now = Instant::now();
    let window = Duration::from_millis(app.config.notifications.coalesce_ms.max(1000) as u64);
    let key = (mi, pane.clone().unwrap_or_else(|| format!("title:{title}")));
    // Coalesce: another notification about the same pane while its toast is still up (or
    // within `coalesce_ms`) bumps a count on that toast instead of stacking a new one.
    let target = pane.clone().map(|p| (mi, p));
    let mut count = 1;
    if let Some((_, n, last)) = app.parity.notes.groups.get(&key).cloned()
        && now.duration_since(last) < window.max(TOAST_TTL)
        && let Some(t) = app
            .toasts
            .iter_mut()
            .rev()
            .find(|t| t.pane == target && t.until > now)
    {
        count = n + 1;
        t.text = format!("{text} (×{count})");
        t.until = now + TOAST_TTL;
    } else {
        app.toasts.push(Toast {
            text: text.clone(),
            until: now + TOAST_TTL,
            pane: target,
        });
        if app.toasts.len() > 3 {
            app.toasts.remove(0);
        }
    }
    app.parity.notes.groups.insert(key, (text, count, now));
    app.parity
        .notes
        .groups
        .retain(|_, (_, _, at)| now.duration_since(*at) < Duration::from_secs(60));
    app.dirty = true;
    // Forward to the host terminal so it can raise its own notification (08 §7.1) — unless the
    // server already showed a native one, or this one was coalesced into a visible toast.
    if !app.host_focused && count == 1 && !delivered.iter().any(|d| d == "native") {
        let osc = format!("\x1b]9;{}\x07", host_safe(&title, 200));
        if cfg!(test) {
            app.parity.notes.osc_sink.push(osc);
        } else {
            let _ = std::io::stdout().write_all(osc.as_bytes());
        }
    }
}

#[cfg(test)]
mod host_safe_tests {
    use super::host_safe;

    #[test]
    fn strips_c1_newlines_and_bidi() {
        let out = host_safe(
            "a\u{9c}b\u{9d}c\u{9b}d\ne\r\x18\x1a\x1b\x07\u{202e}f\u{2066}g\u{200f}",
            200,
        );
        assert_eq!(out, "abcdefg");
    }

    #[test]
    fn caps_length() {
        assert_eq!(host_safe(&"x".repeat(500), 200).chars().count(), 200);
    }
}
