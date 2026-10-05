//! A small quote-aware shell lexer: enough to split a command line into
//! pipelines of simple commands and to find redirect targets. It is a
//! heuristic, not a shell parser.

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Cmd {
    pub words: Vec<String>,
    /// Targets of `>`, `>>`, `&>` redirects.
    pub redirects: Vec<String>,
}

impl Cmd {
    fn is_empty(&self) -> bool {
        self.words.is_empty() && self.redirects.is_empty()
    }
}

/// `;`/`&&`/`||`/newline-separated groups, each a `|`-separated list of commands.
pub(crate) type Pipeline = Vec<Cmd>;

#[derive(Clone, Copy, PartialEq)]
enum Pending {
    None,
    /// Next word is a write redirect target.
    Out,
    /// Next word is a read redirect target or fd duplication; ignore.
    Skip,
}

struct Lexer {
    pipelines: Vec<Pipeline>,
    pipeline: Pipeline,
    cmd: Cmd,
    word: String,
    has_word: bool,
    pending: Pending,
}

impl Lexer {
    fn flush_word(&mut self) {
        if !self.has_word {
            return;
        }
        let w = std::mem::take(&mut self.word);
        self.has_word = false;
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::None => self.cmd.words.push(w),
            Pending::Out => self.cmd.redirects.push(w),
            Pending::Skip => {}
        }
    }
    fn end_cmd(&mut self) {
        self.flush_word();
        self.pending = Pending::None;
        let c = std::mem::take(&mut self.cmd);
        if !c.is_empty() {
            self.pipeline.push(c);
        }
    }
    fn end_pipeline(&mut self) {
        self.end_cmd();
        let p = std::mem::take(&mut self.pipeline);
        if !p.is_empty() {
            self.pipelines.push(p);
        }
    }
    /// Drop a bare fd number (`2>`) preceding a redirect operator.
    fn redirect_prefix(&mut self) {
        if self.has_word && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.has_word = false;
        } else {
            self.flush_word();
        }
    }
}

pub(crate) fn parse(input: &str) -> Vec<Pipeline> {
    let mut lx = Lexer {
        pipelines: vec![],
        pipeline: vec![],
        cmd: Cmd::default(),
        word: String::new(),
        has_word: false,
        pending: Pending::None,
    };
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' => {
                lx.has_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    lx.word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                lx.has_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    lx.word.push(chars[i]);
                    i += 1;
                }
            }
            '\\' => {
                if let Some(n) = next {
                    lx.has_word = true;
                    lx.word.push(n);
                    i += 1;
                }
            }
            c if c.is_whitespace() && c != '\n' => lx.flush_word(),
            '\n' | ';' => lx.end_pipeline(),
            '&' if next == Some('&') => {
                lx.end_pipeline();
                i += 1;
            }
            '&' if next == Some('>') => {
                // `&>file` / `&>>file`
                lx.flush_word();
                i += 1;
                if chars.get(i + 1) == Some(&'>') {
                    i += 1;
                }
                lx.pending = Pending::Out;
            }
            '&' => lx.end_pipeline(),
            '|' if next == Some('|') => {
                lx.end_pipeline();
                i += 1;
            }
            '|' => {
                lx.end_cmd();
                if next == Some('&') {
                    i += 1;
                }
            }
            '(' | ')' | '`' => {
                if lx.word == "$" {
                    lx.word.clear();
                    lx.has_word = false;
                }
                lx.end_pipeline();
            }
            '>' => {
                lx.redirect_prefix();
                if next == Some('>') {
                    i += 1;
                }
                if chars.get(i + 1) == Some(&'&') {
                    i += 1;
                    lx.pending = Pending::Skip;
                } else {
                    lx.pending = Pending::Out;
                }
            }
            '<' => {
                lx.redirect_prefix();
                // `<<` heredoc delimiter and `<<<` here-string: skip next word.
                while chars.get(i + 1) == Some(&'<') {
                    i += 1;
                }
                lx.pending = Pending::Skip;
            }
            c => {
                lx.has_word = true;
                lx.word.push(c);
            }
        }
        i += 1;
    }
    lx.end_pipeline();
    lx.pipelines
}

/// Strip leading `VAR=value` assignments and transparent wrappers.
pub(crate) fn strip_wrappers(words: &[String]) -> &[String] {
    let mut w = words;
    loop {
        match w.first().map(String::as_str) {
            Some(a) if is_assignment(a) => w = &w[1..],
            Some("env" | "time" | "nohup" | "command" | "exec" | "nice" | "builtin") => {
                w = &w[1..];
                while let Some(a) = w.first() {
                    if a.starts_with('-') || is_assignment(a) {
                        w = &w[1..];
                    } else {
                        break;
                    }
                }
            }
            _ => return w,
        }
    }
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.chars().next().unwrap().is_ascii_digit()
        }
        None => false,
    }
}

/// `git -C dir -c k=v <sub> ...` -> (sub, args after sub).
pub(crate) fn git_sub(words: &[String]) -> Option<(&str, &[String])> {
    let mut i = 1;
    while i < words.len() {
        let w = words[i].as_str();
        match w {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => i += 2,
            _ if w.starts_with('-') => i += 1,
            _ => return Some((w, &words[i + 1..])),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(s: &str) -> Vec<Vec<Vec<String>>> {
        parse(s)
            .into_iter()
            .map(|p| p.into_iter().map(|c| c.words).collect())
            .collect()
    }

    #[test]
    fn splits_operators() {
        let p = flat("cd a && ls -la | wc -l; echo hi || true");
        assert_eq!(p.len(), 4);
        assert_eq!(p[1].len(), 2);
        assert_eq!(p[1][1], vec!["wc", "-l"]);
    }

    #[test]
    fn quotes_hide_operators() {
        let p = flat(r#"echo "a && b; c | d" 'x;y'"#);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0][0], vec!["echo", "a && b; c | d", "x;y"]);
    }

    #[test]
    fn redirects() {
        let p = parse("echo hi > out.txt 2>&1 >> log");
        assert_eq!(p[0][0].words, vec!["echo", "hi"]);
        assert_eq!(p[0][0].redirects, vec!["out.txt", "log"]);
        let p = parse("cmd &>/dev/null");
        assert_eq!(p[0][0].redirects, vec!["/dev/null"]);
    }

    #[test]
    fn subshell_exposes_inner() {
        let p = flat("echo $(rm -rf x)");
        assert_eq!(p[1][0], vec!["rm", "-rf", "x"]);
    }

    #[test]
    fn wrappers() {
        let w: Vec<String> = ["FOO=1", "env", "-i", "BAR=2", "rm", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(strip_wrappers(&w), &["rm", "x"]);
    }
}
