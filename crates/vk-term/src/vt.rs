//! The `VtEngine` seam (03 §1): what the server needs from a VT engine, implemented by the one
//! engine that ships ([`crate::Engine`], libghostty-vt). It is an internal seam, not a promise
//! of pluggability: detectors, snapshots, search and the render server can be written against
//! it, and tests can drive any implementation through the same calls.
//!
//! As built, effects are [`EngineEffect`] (= [`crate::Effect`]): query replies, bell, title,
//! notifications, OSC 52 set/query, OSC 7 cwd, OSC 133 marks, OSC 9;4 progress and OSC 1337
//! user vars. OSC 8 hyperlinks and OSC 133 prompt rows are not effects: the engine keeps them
//! in its grid, and they come back on every row (`Row::links`, `Row::mark`), so they
//! survive scrollback, reflow and snapshots without a side table.

use crate::engine::{Effect, Engine, LastCommand};
use vk_proto::render::{Cursor, PaneModes, Row};

pub type EngineEffect = Effect;

pub trait VtEngine: Send + 'static {
    /// Construct with an initial size and the in-memory scrollback rows to keep.
    fn create(cols: u16, rows: u16, scrollback: usize) -> Self
    where
        Self: Sized;
    /// Feed PTY output. Partial UTF-8 and escape sequences may span calls.
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<EngineEffect>);
    fn resize(&mut self, cols: u16, rows: u16);
    /// Visible rows damaged since the last call; `None` = everything.
    fn take_damage(&mut self) -> Option<Vec<u16>>;
    fn size(&self) -> (u16, u16);
    fn visible_row(&self, y: u16) -> Row;
    fn scrollback_len(&self) -> usize;
    /// 0 = oldest row kept in memory.
    fn scrollback_row(&self, idx: usize) -> Option<Row>;
    fn cursor(&self) -> Cursor;
    fn modes(&self) -> PaneModes;
    fn title(&self) -> String;
    /// OSC 7 working directory (decoded local path).
    fn cwd(&self) -> Option<String>;
    /// Absolute number of lines scrolled into history (row numbering of marks and archive).
    fn scrolled_total(&self) -> u64;
    /// Absolute lines of OSC 133 prompt rows in memory.
    fn prompt_lines(&self) -> Vec<u64>;
    /// The last command's output from OSC 133 marks.
    fn last_command(&self) -> Option<LastCommand>;
    /// Lossless state for snapshots (01 §4), including the unfinished parser continuation.
    fn serialize(&self) -> Vec<u8>;
    fn deserialize(bytes: &[u8], scrollback: usize) -> anyhow::Result<Self>
    where
        Self: Sized;
    /// While replaying the journal, replies and side-effect effects are suppressed.
    fn set_replaying(&mut self, replaying: bool);
}

impl VtEngine for Engine {
    fn create(cols: u16, rows: u16, scrollback: usize) -> Self {
        Engine::new(cols, rows, scrollback)
    }
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<EngineEffect>) {
        Engine::feed(self, bytes, out)
    }
    fn resize(&mut self, cols: u16, rows: u16) {
        Engine::resize(self, cols, rows)
    }
    fn take_damage(&mut self) -> Option<Vec<u16>> {
        Engine::take_damage(self)
    }
    fn size(&self) -> (u16, u16) {
        (self.cols(), self.rows())
    }
    fn visible_row(&self, y: u16) -> Row {
        self.row(y)
    }
    fn scrollback_len(&self) -> usize {
        self.history_len()
    }
    fn scrollback_row(&self, idx: usize) -> Option<Row> {
        self.history_row(idx)
    }
    fn cursor(&self) -> Cursor {
        Engine::cursor(self)
    }
    fn modes(&self) -> PaneModes {
        Engine::modes(self)
    }
    fn title(&self) -> String {
        Engine::title(self)
    }
    fn cwd(&self) -> Option<String> {
        Engine::cwd(self).map(str::to_string)
    }
    fn scrolled_total(&self) -> u64 {
        Engine::scrolled_total(self)
    }
    fn prompt_lines(&self) -> Vec<u64> {
        Engine::prompt_lines(self)
    }
    fn last_command(&self) -> Option<LastCommand> {
        Engine::last_command(self)
    }
    fn serialize(&self) -> Vec<u8> {
        self.snapshot()
    }
    fn deserialize(bytes: &[u8], scrollback: usize) -> anyhow::Result<Self> {
        Engine::restore(bytes, scrollback)
    }
    fn set_replaying(&mut self, replaying: bool) {
        Engine::set_replaying(self, replaying)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generic over the seam: a prompt, an OSC 8 link and a snapshot round trip.
    fn exercise<E: VtEngine>() {
        let mut e = E::create(20, 4, 50);
        let mut fx = Vec::new();
        e.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07\x1b]8;;https://a.b/\x07x\x1b]8;;\x07\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ",
            &mut fx,
        );
        assert!(fx.contains(&Effect::Mark {
            kind: 'D',
            exit: Some(0)
        }));
        assert_eq!(e.prompt_lines(), vec![0, 2]);
        assert_eq!(e.visible_row(1).links[0].uri, "https://a.b/");
        let lc = e.last_command().unwrap();
        assert_eq!(lc.rows.len(), 1);
        let back = E::deserialize(&e.serialize(), 50).unwrap();
        assert_eq!(back.prompt_lines(), e.prompt_lines());
        assert_eq!(back.visible_row(1), e.visible_row(1));
        assert_eq!(back.size(), (20, 4));
    }

    #[test]
    fn engine_implements_the_seam() {
        exercise::<Engine>();
    }
}
