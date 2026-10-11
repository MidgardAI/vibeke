//! Diagnostics that a plugin run keeps next to its result (18 §12): traces, server logs and
//! command logs, copied into `<results>/<stem>-<os>/logs/`. They are for people reading a
//! failed run, never for verdicts: the report ignores them. Every file is redacted with
//! `vk-redact` and cut to [`LOG_MAX_BYTES`] (the tail is kept, where failures are), so it stays
//! well under the result unpacker's per-file limit.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The most a log file keeps.
pub const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Write `text`, redacted, keeping at most `max` bytes of its tail.
pub fn write_redacted(dst: &Path, text: &str, max: u64) -> std::io::Result<()> {
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let red = vk_redact::redact(text);
    let bytes = red.as_bytes();
    let max = max as usize;
    let out = if bytes.len() > max {
        let mut start = bytes.len() - max;
        while start < bytes.len() && !red.is_char_boundary(start) {
            start += 1;
        }
        format!("[{start} earlier bytes cut]\n{}", &red[start..])
    } else {
        red.into_owned()
    };
    std::fs::write(dst, out)
}

/// Copy the tail of `src` (at most `max` bytes), redacted, to `dst`. `Ok(false)` when `src` does
/// not exist or is not a regular file (symbolic links are not followed).
pub fn copy_redacted(src: &Path, dst: &Path, max: u64) -> std::io::Result<bool> {
    let meta = match std::fs::symlink_metadata(src) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut f = std::fs::File::open(src)?;
    // Read a little more than `max`: redaction may shorten the text.
    let want = max.saturating_add(max / 4);
    let skipped = meta.len().saturating_sub(want);
    f.seek(SeekFrom::Start(skipped))?;
    let mut buf = Vec::new();
    f.take(want).read_to_end(&mut buf)?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if skipped > 0 {
        text = format!("[{skipped} earlier bytes not read]\n{text}");
    }
    write_redacted(dst, &text, max)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

    #[test]
    fn copies_redacted_tail() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("server.log");
        let body = format!("{}token {SECRET}\nlast line\n", "x".repeat(10_000));
        std::fs::write(&src, &body).unwrap();
        let dst = d.path().join("logs/server.log");
        assert!(copy_redacted(&src, &dst, 1000).unwrap());
        let got = std::fs::read_to_string(&dst).unwrap();
        assert!(!got.contains(SECRET), "{got}");
        assert!(got.ends_with("last line\n"));
        assert!(got.len() < 1100, "{}", got.len());
        assert!(got.starts_with('['));
    }

    #[test]
    fn missing_and_linked_sources_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("out.log");
        assert!(!copy_redacted(&d.path().join("none"), &dst, 100).unwrap());
        let real = d.path().join("real");
        std::fs::write(&real, "x").unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(!copy_redacted(&link, &dst, 100).unwrap());
        assert!(!dst.exists());
    }

    #[test]
    fn writes_short_text_whole() {
        let d = tempfile::tempdir().unwrap();
        let dst = d.path().join("a/b.txt");
        write_redacted(&dst, "héllo", 100).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "héllo");
        write_redacted(&dst, &"é".repeat(100), 51).unwrap();
        let got = std::fs::read_to_string(&dst).unwrap();
        assert!(got.ends_with('é'));
    }
}
