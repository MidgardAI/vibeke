//! Showing untrusted text on a terminal.

use std::borrow::Cow;

/// Make `s` safe to print on a terminal: every control character — C0 (ESC, BEL, CR, LF, …),
/// DEL and C1 (U+0080–U+009F, which include the 8-bit CSI/OSC/ST introducers) — is replaced by
/// a visible escape (`\x1b`, `\u{9b}`), so page- or peer-supplied strings can't start an escape
/// sequence (OSC 52 clipboard writes, title changes, cursor moves) or forge lines. Tabs are
/// escaped too. Borrows when there is nothing to escape.
pub fn escape_controls(s: &str) -> Cow<'_, str> {
    if !s.chars().any(char::is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_control() {
            let n = c as u32;
            if n < 0x80 {
                out.push_str(&format!("\\x{n:02x}"));
            } else {
                out.push_str(&format!("\\u{{{n:x}}}"));
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_become_visible() {
        assert!(matches!(escape_controls("plain · ok"), Cow::Borrowed(_)));
        assert_eq!(
            escape_controls("a\x1b]52;c;aGk=\x07b\u{9b}2J\r\n\x7f"),
            "a\\x1b]52;c;aGk=\\x07b\\u{9b}2J\\x0d\\x0a\\x7f"
        );
        assert!(
            !escape_controls("\x1b\u{85}\u{9d}")
                .chars()
                .any(char::is_control)
        );
    }
}
