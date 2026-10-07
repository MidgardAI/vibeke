//! DCS tmux passthrough unwrap (03 §8, `terminal.allow_passthrough`). Programs that believe
//! they run inside tmux wrap sequences meant for the outer terminal as
//! `ESC P tmux; <payload with every ESC doubled> ESC \`. With passthrough allowed the pane
//! unwraps them and processes the payload as if the program had written it directly (kitty
//! graphics, OSC 52, notifications…); otherwise the whole wrapper is swallowed (the VT parser
//! would end the DCS at the first `ESC` and run the payload as if written directly, which is
//! exactly what the setting forbids).
//!
//! Streaming: a wrapper split across writes is held until it completes. A payload larger
//! than [`MAX_PAYLOAD`] is dropped.

/// `ESC P tmux;`
const INTRO: &[u8] = b"\x1bPtmux;";
/// Largest payload unwrapped (a 32 MiB image as base64 plus headroom).
pub const MAX_PAYLOAD: usize = 48 << 20;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Unwrap {
    /// A possible start of [`INTRO`] at the end of the last write.
    partial: Vec<u8>,
    /// Inside a wrapper: the unescaped payload so far.
    payload: Option<Vec<u8>>,
    /// The last payload byte was an `ESC` (doubled `ESC` or the terminator follows).
    esc: bool,
    /// The payload grew past [`MAX_PAYLOAD`]: the rest of the wrapper is skipped.
    overflow: bool,
    /// Passthrough is not allowed: wrappers are swallowed whole, payload never buffered.
    discard: bool,
}

impl Unwrap {
    /// A filter that removes every tmux wrapper together with its payload. Nothing is ever
    /// held back at a write boundary (the engine's output must not depend on how the stream
    /// is cut, and the filter's state is not part of a snapshot), so only a wrapper whose
    /// introducer arrives whole in one write is recognised.
    pub fn discarding() -> Self {
        Unwrap {
            discard: true,
            ..Default::default()
        }
    }

    /// Filter one write: what the engine should see.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut input: Vec<u8> = std::mem::take(&mut self.partial);
        input.extend_from_slice(bytes);
        let mut i = 0;
        while i < input.len() {
            if let Some(p) = self.payload.as_mut() {
                let b = input[i];
                i += 1;
                if self.esc {
                    self.esc = false;
                    match b {
                        b'\\' => {
                            // ST: the wrapper is complete.
                            let p = self.payload.take().unwrap_or_default();
                            if !std::mem::take(&mut self.overflow) {
                                out.extend_from_slice(&p);
                            }
                            continue;
                        }
                        0x1b => push(p, 0x1b, &mut self.overflow),
                        // A lone ESC inside the payload (not doubled): keep both bytes.
                        b => {
                            push(p, 0x1b, &mut self.overflow);
                            push(p, b, &mut self.overflow);
                        }
                    }
                } else if b == 0x1b {
                    self.esc = true;
                } else {
                    push(p, b, &mut self.overflow);
                }
                continue;
            }
            // Outside: look for the introducer.
            match input[i..].iter().position(|&b| b == 0x1b) {
                None => {
                    out.extend_from_slice(&input[i..]);
                    break;
                }
                Some(k) => {
                    out.extend_from_slice(&input[i..i + k]);
                    i += k;
                    let rest = &input[i..];
                    if rest.len() >= INTRO.len() {
                        if rest.starts_with(INTRO) {
                            self.payload = Some(Vec::new());
                            self.overflow = self.discard;
                            i += INTRO.len();
                        } else {
                            out.push(0x1b);
                            i += 1;
                        }
                    } else if !self.discard && INTRO.starts_with(rest) {
                        // Maybe the start of a wrapper: wait for the next write.
                        self.partial = rest.to_vec();
                        break;
                    } else {
                        out.push(0x1b);
                        i += 1;
                    }
                }
            }
        }
        out
    }

    /// Nothing is held back.
    pub fn idle(&self) -> bool {
        self.partial.is_empty() && self.payload.is_none()
    }
}

fn push(p: &mut Vec<u8>, b: u8, overflow: &mut bool) {
    if *overflow || p.len() >= MAX_PAYLOAD {
        *overflow = true;
        return;
    }
    p.push(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_tmux_dcs_with_doubled_escapes() {
        let mut u = Unwrap::default();
        let wrapped = b"a\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\b";
        assert_eq!(u.feed(wrapped), b"a\x1b]52;c;aGk=\x07b");
        assert!(u.idle());
        // Kitty graphics inside: the inner ST is a doubled ESC then `\`.
        let k = b"\x1bPtmux;\x1b\x1b_Ga=T,f=24;AAAA\x1b\x1b\\\x1b\\";
        assert_eq!(u.feed(k), b"\x1b_Ga=T,f=24;AAAA\x1b\\");
        // Other escapes pass through untouched.
        assert_eq!(
            u.feed(b"\x1b[31mred\x1bP1$r\x1b\\"),
            b"\x1b[31mred\x1bP1$r\x1b\\"
        );
    }

    #[test]
    fn wrappers_split_across_writes() {
        let mut u = Unwrap::default();
        let all = b"x\x1bPtmux;\x1b\x1b]2;title\x07\x1b\\y";
        let mut out = Vec::new();
        for b in all {
            out.extend(u.feed(&[*b]));
        }
        assert_eq!(out, b"x\x1b]2;title\x07y");
        // A held prefix that turns out not to be a wrapper is released.
        let mut u = Unwrap::default();
        assert_eq!(u.feed(b"\x1bPt"), b"");
        assert!(!u.idle());
        assert_eq!(u.feed(b"x"), b"\x1bPtx");
    }

    #[test]
    fn discarding_swallows_the_wrapper_and_its_payload() {
        let mut u = Unwrap::discarding();
        let wrapped = b"a\x1bPtmux;\x1b\x1b]2;t\x07\x1b\\b";
        assert_eq!(u.feed(wrapped), b"ab");
        // The payload may span writes; a bare ESC at a write boundary is never held.
        assert_eq!(u.feed(b"x\x1bPtmux;\x1b\x1b]2;t"), b"x");
        assert_eq!(u.feed(b"\x07\x1b"), b"");
        assert_eq!(u.feed(b"\\y"), b"y");
        assert_eq!(u.feed(b"\x1b"), b"\x1b");
        assert!(u.idle());
        assert_eq!(u.feed(b"\x1b[31mx"), b"\x1b[31mx");
    }

    #[test]
    fn oversized_payloads_are_dropped() {
        let mut u = Unwrap::default();
        let mut big = INTRO.to_vec();
        big.extend(std::iter::repeat_n(b'A', MAX_PAYLOAD + 10));
        big.extend_from_slice(b"\x1b\\after");
        assert_eq!(u.feed(&big), b"after");
    }
}
