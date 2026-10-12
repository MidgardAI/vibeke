//! Envelope lines in text that other callers read (spec 19 §4.1): pane reads, agent reads and
//! transcripts show `↪ vibeke message <id> from @x · … […]`, without the body preview and the
//! nonce. The pattern tolerates line wrapping: the screen may break the line anywhere in the
//! preview, and the nonce bracket may continue on the next row (with indentation or a box edge
//! in front of it).

use regex::{Captures, Regex};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::LazyLock;

/// A soft wrap as text reads show it: a line break, then indentation or a box edge.
const WRAP: &str = r"(?:[ \t]*\n[\s│┃|]*)?";

/// `word` with a possible wrap between any two of its characters.
fn wrapped(word: &str) -> String {
    word.chars()
        .map(|c| regex::escape(&c.to_string()))
        .collect::<Vec<_>>()
        .join(WRAP)
}

static ENVELOPE: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = format!(
        concat!(
            // `↪ vibeke message m_<id> from `, with a wrap possible anywhere in it.
            r"(?P<head>(?:↪\s*)?{vibeke}\s+{message}\s+m{w}_(?:{w}[0-9a-z])+\s+{from}\s+)",
            // The sender label never holds `·` or brackets.
            r"(?P<from>[^·\[\]]{{1,80}}?)",
            // ` · <preview>`: the preview has no brackets (the envelope replaces them).
            r"·[^\[\]]{{0,200}}?",
            // `[<nonce>]`: 12 base32 characters, maybe broken by a wrap.
            r"\[(?:[A-Z2-7][\s│┃|]*){{11}}[A-Z2-7][\s│┃|]*\]",
        ),
        vibeke = wrapped("vibeke"),
        message = wrapped("message"),
        from = wrapped("from"),
        w = WRAP,
    );
    Regex::new(&pattern).expect("valid envelope pattern")
});

/// `text` with every envelope's preview and nonce replaced by `… […]`. Borrowed when nothing
/// changed.
pub fn scrub(text: &str) -> Cow<'_, str> {
    // Every envelope has its nonce bracket (the words before it may be broken by a wrap).
    if !text.contains('[') {
        return Cow::Borrowed(text);
    }
    ENVELOPE.replace_all(text, |c: &Captures| {
        format!("{}{}· … […]", &c["head"], &c["from"])
    })
}

/// A one-line envelope in a title, maybe cut short (titles are at most 80 characters, so the
/// nonce is often gone): `↪ vibeke message <id> from <sender> · <preview…>`.
static TITLE_ENVELOPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:↪\s*)?vibeke\s+message\s+m_[0-9a-z]+…?(?:\s+from\s+(?P<from>[^·\[\]]{1,80}?))?\s*(?:·.*)?$")
        .expect("valid title envelope pattern")
});

/// A nonce bracket on its own (`[EQ4B36KPSJ6A]`), as a harness keeps it when it names a
/// thread after the envelope's preview, or one cut short at the end of a title.
static TITLE_NONCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\[[A-Z2-7]{12}\]|\s*\[[A-Z2-7]{0,11}…$").expect("valid nonce pattern")
});

/// A run title (or another one-line summary) that came from a transcript, a harness's thread
/// name or a hook, without envelope parts: an envelope becomes `Message from <sender>`, and a
/// nonce bracket anywhere is removed. The preview a harness turned into its own wording stays.
/// `None` when nothing is left.
pub fn scrub_title(title: &str) -> Option<String> {
    let s = scrub(title);
    let s = TITLE_ENVELOPE.replace(&s, |c: &Captures| match c.name("from") {
        Some(f) => format!("Message from {}", f.as_str().trim()),
        None => "Message".to_string(),
    });
    let s = TITLE_NONCE.replace_all(&s, "");
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// A pane's terminal title (OSC 0/2), which a harness may set from its thread name:
/// [`scrub_title`], but without the regex work for the usual title.
pub fn scrub_terminal_title(title: String) -> String {
    if !title.contains('[') && !title.contains("vibeke") {
        return title;
    }
    scrub_title(&title).unwrap_or_default()
}

/// [`scrub`] applied to every string in a JSON value (transcript turns and items).
pub fn scrub_value(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Cow::Owned(clean) = scrub(s) {
                *s = clean;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(scrub_value),
        Value::Object(o) => o.values_mut().for_each(scrub_value),
        _ => {}
    }
}

#[cfg(test)]
#[path = "scrub_tests.rs"]
mod tests;
