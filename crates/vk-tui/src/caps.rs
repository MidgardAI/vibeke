//! Host terminal capability probe (03 §6.1). Pure functions: [`probe_queries`] produces the
//! bytes to write at attach, [`parse_replies`] digests whatever came back within the timeout.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Background {
    Light,
    Dark,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Osc52 {
    Allowed,
    #[default]
    Unknown,
}

/// Desktop notification escape the host shows natively (03 §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Notifications {
    Osc9,
    Osc777,
    Osc99,
    #[default]
    None,
}

/// Environment hints merged with probe replies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvHints {
    pub term: String,
    pub term_program: String,
    pub colorterm: String,
    /// VTE-based (`VTE_VERSION`) or Windows Terminal (`WT_SESSION`): OSC 8 capable.
    pub vte_or_wt: bool,
}

impl EnvHints {
    pub fn from_env() -> Self {
        let g = |k| std::env::var(k).unwrap_or_default();
        EnvHints {
            term: g("TERM"),
            term_program: g("TERM_PROGRAM"),
            colorterm: g("COLORTERM"),
            vte_or_wt: !g("VTE_VERSION").is_empty() || !g("WT_SESSION").is_empty(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeResult {
    pub kitty_keyboard: bool,
    pub sync_update: bool,
    pub truecolor: bool,
    pub background: Background,
    pub osc52: Osc52,
    pub xtversion: Option<String>,
    /// Primary device attributes of the terminal (first DA1 reply), if seen.
    pub da1: Option<Vec<u32>>,
    /// Secondary device attributes (`CSI > ... c`), if seen.
    pub da2: Option<Vec<u32>>,
    /// The DA1 sentinel (second DA1 reply) arrived: no more answers are coming.
    pub complete: bool,
    /// Curly/coloured underlines (SGR 4:3, 58).
    pub undercurl: bool,
    /// OSC 8 hyperlinks: the compositor passes pane links through.
    pub osc8: bool,
    /// Focus reporting (DECRQM 1004 answered set/reset).
    pub focus_events: bool,
    /// SGR mouse encoding (DECRQM 1006).
    pub sgr_mouse: bool,
    /// Bracketed paste (DECRQM 2004).
    pub bracketed_paste: bool,
    /// Sixel graphics (DA1 attribute 4).
    pub sixel: bool,
    pub notifications: Notifications,
}

impl ProbeResult {
    /// `terminal.host_overrides` (03 §6.1): the escape hatch for terminals that misreport.
    /// Returns the keys that are not probe fields (the caller applies them elsewhere, e.g.
    /// `kitty_graphics`).
    pub fn apply_overrides<'a>(
        &mut self,
        o: impl IntoIterator<Item = (&'a String, &'a bool)>,
    ) -> Vec<(String, bool)> {
        let mut rest = Vec::new();
        for (k, &v) in o {
            match k.as_str() {
                "kitty_keyboard" | "kitty_kbd" => self.kitty_keyboard = v,
                "sync_update" => self.sync_update = v,
                "truecolor" => self.truecolor = v,
                "undercurl" => self.undercurl = v,
                "osc52" => {
                    self.osc52 = if v { Osc52::Allowed } else { Osc52::Unknown };
                }
                "osc8" | "hyperlinks" => self.osc8 = v,
                "focus_events" => self.focus_events = v,
                "sgr_mouse" => self.sgr_mouse = v,
                "bracketed_paste" => self.bracketed_paste = v,
                "sixel" => self.sixel = v,
                _ => rest.push((k.clone(), v)),
            }
        }
        rest
    }
}

/// Queries to write before entering the alternate screen. Replies arrive in query order; the
/// trailing DA1 is the sentinel (every terminal answers it, and answers it last).
pub fn probe_queries() -> Vec<u8> {
    probe_queries_with(&[])
}

/// [`probe_queries`] with `extra` queries (browser-pane graphics probes, 03 §6.1) inserted
/// before the DA1 sentinel.
pub fn probe_queries_with(extra: &[u8]) -> Vec<u8> {
    let mut q = Vec::new();
    q.extend_from_slice(b"\x1b[c"); // DA1
    q.extend_from_slice(b"\x1b[>c"); // DA2
    q.extend_from_slice(b"\x1b[>0q"); // XTVERSION
    q.extend_from_slice(b"\x1b[?u"); // kitty keyboard flags
    q.extend_from_slice(b"\x1b[?2026$p"); // DECRQM synchronized output
    q.extend_from_slice(b"\x1b[?2004$p"); // DECRQM bracketed paste
    q.extend_from_slice(b"\x1b[?1004$p"); // DECRQM focus events
    q.extend_from_slice(b"\x1b[?1006$p"); // DECRQM SGR mouse
    q.extend_from_slice(b"\x1b]11;?\x1b\\"); // OSC 11 background colour
    q.extend_from_slice(extra);
    q.extend_from_slice(b"\x1b[c"); // DA1 sentinel
    q
}

fn parse_nums(params: &[u8]) -> Vec<u32> {
    params
        .split(|&b| b == b';')
        .filter_map(|p| std::str::from_utf8(p).ok()?.parse().ok())
        .collect()
}

/// Parse `rgb:RRRR/GGGG/BBBB` (1-4 hex digits per channel) or `#rrggbb` to 0..=1 floats.
fn parse_color(s: &str) -> Option<(f64, f64, f64)> {
    let chan = |h: &str| -> Option<f64> {
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some(v as f64 / max as f64)
    };
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut it = rest.split('/');
        let (r, g, b) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        return Some((chan(r)?, chan(g)?, chan(b)?));
    }
    if let Some(h) = s.strip_prefix('#')
        && h.len() == 6
    {
        return Some((chan(&h[0..2])?, chan(&h[2..4])?, chan(&h[4..6])?));
    }
    None
}

fn background_of(rgb: (f64, f64, f64)) -> Background {
    let lum = 0.2126 * rgb.0 + 0.7152 * rgb.1 + 0.0722 * rgb.2;
    if lum > 0.5 {
        Background::Light
    } else {
        Background::Dark
    }
}

/// Find the end of a string-terminated sequence (BEL or ST) starting at `from`; returns
/// (content_end, next_index).
fn find_terminator(b: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < b.len() {
        match b[i] {
            0x07 => return Some((i, i + 1)),
            0x1b if i + 1 < b.len() && b[i + 1] == b'\\' => return Some((i, i + 2)),
            0x1b if i + 1 >= b.len() => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Digest terminal replies. Tolerates interleaved replies, user keystrokes and other junk
/// between replies, and a partial reply at the end.
pub fn parse_replies(bytes: &[u8], env: &EnvHints) -> ProbeResult {
    let mut r = ProbeResult::default();
    let mut da1_count = 0;
    let mut bg_rgb = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        let Some(&kind) = bytes.get(i + 1) else { break };
        match kind {
            b'[' => {
                let mut j = i + 2;
                while j < bytes.len() && (0x30..=0x3f).contains(&bytes[j]) {
                    j += 1;
                }
                let params_end = j;
                while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                    j += 1;
                }
                let inter = &bytes[params_end..j];
                let Some(&fin) = bytes.get(j) else { break };
                if !(0x40..=0x7e).contains(&fin) {
                    // Malformed: resume right after the ESC.
                    i += 1;
                    continue;
                }
                let params = &bytes[i + 2..params_end];
                match (params.first(), inter, fin) {
                    (Some(b'?'), [], b'c') => {
                        let nums = parse_nums(&params[1..]);
                        da1_count += 1;
                        if r.da1.is_none() {
                            r.da1 = Some(nums);
                        }
                    }
                    (Some(b'>'), [], b'c') => r.da2 = Some(parse_nums(&params[1..])),
                    (Some(b'?'), [], b'u') => r.kitty_keyboard = true,
                    (Some(b'?'), [b'$'], b'y') => {
                        let nums = parse_nums(&params[1..]);
                        if let [mode, state] = nums[..] {
                            // 1 set, 2 reset, 3 permanently set (0 unknown, 4 permanently reset).
                            let known = matches!(state, 1..=3);
                            match mode {
                                2026 => r.sync_update = known,
                                2004 => r.bracketed_paste = known,
                                1004 => r.focus_events = known,
                                1006 => r.sgr_mouse = known,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                i = j + 1;
            }
            b']' | b'P' | b'_' | b'^' | b'X' => {
                let Some((end, next)) = find_terminator(bytes, i + 2) else {
                    break;
                };
                let body = &bytes[i + 2..end];
                if kind == b']' {
                    if let Some(rest) = body.strip_prefix(b"11;")
                        && let Ok(s) = std::str::from_utf8(rest)
                    {
                        bg_rgb = parse_color(s.trim());
                    }
                } else if kind == b'P'
                    && let Some(rest) = body.strip_prefix(b">|")
                    && let Ok(s) = std::str::from_utf8(rest)
                {
                    let s = s.trim();
                    if !s.is_empty() {
                        r.xtversion = Some(s.to_string());
                    }
                }
                i = next;
            }
            _ => i += 1,
        }
    }
    r.complete = da1_count >= 2;
    if let Some(rgb) = bg_rgb {
        r.background = background_of(rgb);
    }
    let xt = r.xtversion.as_deref().unwrap_or("").to_ascii_lowercase();
    r.truecolor = detect_truecolor(env, &xt);
    r.osc52 = detect_osc52(env, &xt);
    r.osc8 = detect_osc8(env, &xt);
    r.undercurl = r.kitty_keyboard || detect_undercurl(env, &xt);
    r.sixel = r
        .da1
        .as_ref()
        .is_some_and(|a| a.get(1..).is_some_and(|f| f.contains(&4)));
    r.notifications = detect_notifications(env, &xt);
    r
}

/// The graphics part of `terminal.host_overrides` (keys the probe result does not hold).
pub fn graphics_overrides<'a>(
    mut caps: crate::screen::HostCaps,
    o: impl IntoIterator<Item = (&'a String, &'a bool)>,
) -> crate::screen::HostCaps {
    for (k, &v) in o {
        match k.as_str() {
            "kitty_graphics" => caps.kitty_graphics = v,
            "kitty_shm" => caps.kitty_shm = v,
            "iterm2_images" => caps.iterm2_images = v,
            "sgr_pixels" => caps.sgr_pixels = v,
            _ => {}
        }
    }
    caps
}

/// Known terminal by `TERM_PROGRAM`, `TERM` or the XTVERSION name.
fn known(env: &EnvHints, xt: &str, progs: &[&str], terms: &[&str], xts: &[&str]) -> bool {
    let term = env.term.to_ascii_lowercase();
    progs.contains(&env.term_program.as_str())
        || terms
            .iter()
            .any(|t| term == *t || term.starts_with(&format!("{t}-")))
        || xts.iter().any(|n| xt.starts_with(n))
}

/// OSC 8 hyperlinks: terminals known to render them (unknown ones get plain text; an
/// override turns them on).
fn detect_osc8(env: &EnvHints, xt: &str) -> bool {
    known(
        env,
        xt,
        &[
            "iTerm.app",
            "ghostty",
            "WezTerm",
            "vscode",
            "rio",
            "kitty",
            "Hyper",
        ],
        &[
            "xterm-kitty",
            "xterm-ghostty",
            "wezterm",
            "foot",
            "alacritty",
            "contour",
            "rio",
        ],
        &[
            "kitty",
            "ghostty",
            "wezterm",
            "iterm2",
            "foot",
            "alacritty",
            "contour",
            "rio",
        ],
    ) || env.vte_or_wt
}

fn detect_undercurl(env: &EnvHints, xt: &str) -> bool {
    known(
        env,
        xt,
        &["iTerm.app", "ghostty", "WezTerm", "kitty", "vscode"],
        &["xterm-kitty", "xterm-ghostty", "wezterm", "foot", "contour"],
        &["kitty", "ghostty", "wezterm", "iterm2", "foot", "contour"],
    )
}

fn detect_notifications(env: &EnvHints, xt: &str) -> Notifications {
    if known(env, xt, &["kitty"], &["xterm-kitty"], &["kitty"]) {
        Notifications::Osc99
    } else if known(
        env,
        xt,
        &["ghostty", "WezTerm"],
        &["xterm-ghostty", "wezterm", "foot"],
        &["ghostty", "wezterm", "foot"],
    ) {
        Notifications::Osc777
    } else if known(env, xt, &["iTerm.app"], &[], &["iterm2"]) {
        Notifications::Osc9
    } else {
        Notifications::None
    }
}

fn detect_truecolor(env: &EnvHints, xt: &str) -> bool {
    let ct = env.colorterm.to_ascii_lowercase();
    if ct == "truecolor" || ct == "24bit" {
        return true;
    }
    let term = env.term.to_ascii_lowercase();
    if term.contains("truecolor") || term.contains("24bit") || term.ends_with("-direct") {
        return true;
    }
    if matches!(
        env.term_program.as_str(),
        "iTerm.app" | "ghostty" | "WezTerm" | "vscode" | "Hyper" | "rio" | "Alacritty"
    ) {
        return true;
    }
    if matches!(
        term.as_str(),
        "xterm-kitty" | "xterm-ghostty" | "alacritty" | "foot" | "wezterm"
    ) || term.starts_with("foot")
    {
        return true;
    }
    [
        "kitty",
        "ghostty",
        "wezterm",
        "iterm2",
        "foot",
        "alacritty",
        "contour",
    ]
    .iter()
    .any(|n| xt.starts_with(n))
}

fn detect_osc52(env: &EnvHints, xt: &str) -> Osc52 {
    let term = env.term.to_ascii_lowercase();
    let known_prog = matches!(
        env.term_program.as_str(),
        "iTerm.app" | "ghostty" | "kitty" | "WezTerm" | "tmux"
    );
    let known_term = matches!(term.as_str(), "xterm-kitty" | "xterm-ghostty" | "wezterm")
        || term.starts_with("foot")
        || term.starts_with("tmux");
    let known_xt = ["kitty", "ghostty", "wezterm", "iterm2", "foot", "tmux"]
        .iter()
        .any(|n| xt.starts_with(n));
    if known_prog || known_term || known_xt {
        Osc52::Allowed
    } else {
        Osc52::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(term: &str, prog: &str, ct: &str) -> EnvHints {
        EnvHints {
            term: term.into(),
            term_program: prog.into(),
            colorterm: ct.into(),
            vte_or_wt: false,
        }
    }

    #[test]
    fn queries_end_with_da1_sentinel() {
        let q = probe_queries();
        assert!(q.ends_with(b"\x1b[c"));
        let s = String::from_utf8(q).unwrap();
        for needle in [
            "\x1b[c",
            "\x1b[>c",
            "\x1b[>0q",
            "\x1b[?u",
            "\x1b[?2026$p",
            "\x1b[?2004$p",
            "\x1b[?1004$p",
            "\x1b[?1006$p",
            "\x1b]11;?",
        ] {
            assert!(s.contains(needle), "{needle:?}");
        }
    }

    // Replies as sent by Ghostty 1.x (DA1 1;2;..c style varies; values are representative).
    const GHOSTTY: &[u8] = b"\x1b[?62;22c\x1b[>1;10;0c\x1bP>|ghostty 1.2.0\x1b\\\x1b[?0u\x1b[?2026;2$y\x1b]11;rgb:2828/2c2c/3434\x1b\\\x1b[?62;22c";
    const KITTY: &[u8] = b"\x1b[?62;c\x1b[>1;4000;29c\x1bP>|kitty(0.35.2)\x1b\\\x1b[?0u\x1b[?2026;2$y\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?62;c";
    const ITERM: &[u8] = b"\x1b[?62;4;6;22c\x1b[>0;95;0c\x1bP>|iTerm2 3.5.11\x1b\\\x1b[?0u\x1b[?2026;2$y\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\\x1b[?62;4;6;22c";
    // Terminal.app: answers DA1/DA2 only, ignores the rest.
    const TERMINAL_APP: &[u8] = b"\x1b[?1;2c\x1b[>1;95;0c\x1b[?1;2c";

    #[test]
    fn ghostty() {
        let r = parse_replies(GHOSTTY, &env("xterm-ghostty", "ghostty", "truecolor"));
        assert!(r.kitty_keyboard && r.sync_update && r.truecolor && r.complete);
        assert_eq!(r.background, Background::Dark);
        assert_eq!(r.xtversion.as_deref(), Some("ghostty 1.2.0"));
        assert_eq!(r.osc52, Osc52::Allowed);
        assert_eq!(r.da1, Some(vec![62, 22]));
        assert_eq!(r.da2, Some(vec![1, 10, 0]));
    }

    #[test]
    fn kitty() {
        let r = parse_replies(KITTY, &env("xterm-kitty", "", ""));
        assert!(r.kitty_keyboard && r.sync_update && r.truecolor && r.complete);
        assert_eq!(r.background, Background::Light);
        assert_eq!(r.xtversion.as_deref(), Some("kitty(0.35.2)"));
        assert_eq!(r.osc52, Osc52::Allowed);
    }

    #[test]
    fn iterm2() {
        let r = parse_replies(ITERM, &env("xterm-256color", "iTerm.app", ""));
        assert!(r.kitty_keyboard && r.sync_update && r.truecolor && r.complete);
        assert_eq!(r.osc52, Osc52::Allowed);
        assert_eq!(r.background, Background::Dark);
    }

    #[test]
    fn terminal_app_lacks_kitty_kbd() {
        let r = parse_replies(TERMINAL_APP, &env("xterm-256color", "Apple_Terminal", ""));
        assert!(!r.kitty_keyboard && !r.sync_update && !r.truecolor);
        assert!(r.complete);
        assert_eq!(r.background, Background::Unknown);
        assert_eq!(r.xtversion, None);
        assert_eq!(r.osc52, Osc52::Unknown);
    }

    #[test]
    fn partial_and_missing_sentinel_is_incomplete() {
        let cut = &GHOSTTY[..GHOSTTY.len() - 5];
        let r = parse_replies(cut, &env("", "", ""));
        assert!(!r.complete);
        assert!(r.kitty_keyboard);
        // truncated mid-OSC: ignored, no panic
        let r = parse_replies(b"\x1b[?62c\x1b]11;rgb:ff", &env("", "", ""));
        assert_eq!(r.background, Background::Unknown);
        assert!(!r.complete);
        let r = parse_replies(b"\x1b", &env("", "", ""));
        assert_eq!(r, parse_replies(b"", &env("", "", "")));
    }

    #[test]
    fn junk_and_keystrokes_interleaved() {
        let mut v = b"hello\x1b[A\x1b[?62c".to_vec();
        v.extend_from_slice(b"\x1b\x1b[?u zz \x07\x1b[?2026;1$y\x1b[?1;2c\xff\xfe");
        let r = parse_replies(&v, &env("", "", ""));
        assert!(r.kitty_keyboard && r.sync_update && r.complete);
        // ESC followed by garbage inside CSI
        let r = parse_replies(b"\x1b[?6\x01\x1b[?u", &env("", "", ""));
        assert!(r.kitty_keyboard);
    }

    #[test]
    fn decrqm_states() {
        let e = env("", "", "");
        assert!(parse_replies(b"\x1b[?2026;1$y", &e).sync_update);
        assert!(parse_replies(b"\x1b[?2026;2$y", &e).sync_update);
        assert!(parse_replies(b"\x1b[?2026;3$y", &e).sync_update);
        assert!(!parse_replies(b"\x1b[?2026;0$y", &e).sync_update);
        assert!(!parse_replies(b"\x1b[?2026;4$y", &e).sync_update);
        assert!(!parse_replies(b"\x1b[?2004;1$y", &e).sync_update);
    }

    #[test]
    fn background_formats() {
        let e = env("", "", "");
        let bg = |s: &[u8]| parse_replies(s, &e).background;
        assert_eq!(bg(b"\x1b]11;rgb:ff/ff/ff\x07"), Background::Light);
        assert_eq!(bg(b"\x1b]11;rgb:0/0/0\x07"), Background::Dark);
        assert_eq!(bg(b"\x1b]11;#fdf6e3\x1b\\"), Background::Light);
        assert_eq!(bg(b"\x1b]11;rgb:zz/00/00\x07"), Background::Unknown);
        assert_eq!(bg(b"\x1b]10;rgb:ff/ff/ff\x07"), Background::Unknown);
        // mid grey boundary is dark/light by luminance
        assert_eq!(bg(b"\x1b]11;rgb:8080/8080/8080\x07"), Background::Light);
        assert_eq!(bg(b"\x1b]11;rgb:7f7f/7f7f/7f7f\x07"), Background::Dark);
    }

    #[test]
    fn truecolor_from_env() {
        let t = |term, prog, ct| parse_replies(b"", &env(term, prog, ct)).truecolor;
        assert!(t("xterm-256color", "", "truecolor"));
        assert!(t("xterm-256color", "", "24bit"));
        assert!(t("xterm-256color", "WezTerm", ""));
        assert!(t("alacritty", "", ""));
        assert!(!t("xterm-256color", "Apple_Terminal", ""));
        assert!(!t("xterm", "", ""));
    }

    #[test]
    fn decrqm_modes_and_derived_caps() {
        let e = env("", "", "");
        let r = parse_replies(
            b"\x1b[?2004;2$y\x1b[?1004;1$y\x1b[?1006;0$y\x1b[?62;4;22c\x1b[?62;4;22c",
            &e,
        );
        assert!(r.bracketed_paste && r.focus_events && !r.sgr_mouse);
        assert!(r.sixel, "DA1 attribute 4");
        assert!(r.complete);
        let r = parse_replies(GHOSTTY, &env("xterm-ghostty", "ghostty", "truecolor"));
        assert!(r.osc8 && r.undercurl && !r.sixel);
        assert_eq!(r.notifications, Notifications::Osc777);
        let r = parse_replies(KITTY, &env("xterm-kitty", "", ""));
        assert_eq!(r.notifications, Notifications::Osc99);
        let r = parse_replies(ITERM, &env("xterm-256color", "iTerm.app", ""));
        assert!(r.osc8);
        assert_eq!(r.notifications, Notifications::Osc9);
        let r = parse_replies(TERMINAL_APP, &env("xterm-256color", "Apple_Terminal", ""));
        assert!(!r.osc8 && !r.undercurl);
        assert_eq!(r.notifications, Notifications::None);
    }

    #[test]
    fn host_overrides_win() {
        let mut r = parse_replies(TERMINAL_APP, &env("xterm-256color", "Apple_Terminal", ""));
        let mut o = std::collections::BTreeMap::new();
        o.insert("osc8".to_string(), true);
        o.insert("kitty_keyboard".to_string(), true);
        o.insert("truecolor".to_string(), true);
        o.insert("kitty_graphics".to_string(), false);
        let rest = r.apply_overrides(&o);
        assert!(r.osc8 && r.kitty_keyboard && r.truecolor);
        assert_eq!(rest, vec![("kitty_graphics".to_string(), false)]);
    }

    #[test]
    fn osc52_from_env() {
        let o = |term, prog| parse_replies(b"", &env(term, prog, "")).osc52;
        assert_eq!(o("screen", "tmux"), Osc52::Allowed);
        assert_eq!(o("foot", ""), Osc52::Allowed);
        assert_eq!(o("xterm-256color", "WezTerm"), Osc52::Allowed);
        assert_eq!(o("xterm-256color", "Apple_Terminal"), Osc52::Unknown);
    }
}
