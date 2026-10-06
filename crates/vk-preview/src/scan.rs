//! Output URL detection (06 B2): `http://localhost:<port>` URLs and known dev-server banners
//! in a pane's output.
//!
//! The hot path ([`LineScanner::feed`]) only splits bytes into lines and keeps the ones that
//! contain `://`; escape stripping and the regex run later, off the pane's feed loop.

use regex::Regex;
use std::sync::OnceLock;

/// Longest partial line kept between chunks.
const MAX_TAIL: usize = 4096;
/// Longest line handed on for parsing.
const MAX_LINE: usize = 4096;

/// Splits pane output into lines and keeps candidates. One per pane.
#[derive(Default, Debug)]
pub struct LineScanner {
    tail: Vec<u8>,
}

fn has_scheme_sep(b: &[u8]) -> bool {
    b.windows(3).any(|w| w == b"://")
}

impl LineScanner {
    /// Feed raw output; complete lines that may hold a URL are pushed to `out`.
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut rest = data;
        while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
            let (line, after) = rest.split_at(nl);
            rest = &after[1..];
            if self.tail.is_empty() {
                if has_scheme_sep(line) {
                    out.push(line[..line.len().min(MAX_LINE)].to_vec());
                }
            } else {
                self.tail.extend_from_slice(line);
                let full = std::mem::take(&mut self.tail);
                if has_scheme_sep(&full) {
                    out.push(full[..full.len().min(MAX_LINE)].to_vec());
                }
            }
        }
        if !rest.is_empty() {
            self.tail.extend_from_slice(rest);
            if self.tail.len() > MAX_TAIL {
                // A huge unterminated line (progress bar, minified dump): keep its end only.
                let cut = self.tail.len() - 512;
                self.tail.drain(..cut);
            }
        }
    }
}

/// Text with CSI/OSC/other escape sequences removed, plus OSC 8 hyperlink targets.
pub fn strip_escapes(raw: &[u8]) -> (String, Vec<String>) {
    let mut out = Vec::with_capacity(raw.len());
    let mut links = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b == 0x1b && i + 1 < raw.len() {
            match raw[i + 1] {
                b'[' => {
                    i += 2;
                    while i < raw.len() && !(0x40..=0x7e).contains(&raw[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                b']' => {
                    let start = i + 2;
                    let mut j = start;
                    let mut end = raw.len();
                    let mut next = raw.len();
                    while j < raw.len() {
                        if raw[j] == 0x07 {
                            end = j;
                            next = j + 1;
                            break;
                        }
                        if raw[j] == 0x1b && j + 1 < raw.len() && raw[j + 1] == b'\\' {
                            end = j;
                            next = j + 2;
                            break;
                        }
                        j += 1;
                    }
                    let body = String::from_utf8_lossy(&raw[start..end]);
                    if let Some(rest) = body.strip_prefix("8;")
                        && let Some((_, uri)) = rest.split_once(';')
                        && !uri.is_empty()
                    {
                        links.push(uri.to_string());
                    }
                    i = next;
                }
                _ => i += 2,
            }
            continue;
        }
        if b == b'\r' || (b < 0x20 && b != b'\t') {
            i += 1;
            continue;
        }
        out.push(b);
        i += 1;
    }
    (String::from_utf8_lossy(&out).into_owned(), links)
}

/// A preview candidate found in output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    /// The URL as printed (escapes removed).
    pub url: String,
    /// From a known banner (`vite`, `next`).
    pub label: Option<String>,
    pub banner: bool,
}

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(https?)://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\]|\[::\])(?::(\d{1,5}))?(/[^\s"'<>`]*)?"#,
        )
        .expect("url regex")
    })
}

fn banner_label(text: &str) -> Option<&'static str> {
    // Vite: "  ➜  Local:   http://localhost:5173/"; Next: "   - Local:        http://localhost:3000".
    let t = text.trim_start();
    if t.starts_with('\u{279c}') && t.contains("Local:") {
        return Some("vite");
    }
    if t.starts_with("- Local:") || t.starts_with("▲ Local:") {
        return Some("next");
    }
    if t.starts_with("> Local:") {
        return Some("vite");
    }
    None
}

/// Parse one candidate line.
pub fn parse_line(raw: &[u8]) -> Vec<Found> {
    let (text, links) = strip_escapes(raw);
    let label = banner_label(&text);
    let mut out: Vec<Found> = Vec::new();
    let mut push = |s: &str| {
        for c in url_re().captures_iter(s) {
            let scheme = c[1].to_ascii_lowercase();
            let host = c[2].to_ascii_lowercase();
            let port = match c.get(3) {
                Some(p) => match p.as_str().parse::<u16>() {
                    Ok(p) if p != 0 => p,
                    _ => continue,
                },
                None if scheme == "https" => 443,
                None => 80,
            };
            let mut path = c.get(4).map(|m| m.as_str()).unwrap_or("/").to_string();
            while path.len() > 1 && path.ends_with(['.', ',', ';', ')', ']', '}', '!', '?', ':']) {
                path.pop();
            }
            let mut url = format!("{scheme}://{host}");
            if c.get(3).is_some() {
                url.push_str(&format!(":{port}"));
            }
            url.push_str(&path);
            let f = Found {
                scheme,
                host,
                port,
                path,
                url,
                label: label.map(str::to_string),
                banner: label.is_some(),
            };
            if !out.iter().any(|x| x.port == f.port && x.path == f.path) {
                out.push(f);
            }
        }
    };
    push(&text);
    for l in &links {
        push(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_across_chunks() {
        let mut s = LineScanner::default();
        let mut out = Vec::new();
        s.feed(b"compiling...\nready at http://local", &mut out);
        assert!(out.is_empty());
        s.feed(b"host:5173/ now\nother line\n", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], b"ready at http://localhost:5173/ now".to_vec());
        // Lines without "://" are never kept.
        out.clear();
        s.feed(&vec![b'x'; 100_000], &mut out);
        s.feed(b"\n", &mut out);
        assert!(out.is_empty());
        assert!(s.tail.is_empty());
    }

    #[test]
    fn vite_banner_with_colours() {
        let raw = b"  \xe2\x9e\x9c  \x1b[1mLocal\x1b[22m:   \x1b[36mhttp://localhost:\x1b[1m5173\x1b[22m/\x1b[39m";
        let f = parse_line(raw);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].port, 5173);
        assert_eq!(f[0].path, "/");
        assert_eq!(f[0].url, "http://localhost:5173/");
        assert_eq!(f[0].label.as_deref(), Some("vite"));
        assert!(f[0].banner);
        // The network line has no loopback URL.
        assert!(parse_line(b"  \xe2\x9e\x9c  Network: http://192.168.1.20:5173/").is_empty());
    }

    #[test]
    fn next_banner_and_plain_urls() {
        let f = parse_line(b"   - Local:        http://localhost:3000");
        assert_eq!((f[0].port, f[0].path.as_str()), (3000, "/"));
        assert_eq!(f[0].label.as_deref(), Some("next"));
        let f = parse_line(b"Serving HTTP on 127.0.0.1 port 8000 (http://127.0.0.1:8000/) ...");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].port, 8000);
        assert_eq!(f[0].path, "/");
        assert!(!f[0].banner);
        let f = parse_line(b"open http://[::1]:4000/dashboard?x=1, then https://localhost/");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].host, "[::1]");
        assert_eq!(f[0].path, "/dashboard?x=1");
        assert_eq!((f[1].port, f[1].scheme.as_str()), (443, "https"));
        assert!(parse_line(b"see https://example.com:8080/").is_empty());
        assert!(parse_line(b"http://localhost:99999/").is_empty());
        // Python's http.server banner binds the IPv6 wildcard.
        let py = parse_line(b"Serving HTTP on :: port 8119 (http://[::]:8119/) ...");
        assert_eq!(py.len(), 1);
        assert_eq!(py[0].host, "[::]");
        let f = parse_line(b"listening on http://0.0.0.0:8080");
        assert_eq!(f[0].port, 8080);
    }

    #[test]
    fn osc8_links() {
        let raw =
            b"\x1b]8;;http://localhost:6006/?path=/story\x1b\\Storybook\x1b]8;;\x1b\\ started";
        let f = parse_line(raw);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].port, 6006);
        assert_eq!(f[0].path, "/?path=/story");
    }
}
