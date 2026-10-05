//! Host terminal setup and teardown (03 §6.1, §7.1): raw mode, capability probe, alt screen,
//! mouse, bracketed paste, focus events, kitty keyboard. Teardown also runs from a panic hook.

use crate::caps::{self, EnvHints, ProbeResult};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Query the host and wait (≤ 200 ms) for replies, ending at the DA1 sentinel.
pub fn probe() -> ProbeResult {
    let env = EnvHints::from_env();
    let mut out = std::io::stdout();
    let _ = out.write_all(&caps::probe_queries());
    let _ = out.flush();
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut stdin = std::io::stdin();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: polling stdin with a valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 1024];
        match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
        }
        let r = caps::parse_replies(&buf, &env);
        if r.complete {
            return r;
        }
    }
    caps::parse_replies(&buf, &env)
}

pub fn enter(kitty: bool) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if kitty {
        execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
        KITTY_PUSHED.store(true, Ordering::SeqCst);
    } else {
        // modifyOtherKeys level 2 for hosts without the kitty protocol (03 §7.1).
        out.write_all(b"\x1b[>4;2m")?;
    }
    out.write_all(b"\x1b[?25l")?;
    out.flush()?;
    ACTIVE.store(true, Ordering::SeqCst);
    Ok(())
}

pub fn leave() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout();
    if KITTY_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = out.write_all(b"\x1b[>4;0m\x1b[0 q\x1b[?25h\x1b[0m");
    let _ = execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = out.flush();
    let _ = disable_raw_mode();
}

pub fn raw() -> std::io::Result<()> {
    enable_raw_mode()
}

pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        prev(info);
    }));
}

pub fn size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}
