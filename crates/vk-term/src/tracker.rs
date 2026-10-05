//! Escape scanner running beside libghostty-vt for the two things the engine parses but does
//! not expose (see `vendor/libghostty-vt.patches.md`, "Gaps handled outside the engine"):
//!
//! - `CSI > 4 ; Pv m` — the xterm modifyOtherKeys **level** (Ghostty keeps only a level-2 bool).
//! - `OSC 99` — kitty desktop notifications (Ghostty parses and drops them).
//!
//! Everything else the M0 tracker did (pending parser bytes, OSC 7/9/777/133, XTVERSION, DA3,
//! sync-update 2026) is now the engine's: parser continuation lives in the native snapshot.
//! The scanner skips ground-state text with `memchr`-style search for ESC, so it costs little.
//! After a restore it is resynchronised by feeding it the engine's exported continuation.

const MAX_OSC: usize = 64 * 1024;
const MAX_CSI: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    /// OSC body; `collect` says whether this OSC is one we keep (prefix "99;").
    Osc,
    OscEsc,
    /// DCS / APC / PM / SOS string: skipped until ST.
    Str,
    StrEsc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    /// `CSI > 4 ; Pv m`: new modifyOtherKeys level (0..=2).
    ModifyOtherKeys(u8),
    /// Full reset (`ESC c`).
    Reset,
    /// Body of an `OSC 99 ; ...` sequence, without the leading `99;`.
    Osc99(Vec<u8>),
}

#[derive(Debug, Clone, Default)]
pub struct Tracker {
    state: State,
    seq: Vec<u8>,
    /// For `Osc`: still undecided (< 3 bytes) or confirmed "99;".
    osc_keep: bool,
}

impl Tracker {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Tracked>) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(p) => {
                        self.state = State::Esc;
                        i += p + 1;
                    }
                    None => return,
                },
                State::Str => match bytes[i..]
                    .iter()
                    .position(|&b| matches!(b, 0x1b | 0x18 | 0x1a))
                {
                    Some(p) => {
                        let b = bytes[i + p];
                        self.state = if b == 0x1b {
                            State::StrEsc
                        } else {
                            State::Ground
                        };
                        i += p + 1;
                    }
                    None => return,
                },
                _ => {
                    self.step(bytes[i], out);
                    i += 1;
                }
            }
        }
    }

    fn step(&mut self, b: u8, out: &mut Vec<Tracked>) {
        use State::*;
        // CAN / SUB abort any sequence.
        if matches!(b, 0x18 | 0x1a) {
            self.state = Ground;
            return;
        }
        match self.state {
            Ground | Str => unreachable!("handled in feed"),
            Esc => match b {
                b'[' => {
                    self.seq.clear();
                    self.state = Csi;
                }
                b']' => {
                    self.seq.clear();
                    self.osc_keep = true;
                    self.state = Osc;
                }
                b'P' | b'_' | b'^' | b'X' => self.state = Str,
                b'c' => {
                    out.push(Tracked::Reset);
                    self.state = Ground;
                }
                0x1b => {}
                // Intermediates keep us in the escape; anything else ends it.
                0x20..=0x2f => {}
                _ => self.state = Ground,
            },
            Csi => match b {
                0x40..=0x7e => {
                    self.state = Ground;
                    if b == b'm' && self.seq.first() == Some(&b'>') {
                        let ps = params(&self.seq[1..]);
                        if ps.first() == Some(&4) {
                            out.push(Tracked::ModifyOtherKeys(
                                ps.get(1).copied().unwrap_or(0).min(2) as u8,
                            ));
                        }
                    }
                }
                0x1b => self.state = Esc,
                _ => {
                    if self.seq.len() < MAX_CSI {
                        self.seq.push(b)
                    }
                }
            },
            Osc => match b {
                0x07 => {
                    self.state = Ground;
                    self.finish_osc(out);
                }
                0x1b => self.state = OscEsc,
                _ => {
                    if self.osc_keep {
                        self.seq.push(b);
                        if self.seq.len() <= 3 {
                            self.osc_keep = b"99;".starts_with(&self.seq);
                        } else if self.seq.len() > MAX_OSC {
                            self.osc_keep = false;
                        }
                    }
                }
            },
            OscEsc => {
                if b == b'\\' {
                    self.state = Ground;
                    self.finish_osc(out);
                } else {
                    // ESC ends the OSC (as in the VT parser) and starts a new escape.
                    self.state = Esc;
                    self.step(b, out);
                }
            }
            StrEsc => {
                self.state = if b == b'\\' { Ground } else { Esc };
                if self.state == Esc {
                    self.step(b, out);
                }
            }
        }
    }

    fn finish_osc(&mut self, out: &mut Vec<Tracked>) {
        if self.osc_keep && self.seq.len() >= 3 {
            out.push(Tracked::Osc99(self.seq[3..].to_vec()));
        }
        self.seq.clear();
        self.osc_keep = false;
    }
}

pub fn params(p: &[u8]) -> Vec<u32> {
    if p.is_empty() {
        return vec![];
    }
    p.split(|&b| b == b';')
        .map(|s| {
            let s = s.split(|&b| b == b':').next().unwrap_or(&[]);
            std::str::from_utf8(s)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<Tracked> {
        let mut t = Tracker::default();
        let mut out = vec![];
        for c in chunks {
            t.feed(c, &mut out);
        }
        out
    }

    #[test]
    fn extracts_across_splits() {
        let all: &[u8] = b"a\x1b[>4;2mx\x1b]99;i=1:p=body;hi\x1b\\\x1b]7;file://h/x\x07\x1bc";
        let want = vec![
            Tracked::ModifyOtherKeys(2),
            Tracked::Osc99(b"i=1:p=body;hi".to_vec()),
            Tracked::Reset,
        ];
        assert_eq!(run(&[all]), want);
        for cut in 1..all.len() {
            assert_eq!(run(&[&all[..cut], &all[cut..]]), want, "cut {cut}");
        }
    }

    #[test]
    fn strings_and_aborts() {
        // ESC [ inside an APC payload is not a CSI; CAN aborts a CSI.
        assert_eq!(
            run(&[b"\x1b_Gx=1;\x1b[>4;1m"]),
            vec![Tracked::ModifyOtherKeys(1)]
        );
        assert_eq!(run(&[b"\x1b_G\x9b>4;1m\x1b\\"]), vec![]);
        assert_eq!(run(&[b"\x1b[>4\x18;2m"]), vec![]);
        assert_eq!(run(&[b"\x1b[>4m"]), vec![Tracked::ModifyOtherKeys(0)]);
        assert_eq!(run(&[b"\x1b]999;x\x07"]), vec![]);
    }
}
