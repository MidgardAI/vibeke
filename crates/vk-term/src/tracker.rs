//! Escape framing tracker running alongside the VT engine. It keeps the raw bytes of an
//! incomplete sequence (so a snapshot taken mid-sequence captures parser state exactly, 03 §2.3)
//! and extracts the few sequences the engine ignores: OSC 7/9/99/777/133, XTVERSION,
//! modifyOtherKeys and synchronized-update (2026) brackets.

const MAX_PENDING: usize = 1 << 20;
const MAX_OSC: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Utf8(u8),
    Esc,
    EscInter,
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    Osc(Vec<u8>),
    Csi {
        private: Option<u8>,
        params: Vec<u8>,
        inter: Vec<u8>,
        fin: u8,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Tracker {
    state: State,
    pending: Vec<u8>,
    seq: Vec<u8>,
}

impl Tracker {
    /// Bytes of the sequence currently being received (empty at a safe cut point).
    pub fn pending(&self) -> &[u8] {
        if self.state == State::Ground {
            &[]
        } else {
            &self.pending
        }
    }

    pub fn is_safe(&self) -> bool {
        self.state == State::Ground
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Tracked>) {
        for &b in bytes {
            if self.state != State::Ground && self.pending.len() < MAX_PENDING {
                self.pending.push(b);
            }
            self.step(b, out);
            if self.state == State::Ground {
                self.pending.clear();
            }
        }
    }

    fn begin(&mut self, s: State, b: u8) {
        if self.state == State::Ground {
            self.pending.clear();
            self.pending.push(b);
        }
        self.seq.clear();
        self.state = s;
    }

    fn step(&mut self, b: u8, out: &mut Vec<Tracked>) {
        use State::*;
        if matches!(b, 0x18 | 0x1a) && !matches!(self.state, Ground | Utf8(_)) {
            self.state = Ground;
            return;
        }
        match self.state {
            Ground => match b {
                0x1b => self.begin(Esc, b),
                0xc2..=0xdf => self.begin(Utf8(1), b),
                0xe0..=0xef => self.begin(Utf8(2), b),
                0xf0..=0xf4 => self.begin(Utf8(3), b),
                _ => {}
            },
            Utf8(n) => {
                if (0x80..=0xbf).contains(&b) {
                    self.state = if n == 1 { Ground } else { Utf8(n - 1) };
                } else {
                    self.state = Ground;
                    self.pending.clear();
                    self.step(b, out);
                }
            }
            Esc => match b {
                b'[' => {
                    self.seq.clear();
                    self.state = Csi
                }
                b']' => {
                    self.seq.clear();
                    self.state = Osc
                }
                b'P' | b'_' | b'^' | b'X' => self.state = Str,
                0x20..=0x2f => self.state = EscInter,
                0x1b => self.begin(Esc, b),
                _ => self.state = Ground,
            },
            EscInter => match b {
                0x20..=0x2f => {}
                _ => self.state = Ground,
            },
            Csi => match b {
                0x40..=0x7e => {
                    self.state = Ground;
                    let (private, rest) = match self.seq.first() {
                        Some(&c @ (b'?' | b'>' | b'=' | b'<')) => (Some(c), &self.seq[1..]),
                        _ => (None, &self.seq[..]),
                    };
                    let split = rest
                        .iter()
                        .position(|b| (0x20..=0x2f).contains(b))
                        .unwrap_or(rest.len());
                    out.push(Tracked::Csi {
                        private,
                        params: rest[..split].to_vec(),
                        inter: rest[split..].to_vec(),
                        fin: b,
                    });
                }
                0x1b => self.begin(Esc, b),
                _ => {
                    if self.seq.len() < 256 {
                        self.seq.push(b)
                    }
                }
            },
            Osc => match b {
                0x07 => {
                    self.state = Ground;
                    out.push(Tracked::Osc(std::mem::take(&mut self.seq)));
                }
                0x1b => self.state = OscEsc,
                _ => {
                    if self.seq.len() < MAX_OSC {
                        self.seq.push(b)
                    }
                }
            },
            OscEsc => {
                self.state = Ground;
                if b == b'\\' {
                    out.push(Tracked::Osc(std::mem::take(&mut self.seq)));
                } else {
                    self.step(b, out);
                }
            }
            Str => {
                if b == 0x1b {
                    self.state = StrEsc
                }
            }
            StrEsc => {
                if b == b'\\' {
                    self.state = Ground
                } else if b != 0x1b {
                    self.state = Str
                }
            }
        }
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

    #[test]
    fn pending_bytes_captured() {
        let mut t = Tracker::default();
        let mut out = vec![];
        t.feed(b"ab\x1b[3", &mut out);
        assert_eq!(t.pending(), b"\x1b[3");
        t.feed(b"1m", &mut out);
        assert!(t.pending().is_empty());
        t.feed(&"é".as_bytes()[..1], &mut out);
        assert_eq!(t.pending(), &"é".as_bytes()[..1]);
        t.feed(&"é".as_bytes()[1..], &mut out);
        t.feed(b"\x1b]7;file://h/tmp\x1b\\", &mut out);
        assert!(out.contains(&Tracked::Osc(b"7;file://h/tmp".to_vec())));
    }
}
