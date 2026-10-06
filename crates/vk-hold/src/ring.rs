//! Output journal: a byte ring addressed by a monotonically increasing offset, with a side
//! index of markers (resize, input written, server attach/detach) and safe cut points.

use std::collections::VecDeque;
use vk_proto::holder::MarkerKind;

const CUT_SPACING: u64 = 4096;

pub struct Ring {
    data: VecDeque<u8>,
    start: u64,
    cap: usize,
    markers: VecDeque<(u64, MarkerKind)>,
    cuts: VecDeque<u64>,
}

pub enum Item<'a> {
    Bytes(u64, &'a [u8], &'a [u8]),
    Marker(u64, &'a MarkerKind),
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring {
            data: VecDeque::new(),
            start: 0,
            cap: cap.max(4096),
            markers: VecDeque::new(),
            cuts: VecDeque::new(),
        }
    }

    pub fn start(&self) -> u64 {
        self.start
    }
    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
        if self.data.len() > self.cap {
            let min_start = self.end() - self.cap as u64;
            // Prefer to start on a safe cut point so replay from the ring start parses cleanly.
            let new_start = self
                .cuts
                .iter()
                .copied()
                .find(|&c| c >= min_start)
                .unwrap_or(min_start)
                .min(self.end());
            let drop = (new_start - self.start) as usize;
            self.data.drain(..drop);
            self.start = new_start;
            // Pipe mode: the stream of the bytes that now start the ring must survive the
            // trim, or replay would attribute them to the wrong stream.
            let mut stream = None;
            while self.markers.front().is_some_and(|(o, _)| *o < self.start) {
                if let Some((_, k @ MarkerKind::Stream { .. })) = self.markers.pop_front() {
                    stream = Some(k);
                }
            }
            if let Some(k) = stream
                && !self.markers.front().is_some_and(|(o, m)| {
                    *o == self.start && matches!(m, MarkerKind::Stream { .. })
                })
            {
                self.markers.push_front((self.start, k));
            }
            while self.cuts.front().is_some_and(|&o| o < self.start) {
                self.cuts.pop_front();
            }
        }
    }

    /// Record that `offset` is a safe cut point (not inside UTF-8 or an escape sequence).
    pub fn mark_cut(&mut self, offset: u64) {
        if self
            .cuts
            .back()
            .is_none_or(|&last| offset >= last + CUT_SPACING)
        {
            self.cuts.push_back(offset);
        }
    }

    /// Pipe mode: the stream of the byte at `offset` (the last `Stream` marker at or before
    /// it), if any.
    pub fn stream_at(&self, offset: u64) -> Option<vk_proto::holder::Stream> {
        self.markers
            .iter()
            .rev()
            .filter(|(o, _)| *o <= offset)
            .find_map(|(_, m)| match m {
                MarkerKind::Stream { stream } => Some(*stream),
                _ => None,
            })
    }

    pub fn last_cut(&self) -> Option<u64> {
        self.cuts.back().copied()
    }

    pub fn marker(&mut self, kind: MarkerKind) -> u64 {
        let at = self.end();
        self.markers.push_back((at, kind));
        at
    }

    /// Items from `from` (clamped to the ring start) to the end, markers interleaved in order:
    /// a marker at offset `o` comes before bytes starting at `o`.
    pub fn read_from(&self, from: u64) -> Vec<Item<'_>> {
        let from = from.max(self.start).min(self.end());
        let mut out = Vec::new();
        let mut pos = from;
        let (a, b) = self.data.as_slices();
        let slice = |s: u64, e: u64| -> Item<'_> {
            let s = (s - self.start) as usize;
            let e = (e - self.start) as usize;
            if e <= a.len() {
                Item::Bytes(self.start + s as u64, &a[s..e], &[])
            } else if s >= a.len() {
                Item::Bytes(self.start + s as u64, &b[s - a.len()..e - a.len()], &[])
            } else {
                Item::Bytes(self.start + s as u64, &a[s..], &b[..e - a.len()])
            }
        };
        for (o, m) in self.markers.iter().filter(|(o, _)| *o >= from) {
            if *o > pos {
                out.push(slice(pos, *o));
                pos = *o;
            }
            out.push(Item::Marker(*o, m));
        }
        if pos < self.end() {
            out.push(slice(pos, self.end()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(items: &[Item]) -> Vec<u8> {
        let mut v = vec![];
        for i in items {
            if let Item::Bytes(_, a, b) = i {
                v.extend_from_slice(a);
                v.extend_from_slice(b);
            }
        }
        v
    }

    #[test]
    fn markers_interleave() {
        let mut r = Ring::new(8192);
        r.push(b"abc");
        r.marker(MarkerKind::Resize {
            cols: 10,
            rows: 5,
            px_w: 0,
            px_h: 0,
        });
        r.push(b"def");
        let items = r.read_from(0);
        assert_eq!(items.len(), 3);
        assert!(matches!(items[1], Item::Marker(3, _)));
        assert_eq!(bytes_of(&items), b"abcdef");
        assert_eq!(bytes_of(&r.read_from(4)), b"ef");
    }

    #[test]
    fn overflow_trims_to_cut() {
        let mut r = Ring::new(4096);
        r.push(&[b'x'; 3000]);
        r.mark_cut(3000);
        r.push(&[b'y'; 3000]);
        assert_eq!(r.start(), 3000);
        assert_eq!(r.end(), 6000);
        assert_eq!(bytes_of(&r.read_from(0)), vec![b'y'; 3000]);
    }

    #[test]
    fn trimming_keeps_the_stream_of_the_new_start() {
        use vk_proto::holder::Stream;
        let mut r = Ring::new(4096);
        r.marker(MarkerKind::Stream {
            stream: Stream::Stdout,
        });
        r.push(&[b'o'; 200]);
        r.marker(MarkerKind::Stream {
            stream: Stream::Stdin,
        });
        r.push(&[b'i'; 4500]);
        // Trimmed into the stdin run: its marker moved to the new start.
        assert!(r.start() > 200);
        assert_eq!(r.stream_at(r.start()), Some(Stream::Stdin));
        assert!(matches!(
            r.read_from(0)[0],
            Item::Marker(
                _,
                MarkerKind::Stream {
                    stream: Stream::Stdin
                }
            )
        ));
        r.marker(MarkerKind::Stream {
            stream: Stream::Stdout,
        });
        r.push(b"x");
        assert_eq!(r.stream_at(r.end() - 1), Some(Stream::Stdout));
    }
}
