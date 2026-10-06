//! Dropped/pasted local path detection and rewriting (06 §A11.1-A11.3, client side).
//!
//! Pure text handling: parse a paste into path tokens (POSIX shell-word rules plus `file://`
//! URLs), check they exist locally, and substitute new paths re-escaped in the original style.
//! The transfer itself (blob upload, namespace check) lives elsewhere.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// How a path token was written in the pasted text; replacements reuse the same style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quote {
    /// No quoting or escaping at all.
    Bare,
    /// Contains backslash escapes (`/a\ b.png`).
    BackslashEscaped,
    SingleQuoted,
    DoubleQuoted,
    /// `file:///a%20b.png`
    FileUrl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteMode {
    /// The paste consists solely of path tokens and whitespace.
    PathsOnly,
    /// Path tokens found inside other text (which is left untouched).
    Embedded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathToken {
    pub style: Quote,
    /// Byte span of the whole token (quotes/escapes included) in the original text.
    pub span: Range<usize>,
    /// The decoded path: absolute, or starting with `~/`.
    pub path: String,
}

impl PathToken {
    /// The path with `~/` expanded against `home`.
    pub fn local_path(&self, home: &Path) -> PathBuf {
        match self.path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(&self.path),
        }
    }

    /// Last path component (trailing slashes ignored).
    pub fn basename(&self) -> &str {
        let t = self.path.trim_end_matches('/');
        t.rsplit('/').next().unwrap_or(t)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedPaste {
    pub mode: PasteMode,
    /// In order of appearance, non-overlapping.
    pub tokens: Vec<PathToken>,
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

struct Word {
    decoded: String,
    style: Quote,
    end: usize,
}

/// Read one shell word starting at byte `start`. `None` on an unterminated quote.
fn parse_word(text: &str, start: usize) -> Option<Word> {
    let mut decoded = String::new();
    let mut backslash = false;
    let mut first_quote: Option<Quote> = None;
    let mut it = text[start..].char_indices().peekable();
    let mut end = text.len();
    while let Some((off, c)) = it.next() {
        match c {
            c if is_ws(c) => {
                end = start + off;
                break;
            }
            '\\' => {
                backslash = true;
                match it.next() {
                    None => decoded.push('\\'),
                    Some((_, '\n')) => {} // line continuation
                    Some((_, n)) => decoded.push(n),
                }
            }
            '\'' => {
                first_quote.get_or_insert(Quote::SingleQuoted);
                loop {
                    match it.next() {
                        None => return None,
                        Some((_, '\'')) => break,
                        Some((_, n)) => decoded.push(n),
                    }
                }
            }
            '"' => {
                first_quote.get_or_insert(Quote::DoubleQuoted);
                loop {
                    match it.next() {
                        None => return None,
                        Some((_, '"')) => break,
                        Some((_, '\\')) => match it.peek().map(|&(_, n)| n) {
                            Some(n @ ('$' | '`' | '"' | '\\')) => {
                                decoded.push(n);
                                it.next();
                            }
                            Some('\n') => {
                                it.next();
                            }
                            _ => decoded.push('\\'),
                        },
                        Some((_, n)) => decoded.push(n),
                    }
                }
            }
            c => decoded.push(c),
        }
    }
    let style = match first_quote {
        Some(q) => q,
        None if backslash => Quote::BackslashEscaped,
        None => Quote::Bare,
    };
    Some(Word {
        decoded,
        style,
        end,
    })
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1..i + 3)?;
            let h = std::str::from_utf8(h).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn valid_path(p: &str) -> bool {
    (p.starts_with('/') || p.starts_with("~/"))
        && p != "/"
        && p != "~/"
        && !p.chars().any(|c| c.is_control())
}

/// Turn a parsed word into a path token payload, or reject it.
fn classify(w: &Word) -> Option<(Quote, String)> {
    if w.style == Quote::Bare
        && let Some(rest) = w.decoded.strip_prefix("file://")
    {
        let path_part = if rest.starts_with('/') {
            rest
        } else {
            let slash = rest.find('/')?;
            if &rest[..slash] != "localhost" {
                return None;
            }
            &rest[slash..]
        };
        let p = percent_decode(path_part)?;
        return (p.starts_with('/') && valid_path(&p)).then_some((Quote::FileUrl, p));
    }
    valid_path(&w.decoded).then(|| (w.style, w.decoded.clone()))
}

/// Parse a paste consisting solely of path tokens and whitespace. `None` if it contains
/// anything else (or no path at all).
pub fn parse_paste(text: &str) -> Option<ParsedPaste> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next()?;
        if is_ws(c) {
            i += c.len_utf8();
            continue;
        }
        let w = parse_word(text, i)?;
        let (style, path) = classify(&w)?;
        tokens.push(PathToken {
            style,
            span: i..w.end,
            path,
        });
        i = w.end;
    }
    (!tokens.is_empty()).then_some(ParsedPaste {
        mode: PasteMode::PathsOnly,
        tokens,
    })
}

const CANDIDATE_PREFIXES: [&str; 9] = [
    "/",
    "~/",
    "file://",
    "'/",
    "'~/",
    "'file://",
    "\"/",
    "\"~/",
    "\"file://",
];

/// Find path tokens inside mixed text. Punctuation glued to a path (`/etc/hosts.`) becomes part
/// of the token, which then typically fails the existence check and is left alone.
pub fn parse_embedded(text: &str) -> Option<ParsedPaste> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let Some(c) = text[i..].chars().next() else {
            break;
        };
        if is_ws(c) {
            i += c.len_utf8();
            continue;
        }
        let rest = &text[i..];
        if CANDIDATE_PREFIXES.iter().any(|p| rest.starts_with(p))
            && let Some(w) = parse_word(text, i)
            && let Some((style, path)) = classify(&w)
        {
            tokens.push(PathToken {
                style,
                span: i..w.end,
                path,
            });
            i = w.end;
            continue;
        }
        // Not a path: skip to the next whitespace.
        i += rest.find(is_ws).unwrap_or(rest.len());
    }
    (!tokens.is_empty()).then_some(ParsedPaste {
        mode: PasteMode::Embedded,
        tokens,
    })
}

/// True if every token exists locally (file or directory; `~/` expanded against `home`).
pub fn existing_local_paths(p: &ParsedPaste, home: &Path) -> bool {
    !p.tokens.is_empty()
        && p.tokens
            .iter()
            .all(|t| std::fs::metadata(t.local_path(home)).is_ok_and(|m| m.is_file() || m.is_dir()))
}

/// True if every token resolves under one of `roots` (06 A11.4: a local sandboxed pane can
/// already read its checkout and the inbox, so those pastes go through untouched).
pub fn all_under(p: &ParsedPaste, home: &Path, roots: &[String]) -> bool {
    p.tokens.iter().all(|t| {
        let f = t.local_path(home);
        let f = f.canonicalize().unwrap_or(f);
        roots.iter().any(|r| f.starts_with(r))
    })
}

fn shell_safe(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || "/._-+@%:,=~".contains(c)
    } else {
        !c.is_control()
    }
}

fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn encode(path: &str, style: Quote) -> String {
    let has_control = path.chars().any(|c| c.is_control());
    match style {
        Quote::FileUrl => format!("file://{}", percent_encode(path)),
        Quote::SingleQuoted => single_quote(path),
        Quote::DoubleQuoted if has_control => single_quote(path),
        Quote::DoubleQuoted => {
            let mut out = String::from("\"");
            for c in path.chars() {
                if matches!(c, '\\' | '$' | '`' | '"') {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
            out
        }
        Quote::Bare | Quote::BackslashEscaped if has_control => single_quote(path),
        Quote::Bare | Quote::BackslashEscaped => {
            let mut out = String::with_capacity(path.len() + 4);
            for c in path.chars() {
                if !shell_safe(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
    }
}

/// Substitute each token with its replacement (same order), re-escaped in the token's original
/// style; all other text is preserved byte for byte. Extra/missing replacements leave the
/// corresponding tokens unchanged.
pub fn rewrite(original: &str, p: &ParsedPaste, replacements: &[String]) -> String {
    debug_assert_eq!(p.tokens.len(), replacements.len());
    let mut out = String::with_capacity(original.len() + 32);
    let mut last = 0;
    for (tok, rep) in p.tokens.iter().zip(replacements) {
        out.push_str(&original[last..tok.span.start]);
        out.push_str(&encode(rep, tok.style));
        last = tok.span.end;
    }
    out.push_str(&original[last..]);
    out
}

/// Relative inbox location `<first 12 hex chars>/<basename>`.
pub fn inbox_name(hash_hex: &str, basename: &str) -> String {
    let h: String = hash_hex.chars().take(12).collect();
    format!("{h}/{basename}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INBOX: &str = "/home/demo/.local/state/vibeke/inbox/3f9a1c0b2e7d";

    fn rep(name: &str) -> String {
        format!("{INBOX}/{name}")
    }

    fn paths(text: &str) -> Vec<(Quote, String)> {
        parse_paste(text)
            .unwrap_or_else(|| panic!("not paths-only: {text:?}"))
            .tokens
            .into_iter()
            .map(|t| (t.style, t.path))
            .collect()
    }

    #[test]
    fn canonical_example() {
        let text = r"/Users/demo/Desktop/Screenshot\ 2026-10-05\ at\ 20.49.03.png";
        let p = parse_paste(text).unwrap();
        assert_eq!(p.mode, PasteMode::PathsOnly);
        assert_eq!(p.tokens.len(), 1);
        assert_eq!(p.tokens[0].style, Quote::BackslashEscaped);
        assert_eq!(
            p.tokens[0].path,
            "/Users/demo/Desktop/Screenshot 2026-10-05 at 20.49.03.png"
        );
        assert_eq!(p.tokens[0].span, 0..text.len());
        assert_eq!(
            p.tokens[0].basename(),
            "Screenshot 2026-10-05 at 20.49.03.png"
        );
        let out = rewrite(text, &p, &[rep("Screenshot 2026-10-05 at 20.49.03.png")]);
        assert_eq!(
            out,
            r"/home/demo/.local/state/vibeke/inbox/3f9a1c0b2e7d/Screenshot\ 2026-10-05\ at\ 20.49.03.png"
        );
    }

    #[test]
    fn quoting_styles_detected() {
        assert_eq!(
            paths("/Users/x/a.png"),
            vec![(Quote::Bare, "/Users/x/a.png".into())]
        );
        assert_eq!(
            paths("'/Users/x/a b.png'"),
            vec![(Quote::SingleQuoted, "/Users/x/a b.png".into())]
        );
        assert_eq!(
            paths("\"/Users/x/a b.png\""),
            vec![(Quote::DoubleQuoted, "/Users/x/a b.png".into())]
        );
        assert_eq!(
            paths("~/Desktop/a\\ b.png"),
            vec![(Quote::BackslashEscaped, "~/Desktop/a b.png".into())]
        );
        assert_eq!(
            paths("file:///Users/x/a%20b.png"),
            vec![(Quote::FileUrl, "/Users/x/a b.png".into())]
        );
        assert_eq!(
            paths("file://localhost/Users/x/a.png"),
            vec![(Quote::FileUrl, "/Users/x/a.png".into())]
        );
    }

    #[test]
    fn shell_word_edge_cases() {
        // embedded single quote via '\''
        assert_eq!(
            paths(r"'/tmp/it'\''s.png'"),
            vec![(Quote::SingleQuoted, "/tmp/it's.png".into())]
        );
        // backslash escapes of special chars
        assert_eq!(
            paths(r"/tmp/a\(1\)\ \&\ b.png"),
            vec![(Quote::BackslashEscaped, "/tmp/a(1) & b.png".into())]
        );
        // double quote escapes
        assert_eq!(
            paths(r#""/tmp/a \"q\" \$x \\ b""#),
            vec![(Quote::DoubleQuoted, r#"/tmp/a "q" $x \ b"#.into())]
        );
        // backslash that doesn't escape anything inside double quotes stays
        assert_eq!(
            paths(r#""/tmp/a\nb""#),
            vec![(Quote::DoubleQuoted, r"/tmp/a\nb".into())]
        );
        // glued quoted segment, style follows first quote
        assert_eq!(
            paths("/tmp/'a b'/c"),
            vec![(Quote::SingleQuoted, "/tmp/a b/c".into())]
        );
    }

    #[test]
    fn multiple_files_all_separators() {
        let text = "/a/one.png '/b/two three.png' /c/four\\ five.png\n~/six.png file:///d/seven%20eight.png \n";
        let p = parse_paste(text).unwrap();
        let got: Vec<_> = p
            .tokens
            .iter()
            .map(|t| (t.style, t.path.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Quote::Bare, "/a/one.png"),
                (Quote::SingleQuoted, "/b/two three.png"),
                (Quote::BackslashEscaped, "/c/four five.png"),
                (Quote::Bare, "~/six.png"),
                (Quote::FileUrl, "/d/seven eight.png"),
            ]
        );
        // spans index into the original
        assert_eq!(&text[p.tokens[1].span.clone()], "'/b/two three.png'");
        let reps: Vec<String> = ["1.png", "2 2.png", "3 3.png", "4.png", "5 5.png"]
            .iter()
            .map(|n| rep(n))
            .collect();
        let out = rewrite(text, &p, &reps);
        let expect = format!(
            "{i}/1.png '{i}/2 2.png' {i}/3\\ 3.png\n{i}/4.png file://{e}/5%205.png \n",
            i = INBOX,
            e = INBOX
        );
        assert_eq!(out, expect);
    }

    #[test]
    fn trailing_space_and_whitespace_preserved() {
        // Ghostty/iTerm2 style: trailing space after each file
        let text = "  /a/b.png   /c/d.png ";
        let p = parse_paste(text).unwrap();
        let out = rewrite(text, &p, &[rep("b.png"), rep("d.png")]);
        assert_eq!(out, format!("  {INBOX}/b.png   {INBOX}/d.png "));
    }

    #[test]
    fn non_ascii_names() {
        let text =
            r"/Users/demo/Desktop/Skjermbilde\ 2026-10-05\ kl.\ 20.49.03.png /Users/e/æøå.txt";
        let p = parse_paste(text).unwrap();
        assert_eq!(
            p.tokens[0].path,
            "/Users/demo/Desktop/Skjermbilde 2026-10-05 kl. 20.49.03.png"
        );
        assert_eq!(p.tokens[1].path, "/Users/e/æøå.txt");
        assert_eq!(p.tokens[1].style, Quote::Bare);
        let out = rewrite(
            text,
            &p,
            &[
                rep("Skjermbilde 2026-10-05 kl. 20.49.03.png"),
                rep("æøå.txt"),
            ],
        );
        assert_eq!(
            out,
            format!("{INBOX}/Skjermbilde\\ 2026-10-05\\ kl.\\ 20.49.03.png {INBOX}/æøå.txt")
        );
        // narrow no-break space (macOS screenshot names) is part of the word, not whitespace
        let nnbsp = "/Users/e/Screen\\ Shot\\ at\\ 8.49.03\u{202f}PM.png";
        assert_eq!(
            paths(nnbsp),
            vec![(
                Quote::BackslashEscaped,
                "/Users/e/Screen Shot at 8.49.03\u{202f}PM.png".into()
            )]
        );
        // file URL with percent-encoded UTF-8
        assert_eq!(
            paths("file:///Users/e/%C3%A6%C3%B8%C3%A5.txt"),
            vec![(Quote::FileUrl, "/Users/e/æøå.txt".into())]
        );
        let p = parse_paste("file:///Users/e/%C3%A6.txt").unwrap();
        assert_eq!(
            rewrite(
                "file:///Users/e/%C3%A6.txt",
                &p,
                &["/in/box/æ ø.txt".into()]
            ),
            "file:///in/box/%C3%A6%20%C3%B8.txt"
        );
    }

    #[test]
    fn rewrite_reescapes_per_style() {
        let r = "/in/box/it's a (b).png";
        let one = |text: &str| {
            let p = parse_paste(text).unwrap();
            rewrite(text, &p, &[r.to_string()])
        };
        assert_eq!(one("'/x/a b.png'"), r"'/in/box/it'\''s a (b).png'");
        assert_eq!(one("\"/x/a b.png\""), "\"/in/box/it's a (b).png\"");
        assert_eq!(one(r"/x/a\ b.png"), r"/in/box/it\'s\ a\ \(b\).png");
        // bare stays bare when no escaping is needed, else gets backslashes
        assert_eq!(one("/x/a.png"), r"/in/box/it\'s\ a\ \(b\).png");
        let p = parse_paste("/x/a.png").unwrap();
        assert_eq!(
            rewrite("/x/a.png", &p, &["/in/box/plain.png".into()]),
            "/in/box/plain.png"
        );
        assert_eq!(
            one("file:///x/a.png"),
            "file:///in/box/it%27s%20a%20%28b%29.png"
        );
        // dollar and backtick in double quotes
        let p = parse_paste("\"/x/a b\"").unwrap();
        assert_eq!(
            rewrite("\"/x/a b\"", &p, &["/in/$HOME/`x`\"".into()]),
            "\"/in/\\$HOME/\\`x\\`\\\"\""
        );
    }

    #[test]
    fn rewrite_control_chars_fall_back_to_single_quotes() {
        let p = parse_paste("/x/a.png").unwrap();
        assert_eq!(
            rewrite("/x/a.png", &p, &["/in/a\nb.png".into()]),
            "'/in/a\nb.png'"
        );
    }

    #[test]
    fn rewrite_roundtrip_through_parser() {
        // Whatever we emit must parse back to the replacement path.
        let nasty = [
            "/in/box/a b.png",
            "/in/box/it's.png",
            "/in/box/q\"uote$.png",
            "/in/box/(x)[y]{z}&;|*?<>!#`^.png",
            "/in/box/æ ø å\u{202f}PM.png",
            "/in/box/back\\slash.png",
        ];
        for style_src in [
            "/x/a.png",
            "/x/a\\ b.png",
            "'/x/a b.png'",
            "\"/x/a b.png\"",
            "file:///x/a.png",
        ] {
            let p = parse_paste(style_src).unwrap();
            for n in nasty {
                let out = rewrite(style_src, &p, &[n.to_string()]);
                let q = parse_paste(&out)
                    .unwrap_or_else(|| panic!("reparse failed: {out:?} from {style_src:?}"));
                assert_eq!(q.tokens.len(), 1, "{out:?}");
                assert_eq!(q.tokens[0].path, n, "{out:?}");
            }
        }
    }

    #[test]
    fn rejects_non_path_pastes() {
        for t in [
            "",
            "   \n",
            "see /etc/hosts please",
            "/etc/hosts please",
            "please /etc/hosts",
            "hello",
            "C:\\Users\\x\\a.png",
            "C:/Users/x/a.png",
            "\\\\server\\share\\a.png",
            "relative/path.png",
            "./a.png",
            "../a.png",
            "~user/a.png",
            "~",
            "/",
            "~/",
            "'/unterminated.png",
            "\"/unterminated.png",
            "file://host/a.png",
            "file:///a%zz.png",
            "file:///a%ff.png", // invalid UTF-8
            "file://",
            "http://example.com/a.png",
            "/a/b.png; rm -rf /",
            "/a/b.png\x07",
        ] {
            assert_eq!(parse_paste(t), None, "{t:?}");
        }
    }

    #[test]
    fn embedded_finds_paths_in_mixed_text() {
        let text = "look at '/Users/x/a b.png' and /tmp/c.txt, also \"/d/e f\" ok don't\n";
        let p = parse_embedded(text).unwrap();
        assert_eq!(p.mode, PasteMode::Embedded);
        let got: Vec<_> = p
            .tokens
            .iter()
            .map(|t| (t.style, t.path.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Quote::SingleQuoted, "/Users/x/a b.png"),
                (Quote::Bare, "/tmp/c.txt,"), // glued punctuation is part of the token
                (Quote::DoubleQuoted, "/d/e f"),
            ]
        );
        let out = rewrite(
            text,
            &p,
            &["/i/1 1.png".into(), "/i/2.txt".into(), "/i/3 3".into()],
        );
        // the glued comma was part of the token, so it is replaced along with it
        assert_eq!(
            out,
            "look at '/i/1 1.png' and /i/2.txt also \"/i/3 3\" ok don't\n"
        );
    }

    #[test]
    fn embedded_leaves_other_text_byte_for_byte() {
        let text = "  weird\t text \u{1F600}  /a/b.png\r\n tail  ";
        let p = parse_embedded(text).unwrap();
        assert_eq!(p.tokens.len(), 1);
        let out = rewrite(text, &p, &["/z/y.png".into()]);
        assert_eq!(out, "  weird\t text \u{1F600}  /z/y.png\r\n tail  ");
        // identity rewrite
        let same = rewrite(text, &p, &["/a/b.png".into()]);
        assert_eq!(same, text);
    }

    #[test]
    fn embedded_ignores_non_paths() {
        assert_eq!(parse_embedded("just words and a/b relative"), None);
        assert_eq!(parse_embedded("a lone / slash"), None);
        assert_eq!(parse_embedded("http://x.y/z and C:\\a"), None);
        assert_eq!(parse_embedded(""), None);
        // unterminated quote candidate is skipped, later path still found
        let p = parse_embedded("it's '/oops then /ok.png").unwrap();
        assert_eq!(p.tokens.len(), 1);
        assert_eq!(p.tokens[0].path, "/ok.png");
        assert_eq!(p.tokens[0].span.clone(), 17..24);
    }

    #[test]
    fn embedded_on_pure_path_text_matches_paths_only_tokens() {
        let text = r"/a/b\ c.png '/d e.png'";
        assert_eq!(
            parse_embedded(text).unwrap().tokens,
            parse_paste(text).unwrap().tokens
        );
    }

    #[test]
    fn existence_checks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("Desktop")).unwrap();
        std::fs::write(home.join("Desktop/Skjermbilde 1.png"), b"x").unwrap();
        std::fs::write(home.join("æøå.txt"), b"x").unwrap();
        let abs = home.join("Desktop/Skjermbilde 1.png");
        let text = format!(
            "{} ~/æøå.txt '~/Desktop'",
            abs.to_str().unwrap().replace(' ', "\\ ")
        );
        let p = parse_paste(&text).unwrap();
        assert_eq!(p.tokens.len(), 3);
        assert!(existing_local_paths(&p, home));
        assert_eq!(p.tokens[1].local_path(home), home.join("æøå.txt"));

        let missing = parse_paste(&format!(
            "{} ~/nope.txt",
            abs.display().to_string().replace(' ', "\\ ")
        ))
        .unwrap();
        assert!(!existing_local_paths(&missing, home));
        // home differs: `~/` resolves elsewhere
        let other = tempfile::tempdir().unwrap();
        assert!(!existing_local_paths(
            &parse_paste("~/æøå.txt").unwrap(),
            other.path()
        ));
        // empty token list never "exists"
        let empty = ParsedPaste {
            mode: PasteMode::PathsOnly,
            tokens: vec![],
        };
        assert!(!existing_local_paths(&empty, home));
    }

    #[test]
    fn sandbox_visible_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("co/src")).unwrap();
        std::fs::create_dir_all(root.join("Desktop")).unwrap();
        std::fs::write(root.join("co/src/a.rs"), b"x").unwrap();
        std::fs::write(root.join("Desktop/shot.png"), b"x").unwrap();
        let roots = vec![root.join("co").to_string_lossy().into_owned()];
        let inside = parse_paste(root.join("co/src/a.rs").to_str().unwrap()).unwrap();
        assert!(all_under(&inside, &root, &roots));
        let outside = parse_paste(root.join("Desktop/shot.png").to_str().unwrap()).unwrap();
        assert!(!all_under(&outside, &root, &roots));
        let mixed = parse_paste(&format!(
            "{} {}",
            root.join("co/src/a.rs").display(),
            root.join("Desktop/shot.png").display()
        ))
        .unwrap();
        assert!(!all_under(&mixed, &root, &roots));
    }

    #[test]
    fn inbox_names() {
        assert_eq!(
            inbox_name("3f9a1c0b2e7d99aabbcc", "Screenshot 1.png"),
            "3f9a1c0b2e7d/Screenshot 1.png"
        );
        assert_eq!(inbox_name("abc", "x"), "abc/x");
    }

    #[test]
    fn basename_of_directories() {
        let p = parse_paste("/a/b/dir/").unwrap();
        assert_eq!(p.tokens[0].basename(), "dir");
    }
}
