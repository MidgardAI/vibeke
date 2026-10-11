//! The PTY driver (18 §10.1): runs a program in a pseudo-terminal, decodes its output with
//! `vk-term`, writes `Effect::Reply` bytes (terminal query answers) back to the PTY, sends
//! input, resizes, waits for readiness and shuts down cleanly.
//!
//! The child is spawned the way `crates/vk-hold/src/pty.rs` spawns a pane: a new session with
//! the PTY as its controlling terminal, and an environment that is exactly [`Spawn::env`].
//!
//! `vk-term` does not expose the palette a program set, so a small scanner ([`OscScanner`])
//! follows OSC 4, 10, 11, 104, 110 and 111 in the raw output for [`PtyDriver::snapshot`].

use crate::grid::PaletteChanges;
use anyhow::{Context, bail};
use rustix::fd::OwnedFd;
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct PtyDriver {
    master: Arc<File>,
    child: Child,
    engine: Arc<Mutex<vk_term::Engine>>,
    palette: Arc<Mutex<PaletteChanges>>,
    reader: Option<JoinHandle<()>>,
    /// Set once the child was reaped, so `Drop` does not signal a reused pid.
    reaped: bool,
}

pub struct Spawn<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    /// The complete environment (an allowlist; nothing is inherited).
    pub env: &'a [(String, String)],
    pub cols: u16,
    pub rows: u16,
}

fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn write_all(f: &File, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match (&*f).write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl PtyDriver {
    pub fn spawn(spec: &Spawn<'_>) -> anyhow::Result<PtyDriver> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).context("openpt")?;
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
        grantpt(&master).context("grantpt")?;
        unlockpt(&master).context("unlockpt")?;
        let name = ptsname(&master, Vec::new()).context("ptsname")?;
        let slave = rustix::fs::open(
            name.as_c_str(),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )
        .context("open slave")?;
        tcsetwinsize(&slave, winsize(spec.cols, spec.rows))?;

        let mut cmd = Command::new(spec.program);
        cmd.args(spec.args).current_dir(spec.cwd).env_clear();
        for (k, v) in spec.env {
            cmd.env(k, v);
        }
        let s_in: OwnedFd = slave.try_clone()?;
        let s_out: OwnedFd = slave.try_clone()?;
        cmd.stdin(Stdio::from(s_in))
            .stdout(Stdio::from(s_out))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::tcsetpgrp(0, libc::getpid());
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                libc::signal(libc::SIGHUP, libc::SIG_DFL);
                Ok(())
            });
        }
        // `cmd` owns the only parent copies of the slave; they close when it drops, so the
        // master reads EOF/EIO once the child's side is gone.
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", spec.program.display()))?;
        drop(cmd);

        let master = Arc::new(File::from(master));
        let engine = Arc::new(Mutex::new(vk_term::Engine::new(
            spec.cols.max(1),
            spec.rows.max(1),
            1000,
        )));
        let palette = Arc::new(Mutex::new(PaletteChanges::default()));
        let reader = {
            let (master, engine, palette) = (master.clone(), engine.clone(), palette.clone());
            std::thread::Builder::new()
                .name("pty-reader".into())
                .spawn(move || read_loop(&master, &engine, &palette))?
        };
        Ok(PtyDriver {
            master,
            child,
            engine,
            palette,
            reader: Some(reader),
            reaped: false,
        })
    }

    pub fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        write_all(&self.master, bytes).context("write to pty")
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        // Resize the engine first, so output drawn for the new size lands in a grid of that
        // size.
        self.engine.lock().unwrap().resize(cols.max(1), rows.max(1));
        tcsetwinsize(&*self.master, winsize(cols, rows)).context("resize pty")?;
        Ok(())
    }

    /// The current screen as `vk-term` rows, plus the palette changes the program made.
    /// Returns `(rows, cols, nrows, palette)`, ready for [`crate::grid::Grid::from_rows`].
    pub fn snapshot(
        &self,
    ) -> (
        Vec<vk_proto::render::Row>,
        u16,
        u16,
        crate::grid::PaletteChanges,
    ) {
        let (rows, cols, nrows) = {
            let e = self.engine.lock().unwrap();
            (e.visible_rows(), e.cols(), e.rows())
        };
        let palette = self.palette.lock().unwrap().clone();
        (rows, cols, nrows, palette)
    }

    fn screen_text(&self) -> String {
        let (rows, ..) = self.snapshot();
        rows.iter().map(|r| r.text()).collect::<Vec<_>>().join("\n")
    }

    /// Wait until the screen text contains `needle`.
    pub fn wait_for_text(&self, needle: &str, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.screen_text();
            if text.contains(needle) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("timed out after {timeout:?} waiting for {needle:?}; screen:\n{text}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGTERM the process group, wait up to `grace`, then SIGKILL. Best effort (18 §5.4).
    pub fn shutdown(mut self, grace: Duration) -> anyhow::Result<Option<i32>> {
        let pgid = self.child.id() as libc::pid_t;
        // The child is a session leader, so its process group id is its pid.
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        let status = loop {
            if let Some(s) = self.child.try_wait()? {
                break Some(s);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Kill the group even when the leader exited: descendants may still run.
        // SAFETY: as above.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let status = match status {
            Some(s) => s,
            None => self.child.wait()?,
        };
        self.reaped = true;
        // The reader ends on EOF/EIO once no process holds the slave. Join it only when it is
        // done; a leftover process outside the group must not hang the harness.
        if let Some(r) = self.reader.take()
            && r.is_finished()
        {
            let _ = r.join();
        }
        Ok(status.code())
    }
}

impl Drop for PtyDriver {
    /// A driver dropped without [`PtyDriver::shutdown`] (a failed test) still kills its group.
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let pgid = self.child.id() as libc::pid_t;
        // SAFETY: kill has no memory-safety preconditions.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn read_loop(master: &File, engine: &Mutex<vk_term::Engine>, palette: &Mutex<PaletteChanges>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut scanner = OscScanner::default();
    let mut effects = Vec::new();
    loop {
        let n = match (&*master).read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            // EIO on Linux once the slave side is closed.
            Err(_) => return,
        };
        let bytes = &buf[..n];
        scanner.feed(bytes, &mut palette.lock().unwrap());
        engine.lock().unwrap().feed(bytes, &mut effects);
        for e in effects.drain(..) {
            if let vk_term::Effect::Reply(reply) = e
                && write_all(master, &reply).is_err()
            {
                return;
            }
        }
    }
}

/// Follows OSC 4/10/11 (set) and 104/110/111 (reset) in a byte stream, across reads.
/// Terminated by BEL or ST (`ESC \`); queries (`?`) and colour names are ignored.
#[derive(Debug, Default)]
pub struct OscScanner {
    state: ScanState,
    buf: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// Longest OSC body the scanner keeps; longer ones (images, clipboard data) are skipped.
const OSC_MAX: usize = 4096;

impl OscScanner {
    pub fn feed(&mut self, bytes: &[u8], out: &mut PaletteChanges) {
        for &b in bytes {
            self.state = match (self.state, b) {
                (ScanState::Ground, 0x1b) => ScanState::Esc,
                (ScanState::Ground, _) => ScanState::Ground,
                (ScanState::Esc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::Esc, 0x1b) => ScanState::Esc,
                (ScanState::Esc, _) => ScanState::Ground,
                (ScanState::Osc, 0x07) => {
                    self.finish(out);
                    ScanState::Ground
                }
                (ScanState::Osc, 0x1b) => ScanState::OscEsc,
                // CAN and SUB abort a control string.
                (ScanState::Osc, 0x18 | 0x1a) => ScanState::Ground,
                (ScanState::Osc, b) => {
                    if self.buf.len() < OSC_MAX {
                        self.buf.push(b);
                    }
                    ScanState::Osc
                }
                (ScanState::OscEsc, b'\\') => {
                    self.finish(out);
                    ScanState::Ground
                }
                // ESC followed by anything else aborts the OSC and starts a new sequence.
                (ScanState::OscEsc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::OscEsc, 0x1b) => ScanState::Esc,
                (ScanState::OscEsc, _) => ScanState::Ground,
            };
        }
    }

    fn finish(&mut self, out: &mut PaletteChanges) {
        if self.buf.len() >= OSC_MAX {
            return;
        }
        let Ok(body) = std::str::from_utf8(&self.buf) else {
            return;
        };
        let mut parts = body.split(';');
        let Some(code) = parts.next().and_then(|c| c.parse::<u32>().ok()) else {
            return;
        };
        let rest: Vec<&str> = parts.collect();
        match code {
            4 => {
                for pair in rest.chunks(2) {
                    let [i, spec] = pair else { break };
                    if let (Ok(i), Some(c)) = (i.parse::<u8>(), parse_color_spec(spec)) {
                        out.overrides.insert(i, c);
                    }
                }
            }
            // OSC 10 with more parameters goes on to 11, 12, …
            10 | 11 => {
                for (k, spec) in rest.iter().enumerate() {
                    let Some(c) = parse_color_spec(spec) else {
                        continue;
                    };
                    match code as usize + k {
                        10 => out.fg = Some(c),
                        11 => out.bg = Some(c),
                        _ => {}
                    }
                }
            }
            104 => {
                if rest.iter().all(|p| p.is_empty()) {
                    out.overrides.clear();
                } else {
                    for p in rest {
                        if let Ok(i) = p.parse::<u8>() {
                            out.overrides.remove(&i);
                        }
                    }
                }
            }
            110 => out.fg = None,
            111 => out.bg = None,
            _ => {}
        }
    }
}

/// `rgb:r/g/b` (1–4 hex digits per channel, scaled to 8 bits) or `#rgb`, `#rrggbb`,
/// `#rrrgggbbb`, `#rrrrggggbbbb`. Anything else (queries, colour names) is `None`.
pub fn parse_color_spec(spec: &str) -> Option<(u8, u8, u8)> {
    fn scale(h: &str) -> Option<u8> {
        if h.is_empty() || h.len() > 4 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    }
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut it = rest.split('/');
        let (r, g, b) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        return Some((scale(r)?, scale(g)?, scale(b)?));
    }
    if let Some(h) = spec.strip_prefix('#') {
        if h.is_empty() || h.len() % 3 != 0 || h.len() > 12 {
            return None;
        }
        let n = h.len() / 3;
        // `#` forms keep the high bits (XParseColor), unlike `rgb:` which scales.
        let chan = |i: usize| -> Option<u8> {
            let part = &h[i * n..(i + 1) * n];
            if !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let v = u32::from_str_radix(part, 16).ok()?;
            Some(match n {
                1 => (v << 4) as u8,
                _ => (v >> (4 * (n - 2))) as u8,
            })
        };
        return Some((chan(0)?, chan(1)?, chan(2)?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn scan(chunks: &[&[u8]]) -> PaletteChanges {
        let mut s = OscScanner::default();
        let mut p = PaletteChanges::default();
        for c in chunks {
            s.feed(c, &mut p);
        }
        p
    }

    #[test]
    fn color_specs() {
        assert_eq!(parse_color_spec("rgb:ff/00/80"), Some((255, 0, 128)));
        assert_eq!(parse_color_spec("rgb:ffff/0000/8080"), Some((255, 0, 128)));
        assert_eq!(parse_color_spec("rgb:f/0/8"), Some((255, 0, 136)));
        assert_eq!(parse_color_spec("#ff0080"), Some((255, 0, 128)));
        assert_eq!(parse_color_spec("#f08"), Some((240, 0, 128)));
        assert_eq!(parse_color_spec("?"), None);
        assert_eq!(parse_color_spec("red"), None);
        assert_eq!(parse_color_spec("rgb:ff/00"), None);
    }

    #[test]
    fn osc_bel_and_st_terminators() {
        let p =
            scan(&[b"a\x1b]4;1;rgb:ff/00/00\x07b\x1b]10;#00ff00\x1b\\c\x1b]11;rgb:00/00/ff\x07"]);
        assert_eq!(p.overrides.get(&1), Some(&(255, 0, 0)));
        assert_eq!(p.fg, Some((0, 255, 0)));
        assert_eq!(p.bg, Some((0, 0, 255)));
    }

    #[test]
    fn osc_split_across_reads() {
        let p = scan(&[b"\x1b", b"]4;", b"2;rgb:00/ff", b"/00;3;#0000ff\x1b", b"\\"]);
        assert_eq!(p.overrides.get(&2), Some(&(0, 255, 0)));
        assert_eq!(p.overrides.get(&3), Some(&(0, 0, 255)));
    }

    #[test]
    fn osc_resets_and_queries() {
        let mut s = OscScanner::default();
        let mut p = PaletteChanges::default();
        s.feed(
            b"\x1b]4;1;#ff0000;2;#00ff00\x07\x1b]10;#ffffff;#000000\x07",
            &mut p,
        );
        assert_eq!(p.overrides.len(), 2);
        assert_eq!((p.fg, p.bg), (Some((255, 255, 255)), Some((0, 0, 0))));
        s.feed(b"\x1b]104;1\x07\x1b]4;5;?\x07\x1b]110\x07", &mut p);
        assert_eq!(p.overrides.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert_eq!(p.fg, None);
        assert_eq!(p.bg, Some((0, 0, 0)));
        s.feed(b"\x1b]104\x1b\\\x1b]111\x1b\\", &mut p);
        assert!(p.overrides.is_empty());
        assert_eq!(p.bg, None);
    }

    #[test]
    fn osc_aborted_by_new_escape() {
        // An unterminated OSC 4 followed by a CSI must not apply.
        let p = scan(&[b"\x1b]4;1;#ff0000\x1b[0m\x1b]11;#010203\x07"]);
        assert!(p.overrides.is_empty());
        assert_eq!(p.bg, Some((1, 2, 3)));
    }

    fn env() -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("LANG".into(), "C".into()),
        ]
    }

    fn spawn_sh(script: &str) -> PtyDriver {
        let args = vec!["-c".to_string(), script.to_string()];
        let env = env();
        PtyDriver::spawn(&Spawn {
            program: Path::new("/bin/sh"),
            args: &args,
            cwd: Path::new("/"),
            env: &env,
            cols: 40,
            rows: 5,
        })
        .unwrap()
    }

    #[test]
    fn pty_snapshot_has_text_and_bold() {
        let d = spawn_sh(r#"printf '\033[1mhi\033[0m'; sleep 5"#);
        d.wait_for_text("hi", Duration::from_secs(10)).unwrap();
        let (rows, cols, nrows, palette) = d.snapshot();
        assert_eq!((cols, nrows), (40, 5));
        assert_eq!(palette, PaletteChanges::default());
        let g = Grid::from_rows(&rows, cols, nrows);
        assert_eq!(g.cells[0][0].text, "h");
        assert_eq!(g.cells[0][1].text, "i");
        assert!(g.cells[0][0].attrs.bold && g.cells[0][1].attrs.bold);
        assert!(!g.cells[0][2].attrs.bold);
        d.resize(30, 4).unwrap();
        let (_, cols, nrows, _) = d.snapshot();
        assert_eq!((cols, nrows), (30, 4));
        d.shutdown(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn pty_replies_reach_the_child() {
        // The child asks for the cursor position and prints the 6-byte reply (ESC → E).
        let d = spawn_sh(
            r#"stty -icanon -echo min 6; printf '\033[6n'; r=$(dd bs=6 count=1 2>/dev/null); printf '\ngot:%s.' "$r" | tr '\033' 'E'; sleep 5"#,
        );
        d.wait_for_text("got:E[1;1R.", Duration::from_secs(10))
            .unwrap();
        d.shutdown(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn pty_write_reaches_the_child() {
        let d = spawn_sh(r#"read line; printf 'echo:%s.' "$line"; sleep 5"#);
        d.write(b"ping\r").unwrap();
        d.wait_for_text("echo:ping.", Duration::from_secs(10))
            .unwrap();
        let code = d.shutdown(Duration::from_secs(2)).unwrap();
        // Killed by a signal: no exit code.
        assert_eq!(code, None);
    }
}
