//! Platform-neutral filesystem + network policy of a `sandbox`-level process tree (13 §5–§7).
//!
//! [`SandboxSpec`] is what the server knows (checkout, git layout, private dir, network mode,
//! projected credentials); [`Policy`] is the resolved rule set with canonical paths that the
//! Seatbelt and bubblewrap/Landlock backends render. Rules are layered so later, more specific
//! rules win (the same semantics Seatbelt uses):
//!
//! 1. read everything outside `$HOME`, except [`Policy::deny_read`] (home, Vibeke runtime/state/
//!    config, the user's `$TMPDIR`, `/tmp`, `/Volumes`, `/Users/Shared`);
//! 2. re-allow [`Policy::allow_read`] (toolchains in home, `~/.gitconfig`, the checkout, the git
//!    common dir, the inbox, the private dir, projected credentials);
//! 3. write only [`Policy::allow_write`] (checkout, private dir) plus the git paths a commit needs;
//! 4. never write [`Policy::deny_write`] (`.git` hooks/config/info, the `.git` link file, the
//!    checkout root itself, projected credential files, git-executed files inside the checkout
//!    ([`crate::gitexec`]), protected host state that would sit inside the checkout) — applied
//!    last.
//!
//! [`check_checkout`] refuses checkouts that would make protected host state writable at all
//! (`$HOME`, `/`, a directory containing Vibeke's state or the user's credential dirs).

use crate::net::NetworkProfile;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the task's git metadata lives (13 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLayout {
    /// `git rev-parse --git-common-dir` (the main repo's `.git`).
    pub common_dir: PathBuf,
    /// `git rev-parse --git-dir` (`<common>/worktrees/<name>` for a worktree).
    pub git_dir: PathBuf,
    /// Branch the task may move (`refs/heads/<branch>`).
    pub branch: Option<String>,
}

impl GitLayout {
    /// Ask git for the layout of the checkout at `path`.
    pub fn detect(path: &Path) -> Option<GitLayout> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
                "--git-dir",
                "--abbrev-ref",
                "HEAD",
            ])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let common_dir = PathBuf::from(lines.next()?);
        let git_dir = PathBuf::from(lines.next()?);
        let branch = lines
            .next()
            .map(str::to_string)
            .filter(|b| !b.is_empty() && b != "HEAD");
        Some(GitLayout {
            common_dir,
            git_dir,
            branch,
        })
    }
}

impl GitLayout {
    /// [`GitLayout::detect`], refusing (fail closed) a layout whose git dirs lie outside what
    /// the checkout legitimately owns. A `.git` *file* can point anywhere; trusting it would
    /// make another host repository's object store, refs and worktree state writable inside
    /// the box. Accepted:
    ///
    /// * git and common dirs inside the checkout itself or one of `trusted` (the task's main
    ///   repository);
    /// * a linked worktree: `git_dir` is `<common>/worktrees/<name>` and its `gitdir` back-link
    ///   names this checkout's `.git` (git writes it when it creates the worktree; a planted
    ///   `.git` file pointing at another repo's worktree fails it).
    pub fn detect_checked(path: &Path, trusted: &[PathBuf]) -> Result<Option<GitLayout>, String> {
        let Some(g) = GitLayout::detect(path) else {
            return Ok(None);
        };
        g.verify(path, trusted)?;
        Ok(Some(g))
    }

    /// The containment check of [`GitLayout::detect_checked`].
    pub fn verify(&self, checkout: &Path, trusted: &[PathBuf]) -> Result<(), String> {
        let co = canon(checkout);
        let git_dir = canon(&self.git_dir);
        let common = canon(&self.common_dir);
        let trusted: Vec<PathBuf> = trusted.iter().map(|t| canon(t)).collect();
        let inside = |p: &Path| p.starts_with(&co) || trusted.iter().any(|t| p.starts_with(t));
        let linked = git_dir.parent().is_some_and(|w| {
            w.file_name().is_some_and(|n| n == "worktrees") && w.parent() == Some(common.as_path())
        }) && backlink_matches(&git_dir, &co);
        if (inside(&git_dir) && inside(&common)) || linked {
            return Ok(());
        }
        let culprit = if inside(&git_dir) { &common } else { &git_dir };
        Err(format!(
            "refusing to isolate {}: its git metadata ({}) lies outside the checkout and is not a \
             worktree of it; a `.git` file pointing at another repository would make that \
             repository writable",
            co.display(),
            culprit.display()
        ))
    }
}

/// Does `<git_dir>/gitdir` (git's back-link for a linked worktree) name `<checkout>/.git`?
fn backlink_matches(git_dir: &Path, checkout: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(git_dir.join("gitdir")) else {
        return false;
    };
    let link = Path::new(text.trim());
    let link = if link.is_absolute() {
        link.to_path_buf()
    } else {
        git_dir.join(link)
    };
    canon(&link) == checkout.join(".git")
}

/// Network side of the profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetMode {
    /// No outbound connections at all (profile `none`).
    None,
    /// Only the egress proxy on `127.0.0.1:port`, plus declared loopback ports (task port lease).
    Proxy { port: u16, local_ports: Vec<u16> },
    /// Unrestricted outbound network (restricted plugins that were granted network; 07 §7.7).
    Open,
}

/// Inputs for one contained process tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// The host user's home directory.
    pub home: PathBuf,
    /// Task checkout (read-write).
    pub checkout: PathBuf,
    pub git: Option<GitLayout>,
    /// Private per-pane directory: `tmp/`, `cache/`, ephemeral harness homes (read-write).
    pub private_dir: PathBuf,
    /// Extra read-only paths (inbox, the `vibeke` binary, repo-declared paths).
    pub extra_read: Vec<PathBuf>,
    /// Extra read-write paths (opt-in shared caches).
    pub extra_write: Vec<PathBuf>,
    /// Projected credential files: readable, never writable.
    pub read_only_files: Vec<PathBuf>,
    /// Paths hidden even when they sit under a readable root (Vibeke runtime/state/config,
    /// `$TMPDIR`). Specific `extra_read` entries under them are re-allowed afterwards.
    pub hidden: Vec<PathBuf>,
    /// Home-relative read-only allowlist; `None` uses [`DEFAULT_HOME_READ`].
    pub home_read: Option<Vec<String>>,
    /// Unix sockets the tree may connect to (the per-pane broker, 13 §4.1).
    pub unix_sockets: Vec<PathBuf>,
    pub network: NetMode,
    /// Allow listening on localhost (dev servers for previews).
    pub allow_bind_localhost: bool,
    /// Files and dirs inside the checkout that host git executes or reads as config
    /// ([`crate::gitexec::exec_targets`]): never writable.
    #[serde(default)]
    pub protected: Vec<PathBuf>,
    /// Vibeke's control directory (generated profiles, exec specs): never readable or
    /// writable from inside, even when another grant covers it.
    #[serde(default)]
    pub control_dir: Option<PathBuf>,
}

/// Home-relative paths readable inside a sandbox by default (13 §5: git config, toolchains,
/// read-only caches). Shell rc files are deliberately absent: they often export secrets.
pub const DEFAULT_HOME_READ: &[&str] = &[
    ".gitconfig",
    ".gitignore_global",
    ".config/git",
    ".cargo/bin",
    ".cargo/registry",
    ".cargo/git",
    ".cargo/config.toml",
    ".cargo/env",
    ".rustup",
    ".bun/bin",
    ".bun/install/global",
    ".deno/bin",
    ".nvm",
    ".volta",
    ".fnm",
    ".local/share/fnm",
    ".local/share/mise",
    ".local/state/mise",
    ".config/mise",
    ".local/bin",
    ".local/share/pnpm",
    ".asdf",
    ".pyenv",
    ".rbenv",
    ".sdkman",
    ".nix-profile",
    "go/bin",
    "go/pkg/mod",
];

/// Home-relative paths that are never readable, even when a broader allowlist entry covers
/// them (registry tokens next to toolchains).
pub const HOME_NEVER_READ: &[&str] = &[
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".git-credentials",
    ".config/gh",
    ".ssh",
    ".aws",
    ".gnupg",
    ".docker/config.json",
    ".kube",
];

/// Resolved, canonical rule set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Policy {
    pub deny_read: Vec<PathBuf>,
    pub allow_read: Vec<PathBuf>,
    /// Hidden roots (layer 2b: denied again after the home allowlist).
    pub hidden: Vec<PathBuf>,
    /// Re-allowed under hidden roots (inbox, private dir, `vibeke` binary).
    pub allow_read_late: Vec<PathBuf>,
    pub never_read: Vec<PathBuf>,
    pub allow_write: Vec<PathBuf>,
    /// Git dirs: read-only as a whole (layer 3b) …
    pub deny_write_git: Vec<PathBuf>,
    /// … except what a commit on the task branch needs (layer 3c).
    pub allow_write_git: Vec<PathBuf>,
    /// Anchored regexes (POSIX ERE as Seatbelt understands them) for git ref files.
    pub allow_write_regex: Vec<String>,
    /// Final layer: never writable (subtrees).
    pub deny_write: Vec<PathBuf>,
    /// Final layer: never writable (exact paths: the checkout root, its `.git` link file).
    pub deny_write_literal: Vec<PathBuf>,
    pub unix_sockets: Vec<PathBuf>,
    pub network: Option<(u16, Vec<u16>)>,
    /// Unrestricted outbound network ([`NetMode::Open`]).
    #[serde(default)]
    pub network_open: bool,
    pub allow_bind_localhost: bool,
    /// The checkout (for cwd mapping and paste visibility).
    pub checkout: PathBuf,
}

/// Canonicalize what exists; for missing paths canonicalize the longest existing ancestor
/// (Seatbelt matches resolved paths: `/var` is `/private/var` on macOS).
pub fn canon(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut tail = Vec::new();
    let mut cur = p.to_path_buf();
    while let Some(parent) = cur.parent().map(Path::to_path_buf) {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// Escape a literal path for an anchored POSIX regex.
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn push_unique(v: &mut Vec<PathBuf>, p: PathBuf) {
    if !v.contains(&p) {
        v.push(p);
    }
}

impl Policy {
    pub fn from_spec(spec: &SandboxSpec) -> Policy {
        let home = canon(&spec.home);
        let checkout = canon(&spec.checkout);
        let private = canon(&spec.private_dir);
        let mut p = Policy {
            checkout: checkout.clone(),
            allow_bind_localhost: spec.allow_bind_localhost,
            ..Default::default()
        };
        // Layer 1: deny home and shared/removable locations.
        push_unique(&mut p.deny_read, home.clone());
        for d in [
            "/Volumes",
            "/Users/Shared",
            "/private/tmp",
            "/private/var/tmp",
        ] {
            if Path::new(d).exists() {
                push_unique(&mut p.deny_read, canon(Path::new(d)));
            }
        }
        // Layer 2: home allowlist (only what exists; missing entries are harmless but noisy).
        let home_read: Vec<String> = spec
            .home_read
            .clone()
            .unwrap_or_else(|| DEFAULT_HOME_READ.iter().map(|s| s.to_string()).collect());
        for rel in home_read {
            let rel = rel.trim_start_matches("~/").trim_start_matches('/');
            let path = home.join(rel);
            if path.exists() {
                push_unique(&mut p.allow_read, canon(&path));
            }
        }
        push_unique(&mut p.allow_read, checkout.clone());
        if let Some(g) = &spec.git {
            push_unique(&mut p.allow_read, canon(&g.common_dir));
            push_unique(&mut p.allow_read, canon(&g.git_dir));
        }
        // Layer 2b: hidden roots, then specific re-allows.
        for h in &spec.hidden {
            push_unique(&mut p.hidden, canon(h));
        }
        // The checkout and git dirs are re-allowed late too, so a checkout that happens to sit
        // under a hidden root (tests under $TMPDIR) stays readable.
        push_unique(&mut p.allow_read_late, checkout.clone());
        if let Some(g) = &spec.git {
            push_unique(&mut p.allow_read_late, canon(&g.common_dir));
            push_unique(&mut p.allow_read_late, canon(&g.git_dir));
        }
        push_unique(&mut p.allow_read_late, private.clone());
        // Writable extras are readable too (ephemeral harness homes live under the hidden
        // state root).
        for r in spec.extra_read.iter().chain(spec.extra_write.iter()) {
            push_unique(&mut p.allow_read_late, canon(r));
        }
        for f in &spec.read_only_files {
            push_unique(&mut p.allow_read_late, canon(f));
        }
        for rel in HOME_NEVER_READ {
            let literal = home.join(rel);
            // Both the path and what it resolves to: `~/.ssh -> /opt/keys` must hide the
            // target too (Seatbelt and the bind mounts match resolved paths).
            if let Ok(target) = literal.canonicalize() {
                push_unique(&mut p.never_read, target);
            }
            push_unique(&mut p.never_read, literal);
        }
        if let Some(c) = &spec.control_dir {
            push_unique(&mut p.never_read, canon(c));
        }
        // Layer 3: writes.
        push_unique(&mut p.allow_write, checkout.clone());
        push_unique(&mut p.allow_write, private.clone());
        for w in &spec.extra_write {
            push_unique(&mut p.allow_write, canon(w));
        }
        if let Some(g) = &spec.git {
            let common = canon(&g.common_dir);
            let gitdir = canon(&g.git_dir);
            // Everything under the git dirs is read-only unless re-allowed below.
            push_unique(&mut p.deny_write_git, common.clone());
            push_unique(&mut p.deny_write_git, gitdir.clone());
            let mut late_allow: Vec<PathBuf> = vec![common.join("objects"), common.join("logs")];
            if gitdir != common {
                // Worktree-private dir: index, HEAD, logs, rebase state.
                late_allow.push(gitdir.clone());
            } else {
                for d in ["rebase-merge", "rebase-apply", "sequencer"] {
                    late_allow.push(gitdir.join(d));
                }
                p.allow_write_regex.push(format!(
                    "^{}/(index|HEAD|ORIG_HEAD|FETCH_HEAD|MERGE_HEAD|MERGE_MSG|MERGE_MODE|COMMIT_EDITMSG|AUTO_MERGE|REVERT_HEAD|CHERRY_PICK_HEAD)(\\.lock)?$",
                    regex_escape(&gitdir.to_string_lossy())
                ));
            }
            if let Some(b) = &g.branch {
                p.allow_write_regex.push(format!(
                    "^{}/refs/heads/{}(\\.lock)?$",
                    regex_escape(&common.to_string_lossy()),
                    regex_escape(b)
                ));
            }
            p.allow_write_git.extend(late_allow);
            // Layer 4 (final): never writable.
            for rel in ["hooks", "config", "info", "config.worktree"] {
                push_unique(&mut p.deny_write, common.join(rel));
            }
            for rel in ["config.worktree", "commondir", "gitdir", "hooks", "config"] {
                push_unique(&mut p.deny_write, gitdir.join(rel));
            }
        }
        // The checkout root and its `.git` link file: renaming either would let the box swap
        // in a git dir of its choosing for host-side git commands.
        push_unique(&mut p.deny_write_literal, checkout.join(".git"));
        push_unique(&mut p.deny_write_literal, checkout.clone());
        for f in &spec.read_only_files {
            push_unique(&mut p.deny_write, canon(f));
        }
        for f in &spec.protected {
            push_unique(&mut p.deny_write, canon(f));
        }
        // Protected host state stays protected even when it sits inside the checkout
        // ([`check_checkout`] refuses such checkouts; this is the second line).
        for h in p.hidden.clone() {
            if h.starts_with(&checkout) && !private.starts_with(&h) {
                push_unique(&mut p.never_read, h.clone());
                push_unique(&mut p.deny_write, h);
            }
        }
        for n in p.never_read.clone() {
            push_unique(&mut p.deny_write, n);
        }
        for s in &spec.unix_sockets {
            push_unique(&mut p.unix_sockets, canon(s));
        }
        p.network = match &spec.network {
            NetMode::None => None,
            NetMode::Proxy { port, local_ports } => Some((*port, local_ports.clone())),
            NetMode::Open => None,
        };
        p.network_open = matches!(spec.network, NetMode::Open);
        p
    }

    /// Can a contained process read `path` (06 A11.1 `pane.can_see_paths`)? Mirrors the layer
    /// order; system paths outside the deny roots are visible.
    pub fn can_read(&self, path: &Path) -> bool {
        let path = canon(path);
        let under = |roots: &[PathBuf]| roots.iter().any(|r| path.starts_with(r));
        if under(&self.never_read) {
            return false;
        }
        if under(&self.allow_read_late) {
            return true;
        }
        if under(&self.hidden) {
            return false;
        }
        if under(&self.allow_read) {
            return true;
        }
        !under(&self.deny_read)
    }

    /// Roots the client can treat as visible without asking (06 A11.4): the checkout, the
    /// private dir and re-allowed extras.
    pub fn visible_roots(&self) -> Vec<String> {
        let mut v = vec![self.checkout.to_string_lossy().into_owned()];
        for p in &self.allow_read_late {
            let s = p.to_string_lossy().into_owned();
            if !v.contains(&s) {
                v.push(s);
            }
        }
        v
    }
}

/// Home-relative paths that a checkout must never contain (13 §5): the user's shell startup
/// files, tool config and credentials live directly in them.
pub const PROTECTED_HOME: &[&str] = &[
    ".config",
    ".local/state",
    ".local/share",
    "Library",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".bashrc",
    ".bash_profile",
    ".profile",
];

/// Refuse a checkout that would put protected host state inside the sandbox's writable area:
/// `/`, `$HOME` or any ancestor of it, a directory that contains one of `hidden` (Vibeke's
/// runtime/state/config), or one that contains the user's credential/config dirs.
pub fn check_checkout(home: &Path, checkout: &Path, hidden: &[PathBuf]) -> Result<(), String> {
    let co = canon(checkout);
    let home = canon(home);
    let why = |what: &str| {
        Err(format!(
            "refusing to isolate {}: {what}; start the agent in a project directory instead",
            co.display()
        ))
    };
    if co.parent().is_none() {
        return why("it is the filesystem root");
    }
    if home.starts_with(&co) {
        return why("it is (or contains) your home directory");
    }
    for h in hidden {
        let h = canon(h);
        if h.starts_with(&co) {
            return why(&format!("it contains Vibeke's own state ({})", h.display()));
        }
    }
    for rel in PROTECTED_HOME.iter().chain(HOME_NEVER_READ.iter()) {
        let p = home.join(rel);
        if p.starts_with(&co) {
            return why(&format!("it contains {}", p.display()));
        }
    }
    Ok(())
}

/// The per-user temporary directory as the OS reports it (`confstr(_CS_DARWIN_USER_TEMP_DIR)`
/// on macOS), independent of the server's `$TMPDIR`, which may be unset or point elsewhere.
/// `None` on other platforms or when the call fails.
pub fn os_user_temp_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut buf = vec![0u8; 1024];
        // SAFETY: `buf` is a writable buffer of the given length; confstr NUL-terminates.
        let n = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_TEMP_DIR,
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n == 0 || n > buf.len() {
            return None;
        }
        let bytes = &buf[..n - 1];
        if bytes.is_empty() {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Inputs that select a network mode from a profile and a running proxy.
pub fn net_mode(profile: NetworkProfile, proxy_port: Option<u16>, local_ports: &[u16]) -> NetMode {
    match (profile.uses_proxy(), proxy_port) {
        (true, Some(port)) => NetMode::Proxy {
            port,
            local_ports: local_ports.to_vec(),
        },
        _ => NetMode::None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn spec(root: &Path) -> SandboxSpec {
        SandboxSpec {
            home: root.join("home"),
            checkout: root.join("home/code/repo-task"),
            git: Some(GitLayout {
                common_dir: root.join("home/code/repo/.git"),
                git_dir: root.join("home/code/repo/.git/worktrees/repo-task"),
                branch: Some("vk/task".into()),
            }),
            private_dir: root.join("state/sbx/p1"),
            extra_read: vec![root.join("state/inbox")],
            extra_write: vec![],
            read_only_files: vec![root.join("state/sbx/p1/codex/auth.json")],
            hidden: vec![root.join("state"), root.join("run")],
            home_read: Some(vec![".gitconfig".into(), ".cargo/bin".into()]),
            unix_sockets: vec![root.join("state/sbx/p1/broker.sock")],
            network: NetMode::Proxy {
                port: 4100,
                local_ports: vec![20000],
            },
            allow_bind_localhost: true,
            protected: vec![root.join("home/code/repo-task/.githooks")],
            control_dir: Some(root.join("state/sbx/.ctl")),
        }
    }

    #[test]
    fn layers_resolve_visibility() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        for d in [
            "home/.cargo/bin",
            "home/.ssh",
            "home/Desktop",
            "home/code/repo/.git/worktrees/repo-task",
            "home/code/repo-task",
            "home/code/other",
            "state/inbox",
            "state/sbx/p1",
            "run",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("home/.gitconfig"), "").unwrap();
        let p = Policy::from_spec(&spec(&root));
        assert!(p.can_read(&root.join("home/code/repo-task/src/main.rs")));
        assert!(p.can_read(&root.join("home/.gitconfig")));
        assert!(p.can_read(&root.join("home/.cargo/bin/cargo")));
        assert!(p.can_read(&root.join("home/code/repo/.git/HEAD")));
        assert!(p.can_read(&root.join("state/inbox/abc/x.png")));
        assert!(!p.can_read(&root.join("home/.ssh/id_ed25519")));
        assert!(!p.can_read(&root.join("home/Desktop/shot.png")));
        assert!(!p.can_read(&root.join("home/code/other/secret.rs")));
        assert!(!p.can_read(&root.join("state/state.db")));
        assert!(!p.can_read(&root.join("run/vibeke.sock")));
        assert!(p.can_read(Path::new("/usr/bin/env")));
        // The home allowlist only lists what exists.
        assert!(!p.allow_read.iter().any(|x| x.ends_with(".rustup")));
        let roots = p.visible_roots();
        assert!(roots[0].ends_with("repo-task"));
        assert!(roots.iter().any(|r| r.ends_with("state/inbox")));
    }

    #[test]
    fn git_rules_protect_hooks_and_config() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let p = Policy::from_spec(&spec(&root));
        let common = root.join("home/code/repo/.git");
        assert!(p.deny_write.contains(&common.join("hooks")));
        assert!(p.deny_write.contains(&common.join("config")));
        assert!(p.deny_write.contains(&common.join("info")));
        assert!(
            p.deny_write
                .contains(&common.join("worktrees/repo-task/config.worktree"))
        );
        assert!(
            p.deny_write_literal
                .contains(&root.join("home/code/repo-task/.git"))
        );
        assert!(p.deny_write_git.contains(&common));
        assert!(p.allow_write_git.contains(&common.join("objects")));
        assert!(
            p.allow_write_git
                .contains(&common.join("worktrees/repo-task"))
        );
        assert!(
            p.allow_write_regex
                .iter()
                .any(|r| r.ends_with("/refs/heads/vk/task(\\.lock)?$"))
        );
        assert!(p.deny_write.iter().any(|x| x.ends_with("auth.json")));
    }

    #[test]
    fn protected_targets_and_state_inside_the_checkout_are_never_writable() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let mut s = spec(&root);
        // Vibeke state that (somehow) sits inside the checkout.
        s.hidden.push(root.join("home/code/repo-task/.vk-state"));
        let p = Policy::from_spec(&s);
        let co = root.join("home/code/repo-task");
        assert!(p.deny_write.contains(&co.join(".githooks")));
        assert!(p.deny_write.contains(&co.join(".vk-state")));
        assert!(p.never_read.contains(&co.join(".vk-state")));
        assert!(!p.can_read(&co.join(".vk-state/state.db")));
        assert!(p.can_read(&co.join("src/main.rs")));
        // Credential dirs are write-denied too, not just unreadable.
        assert!(p.deny_write.contains(&root.join("home/.ssh")));
        // A hidden root that is an *ancestor* of the checkout (tests under $TMPDIR) is not.
        assert!(!p.deny_write.contains(&root.join("state")));
    }

    #[test]
    fn checkouts_that_contain_protected_state_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let home = root.join("home");
        for d in ["home/code/repo", "home/.ssh", "state"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let hidden = vec![root.join("state"), home.join(".config/vibeke")];
        assert!(check_checkout(&home, &home.join("code/repo"), &hidden).is_ok());
        for bad in [home.clone(), root.clone(), PathBuf::from("/")] {
            let e = check_checkout(&home, &bad, &hidden).unwrap_err();
            assert!(e.contains("refusing"), "{e}");
        }
        // A directory that contains Vibeke's state root.
        let mut h2 = hidden.clone();
        h2.push(home.join("code/repo/.state"));
        assert!(check_checkout(&home, &home.join("code/repo"), &h2).is_err());
        // `~/.config` (shell/tool config) and `~/.ssh` themselves.
        assert!(check_checkout(&home, &home.join(".config"), &hidden).is_err());
        assert!(check_checkout(&home, &home.join(".ssh"), &[]).is_err());
    }

    #[test]
    fn control_dir_and_resolved_credential_dirs_are_never_readable() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        for d in ["home/code/repo-task", "keys", "state/sbx/.ctl/p1"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("keys"), root.join("home/.ssh")).unwrap();
        let mut s = spec(&root);
        // Even a broad grant over the sandbox root does not expose the control dir.
        s.extra_write.push(root.join("state/sbx"));
        let p = Policy::from_spec(&s);
        let ctl = root.join("state/sbx/.ctl");
        assert!(p.never_read.contains(&ctl));
        assert!(p.deny_write.contains(&ctl));
        assert!(!p.can_read(&ctl.join("p1/profile.sb")));
        // `~/.ssh -> keys`: literal and target are both hidden and write-denied.
        assert!(p.never_read.contains(&root.join("home/.ssh")));
        assert!(p.never_read.contains(&root.join("keys")));
        assert!(p.deny_write.contains(&root.join("keys")));
        assert!(!p.can_read(&root.join("keys/id_ed25519")));
    }

    #[test]
    fn git_layout_outside_the_checkout_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let co = root.join("co");
        let main = root.join("main");
        let other = root.join("other");
        for d in [
            "co",
            "main/.git/worktrees/co",
            "other/.git/worktrees/x",
            "x",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        // A plain repo: everything inside the checkout.
        let plain = GitLayout {
            common_dir: co.join(".git"),
            git_dir: co.join(".git"),
            branch: None,
        };
        assert!(plain.verify(&co, &[]).is_ok());
        // A genuine linked worktree: the back-link names this checkout.
        std::fs::write(
            main.join(".git/worktrees/co/gitdir"),
            format!("{}\n", co.join(".git").display()),
        )
        .unwrap();
        let wt = GitLayout {
            common_dir: main.join(".git"),
            git_dir: main.join(".git/worktrees/co"),
            branch: Some("vk/t".into()),
        };
        assert!(wt.verify(&co, &[]).is_ok());
        // A `.git` file pointing straight at another repository.
        let foreign = GitLayout {
            common_dir: other.join(".git"),
            git_dir: other.join(".git"),
            branch: None,
        };
        assert!(foreign.verify(&co, &[]).is_err());
        // … unless that repository is the task's own main repo.
        assert!(foreign.verify(&co, std::slice::from_ref(&other)).is_ok());
        // Another repo's worktree whose back-link names a different checkout.
        std::fs::write(
            other.join(".git/worktrees/x/gitdir"),
            format!("{}\n", root.join("x/.git").display()),
        )
        .unwrap();
        let hijack = GitLayout {
            common_dir: other.join(".git"),
            git_dir: other.join(".git/worktrees/x"),
            branch: None,
        };
        assert!(hijack.verify(&co, &[]).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn os_user_temp_dir_is_a_directory() {
        let d = os_user_temp_dir().expect("confstr temp dir");
        assert!(d.is_absolute() && d.is_dir(), "{}", d.display());
    }

    #[test]
    fn regex_escaping() {
        assert_eq!(regex_escape("/a.b/c+d"), "/a\\.b/c\\+d");
    }
}
