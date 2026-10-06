//! `vibeke preview show <handle>`: the newest screenshot of a preview, inline in the terminal.
//!
//! Output by terminal (decided from the environment only, nothing is queried):
//! - kitty graphics (kitty, Ghostty, WezTerm): the PNG as chunked `t=d` transmission placed at
//!   the cursor, scaled to the terminal width;
//! - iTerm2 (`LC_TERMINAL=iTerm2`, which ssh forwards, or `TERM_PROGRAM=iTerm.app`): OSC 1337
//!   `File=inline=1`;
//! - anything else, or stdout not a terminal: path + metadata only.
//!
//! Always followed by one metadata line (handle, environment label, binding, URL, age).

use base64::Engine as _;
use serde_json::Value;
use vk_proto::text::escape_controls;

/// How the image is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Kitty,
    Iterm,
    /// No inline graphics: path and metadata.
    Text,
}

/// Kitty image id (inside the reserved range of ids Vibeke uses for screenshots).
const IMAGE_ID: u32 = 16_700_010;

/// Pick the mode from the environment. `tty`: stdout is a terminal.
pub fn detect(env: &dyn Fn(&str) -> Option<String>, tty: bool) -> Mode {
    if !tty {
        return Mode::Text;
    }
    let get = |k: &str| env(k).unwrap_or_default();
    let iterm = get("LC_TERMINAL") == "iTerm2" || get("TERM_PROGRAM") == "iTerm.app";
    if iterm {
        return Mode::Iterm;
    }
    let term = get("TERM");
    let prog = get("TERM_PROGRAM").to_ascii_lowercase();
    let kitty = term == "xterm-kitty"
        || term == "xterm-ghostty"
        || !get("KITTY_WINDOW_ID").is_empty()
        || !get("GHOSTTY_RESOURCES_DIR").is_empty()
        || prog == "ghostty"
        || prog == "wezterm";
    // tmux swallows graphics escapes unless passthrough is set up: stay textual there.
    if kitty && get("TMUX").is_empty() {
        Mode::Kitty
    } else {
        Mode::Text
    }
}

/// The escape sequences that draw `png` (empty for [`Mode::Text`]). `cols` caps the width in
/// cells.
pub fn image_bytes(mode: Mode, png: &[u8], name: &str, cols: u16) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    match mode {
        Mode::Text => {}
        Mode::Kitty => {
            let control = format!("a=T,i={IMAGE_ID},f=100,t=d,q=2,c={}", cols.max(1));
            let data = b64.encode(png);
            let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let more = u8::from(i + 1 < chunks.len());
                out.extend_from_slice(b"\x1b_G");
                if i == 0 {
                    out.extend_from_slice(format!("{control},m={more}").as_bytes());
                } else {
                    out.extend_from_slice(format!("m={more},q=2").as_bytes());
                }
                out.push(b';');
                out.extend_from_slice(chunk);
                out.extend_from_slice(b"\x1b\\");
            }
            out.push(b'\n');
        }
        Mode::Iterm => {
            out.extend_from_slice(
                format!(
                    "\x1b]1337;File=inline=1;size={};name={};width={};preserveAspectRatio=1:{}\x07\n",
                    png.len(),
                    b64.encode(name),
                    cols.max(1),
                    b64.encode(png)
                )
                .as_bytes(),
            );
        }
    }
    out
}

fn age(ms: i64) -> String {
    let s = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
        - ms)
        / 1000;
    match s {
        i64::MIN..=59 => format!("{}s ago", s.max(0)),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// The metadata line(s) printed with (or instead of) the image. Every value is escaped
/// ([`vk_proto::text::escape_controls`]): labels, URLs and the binding reason can carry text the
/// page supplied (its reported build id, for one), which must not reach the terminal as control
/// sequences.
pub fn describe(shot: &Value, image_shown: bool) -> String {
    let st = |k: &str| escape_controls(shot[k].as_str().unwrap_or("")).into_owned();
    let binding = match st("binding").as_str() {
        "bound" => "bound to this code".to_string(),
        "" => String::new(),
        other => format!(
            "{other}: {}",
            escape_controls(
                shot["binding_reason"]
                    .as_str()
                    .unwrap_or("no reason recorded")
            )
        ),
    };
    let mut lines = vec![format!(
        "{} · {} · {} · {}",
        st("handle"),
        st("label"),
        shot["created_at_ms"].as_i64().map(age).unwrap_or_default(),
        st("url")
    )];
    if !binding.is_empty() {
        lines.push(binding);
    }
    if !image_shown {
        lines.push(
            shot["path_on_machine"]
                .as_str()
                .map(|p| format!("image: {}", escape_controls(p)))
                .unwrap_or_else(|| {
                    "image: (not on this machine; use `vibeke screenshot open`)".into()
                }),
        );
    }
    lines.join("\n")
}

/// Terminal width in cells (`COLUMNS`, else the tty), 80 when unknown.
pub fn terminal_cols() -> u16 {
    if let Some(c) = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|c| *c > 0)
    {
        return c;
    }
    // SAFETY: TIOCGWINSZ fills a plain struct on a valid fd; failure leaves it zeroed.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col;
        }
    }
    80
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn mode_follows_the_terminal() {
        assert_eq!(detect(&env(&[("TERM", "xterm-kitty")]), true), Mode::Kitty);
        assert_eq!(
            detect(&env(&[("TERM_PROGRAM", "ghostty")]), true),
            Mode::Kitty
        );
        assert_eq!(
            detect(
                &env(&[("LC_TERMINAL", "iTerm2"), ("TERM", "xterm-256color")]),
                true
            ),
            Mode::Iterm
        );
        // iTerm2 accepts kitty sequences but not unicode placeholders: OSC 1337 wins.
        assert_eq!(
            detect(
                &env(&[("LC_TERMINAL", "iTerm2"), ("KITTY_WINDOW_ID", "1")]),
                true
            ),
            Mode::Iterm
        );
        assert_eq!(
            detect(&env(&[("TERM", "xterm-256color")]), true),
            Mode::Text
        );
        assert_eq!(detect(&env(&[("TERM", "xterm-kitty")]), false), Mode::Text);
        assert_eq!(
            detect(&env(&[("TERM", "xterm-kitty"), ("TMUX", "/tmp/x")]), true),
            Mode::Text
        );
    }

    #[test]
    fn kitty_is_chunked_direct_transmission() {
        let png = vec![7u8; 10_000];
        let out = String::from_utf8(image_bytes(Mode::Kitty, &png, "s1.png", 60)).unwrap();
        assert!(out.starts_with("\x1b_Ga=T,i=16700010,f=100,t=d,q=2,c=60,m=1;"));
        let chunks = out.matches("\x1b_G").count();
        assert_eq!(chunks, 4, "13336 base64 bytes in 4096-byte chunks");
        assert!(out.contains("\x1b_Gm=0,q=2;"));
        assert!(out.ends_with("\x1b\\\n"));
        // No `t=f/t/s`: the bytes travel through the terminal, so this works over ssh.
        assert!(!out.contains("t=f") && !out.contains("t=s") && !out.contains("t=t"));
    }

    #[test]
    fn iterm_uses_osc_1337() {
        let out = String::from_utf8(image_bytes(Mode::Iterm, b"PNGDATA", "s1.png", 40)).unwrap();
        assert!(out.starts_with("\x1b]1337;File=inline=1;size=7;name="));
        assert!(out.contains("width=40;preserveAspectRatio=1:"));
        assert!(out.ends_with("\x07\n"));
        assert!(image_bytes(Mode::Text, b"x", "n", 10).is_empty());
    }

    #[test]
    fn metadata_mentions_the_path_without_an_image() {
        let shot = json!({"handle": "s4", "label": "devbox · headless · fresh context",
            "url": "http://localhost:5173/", "binding": "illustrative",
            "binding_reason": "no checkout", "path_on_machine": "/state/blobs/ab/abc.png",
            "created_at_ms": 0});
        let t = describe(&shot, false);
        assert!(t.contains("s4 · devbox · headless · fresh context"), "{t}");
        assert!(t.contains("illustrative: no checkout"));
        assert!(t.contains("image: /state/blobs/ab/abc.png"));
        assert!(!describe(&shot, true).contains("image:"));
    }
}
