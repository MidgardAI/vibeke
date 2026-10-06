//! `.vibeke/task.toml` (05 §5): files, deps, setup, ports and env of a task
//! workspace, user-level overrides, and `{variable}` templating.
//!
//! Parsing is lenient: unknown tables (`[previews]` is read elsewhere) are
//! ignored. Everything that makes the repo *run code* (`deps.install`,
//! `setup.script`, `setup.run`, and `[env]`, which can alter how every pane
//! behaves) is repo-provided and therefore gated on repo trust by the caller
//! (09 §4); [`TaskFile::commands`] lists exactly what would run so it can be
//! shown first.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Repo-relative path of the task file.
pub const TASK_FILE: &str = ".vibeke/task.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesSpec {
    /// Copied (never symlinked: secrets must not be editable through the task).
    pub copy: Vec<String>,
    /// Symlinked to the source checkout.
    pub link: Vec<String>,
    /// Copy-on-write cloned where possible, else copied.
    pub clone: Vec<String>,
    pub ignore_missing: Option<bool>,
}

impl FilesSpec {
    pub fn ignore_missing(&self) -> bool {
        self.ignore_missing.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DepsStrategy {
    #[default]
    Auto,
    Clone,
    Install,
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DepsSpec {
    pub strategy: Option<DepsStrategy>,
    pub install: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupSpec {
    pub script: Option<String>,
    pub run: Vec<String>,
    /// `"10m"`, `"90s"`, `"1h"`.
    pub timeout: Option<String>,
    /// Start agents even when setup failed (default false).
    pub start_agents_on_failure: Option<bool>,
    /// Start agents immediately, with a note that setup is still running.
    pub parallel_agent: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PortsSpec {
    /// Size of the leased range.
    pub count: Option<u16>,
    /// Env name to offset into the range.
    pub env: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskFile {
    pub files: FilesSpec,
    pub deps: DepsSpec,
    pub setup: SetupSpec,
    pub ports: PortsSpec,
    pub env: BTreeMap<String, String>,
}

/// One command the task would run, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedCommand {
    /// `deps.install` | `setup.run` | `setup.script`
    pub source: &'static str,
    pub command: String,
}

impl TaskFile {
    pub fn parse(text: &str) -> Result<TaskFile> {
        toml::from_str(text).map_err(|e| Error::Config(format!("{TASK_FILE}: {e}")))
    }

    /// Read `<root>/.vibeke/task.toml`; `Ok(None)` when absent.
    pub fn load(root: &Path) -> Result<Option<TaskFile>> {
        match std::fs::read_to_string(root.join(TASK_FILE)) {
            Ok(t) => Ok(Some(Self::parse(&t)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Apply a user-level override: lists and scalars set in `o` replace the
    /// repo's, `env`/`ports.env` merge per key (override wins).
    pub fn apply(&mut self, o: &TaskFile) {
        fn list(dst: &mut Vec<String>, src: &[String]) {
            if !src.is_empty() {
                *dst = src.to_vec();
            }
        }
        list(&mut self.files.copy, &o.files.copy);
        list(&mut self.files.link, &o.files.link);
        list(&mut self.files.clone, &o.files.clone);
        if o.files.ignore_missing.is_some() {
            self.files.ignore_missing = o.files.ignore_missing;
        }
        if o.deps.strategy.is_some() {
            self.deps.strategy = o.deps.strategy;
        }
        if o.deps.install.is_some() {
            self.deps.install = o.deps.install.clone();
        }
        if o.setup.script.is_some() {
            self.setup.script = o.setup.script.clone();
        }
        list(&mut self.setup.run, &o.setup.run);
        if o.setup.timeout.is_some() {
            self.setup.timeout = o.setup.timeout.clone();
        }
        if o.setup.start_agents_on_failure.is_some() {
            self.setup.start_agents_on_failure = o.setup.start_agents_on_failure;
        }
        if o.setup.parallel_agent.is_some() {
            self.setup.parallel_agent = o.setup.parallel_agent;
        }
        if o.ports.count.is_some() {
            self.ports.count = o.ports.count;
        }
        self.ports.env.extend(o.ports.env.clone());
        self.env.extend(o.env.clone());
    }

    /// Commands this file would run (templates unrendered), in run order:
    /// the dependency install (passed in because `auto` decides it), then
    /// `setup.run`, then `setup.script`.
    pub fn commands(&self, install: Option<&str>) -> Vec<PlannedCommand> {
        let mut v = Vec::new();
        if let Some(i) = install.filter(|s| !s.trim().is_empty()) {
            v.push(PlannedCommand {
                source: "deps.install",
                command: i.to_string(),
            });
        }
        for r in &self.setup.run {
            v.push(PlannedCommand {
                source: "setup.run",
                command: r.clone(),
            });
        }
        if let Some(s) = self.setup.script.as_deref().filter(|s| !s.is_empty()) {
            v.push(PlannedCommand {
                source: "setup.script",
                command: s.to_string(),
            });
        }
        v
    }

    pub fn setup_timeout(&self) -> Option<Duration> {
        self.setup.timeout.as_deref().and_then(parse_duration)
    }

    /// Validation problems worth surfacing (never fatal).
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if let Some(t) = &self.setup.timeout
            && parse_duration(t).is_none()
        {
            w.push(format!(
                "setup.timeout {t:?} is not a duration like \"10m\""
            ));
        }
        if let Some(c) = self.ports.count {
            for (k, off) in &self.ports.env {
                if *off >= c {
                    w.push(format!("ports.env.{k} offset {off} is outside count {c}"));
                }
            }
        }
        for k in self.env.keys().chain(self.ports.env.keys()) {
            if !valid_env_name(k) {
                w.push(format!("env name {k:?} is not a valid shell identifier"));
            }
        }
        for k in self.ports.env.keys() {
            if valid_env_name(k) && !trusted_port_env_name(k) {
                w.push(format!(
                    "ports.env.{k} is ignored: port env names must contain PORT and not be a shell or loader variable"
                ));
            }
        }
        w
    }
}

/// Names a repo's `[ports] env` may never set, trusted or not: they change how
/// a shell, loader or interpreter starts (and the value is a number the repo
/// can name a directory after).
const DENIED_ENV: &[&str] = &[
    "ZDOTDIR",
    "BASH_ENV",
    "ENV",
    "PATH",
    "PROMPT_COMMAND",
    "PS1",
    "PS2",
    "PS4",
    "IFS",
    "HOME",
    "SHELL",
    "SHELLOPTS",
    "BASHOPTS",
    "CDPATH",
    "FPATH",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5LIB",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "INPUTRC",
    "TERMINFO",
];
const DENIED_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_", "BASH_FUNC_", "GIT_", "VIBEKE_", "ZSH_"];

/// Never allowed in `[ports] env` (see [`DENIED_ENV`]).
pub fn denied_port_env_name(k: &str) -> bool {
    let u = k.to_ascii_uppercase();
    DENIED_ENV.contains(&u.as_str()) || DENIED_ENV_PREFIXES.iter().any(|p| u.starts_with(p))
}

fn upper_ident(k: &str) -> bool {
    k.starts_with(|c: char| c.is_ascii_uppercase())
        && k.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// May a **trusted** `[ports] env` (or the user's own) set `k`? An upper-case
/// name containing `PORT` (`^[A-Z][A-Z0-9_]*PORT[A-Z0-9_]*$`), never a denied one.
pub fn trusted_port_env_name(k: &str) -> bool {
    upper_ident(k) && k.contains("PORT") && !denied_port_env_name(k)
}

/// May an **untrusted** repo's `[ports] env` set `k`? Only `PORT` and `*_PORT`.
pub fn untrusted_port_env_name(k: &str) -> bool {
    (k == "PORT" || (upper_ident(k) && k.ends_with("_PORT") && k.len() > 5))
        && !denied_port_env_name(k)
}

/// `ports.env` reduced to what may be exported: names `declared` by the user
/// (their own override) pass unless denied; the rest must pass
/// [`trusted_port_env_name`], or [`untrusted_port_env_name`] when the repo is
/// not trusted.
pub fn filter_port_env(
    env: &BTreeMap<String, u16>,
    declared: &BTreeMap<String, u16>,
    trusted: bool,
) -> BTreeMap<String, u16> {
    env.iter()
        .filter(|(k, off)| {
            if declared.get(*k) == Some(off) {
                valid_env_name(k) && !denied_port_env_name(k)
            } else if trusted {
                trusted_port_env_name(k)
            } else {
                untrusted_port_env_name(k)
            }
        })
        .map(|(k, v)| (k.clone(), *v))
        .collect()
}

pub fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

/// `"90s"`, `"10m"`, `"2h"`, `"1d"` or bare seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: u64 = num.trim().parse().ok()?;
    (n > 0).then(|| Duration::from_secs(n.saturating_mul(mult)))
}

/// Does a `[tasks.repos."<key>"]` key name this repo? The key is either a
/// path (matched against the canonical repo root; a leading `~/` is
/// expanded with `home`) or a remote URL (matched against `origin`, ignoring
/// scheme, user, port, a trailing `.git` and the `host:path` vs `host/path`
/// form). URLs are parsed, not pattern-matched: userinfo is only what precedes
/// the host, so `https://evil.example/x@github.com/org/repo` is host
/// `evil.example` and never matches `github.com/org/repo`.
pub fn repo_key_matches(
    key: &str,
    repo_root: &Path,
    remote: Option<&str>,
    home: Option<&Path>,
) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    if key.starts_with('/') || key.starts_with("~/") || key.starts_with("./") {
        let p = match (key.strip_prefix("~/"), home) {
            (Some(rest), Some(h)) => h.join(rest),
            _ => std::path::PathBuf::from(key),
        };
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        return canon(&p) == canon(repo_root);
    }
    let Some(k) = parse_remote(key) else {
        return false;
    };
    remote.and_then(parse_remote).is_some_and(|r| r == k)
}

/// `(host, path)` of a remote URL, lowercased, without userinfo, port, slashes
/// at either end of the path or a trailing `.git`. Forms: `scheme://[user@]host[:port]/path`,
/// scp-like `[user@]host:path`, and `host/path`. `None` when there is no host
/// or no path, or when the host is not a plain hostname (so a stray `@`, `/`
/// or `:` can never shift which part is the host).
pub fn parse_remote(u: &str) -> Option<(String, String)> {
    let u = u.trim();
    let (authority, path) = if let Some((scheme, rest)) = u.split_once("://") {
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        (&rest[..end], &rest[end..])
    } else {
        // scp-like when a `:` comes before any `/`; else `host/path`.
        match (u.find(':'), u.find('/')) {
            (Some(c), s) if s.is_none_or(|s| c < s) => (&u[..c], &u[c + 1..]),
            (_, Some(s)) => (&u[..s], &u[s..]),
            _ => return None,
        }
    };
    // Userinfo is everything up to the last `@` of the authority only.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match hostport.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
        Some((h, "")) => h,
        _ => hostport,
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if !host_ok {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), path.to_ascii_lowercase()))
}

/// Values for `{variable}` templating in `env`, `install` and `setup`.
#[derive(Debug, Clone, Default)]
pub struct TemplateVars {
    pub slug: String,
    pub branch: String,
    pub task: String,
    pub port_base: Option<u16>,
    pub port_end: Option<u16>,
    pub repo_root: String,
    pub worktree: String,
    pub source_root: String,
}

impl TemplateVars {
    fn get(&self, name: &str) -> Option<String> {
        Some(match name {
            "slug" => self.slug.clone(),
            "slug_underscored" => self.slug.replace('-', "_"),
            "branch" => self.branch.clone(),
            "task" => self.task.clone(),
            "port" | "port_base" => self.port_base?.to_string(),
            "port_end" => self.port_end?.to_string(),
            "repo_root" => self.repo_root.clone(),
            "worktree" => self.worktree.clone(),
            "source_root" => self.source_root.clone(),
            _ => return None,
        })
    }

    /// Replace `{name}` with its value. Unknown names and unbalanced braces
    /// are left as written, and a `{` right after a `$` is never a variable,
    /// so shell `${VAR}` survives.
    pub fn render(&self, input: &str) -> String {
        self.render_plain(input)
    }

    /// The environment variable carrying `{name}` in shell commands.
    fn env_name(name: &str) -> Option<&'static str> {
        Some(match name {
            "slug" => "VIBEKE_TASK_SLUG",
            "slug_underscored" => "VIBEKE_TASK_SLUG_UNDERSCORED",
            "branch" => "VIBEKE_BRANCH",
            "task" => "VIBEKE_TASK_ID",
            "port" | "port_base" => "VIBEKE_PORT_BASE",
            "port_end" => "VIBEKE_PORT_END",
            "repo_root" => "VIBEKE_REPO_ROOT",
            "worktree" => "VIBEKE_WORKTREE",
            "source_root" => "VIBEKE_SOURCE_ROOT",
            _ => return None,
        })
    }

    /// The variables [`render_shell`](Self::render_shell) references, to export to the
    /// shell that runs the rendered commands (only the ones with a value).
    pub fn shell_env(&self) -> Vec<(String, String)> {
        [
            "slug",
            "slug_underscored",
            "branch",
            "task",
            "port_base",
            "port_end",
            "repo_root",
            "worktree",
            "source_root",
        ]
        .iter()
        .filter_map(|n| Some((Self::env_name(n)?.to_string(), self.get(n)?)))
        .collect()
    }

    /// [`render`](Self::render) for text a shell will parse. Values are **never** pasted
    /// into the command: `{branch}` becomes a reference to `$VIBEKE_BRANCH` (see
    /// [`shell_env`](Self::shell_env)), written for the quoting context it appears in:
    /// `"${V}"` unquoted, `${V}` inside double quotes, and `'"${V}"'` inside single quotes
    /// (close, expand, reopen). The shell expands a variable once and never re-parses the
    /// result, so a branch named `$(cmd)` or `'; cmd; '` stays data in every context.
    pub fn render_shell(&self, input: &str) -> String {
        #[derive(Clone, Copy, PartialEq)]
        enum Q {
            None,
            Single,
            AnsiC,
            Double,
        }
        let mut out = String::with_capacity(input.len());
        let mut q = Q::None;
        // Command substitutions (`$(…)`, backticks) start a fresh quoting context; the stack
        // holds the enclosing one with its paren depth and whether a backtick opened it.
        let mut stack: Vec<(Q, u32, bool)> = Vec::new();
        let mut depth = 0u32;
        let b = input.as_bytes();
        let mut i = 0;
        while i < input.len() {
            let c = b[i];
            if c == b'{'
                && !out.ends_with('$')
                && let Some(j) = input[i + 1..].find('}')
                && let Some(var) = Self::env_name(&input[i + 1..i + 1 + j])
                && self.get(&input[i + 1..i + 1 + j]).is_some()
            {
                match q {
                    Q::None => out.push_str(&format!("\"${{{var}}}\"")),
                    Q::Double => out.push_str(&format!("${{{var}}}")),
                    Q::Single => out.push_str(&format!("'\"${{{var}}}\"'")),
                    Q::AnsiC => out.push_str(&format!("'\"${{{var}}}\"$'")),
                }
                i += j + 2;
                continue;
            }
            // Track quoting (POSIX quotes, backslash escapes and bash `$'...'`).
            let ch = input[i..].chars().next().unwrap_or('\0');
            let len = ch.len_utf8();
            match (q, c) {
                (Q::None, b'\\') | (Q::Double, b'\\') | (Q::AnsiC, b'\\') => {
                    let next = input[i + 1..].chars().next().map_or(0, char::len_utf8);
                    out.push_str(&input[i..i + 1 + next]);
                    i += 1 + next;
                    continue;
                }
                (Q::None | Q::Double, b'(') if out.ends_with('$') => {
                    stack.push((q, depth, false));
                    (q, depth) = (Q::None, 0);
                }
                (Q::None, b'(') => depth += 1,
                (Q::None, b')') if depth > 0 => depth -= 1,
                (Q::None, b')') if stack.last().is_some_and(|t| !t.2) => {
                    let (pq, pd, _) = stack.pop().unwrap_or((Q::None, 0, false));
                    (q, depth) = (pq, pd);
                }
                (Q::None, b'`') if stack.last().is_some_and(|t| t.2) => {
                    let (pq, pd, _) = stack.pop().unwrap_or((Q::None, 0, false));
                    (q, depth) = (pq, pd);
                }
                (Q::None | Q::Double, b'`') => {
                    stack.push((q, depth, true));
                    (q, depth) = (Q::None, 0);
                }
                (Q::None, b'\'') if out.ends_with('$') => q = Q::AnsiC,
                (Q::None, b'\'') => q = Q::Single,
                (Q::Single, b'\'') | (Q::AnsiC, b'\'') => q = Q::None,
                (Q::None, b'"') => q = Q::Double,
                (Q::Double, b'"') => q = Q::None,
                _ => {}
            }
            out.push_str(&input[i..i + len]);
            i += len;
        }
        out
    }

    fn render_plain(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            if !out.ends_with('$')
                && let Some(j) = after.find('}')
                && let Some(v) = self.get(&after[..j])
            {
                out.push_str(&v);
                rest = &after[j + 1..];
                continue;
            }
            out.push('{');
            rest = after;
        }
        out.push_str(rest);
        out
    }

    /// `[ports] env` plus `[env]` as ready-to-export pairs: templated values,
    /// invalid names dropped, port offsets outside the lease dropped. Port env
    /// names that are denied ([`denied_port_env_name`]) are always dropped;
    /// callers narrow `tf.ports.env` with [`filter_port_env`] first.
    pub fn env_pairs(&self, tf: &TaskFile) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = Vec::new();
        if let Some(base) = self.port_base {
            let end = self.port_end.unwrap_or(base);
            for (k, off) in &tf.ports.env {
                let p = u32::from(base) + u32::from(*off);
                if valid_env_name(k) && !denied_port_env_name(k) && p <= u32::from(end) {
                    v.push((k.clone(), p.to_string()));
                }
            }
        }
        for (k, val) in &tf.env {
            if valid_env_name(k) {
                v.retain(|(n, _)| n != k);
                v.push((k.clone(), self.render(val)));
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        TemplateVars {
            slug: "fix-login".into(),
            branch: "me/fix-login".into(),
            task: "k7".into(),
            port_base: Some(20010),
            port_end: Some(20019),
            repo_root: "/r".into(),
            worktree: "/w".into(),
            source_root: "/r".into(),
        }
    }

    #[test]
    fn renders_known_variables_only() {
        let v = vars();
        assert_eq!(
            v.render("postgres://x/app_{slug_underscored}?p={port}-{port_end}"),
            "postgres://x/app_fix_login?p=20010-20019"
        );
        assert_eq!(
            v.render("${HOME}/{nope}/{branch}"),
            "${HOME}/{nope}/me/fix-login"
        );
        assert_eq!(v.render("{unbalanced {slug"), "{unbalanced {slug");
        assert_eq!(v.render("{{slug}}"), "{fix-login}");
        let mut none = vars();
        none.port_base = None;
        assert_eq!(none.render("{port}"), "{port}");
    }

    #[test]
    fn shell_rendering_references_env_vars_per_quoting_context() {
        let mut v = vars();
        v.branch = "x$(touch pwned)'y".into();
        assert_eq!(
            v.render_shell("echo {branch} {slug} > {worktree}/out"),
            "echo \"${VIBEKE_BRANCH}\" \"${VIBEKE_TASK_SLUG}\" > \"${VIBEKE_WORKTREE}\"/out"
        );
        assert_eq!(
            v.render_shell(r#"echo "b={branch}" 'b={branch}' $'b={branch}' \'{task}"#),
            r#"echo "b=${VIBEKE_BRANCH}" 'b='"${VIBEKE_BRANCH}"'' $'b='"${VIBEKE_BRANCH}"$'' \'"${VIBEKE_TASK_ID}""#
        );
        // Shell `${VAR}`, unknown names and unset ports are left as written.
        let mut none = vars();
        none.port_base = None;
        assert_eq!(
            none.render_shell("${HOME} {nope} {port}"),
            "${HOME} {nope} {port}"
        );
        assert!(
            !none
                .shell_env()
                .iter()
                .any(|(k, _)| k == "VIBEKE_PORT_BASE")
        );
        assert_eq!(v.render("{branch}"), "x$(touch pwned)'y");
    }

    /// Hostile (git-valid) branch names print literally from unquoted, single- and
    /// double-quoted placeholders, run through a real `sh`.
    #[test]
    fn shell_templates_never_execute_branch_names() {
        let dir = tempfile::tempdir().unwrap();
        let hostile = [
            "feature/$(printf${IFS}P1_INJECTED)",
            "x'$(touch pwned1)'y",
            "a\"$(touch pwned2)\"b",
            "c`touch pwned3`d",
            "e';touch pwned4;'f",
            "g\\\"$(touch pwned5)",
            "h${IFS}$HOME",
        ];
        let templates = [
            "printf '%s\\n' {branch}",
            "printf '%s\\n' \"{branch}\"",
            "printf '%s\\n' '{branch}'",
            "printf '%s\\n' \"pre-{branch}-post\" | sed 's/^pre-//; s/-post$//'",
            "printf '%s\\n' 'pre-{branch}-post' | sed 's/^pre-//; s/-post$//'",
            "printf '%s\\n' \"$(printf '%s' \"{branch}\")\"",
        ];
        for b in hostile {
            let mut v = vars();
            v.branch = b.to_string();
            for t in templates {
                let cmd = v.render_shell(t);
                let out = std::process::Command::new("sh")
                    .args(["-c", &cmd])
                    .current_dir(dir.path())
                    .envs(v.shell_env())
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{t} / {b}: {cmd}");
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout).trim_end_matches('\n'),
                    b,
                    "template {t:?} rendered as {cmd:?}"
                );
            }
        }
        let made: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(made.is_empty(), "a branch name ran a command: {made:?}");
    }

    #[test]
    fn hostile_remote_urls_never_match() {
        let root = Path::new("/nonexistent/repo");
        let key = "https://github.com/org/repo.git";
        for r in [
            "https://evil.example/path@github.com/org/repo.git",
            "https://evil.example/@github.com/org/repo",
            "ssh://evil.example/x@github.com:org/repo.git",
            "evil.example:x@github.com/org/repo.git",
            "evil.example/x@github.com:org/repo.git",
            "https://github.com.evil.example/org/repo.git",
            "https://evil.example?@github.com/org/repo.git",
            "https://evil.example#@github.com/org/repo.git",
            "https://github.com/org/repo.git/../../evil/x",
        ] {
            assert!(!repo_key_matches(key, root, Some(r), None), "{r}");
            assert!(!repo_key_matches(r, root, Some(key), None), "{r}");
        }
        // Real forms still match: userinfo before the host, ports, scp-like.
        for r in [
            "https://user:tok@github.com/org/repo.git",
            "ssh://git@github.com:22/org/repo.git",
            "git@github.com:org/repo.git",
            "github.com/Org/Repo/",
        ] {
            assert!(repo_key_matches(key, root, Some(r), None), "{r}");
        }
        assert_eq!(
            parse_remote("https://evil.example/path@github.com/org/repo.git"),
            Some(("evil.example".into(), "path@github.com/org/repo".into()))
        );
        assert_eq!(parse_remote("https://github.com"), None);
        assert_eq!(parse_remote("https://bad host/x"), None);
    }

    #[test]
    fn port_env_names_are_allowlisted() {
        for k in [
            "ZDOTDIR",
            "BASH_ENV",
            "ENV",
            "PATH",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "PROMPT_COMMAND",
            "LD_PORT",
            "VIBEKE_PORT",
        ] {
            assert!(!trusted_port_env_name(k), "{k}");
            assert!(!untrusted_port_env_name(k), "{k}");
        }
        assert!(untrusted_port_env_name("PORT"));
        assert!(untrusted_port_env_name("API_PORT"));
        assert!(!untrusted_port_env_name("_PORT"));
        assert!(!untrusted_port_env_name("PORTS_DIR"));
        assert!(trusted_port_env_name("VITE_PORT_HMR"));
        assert!(!untrusted_port_env_name("VITE_PORT_HMR"));
        assert!(!trusted_port_env_name("api_port"));
        let env: BTreeMap<String, u16> = [
            ("ZDOTDIR", 0),
            ("PORT", 0),
            ("API_PORT", 1),
            ("HMR_PORT_WS", 2),
            ("CUSTOM", 3),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let none = BTreeMap::new();
        let keys = |m: BTreeMap<String, u16>| m.into_keys().collect::<Vec<_>>();
        assert_eq!(
            keys(filter_port_env(&env, &none, false)),
            ["API_PORT", "PORT"]
        );
        assert_eq!(
            keys(filter_port_env(&env, &none, true)),
            ["API_PORT", "HMR_PORT_WS", "PORT"]
        );
        // A name the user declared passes; a denied one never does.
        let declared: BTreeMap<String, u16> =
            [("CUSTOM".to_string(), 3), ("ZDOTDIR".to_string(), 0)]
                .into_iter()
                .collect();
        assert_eq!(
            keys(filter_port_env(&env, &declared, false)),
            ["API_PORT", "CUSTOM", "PORT"]
        );
        // env_pairs drops denied names even if a caller forgot to filter.
        let tf = TaskFile::parse("[ports]\nenv={ZDOTDIR=0, PORT=0}\n").unwrap();
        let e = vars().env_pairs(&tf);
        assert!(!e.iter().any(|(k, _)| k == "ZDOTDIR"));
        assert!(tf.warnings().iter().any(|w| w.contains("ZDOTDIR")));
    }

    #[test]
    fn parses_spec_example_and_ignores_unknown_tables() {
        let tf = TaskFile::parse(
            r#"
[files]
copy = [".env"]
link = ["data/big"]
clone = ["node_modules", "apps/*/node_modules"]
ignore_missing = false
[deps]
strategy = "clone"
install = "pnpm install"
[setup]
run = ["pnpm db:migrate"]
timeout = "10m"
parallel_agent = true
[ports]
count = 10
env = { PORT = 0, API_PORT = 1 }
[env]
A = "1"
[previews]
web = { port_env = "PORT" }
"#,
        )
        .unwrap();
        assert_eq!(tf.files.clone.len(), 2);
        assert!(!tf.files.ignore_missing());
        assert_eq!(tf.deps.strategy, Some(DepsStrategy::Clone));
        assert_eq!(tf.setup_timeout(), Some(Duration::from_secs(600)));
        assert_eq!(tf.ports.env["API_PORT"], 1);
        let c = tf.commands(Some("pnpm install"));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].source, "deps.install");
        assert!(TaskFile::parse("[files\n").is_err());
    }

    #[test]
    fn override_replaces_lists_and_merges_env() {
        let mut base =
            TaskFile::parse("[files]\ncopy=[\".env\"]\n[env]\nA=\"1\"\nB=\"2\"\n").unwrap();
        let over =
            TaskFile::parse("[files]\ncopy=[\".env.x\"]\n[env]\nB=\"3\"\n[setup]\nrun=[\"x\"]\n")
                .unwrap();
        base.apply(&over);
        assert_eq!(base.files.copy, vec![".env.x"]);
        assert_eq!(base.env["A"], "1");
        assert_eq!(base.env["B"], "3");
        assert_eq!(base.setup.run, vec!["x"]);
    }

    #[test]
    fn env_pairs_template_and_bound_ports() {
        let tf = TaskFile::parse(
            "[ports]\nenv={PORT=0, FAR=50, \"bad name\"=1}\n[env]\nDB=\"app_{slug_underscored}\"\nPORT=\"override\"\n",
        )
        .unwrap();
        let e = vars().env_pairs(&tf);
        assert!(e.contains(&("DB".into(), "app_fix_login".into())));
        assert!(e.contains(&("PORT".into(), "override".into())));
        assert!(!e.iter().any(|(k, _)| k == "FAR" || k == "bad name"));
        assert!(!tf.warnings().is_empty());
    }

    #[test]
    fn repo_keys_match_path_and_remote_forms() {
        let root = Path::new("/nonexistent/repo");
        assert!(repo_key_matches("/nonexistent/repo", root, None, None));
        assert!(repo_key_matches(
            "~/repo",
            Path::new("/h/repo"),
            None,
            Some(Path::new("/h"))
        ));
        assert!(!repo_key_matches("/other", root, None, None));
        let r = Some("git@github.com:Acme/App.git");
        assert!(repo_key_matches(
            "https://github.com/acme/app",
            root,
            r,
            None
        ));
        assert!(repo_key_matches("github.com/acme/app.git", root, r, None));
        assert!(!repo_key_matches("github.com/acme/other", root, r, None));
        assert!(!repo_key_matches("github.com/acme/app", root, None, None));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("soon"), None);
    }
}
