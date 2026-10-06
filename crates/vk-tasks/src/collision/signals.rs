//! Signal extraction for the collision tracker (05 §10): which paths a tool call wrote or read,
//! path normalization against a repo root, and the `git status --porcelain` poll.
//!
//! Every adapter ends in the hook vocabulary (`PostToolUse` with a tool name and input), so one
//! extractor serves Claude, Codex (`apply_patch`), pi/omp (`edit`/`write`), OpenCode and ACP.

use super::glob::{glob_match, normalize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// What a tool did to a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Create,
    Modify,
    Delete,
    Rename,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Create => "create",
            Op::Modify => "modify",
            Op::Delete => "delete",
            Op::Rename => "rename",
        }
    }
}

const PATH_KEYS: &[&str] = &["file_path", "path", "filePath", "notebook_path", "file"];

fn path_of(input: &Value) -> Option<String> {
    PATH_KEYS
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// Paths named by an `apply_patch` envelope (`*** Add File:`, `*** Update File:`,
/// `*** Delete File:`, `*** Move to:`).
pub fn patch_paths(text: &str) -> Vec<(String, Op)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("*** ") else {
            continue;
        };
        let item = if let Some(p) = rest.strip_prefix("Add File:") {
            (p, Op::Create)
        } else if let Some(p) = rest.strip_prefix("Update File:") {
            (p, Op::Modify)
        } else if let Some(p) = rest.strip_prefix("Delete File:") {
            (p, Op::Delete)
        } else if let Some(p) = rest.strip_prefix("Move to:") {
            (p, Op::Rename)
        } else {
            continue;
        };
        let p = item.0.trim();
        if !p.is_empty() {
            out.push((p.to_string(), item.1));
        }
    }
    out
}

/// Paths a finished tool call wrote, as the harness reported them (absolute or relative).
/// Shell commands are not listed: their edits are found by the watcher and `git status`.
pub fn edit_paths(tool: &str, input: &Value) -> Vec<(String, Op)> {
    match tool {
        "Write" | "write" => path_of(input)
            .map(|p| vec![(p, Op::Create)])
            .unwrap_or_default(),
        "Edit" | "MultiEdit" | "NotebookEdit" | "edit" | "str_replace" | "Update" => path_of(input)
            .map(|p| vec![(p, Op::Modify)])
            .unwrap_or_default(),
        "Delete" | "delete" => path_of(input)
            .map(|p| vec![(p, Op::Delete)])
            .unwrap_or_default(),
        "Move" | "move" => {
            let mut v = Vec::new();
            if let Some(p) = path_of(input) {
                v.push((p, Op::Delete));
            }
            if let Some(p) = input
                .get("destination")
                .or_else(|| input.get("new_path"))
                .and_then(Value::as_str)
            {
                v.push((p.to_string(), Op::Create));
            }
            v
        }
        "apply_patch" | "ApplyPatch" | "applypatch" => {
            let text = ["input", "patch", "command", "diff"]
                .iter()
                .find_map(|k| match input.get(*k) {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(Value::Array(a)) => Some(
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            patch_paths(&text)
        }
        _ => Vec::new(),
    }
}

/// Paths a tool call read (only whole-file reads; searches name no single file).
pub fn read_paths(tool: &str, input: &Value) -> Vec<String> {
    match tool {
        "Read" | "read" | "NotebookRead" | "view" => path_of(input).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Whether the tool is one an enforced claim may deny (a *reported* edit tool).
pub fn is_edit_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Write"
            | "Edit"
            | "MultiEdit"
            | "NotebookEdit"
            | "write"
            | "edit"
            | "apply_patch"
            | "ApplyPatch"
    )
}

/// `path` relative to `root` with `/` separators, or `None` when it lies outside the root.
/// Symbolic links in the root are resolved (macOS `/var` vs `/private/var`) on a second try.
pub fn relativize(root: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    if p.is_relative() {
        return normalize(path).filter(|n| !n.is_empty());
    }
    if let Ok(rel) = p.strip_prefix(root) {
        return normalize(&rel.to_string_lossy()).filter(|n| !n.is_empty());
    }
    // Resolve symlinks in the root and in the longest existing prefix of the path.
    let croot = std::fs::canonicalize(root).ok()?;
    let mut base = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = std::fs::canonicalize(&base) {
            let mut full = c;
            for t in tail.iter().rev() {
                full.push(t);
            }
            let rel = full.strip_prefix(&croot).ok()?;
            return normalize(&rel.to_string_lossy()).filter(|n| !n.is_empty());
        }
        let name = base.file_name()?.to_os_string();
        tail.push(name);
        if !base.pop() {
            return None;
        }
    }
}

/// Whether a repo-relative path is never tracked: under `.git` or `node_modules`, or matched by
/// one of the configured globs. (Gitignore rules are applied by the caller through
/// `git check-ignore`.)
pub fn is_ignored_path(rel: &str, extra: &[String]) -> bool {
    if rel.split('/').any(|c| c == ".git" || c == "node_modules") {
        return true;
    }
    extra.iter().any(|g| glob_match(g, rel))
}

/// The nearest ancestor of `dir` holding a `.git` entry (directory, or file for a worktree), or
/// `dir` itself. A cheap filesystem walk: no process is spawned.
pub fn repo_root_of(dir: &Path) -> std::path::PathBuf {
    let start = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut cur = start.as_path();
    loop {
        if cur.join(".git").exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return start,
        }
    }
}

// ---- git status --porcelain -----------------------------------------------------------------

/// One `git status --porcelain=v1 -z` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub xy: String,
    pub path: String,
    /// The old path of a rename or copy.
    pub from: Option<String>,
}

/// Parse `git status --porcelain=v1 -z` output.
pub fn parse_porcelain_z(out: &[u8]) -> Vec<StatusEntry> {
    let text = String::from_utf8_lossy(out);
    let mut parts = text.split('\0').filter(|s| !s.is_empty());
    let mut v = Vec::new();
    while let Some(e) = parts.next() {
        if e.len() < 4 {
            continue;
        }
        let (xy, path) = (&e[..2], e[3..].to_string());
        let renamed = xy.contains('R') || xy.contains('C');
        let from = if renamed {
            parts.next().map(str::to_string)
        } else {
            None
        };
        v.push(StatusEntry {
            xy: xy.to_string(),
            path,
            from,
        });
    }
    v
}

/// What a status entry says happened to its path.
pub fn status_op(xy: &str) -> Op {
    if xy == "??" || xy.contains('A') {
        Op::Create
    } else if xy.contains('D') {
        Op::Delete
    } else if xy.contains('R') || xy.contains('C') {
        Op::Rename
    } else {
        Op::Modify
    }
}

/// A fingerprint of a status entry and the file behind it: it changes when the file is written
/// again while it stays "modified".
pub fn fingerprint(xy: &str, size: Option<u64>, mtime_ms: Option<i64>) -> String {
    format!(
        "{xy}:{}:{}",
        size.map(|s| s.to_string()).unwrap_or_default(),
        mtime_ms.map(|s| s.to_string()).unwrap_or_default()
    )
}

/// Paths whose status entry is new or whose fingerprint changed since `prev`. Entries that left
/// the status (committed, reverted) are not writes. `prev == None` is the baseline: nothing is
/// reported.
pub fn status_changes(
    prev: Option<&BTreeMap<String, String>>,
    now: &BTreeMap<String, (String, Op)>,
) -> Vec<(String, Op)> {
    let Some(prev) = prev else {
        return Vec::new();
    };
    now.iter()
        .filter(|(p, (fp, _))| prev.get(*p) != Some(fp))
        .map(|(p, (_, op))| (p.clone(), *op))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_tools_name_their_paths() {
        let e = edit_paths("Edit", &json!({"file_path": "/r/a.rs"}));
        assert_eq!(e, vec![("/r/a.rs".to_string(), Op::Modify)]);
        let w = edit_paths("Write", &json!({"path": "b.rs"}));
        assert_eq!(w, vec![("b.rs".to_string(), Op::Create)]);
        assert!(edit_paths("Bash", &json!({"command": "sed -i x y"})).is_empty());
        assert!(edit_paths("Edit", &json!({})).is_empty());
    }

    #[test]
    fn apply_patch_lists_every_file() {
        let p = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/b.rs\n+z\n*** Delete File: old.rs\n*** Move to: new.rs\n*** End Patch";
        let v = edit_paths("apply_patch", &json!({"input": p}));
        assert_eq!(
            v,
            vec![
                ("src/a.rs".to_string(), Op::Modify),
                ("src/b.rs".to_string(), Op::Create),
                ("old.rs".to_string(), Op::Delete),
                ("new.rs".to_string(), Op::Rename),
            ]
        );
        let arr = edit_paths(
            "apply_patch",
            &json!({"command": ["apply_patch", "*** Update File: x.rs"]}),
        );
        assert_eq!(arr, vec![("x.rs".to_string(), Op::Modify)]);
    }

    #[test]
    fn reads_are_whole_file_only() {
        assert_eq!(
            read_paths("Read", &json!({"file_path": "/r/a"})),
            vec!["/r/a"]
        );
        assert!(read_paths("Grep", &json!({"path": "/r"})).is_empty());
    }

    #[test]
    fn relativize_handles_absolute_relative_and_outside() {
        let root = Path::new("/repo");
        assert_eq!(
            relativize(root, "/repo/src/a.rs").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(relativize(root, "src/./a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(relativize(root, "/elsewhere/a.rs"), None);
        assert_eq!(relativize(root, "../a.rs"), None);
        assert_eq!(relativize(root, "/repo"), None);
    }

    #[test]
    fn relativize_resolves_symlinked_roots() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        std::fs::write(real.join("src/a.rs"), "x").unwrap();
        // The harness reports the resolved path, the tracker knows the link.
        let canon = std::fs::canonicalize(&real).unwrap();
        let p = canon.join("src/a.rs");
        assert_eq!(
            relativize(&link, &p.to_string_lossy()).as_deref(),
            Some("src/a.rs")
        );
        // A file that does not exist yet.
        let p = canon.join("src/new.rs");
        assert_eq!(
            relativize(&link, &p.to_string_lossy()).as_deref(),
            Some("src/new.rs")
        );
    }

    #[test]
    fn ignored_paths() {
        assert!(is_ignored_path(".git/index", &[]));
        assert!(is_ignored_path("web/node_modules/x/y.js", &[]));
        assert!(!is_ignored_path("src/a.rs", &[]));
        assert!(is_ignored_path("dist/a.js", &["dist/**".to_string()]));
    }

    #[test]
    fn porcelain_with_renames_parses() {
        let out = b" M src/a.rs\0?? new file.txt\0R  new.rs\0old.rs\0 D gone.rs\0";
        let v = parse_porcelain_z(out);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].path, "src/a.rs");
        assert_eq!(v[1].path, "new file.txt");
        assert_eq!(v[2].from.as_deref(), Some("old.rs"));
        assert_eq!(status_op(&v[1].xy), Op::Create);
        assert_eq!(status_op(&v[3].xy), Op::Delete);
        assert_eq!(status_op(&v[0].xy), Op::Modify);
    }

    #[test]
    fn status_changes_report_new_and_rewritten_paths_only() {
        let mut prev = BTreeMap::new();
        prev.insert("a".to_string(), fingerprint(" M", Some(1), Some(10)));
        prev.insert("gone".to_string(), fingerprint(" M", Some(1), Some(10)));
        let mut now = BTreeMap::new();
        now.insert(
            "a".to_string(),
            (fingerprint(" M", Some(2), Some(20)), Op::Modify),
        );
        now.insert(
            "b".to_string(),
            (fingerprint("??", Some(5), Some(30)), Op::Create),
        );
        let c = status_changes(Some(&prev), &now);
        assert_eq!(
            c,
            vec![("a".to_string(), Op::Modify), ("b".to_string(), Op::Create)]
        );
        assert!(
            status_changes(None, &now).is_empty(),
            "baseline reports nothing"
        );
        // Unchanged entry: nothing.
        let mut same = BTreeMap::new();
        same.insert(
            "a".to_string(),
            (fingerprint(" M", Some(1), Some(10)), Op::Modify),
        );
        assert!(status_changes(Some(&prev), &same).is_empty());
    }

    #[test]
    fn repo_root_walks_up_to_dot_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        assert_eq!(repo_root_of(&root.join("a/b")), root);
        // A worktree's `.git` is a file.
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: x").unwrap();
        assert_eq!(repo_root_of(&wt), wt);
    }
}
