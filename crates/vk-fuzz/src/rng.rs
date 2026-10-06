//! Tiny deterministic generator (splitmix64) so the stable property tests need no extra crates.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// Derive a generator from fuzzer input so structure-aware targets stay deterministic.
    pub fn from_bytes(data: &[u8]) -> Rng {
        let mut h = 0xcbf29ce484222325u64;
        for b in data {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        Rng(h)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    pub fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }

    pub fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next_u64() as u8).collect()
    }

    /// Mutate a seed: flips, inserts, deletes, truncation, splices of "interesting" values.
    pub fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
        const INTERESTING: &[&[u8]] = &[
            &[0],
            &[0xff],
            &[0xff, 0xff, 0xff, 0xff],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
            &[0x1b],
            &[0x1b, b'['],
            &[0x1b, b']'],
            &[0x1b, b'_', b'G'],
        ];
        let mut v = seed.to_vec();
        for _ in 0..1 + self.below(6) {
            match self.below(6) {
                0 if !v.is_empty() => {
                    let i = self.below(v.len());
                    v[i] ^= 1 << self.below(8);
                }
                1 if !v.is_empty() => {
                    let i = self.below(v.len());
                    v[i] = self.next_u64() as u8;
                }
                2 => {
                    let i = self.below(v.len() + 1);
                    let ins = *self.pick(INTERESTING);
                    v.splice(i..i, ins.iter().copied());
                }
                3 if !v.is_empty() => {
                    let i = self.below(v.len());
                    let j = (i + 1 + self.below(8)).min(v.len());
                    v.drain(i..j);
                }
                4 => {
                    let n = self.below(v.len() + 1);
                    v.truncate(n);
                }
                _ => {
                    let extra = self.bytes(16);
                    v.extend_from_slice(&extra);
                }
            }
        }
        v
    }

    /// Random JSON with keys biased toward `keys` (so structured handlers get exercised).
    pub fn json(&mut self, depth: usize, keys: &[&str]) -> serde_json::Value {
        use serde_json::{Map, Value};
        match self.below(if depth == 0 { 5 } else { 8 }) {
            0 => Value::Null,
            1 => Value::Bool(self.chance(2)),
            2 => Value::from(self.next_u64() as i64),
            3 | 4 => Value::String(self.string(keys)),
            5 | 6 => Value::Array(
                (0..self.below(4))
                    .map(|_| self.json(depth - 1, keys))
                    .collect(),
            ),
            _ => {
                let mut m = Map::new();
                for _ in 0..self.below(6) {
                    let k = self.string(keys);
                    m.insert(k, self.json(depth - 1, keys));
                }
                Value::Object(m)
            }
        }
    }

    pub fn string(&mut self, words: &[&str]) -> String {
        if !words.is_empty() && !self.chance(4) {
            return (*self.pick(words)).to_string();
        }
        let n = self.below(12);
        (0..n)
            .map(|_| match self.below(4) {
                0 => char::from_u32(self.next_u64() as u32 % 0x11_0000).unwrap_or('?'),
                _ => (b' ' + (self.next_u64() % 95) as u8) as char,
            })
            .collect()
    }
}
