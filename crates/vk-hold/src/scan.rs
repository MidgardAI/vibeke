//! The holder's tiny escape-boundary state machine (01 §1.2): UTF-8 continuation and
//! ESC/CSI/OSC/DCS/APC framing only — no screen model. It yields safe cut points and the few
//! complete sequences the holder cares about (mode tracking and server-absent queries).

const MAX_CSI: usize = 64;
const MAX_STR: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
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
pub enum Seq {
    Csi {
        private: Option<u8>,
        params: Vec<u8>,
        inter: Vec<u8>,
        fin: u8,
    },
    Osc(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            state: State::Ground,
            buf: Vec::new(),
            overflow: false,
        }
    }
}

impl Scanner {
    /// True when the next byte starts outside any escape or UTF-8 sequence.
    pub fn is_safe(&self) -> bool {
        self.state == State::Ground
    }

    /// Feed bytes; calls `on_seq` for every complete CSI/OSC and `on_safe(i)` after byte `i`
    /// whenever the stream is at a safe cut point (only reported at the end of runs to keep it cheap).
    pub fn feed(
        &mut self,
        bytes: &[u8],
        mut on_seq: impl FnMut(Seq),
        mut on_safe: impl FnMut(usize),
    ) {
        for (i, &b) in bytes.iter().enumerate() {
            self.step(b, &mut on_seq);
            if self.state == State::Ground && (i + 1 == bytes.len() || bytes[i + 1] == 0x1b) {
                on_safe(i + 1);
            }
        }
    }

    fn push(&mut self, b: u8, cap: usize) {
        if self.buf.len() < cap {
            self.buf.push(b);
        } else {
            self.overflow = true;
        }
    }

    fn step(&mut self, b: u8, on_seq: &mut impl FnMut(Seq)) {
        use State::*;
        // CAN/SUB abort any sequence.
        if matches!(b, 0x18 | 0x1a) && !matches!(self.state, Ground | Utf8(_)) {
            self.state = Ground;
            return;
        }
        match self.state {
            Ground => match b {
                0x1b => self.enter(Esc),
                0xc2..=0xdf => self.state = Utf8(1),
                0xe0..=0xef => self.state = Utf8(2),
                0xf0..=0xf4 => self.state = Utf8(3),
                _ => {}
            },
            Utf8(n) => {
                if (0x80..=0xbf).contains(&b) {
                    self.state = if n == 1 { Ground } else { Utf8(n - 1) };
                } else {
                    // Invalid continuation: the engine replaces it; restart on this byte.
                    self.state = Ground;
                    self.step(b, on_seq);
                }
            }
            Esc => match b {
                b'[' => self.enter(Csi),
                b']' => self.enter(Osc),
                b'P' | b'_' | b'^' | b'X' => self.enter(Str),
                0x20..=0x2f => self.state = EscInter,
                0x1b => self.enter(Esc),
                _ => self.state = Ground,
            },
            EscInter => match b {
                0x20..=0x2f => {}
                0x1b => self.enter(Esc),
                _ => self.state = Ground,
            },
            Csi => match b {
                0x40..=0x7e => {
                    let buf = std::mem::take(&mut self.buf);
                    self.state = Ground;
                    if !self.overflow {
                        on_seq(parse_csi(&buf, b));
                    }
                }
                0x1b => self.enter(Esc),
                _ => self.push(b, MAX_CSI),
            },
            Osc => match b {
                0x07 => self.finish_osc(on_seq),
                0x1b => self.state = OscEsc,
                _ => self.push(b, MAX_STR),
            },
            OscEsc => {
                if b == b'\\' {
                    self.finish_osc(on_seq);
                } else {
                    self.buf.clear();
                    self.state = Ground;
                    self.step(b, on_seq);
                }
            }
            Str => {
                if b == 0x1b {
                    self.state = StrEsc
                }
            }
            StrEsc => {
                if b == b'\\' {
                    self.state = Ground;
                } else if b != 0x1b {
                    self.state = Str;
                }
            }
        }
    }

    fn enter(&mut self, s: State) {
        self.buf.clear();
        self.overflow = false;
        self.state = s;
    }

    fn finish_osc(&mut self, on_seq: &mut impl FnMut(Seq)) {
        let buf = std::mem::take(&mut self.buf);
        self.state = State::Ground;
        if !self.overflow {
            on_seq(Seq::Osc(buf));
        }
    }
}

fn parse_csi(buf: &[u8], fin: u8) -> Seq {
    let (private, rest) = match buf.first() {
        Some(&c @ (b'?' | b'>' | b'=' | b'<')) => (Some(c), &buf[1..]),
        _ => (None, buf),
    };
    let split = rest
        .iter()
        .position(|b| (0x20..=0x2f).contains(b))
        .unwrap_or(rest.len());
    Seq::Csi {
        private,
        params: rest[..split].to_vec(),
        inter: rest[split..].to_vec(),
        fin,
    }
}

/// Numeric parameters of a CSI (`;`-separated, sub-params ignored).
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

    fn run(bytes: &[u8]) -> (Vec<Seq>, bool) {
        let mut s = Scanner::default();
        let mut seqs = vec![];
        s.feed(bytes, |q| seqs.push(q), |_| {});
        (seqs, s.is_safe())
    }

    #[test]
    fn csi_and_safe_points() {
        let (seqs, safe) = run(b"hi\x1b[?2004h\x1b[>c\x1b[6n");
        assert!(safe);
        assert_eq!(seqs.len(), 3);
        assert_eq!(
            seqs[0],
            Seq::Csi {
                private: Some(b'?'),
                params: b"2004".to_vec(),
                inter: vec![],
                fin: b'h'
            }
        );
        assert_eq!(
            seqs[1],
            Seq::Csi {
                private: Some(b'>'),
                params: vec![],
                inter: vec![],
                fin: b'c'
            }
        );
    }

    #[test]
    fn mid_sequence_is_unsafe() {
        assert!(!run(b"\x1b[3").1);
        assert!(!run("é".as_bytes().split_at(1).0).1);
        assert!(!run(b"\x1b]0;title").1);
        assert!(run(b"\x1b]0;title\x07").1);
        assert!(run(b"\x1bP+q\x1b\\").1);
    }

    #[test]
    fn decrqm_inter() {
        let (seqs, _) = run(b"\x1b[?2004$p");
        assert_eq!(
            seqs[0],
            Seq::Csi {
                private: Some(b'?'),
                params: b"2004".to_vec(),
                inter: b"$".to_vec(),
                fin: b'p'
            }
        );
    }
}
