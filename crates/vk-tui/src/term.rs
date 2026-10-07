//! Host terminal setup and teardown (03 §6.1, §7.1): raw mode, capability probe, alt screen,
//! mouse, bracketed paste, focus events, kitty keyboard. Teardown also runs from a panic hook.

use crate::caps::{self, EnvHints, ProbeResult};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, PopKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);

/// Total time the attach probe waits for answers (03 §6.1).
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// Kitty keyboard flags the client pushes (03 §7.1): disambiguate (1), report event types (2),
/// alternate keys (4), all keys as escapes (8) and associated text (16). crossterm has no
/// constant for bit 16, so the push is written directly; crossterm's decoder accepts the
/// extra text field (`CSI code ; mods ; text u`). Bits 8 and 16 stay on although plain text
/// keys then arrive as reports: `keys.altgr_mode` (auto/chord) needs AltGr/Option keys reported
/// as the key plus its text, which a host only does with both.
pub const KITTY_FLAGS: u8 = 0b1_1111;

/// `CSI > flags u`: push [`KITTY_FLAGS`] onto the host's keyboard-mode stack.
pub fn kitty_push_sequence() -> Vec<u8> {
    format!("\x1b[>{KITTY_FLAGS}u").into_bytes()
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
static MOK_SET: AtomicBool = AtomicBool::new(false);

/// xterm modifyOtherKeys level to ask of a host without the kitty protocol (03 §7.1), only
/// where it is known to report keys the decoder reads (`CSI 27;m;k~` / `CSI k;m u`) and to
/// leave the rest alone: tmux (level 2, with `extended-keys` on) and WezTerm (level 1). Other
/// hosts get none: some half-support it and garble plain keys.
pub fn modify_other_keys_level(
    in_tmux: bool,
    term_program: Option<&str>,
    wezterm_pane: bool,
) -> Option<u8> {
    if in_tmux {
        return Some(2);
    }
    if wezterm_pane || term_program.is_some_and(|p| p.eq_ignore_ascii_case("wezterm")) {
        return Some(1);
    }
    None
}

/// Query the host and wait (≤ 150 ms, 03 §6.1) for replies, ending at the DA1 sentinel. Also probes
/// kitty graphics (direct and shared memory), cell/window pixel size and SGR-pixels mouse for
/// browser panes (03 §6.1, 06 B3.2).
pub fn probe() -> (ProbeResult, vk_browser::probe::GraphicsCaps) {
    let env = EnvHints::from_env();
    let mut out = std::io::stdout();
    // A 3-byte shm object for the `t=s` probe; a host that supports it reads and unlinks it.
    let shm_name = format!("/vkp-{:x}", std::process::id());
    let shm_ok = vk_browser::kitty::shm::write(&shm_name, &[0, 0, 0]).is_ok();
    let mut extra = graphics_queries(shm_ok.then_some(shm_name.as_str()));
    // Colour-scheme report (theme auto); the OSC 11 background query is in the base batch.
    extra.extend_from_slice(b"\x1b[?996n");
    let _ = out.write_all(&caps::probe_queries_with(&extra));
    let _ = out.flush();
    let (r, buf) = read_replies(&env);
    crate::appearance::record_probe(&buf);
    if shm_ok {
        vk_browser::kitty::shm::unlink(&shm_name);
    }
    let g = vk_browser::probe::GraphicsCaps::from_replies(&vk_browser::probe::parse_replies(&buf));
    (r, g)
}

/// The browser-pane probe batch without its own DA1 (the shared sentinel follows).
pub fn graphics_queries(shm_name: Option<&str>) -> Vec<u8> {
    use vk_browser::probe as p;
    let mut v = p::kitty_query_direct(p::ID_DIRECT);
    if let Some(n) = shm_name {
        v.extend(p::kitty_query_shm(p::ID_SHM, n));
    }
    v.extend_from_slice(p::CELL_SIZE_QUERY);
    v.extend_from_slice(p::WINDOW_SIZE_QUERY);
    v.extend_from_slice(p::TEXT_AREA_CELLS_QUERY);
    v.extend(p::decrqm(1016));
    v
}

fn read_replies(env: &EnvHints) -> (ProbeResult, Vec<u8>) {
    let mut buf = Vec::new();
    let deadline = Instant::now() + PROBE_TIMEOUT;
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
        let r = caps::parse_replies(&buf, env);
        if r.complete {
            return (r, buf);
        }
    }
    (caps::parse_replies(&buf, env), buf)
}

/// SGR-pixels mouse reporting (DECSET 1016) on or off; enabled only while a browser pane is
/// visible (pixel-precise clicks, 06 B3.2).
pub fn sgr_pixels(on: bool) {
    let mut out = std::io::stdout();
    let _ = out.write_all(if on { b"\x1b[?1016h" } else { b"\x1b[?1016l" });
    let _ = out.flush();
    PIXELS.store(on, Ordering::SeqCst);
}

static PIXELS: AtomicBool = AtomicBool::new(false);

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
        out.write_all(&kitty_push_sequence())?;
        KITTY_PUSHED.store(true, Ordering::SeqCst);
    } else if let Some(level) = modify_other_keys_level(
        std::env::var_os("TMUX").is_some(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var_os("WEZTERM_PANE").is_some(),
    ) {
        write!(out, "\x1b[>4;{level}m")?;
        MOK_SET.store(true, Ordering::SeqCst);
    }
    out.write_all(b"\x1b[?25l")?;
    out.flush()?;
    ACTIVE.store(true, Ordering::SeqCst);
    Ok(())
}

/// The host's window title was pushed (`CSI 22;2t`) before title sync wrote OSC 2; popped on
/// leave.
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

pub fn title_pushed() {
    TITLE_PUSHED.store(true, Ordering::SeqCst);
}

pub fn leave() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout();
    if TITLE_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[23;2t");
    }
    if KITTY_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if PIXELS.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[?1016l");
    }
    if MOK_SET.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(b"\x1b[>4;0m");
    }
    let _ = out.write_all(b"\x1b[0 q\x1b[?25h\x1b[0m");
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

/// Cell size in pixels from TIOCGWINSZ (`ws_xpixel / cols`), when the host reports it.
pub fn cell_px() -> Option<(u16, u16)> {
    let w = crossterm::terminal::window_size().ok()?;
    if w.width == 0 || w.height == 0 || w.columns == 0 || w.rows == 0 {
        return None;
    }
    Some((w.width / w.columns, w.height / w.rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_push_includes_associated_text() {
        assert_eq!(KITTY_FLAGS, 31);
        assert_eq!(kitty_push_sequence(), b"\x1b[>31u");
        assert!(PROBE_TIMEOUT <= Duration::from_millis(150));
    }

    #[test]
    fn modify_other_keys_only_where_understood() {
        assert_eq!(
            modify_other_keys_level(true, Some("WezTerm"), true),
            Some(2)
        );
        assert_eq!(
            modify_other_keys_level(false, Some("WezTerm"), false),
            Some(1)
        );
        assert_eq!(modify_other_keys_level(false, None, true), Some(1));
        assert_eq!(
            modify_other_keys_level(false, Some("Apple_Terminal"), false),
            None
        );
        assert_eq!(modify_other_keys_level(false, Some("ghostty"), false), None);
        assert_eq!(modify_other_keys_level(false, None, false), None);
    }
}
