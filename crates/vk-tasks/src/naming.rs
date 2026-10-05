//! Slugs and branch names (05 §3).

use crate::git::git;
use std::path::Path;

pub const DEFAULT_SLUG_MAX: usize = 40;

fn fold(c: char, out: &mut String) {
    match c {
        'ø' | 'ö' | 'ò' | 'ó' | 'ô' | 'õ' => out.push('o'),
        'æ' => out.push_str("ae"),
        'œ' => out.push_str("oe"),
        'å' | 'ä' | 'à' | 'á' | 'â' | 'ã' => out.push('a'),
        'é' | 'è' | 'ê' | 'ë' => out.push('e'),
        'í' | 'ì' | 'î' | 'ï' => out.push('i'),
        'ú' | 'ù' | 'û' | 'ü' => out.push('u'),
        'ý' | 'ÿ' => out.push('y'),
        'ç' => out.push('c'),
        'ñ' => out.push('n'),
        'ð' => out.push('d'),
        'þ' => out.push_str("th"),
        'ß' => out.push_str("ss"),
        'ł' => out.push('l'),
        c if c.is_ascii_alphanumeric() => out.push(c),
        _ => out.push('-'),
    }
}

/// Like [`slugify`] but may return an empty string.
pub fn slugify_raw(title: &str, max_len: usize) -> String {
    let mut folded = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        fold(c, &mut folded);
    }
    let mut out = String::new();
    for part in folded.split('-').filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(part);
    }
    out.truncate(max_len);
    out.trim_matches('-').to_string()
}

/// Lowercase ASCII-folded slug, hyphen separated, at most `max_len` bytes.
/// Falls back to `"task"` when nothing is left.
pub fn slugify(title: &str, max_len: usize) -> String {
    let s = slugify_raw(title, max_len);
    if s.is_empty() { "task".into() } else { s }
}

/// Return `base`, or `base-2`, `base-3`, ... – the first for which `taken`
/// is false.
pub fn unique_slug(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2u32..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken(c))
        .expect("unbounded iterator")
}

/// Expand `{user}` and `{slug}` in a branch template.
pub fn render_branch(template: &str, user: &str, slug: &str) -> String {
    template.replace("{user}", user).replace("{slug}", slug)
}

/// Handle for `{user}`: `git config user.name` slug, else `$USER`, else `vk`.
pub fn user_handle(repo: &Path) -> String {
    if let Ok(n) = git(repo, &["config", "user.name"]) {
        let s = slugify_raw(&n, 24);
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(u) = std::env::var("USER") {
        let s = slugify_raw(&u, 24);
        if !s.is_empty() {
            return s;
        }
    }
    "vk".into()
}
