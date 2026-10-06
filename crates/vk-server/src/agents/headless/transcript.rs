//! The transcript view of a headless pane (01 §3.3): what the adapters report, kept as entries
//! so the pane can be redrawn. Output is append-only while the run works (the pane's engine
//! keeps scrollback as usual); `ctrl+o` in the pane toggles every tool call between its
//! one-line summary and its full detail (diff, output) and redraws the whole transcript.
//!
//! | Entry | Collapsed | Expanded |
//! |---|---|---|
//! | tool start | `⏺ Bash: cargo test` | same |
//! | tool end | `  ⎿ ✓ +3 −1 · 12 lines` | status line, then the diff (`+`/`-`/`@@` coloured) and the output, indented |
//! | text | as reported | same |
//!
//! Statuses: `✓` done, `✗` failed (with the exit code when known), `⊘` declined or
//! interrupted. A tool call the adapter never closed shows `…` on a redraw.

use std::collections::{HashMap, VecDeque};

/// At most this many entries are kept for redraws (the oldest go first).
const MAX_ENTRIES: usize = 4000;
/// Per tool call: output and diff kept up to this many bytes each.
const MAX_DETAIL: usize = 64 * 1024;
/// Expanded view: lines shown per diff / output before "… N more lines".
const MAX_DIFF_LINES: usize = 400;
const MAX_OUTPUT_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Done,
    Failed,
    /// Declined by the user or policy, or interrupted.
    Declined,
}

/// What an adapter reports for the transcript, in order with its other output.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Text(String),
    ToolStart {
        id: String,
        label: String,
    },
    ToolEnd {
        id: String,
        status: ToolStatus,
        /// The tool's output (command output, read result), plain text.
        output: Option<String>,
        /// A unified diff (`+`/`-`/` `/`@@` lines) of what the tool changed.
        diff: Option<String>,
        exit_code: Option<i64>,
    },
}

#[derive(Debug, Clone)]
struct Tool {
    label: String,
    end: Option<End>,
}

#[derive(Debug, Clone)]
struct End {
    status: ToolStatus,
    output: Option<String>,
    diff: Option<String>,
    exit_code: Option<i64>,
}

#[derive(Debug, Clone)]
enum Entry {
    Text(String),
    Start(String),
    End(String),
}

#[derive(Default)]
pub struct Transcript {
    entries: VecDeque<Entry>,
    tools: HashMap<String, Tool>,
    expanded: bool,
}

impl Transcript {
    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Record `v` and return what to show for it now.
    pub fn push(&mut self, v: View) -> String {
        let (entry, out) = match v {
            View::Text(t) => {
                let out = t.clone();
                (Entry::Text(t), out)
            }
            View::ToolStart { id, label } => {
                let out = start_line(&label);
                self.tools.insert(id.clone(), Tool { label, end: None });
                (Entry::Start(id), out)
            }
            View::ToolEnd {
                id,
                status,
                output,
                diff,
                exit_code,
            } => {
                let end = End {
                    status,
                    output: output.map(|o| cap(o, MAX_DETAIL)).filter(|o| !o.is_empty()),
                    diff: diff.map(|d| cap(d, MAX_DETAIL)).filter(|d| !d.is_empty()),
                    exit_code,
                };
                let tool = self.tools.entry(id.clone()).or_insert_with(|| Tool {
                    label: "tool".into(),
                    end: None,
                });
                tool.end = Some(end);
                let out = end_lines(tool, self.expanded);
                (Entry::End(id), out)
            }
        };
        self.entries.push_back(entry);
        while self.entries.len() > MAX_ENTRIES {
            if let Some(Entry::End(id)) = self.entries.pop_front() {
                self.tools.remove(&id);
            }
        }
        out
    }

    /// Flip collapsed/expanded and return the redraw (`editor`: the line being typed).
    pub fn toggle(&mut self, editor: &str) -> String {
        self.expanded = !self.expanded;
        self.redraw(editor)
    }

    /// Clear the pane (screen and scrollback) and draw every kept entry again.
    pub fn redraw(&self, editor: &str) -> String {
        let mut s = String::from("\x1b[H\x1b[2J\x1b[3J");
        if self.entries.len() == MAX_ENTRIES {
            s.push_str("\x1b[2m(older transcript entries dropped)\x1b[0m\n");
        }
        for e in &self.entries {
            match e {
                Entry::Text(t) => s.push_str(t),
                Entry::Start(id) => {
                    let label = self
                        .tools
                        .get(id)
                        .map(|t| t.label.as_str())
                        .unwrap_or("tool");
                    s.push_str(&start_line(label));
                    if self.tools.get(id).is_some_and(|t| t.end.is_none()) {
                        s.push_str("  \x1b[2m⎿ …\x1b[0m\n");
                    }
                }
                Entry::End(id) => {
                    if let Some(t) = self.tools.get(id) {
                        s.push_str(&end_lines(t, self.expanded));
                    }
                }
            }
        }
        s.push_str(editor);
        s
    }
}

fn start_line(label: &str) -> String {
    format!("\x1b[1m⏺\x1b[0m {label}\n")
}

fn end_lines(t: &Tool, expanded: bool) -> String {
    let Some(e) = &t.end else {
        return String::new();
    };
    let icon = match e.status {
        ToolStatus::Done => "\x1b[32m✓\x1b[0m",
        ToolStatus::Failed => "\x1b[31m✗\x1b[0m",
        ToolStatus::Declined => "\x1b[33m⊘\x1b[0m",
    };
    let mut parts: Vec<String> = Vec::new();
    match (e.status, e.exit_code) {
        (ToolStatus::Declined, _) => parts.push("declined".into()),
        (_, Some(c)) if c != 0 => parts.push(format!("exit {c}")),
        _ => {}
    }
    if let Some(d) = &e.diff {
        let (add, del) = diff_stats(d);
        parts.push(format!("\x1b[32m+{add}\x1b[0m \x1b[31m−{del}\x1b[0m"));
    }
    if let Some(o) = &e.output {
        let n = o.lines().count();
        parts.push(format!("{n} line{}", if n == 1 { "" } else { "s" }));
    }
    let hidden = e.diff.is_some() || e.output.is_some();
    let mut s = format!("  ⎿ {icon}");
    if !parts.is_empty() {
        s.push(' ');
        s.push_str(&parts.join(" · "));
    }
    if hidden && !expanded {
        s.push_str(" \x1b[2m(ctrl+o to expand)\x1b[0m");
    }
    s.push('\n');
    if expanded {
        if let Some(d) = &e.diff {
            push_lines(&mut s, d, MAX_DIFF_LINES, diff_colour);
        }
        if let Some(o) = &e.output {
            push_lines(&mut s, o, MAX_OUTPUT_LINES, |_| "\x1b[2m");
        }
    }
    s
}

fn push_lines(s: &mut String, text: &str, max: usize, colour: fn(&str) -> &'static str) {
    let total = text.lines().count();
    for l in text.lines().take(max) {
        let c = colour(l);
        s.push_str("    ");
        s.push_str(c);
        s.push_str(&sanitize(l));
        if !c.is_empty() {
            s.push_str("\x1b[0m");
        }
        s.push('\n');
    }
    if total > max {
        s.push_str(&format!("    \x1b[2m… {} more lines\x1b[0m\n", total - max));
    }
}

fn diff_colour(l: &str) -> &'static str {
    if l.starts_with("@@") {
        "\x1b[36m"
    } else if l.starts_with('+') && !l.starts_with("+++") {
        "\x1b[32m"
    } else if l.starts_with('-') && !l.starts_with("---") {
        "\x1b[31m"
    } else {
        ""
    }
}

/// Tool output is shown inside the transcript: control characters (escape sequences a
/// command printed) are made visible instead of being interpreted by the pane.
fn sanitize(l: &str) -> String {
    l.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '·',
            c => c,
        })
        .collect()
}

fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut i = max;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
        s.push_str("\n… (truncated)");
    }
    s
}

/// Added and removed lines of a unified diff.
pub fn diff_stats(d: &str) -> (usize, usize) {
    let add = d
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let del = d
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    (add, del)
}

/// A unified-style line diff of `old` → `new` (no file headers): common prefix and suffix
/// trimmed, the middle diffed by LCS when small, else shown as removed-then-added.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out = format!(
        "@@ -{},{} +{},{} @@\n",
        pre + 1,
        am.len(),
        pre + 1,
        bm.len()
    );
    if am.len() * bm.len() <= 250_000 {
        // LCS table over the changed middle.
        let (n, m) = (am.len(), bm.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] = if am[i] == bm[j] {
                    t[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                out.push_str(&format!(" {}\n", am[i]));
                i += 1;
                j += 1;
            } else if i < n && (j == m || t[(i + 1) * (m + 1) + j] >= t[i * (m + 1) + j + 1]) {
                out.push_str(&format!("-{}\n", am[i]));
                i += 1;
            } else {
                out.push_str(&format!("+{}\n", bm[j]));
                j += 1;
            }
        }
    } else {
        for l in am {
            out.push_str(&format!("-{l}\n"));
        }
        for l in bm {
            out.push_str(&format!("+{l}\n"));
        }
    }
    out
}

/// Text of a tool result in the shapes the harnesses use: a string, `[{type: "text", text}]`
/// blocks (Claude, pi), or ACP `[{type: "content", content: {type: "text", text}}]`.
pub fn result_text(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let parts: Vec<String> = a
                .iter()
                .filter_map(|b| {
                    b.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| b.pointer("/content/text").and_then(Value::as_str))
                        .map(str::to_string)
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(_) => v
            .get("content")
            .and_then(result_text)
            .or_else(|| v.get("text").and_then(Value::as_str).map(str::to_string))
            .or_else(|| v.get("output").and_then(Value::as_str).map(str::to_string)),
        _ => None,
    }
}

/// The diff a file-editing tool call implies, from its input: `old_string`/`new_string`
/// (Claude Edit), `oldText`/`newText` (pi edit, ACP diff content), or the whole new `content`
/// (Write).
pub fn input_diff(input: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    if let Some(edits) = input.get("edits").and_then(Value::as_array) {
        let d: String = edits.iter().filter_map(input_diff).collect();
        return (!d.is_empty()).then_some(d);
    }
    match (
        s("old_string").or(s("oldText")),
        s("new_string").or(s("newText")),
        s("content"),
    ) {
        (Some(o), Some(n), _) => Some(line_diff(o, n)),
        (None, Some(n), _) => Some(line_diff("", n)),
        (_, _, Some(c)) => Some(line_diff("", c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        // Strip SGR sequences for assertions.
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn tool_calls_collapse_and_expand_with_diff_output_and_status() {
        let mut t = Transcript::default();
        assert_eq!(
            plain(&t.push(View::Text("› fix it\n".into()))),
            "› fix it\n"
        );
        let s = t.push(View::ToolStart {
            id: "t1".into(),
            label: "Edit src/a.rs".into(),
        });
        assert_eq!(plain(&s), "⏺ Edit src/a.rs\n");
        let s = t.push(View::ToolEnd {
            id: "t1".into(),
            status: ToolStatus::Done,
            output: None,
            diff: Some(line_diff("a\nb\nc\n", "a\nB\nc\nd\n")),
            exit_code: None,
        });
        assert_eq!(plain(&s), "  ⎿ ✓ +2 −1 (ctrl+o to expand)\n");
        t.push(View::ToolStart {
            id: "t2".into(),
            label: "Bash: cargo test".into(),
        });
        let s = t.push(View::ToolEnd {
            id: "t2".into(),
            status: ToolStatus::Failed,
            output: Some("running 3 tests\n\x1b[31mFAILED\x1b[0m".into()),
            diff: None,
            exit_code: Some(101),
        });
        assert_eq!(plain(&s), "  ⎿ ✗ exit 101 · 2 lines (ctrl+o to expand)\n");
        t.push(View::ToolStart {
            id: "t3".into(),
            label: "Bash: rm -rf /".into(),
        });
        let s = t.push(View::ToolEnd {
            id: "t3".into(),
            status: ToolStatus::Declined,
            output: None,
            diff: None,
            exit_code: None,
        });
        assert_eq!(plain(&s), "  ⎿ ⊘ declined\n");
        t.push(View::ToolStart {
            id: "t4".into(),
            label: "Read x".into(),
        });

        // Expanded: the whole transcript is redrawn with details.
        let r = t.toggle("› draft");
        assert!(t.expanded());
        assert!(
            r.starts_with("\x1b[H\x1b[2J\x1b[3J"),
            "clears screen and scrollback"
        );
        let p = plain(&r);
        assert!(
            p.contains("  ⎿ ✓ +2 −1\n    @@ -2,2 +2,3 @@\n    -b\n    +B\n     c\n    +d\n"),
            "{p}"
        );
        assert!(
            p.contains("    running 3 tests\n    ·[31mFAILED·[0m\n"),
            "escapes made visible: {p}"
        );
        assert!(p.contains("⏺ Read x\n  ⎿ …\n"), "unfinished call: {p}");
        assert!(p.ends_with("› draft"), "the line being typed is kept");
        assert!(!p.contains("ctrl+o to expand"));
        // Collapsed again.
        let p = plain(&t.toggle(""));
        assert!(p.contains("  ⎿ ✓ +2 −1 (ctrl+o to expand)\n"));
        assert!(!p.contains("-b"));
    }

    #[test]
    fn diffs_and_results_from_harness_shapes() {
        let d = line_diff("x\n", "x\ny\n");
        assert_eq!(d, "@@ -2,0 +2,1 @@\n+y\n");
        assert_eq!(diff_stats(&d), (1, 0));
        let v = serde_json::json!({"file_path": "a", "old_string": "1\n2", "new_string": "1\n3"});
        assert_eq!(input_diff(&v).unwrap(), "@@ -2,1 +2,1 @@\n-2\n+3\n");
        let w = serde_json::json!({"path": "a", "content": "new"});
        assert_eq!(input_diff(&w).unwrap(), "@@ -1,0 +1,1 @@\n+new\n");
        assert_eq!(input_diff(&serde_json::json!({"command": "ls"})), None);
        assert_eq!(
            result_text(
                &serde_json::json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            Some("a\nb".into())
        );
        assert_eq!(
            result_text(
                &serde_json::json!([{"type": "content", "content": {"type": "text", "text": "acp"}}])
            ),
            Some("acp".into())
        );
        assert_eq!(
            result_text(&serde_json::json!({"content": [{"type": "text", "text": "pi"}]})),
            Some("pi".into())
        );
    }

    #[test]
    fn entries_are_bounded() {
        let mut t = Transcript::default();
        for i in 0..MAX_ENTRIES + 10 {
            t.push(View::ToolStart {
                id: format!("t{i}"),
                label: "x".into(),
            });
            t.push(View::ToolEnd {
                id: format!("t{i}"),
                status: ToolStatus::Done,
                output: Some("y".repeat(MAX_DETAIL * 2)),
                diff: None,
                exit_code: None,
            });
        }
        assert_eq!(t.entries.len(), MAX_ENTRIES);
        assert!(t.tools.len() <= MAX_ENTRIES / 2 + 1);
        assert!(t.tools.values().all(|x| {
            x.end
                .as_ref()
                .is_none_or(|e| e.output.as_ref().is_none_or(|o| o.len() < MAX_DETAIL + 32))
        }));
        assert!(plain(&t.redraw("")).starts_with("(older transcript entries dropped)"));
    }
}
