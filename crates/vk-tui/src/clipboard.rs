//! OS clipboard helpers (06 §A9, §A10): OSC 52 encoding, native copy fallbacks, and reading an
//! image off the local clipboard for remote image paste.

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;

/// OSC 52 set-clipboard sequence (`c` clipboard, `p` primary), ST-terminated.
pub fn osc52_set(data: &[u8], primary: bool) -> Vec<u8> {
    let sel = if primary { 'p' } else { 'c' };
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    format!("\x1b]52;{sel};{b64}\x1b\\").into_bytes()
}

fn pipe_to(cmd: &str, args: &[&str], data: &[u8]) -> Result<()> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Take stdin so it is closed (EOF) before we wait.
    let mut stdin = child.stdin.take().context("no stdin")?;
    let write = stdin.write_all(data);
    drop(stdin);
    let status = child.wait()?;
    write?;
    if !status.success() {
        bail!("{cmd} exited with {status}");
    }
    Ok(())
}

/// Copy `data` to the OS clipboard with the first available tool.
pub fn os_copy(data: &[u8]) -> Result<()> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["-b"]),
        ]
    };
    let mut last: Option<anyhow::Error> = None;
    for (cmd, args) in candidates {
        match pipe_to(cmd, args, data) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e.context(format!("{cmd} failed"))),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no clipboard tool available")))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode a hex string (whitespace ignored). `None` on odd length or a non-hex byte.
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|p| Some(hex_val(p[0])? << 4 | hex_val(p[1])?))
        .collect()
}

/// Parse osascript's rendering of a data value, e.g. `«data PNGf89504E47…»`, into the raw bytes.
pub fn parse_osascript_data(out: &str, class: &str) -> Option<Vec<u8>> {
    let marker = format!("«data {class}");
    let start = out.find(&marker)? + marker.len();
    let end = start + out[start..].find('»')?;
    let bytes = decode_hex(&out[start..end])?;
    (!bytes.is_empty()).then_some(bytes)
}

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

fn run_output(cmd: &str, args: &[&str]) -> Result<Option<Vec<u8>>> {
    match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => Ok(Some(o.stdout)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Pick the best image MIME type from a `--list-types` / `TARGETS` listing.
pub fn pick_image_mime(listing: &str) -> Option<String> {
    let types: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("image/"))
        .collect();
    if types.contains(&"image/png") {
        return Some("image/png".into());
    }
    types.first().map(|s| s.to_string())
}

/// Read an image from the local clipboard, if any: `(mime, bytes)`.
pub fn os_clipboard_image() -> Result<Option<(String, Vec<u8>)>> {
    if cfg!(target_os = "macos") {
        let out = run_output("osascript", &["-e", "the clipboard as «class PNGf»"])?;
        let Some(out) = out else { return Ok(None) };
        let text = String::from_utf8_lossy(&out);
        return Ok(parse_osascript_data(&text, "PNGf")
            .filter(|b| b.starts_with(PNG_MAGIC))
            .map(|b| ("image/png".to_string(), b)));
    }
    // Wayland first, then X11.
    if let Some(list) = run_output("wl-paste", &["--list-types"])? {
        let list = String::from_utf8_lossy(&list);
        if let Some(mime) = pick_image_mime(&list)
            && let Some(data) = run_output("wl-paste", &["--type", &mime])?
            && !data.is_empty()
        {
            return Ok(Some((mime, data)));
        }
        return Ok(None);
    }
    if let Some(list) = run_output("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])? {
        let list = String::from_utf8_lossy(&list);
        if let Some(mime) = pick_image_mime(&list)
            && let Some(data) =
                run_output("xclip", &["-selection", "clipboard", "-t", &mime, "-o"])?
            && !data.is_empty()
        {
            return Ok(Some((mime, data)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_encoding() {
        assert_eq!(osc52_set(b"hello", false), b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(osc52_set(b"", true), b"\x1b]52;p;\x1b\\");
        assert_eq!(osc52_set("æ".as_bytes(), false), b"\x1b]52;c;w6Y=\x1b\\");
    }

    #[test]
    fn hex_decode() {
        assert_eq!(decode_hex("89504e47"), Some(vec![0x89, 0x50, 0x4e, 0x47]));
        assert_eq!(decode_hex("89 50\n4E"), Some(vec![0x89, 0x50, 0x4e]));
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex(""), Some(vec![]));
    }

    #[test]
    fn osascript_png() {
        let out = "«data PNGf89504E470D0A1A0A0000000D»\n";
        let b = parse_osascript_data(out, "PNGf").unwrap();
        assert!(b.starts_with(PNG_MAGIC));
        assert_eq!(b.len(), 12);
        assert_eq!(parse_osascript_data("«data TIFF0102»", "PNGf"), None);
        assert_eq!(parse_osascript_data("«data PNGf0102", "PNGf"), None);
        assert_eq!(parse_osascript_data("«data PNGf012»", "PNGf"), None);
        assert_eq!(parse_osascript_data("«data PNGf»", "PNGf"), None);
        assert_eq!(parse_osascript_data("", "PNGf"), None);
    }

    #[test]
    fn mime_selection() {
        assert_eq!(
            pick_image_mime("text/plain\nimage/jpeg\nimage/png\n").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            pick_image_mime("text/plain\nimage/bmp\n").as_deref(),
            Some("image/bmp")
        );
        assert_eq!(pick_image_mime("text/plain\nTARGETS\n"), None);
    }

    #[test]
    fn missing_tools_are_not_errors_for_reads() {
        // Must never panic or require a GUI; either Ok(None)/Ok(Some) or a plain Err is fine.
        let _ = os_clipboard_image();
    }

    #[test]
    fn pipe_to_reports_failure() {
        assert!(pipe_to("definitely-not-a-command-xyz", &[], b"x").is_err());
        assert!(pipe_to("false", &[], b"x").is_err());
        assert!(pipe_to("cat", &[], b"x").is_ok());
    }
}
