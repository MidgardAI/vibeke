//! Repo-relative path globs for claims and ignores (05 §10).
//!
//! `*` and `?` stay inside one path segment, `**` crosses segments (zero or more). A pattern
//! also covers everything below a directory it names (`src/auth` covers `src/auth/login.ts`),
//! like a claim on "the auth module" should. Matching is case-sensitive and lexical.

/// Normalize a repo-relative path or pattern: `/` separators, no leading `./` or `/`, no empty or
/// `.` segments, `..` resolved lexically. `None` when it escapes the root.
pub fn normalize(p: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

/// One segment: `*` any run, `?` one char (no `/` here, segments are split first).
fn wild(pat: &[char], s: &[char]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while i < s.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == s[i]) && pat[p] != '*' {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = i;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

fn segs(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|k| segs(rest, &path[k..])),
        Some((p, rest)) => match path.split_first() {
            Some((s, tail)) => {
                let pc: Vec<char> = p.chars().collect();
                let sc: Vec<char> = s.chars().collect();
                wild(&pc, &sc) && segs(rest, tail)
            }
            None => false,
        },
    }
}

/// Whether the repo-relative `path` is matched by `pattern` (or lies below a directory it
/// names). Both are normalized first; a pattern that escapes the root matches nothing.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let (Some(pat), Some(path)) = (normalize(pattern), normalize(path)) else {
        return false;
    };
    if pat.is_empty() {
        return false;
    }
    if pat.matches("**").count() > 8 {
        return false;
    }
    let ps: Vec<&str> = pat.split('/').collect();
    let xs: Vec<&str> = path.split('/').collect();
    if segs(&ps, &xs) {
        return true;
    }
    // A named directory covers its contents.
    let mut below = ps.clone();
    below.push("**");
    segs(&below, &xs)
}

/// Whether a pattern is plausible as a claim: not empty, does not escape the root, not the whole
/// repo by accident (`**` alone is allowed on purpose; the caller shows it).
pub fn valid_pattern(pattern: &str) -> Result<String, &'static str> {
    let Some(n) = normalize(pattern) else {
        return Err("the pattern escapes the repository root");
    };
    if n.is_empty() {
        return Err("the pattern is empty");
    }
    if n.contains('\0') {
        return Err("the pattern contains a NUL byte");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_stays_in_a_segment_and_double_star_crosses() {
        assert!(glob_match("src/*.ts", "src/a.ts"));
        assert!(!glob_match("src/*.ts", "src/x/a.ts"));
        assert!(glob_match("src/**/*.ts", "src/x/y/a.ts"));
        assert!(glob_match("src/**/*.ts", "src/a.ts"));
        assert!(glob_match("**/auth.ts", "a/b/auth.ts"));
        assert!(glob_match("**/auth.ts", "auth.ts"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
    }

    #[test]
    fn a_directory_pattern_covers_its_contents() {
        assert!(glob_match("src/auth", "src/auth/login.ts"));
        assert!(glob_match("src/auth/", "src/auth/deep/x.rs"));
        assert!(glob_match("src/auth/**", "src/auth/login.ts"));
        assert!(!glob_match("src/auth", "src/authz/x.ts"));
        assert!(glob_match("./src/auth/**", "src/auth/a"));
    }

    #[test]
    fn escaping_and_empty_patterns_match_nothing() {
        assert!(!glob_match("../x", "x"));
        assert!(!glob_match("", "x"));
        assert!(valid_pattern("../x").is_err());
        assert!(valid_pattern("./").is_err());
        assert_eq!(valid_pattern("./a//b/").unwrap(), "a/b");
    }

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(normalize("a/./b/../c").as_deref(), Some("a/c"));
        assert_eq!(normalize("/a/b").as_deref(), Some("a/b"));
        assert_eq!(normalize("a\\b").as_deref(), Some("a/b"));
        assert_eq!(normalize("../a"), None);
    }

    #[test]
    fn many_double_stars_are_refused_not_exponential() {
        let p = "**/**/**/**/**/**/**/**/**/x";
        assert!(!glob_match(p, "a/b/c/d/e/f/g/h/x"));
    }
}
