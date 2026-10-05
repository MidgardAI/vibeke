//! Approval fingerprints (spec 04 §7.7).
//!
//! `fingerprint = hash(harness, tool, normalized_command_prefix | path_glob, workspace)`.
//! The hash is FNV-1a/64 over a NUL-separated string, so it is stable across
//! runs, platforms and toolchain versions (unlike `DefaultHasher`).

use crate::shell;
use std::path::{Component, Path, PathBuf};

/// What a fingerprinted approval was about.
#[derive(Debug, Clone, Copy)]
pub enum Subject<'a> {
    Command(&'a str),
    Path(&'a str),
}

/// Tools whose subject is a file path rather than a shell command.
const PATH_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Read",
    "apply_patch",
    "write_file",
    "edit_file",
    "read_file",
];

/// Tools whose first non-flag argument is part of the identity of the command.
const TWO_WORD: &[&str] = &[
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "npx",
    "bunx",
    "pip",
    "pip3",
    "uv",
    "cargo",
    "go",
    "git",
    "docker",
    "podman",
    "kubectl",
    "brew",
    "make",
    "just",
    "gem",
    "apt",
    "apt-get",
    "terraform",
    "gh",
    "rustup",
    "poetry",
];

/// Convenience wrapper: decides command vs path from the tool name.
pub fn fingerprint(harness: &str, tool: &str, command_or_path: &str, workspace: &Path) -> String {
    let subject = if PATH_TOOLS.contains(&tool) {
        Subject::Path(command_or_path)
    } else {
        Subject::Command(command_or_path)
    };
    fingerprint_subject(harness, tool, subject, workspace)
}

pub fn fingerprint_subject(
    harness: &str,
    tool: &str,
    subject: Subject<'_>,
    workspace: &Path,
) -> String {
    let norm = match subject {
        Subject::Command(c) => format!("cmd:{}", command_prefix(c)),
        Subject::Path(p) => format!("path:{}", path_glob(p, workspace)),
    };
    let mut h = Fnv64::new();
    for part in [harness, tool, &norm, &workspace.to_string_lossy()] {
        h.write(part.as_bytes());
        h.write(&[0]);
    }
    format!("{:016x}", h.0)
}

/// Normalised command prefix: for each simple command (in order, joined by
/// ` && `), its first word, or first two words for tools in [`TWO_WORD`]
/// (the second word is the first non-flag argument). Compound commands keep
/// every segment so `cd x && rm -rf y` never collides with a bare `cd`.
pub fn command_prefix(cmd: &str) -> String {
    let mut parts: Vec<String> = vec![];
    for pipeline in shell::parse(cmd) {
        for c in pipeline {
            let words = shell::strip_wrappers(&c.words);
            let Some(first) = words.first() else { continue };
            let name = first.rsplit('/').next().unwrap_or(first);
            let mut p = name.to_string();
            if TWO_WORD.contains(&name) {
                let sub = if name == "git" {
                    shell::git_sub(words).map(|(s, _)| s)
                } else {
                    words[1..]
                        .iter()
                        .find(|w| !w.starts_with('-'))
                        .map(String::as_str)
                };
                if let Some(s) = sub {
                    p.push(' ');
                    p.push_str(s);
                }
            }
            if !c.redirects.is_empty() {
                p.push_str(" >");
            }
            parts.push(p);
        }
    }
    parts.join(" && ")
}

/// Glob of the parent directory: `src/foo/*` relative to the workspace when
/// inside it, otherwise the absolute parent `/abs/dir/*`.
pub fn path_glob(path: &str, workspace: &Path) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace.join(p)
    };
    let abs = normalize(&abs);
    let parent = abs.parent().unwrap_or(&abs);
    match parent.strip_prefix(normalize(workspace)) {
        Ok(rel) if rel.as_os_str().is_empty() => "./*".to_string(),
        Ok(rel) => format!("{}/*", rel.display()),
        Err(_) => format!("{}/*", parent.display()),
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

pub(crate) struct Fnv64(pub u64);

impl Fnv64 {
    pub(crate) fn new() -> Self {
        Fnv64(0xcbf29ce484222325)
    }
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = "/w/proj";
    fn fp(tool: &str, s: &str) -> String {
        fingerprint("claude", tool, s, Path::new(WS))
    }

    #[test]
    fn stable_known_value() {
        // Guards against accidental changes of the hash or normalisation.
        assert_eq!(fp("Bash", "pnpm test"), "620879316f6658e6");
        let mut h = Fnv64::new();
        h.write(b"a");
        assert_eq!(h.0, 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn same_prefix_same_fingerprint() {
        assert_eq!(
            fp("Bash", "pnpm test"),
            fp("Bash", "pnpm test --filter web")
        );
        assert_eq!(fp("Bash", "pnpm -r test"), fp("Bash", "pnpm test"));
        assert_ne!(fp("Bash", "pnpm test"), fp("Bash", "pnpm install"));
        assert_eq!(fp("Bash", "ls -la"), fp("Bash", "ls src"));
        assert_ne!(fp("Bash", "ls"), fp("Bash", "cat"));
        assert_ne!(fp("Bash", "git status"), fp("Bash", "git push"));
        assert_eq!(
            fp("Bash", "FOO=1 cargo test -p x"),
            fp("Bash", "cargo test")
        );
    }

    #[test]
    fn compound_commands_keep_all_segments() {
        assert_ne!(fp("Bash", "cd x && rm -rf y"), fp("Bash", "cd x"));
        assert_ne!(fp("Bash", "ls | sh"), fp("Bash", "ls"));
        assert_eq!(command_prefix("cd x && rm -rf y"), "cd && rm");
        assert_ne!(fp("Bash", "echo hi"), fp("Bash", "echo hi > f"));
    }

    #[test]
    fn varies_by_harness_tool_workspace() {
        let a = fingerprint("claude", "Bash", "ls", Path::new("/a"));
        assert_ne!(a, fingerprint("codex", "Bash", "ls", Path::new("/a")));
        assert_ne!(a, fingerprint("claude", "Shell", "ls", Path::new("/a")));
        assert_ne!(a, fingerprint("claude", "Bash", "ls", Path::new("/b")));
    }

    #[test]
    fn paths_use_parent_glob() {
        assert_eq!(fp("Edit", "src/a.rs"), fp("Edit", "/w/proj/src/b.rs"));
        assert_ne!(fp("Edit", "src/a.rs"), fp("Edit", "tests/a.rs"));
        assert_eq!(path_glob("src/x/a.rs", Path::new(WS)), "src/x/*");
        assert_eq!(path_glob("a.rs", Path::new(WS)), "./*");
        assert_eq!(path_glob("/etc/hosts", Path::new(WS)), "/etc/*");
        assert_eq!(path_glob("src/../lib/a.rs", Path::new(WS)), "lib/*");
        assert_ne!(fp("Edit", "src/a.rs"), fp("Write", "src/a.rs"));
    }

    #[test]
    fn explicit_subject() {
        let a = fingerprint_subject("claude", "X", Subject::Command("ls"), Path::new(WS));
        let b = fingerprint_subject("claude", "X", Subject::Path("ls"), Path::new(WS));
        assert_ne!(a, b);
    }
}
