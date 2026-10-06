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
        w
    }
}

fn shell_quote(v: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./:@%+=,-".contains(c);
    if !v.is_empty() && v.chars().all(safe) {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\'', "'\\''"))
    }
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
/// scheme, user, a trailing `.git` and the `host:path` vs `host/path` form).
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
    remote.is_some_and(|r| normalize_remote(r) == normalize_remote(key))
}

fn normalize_remote(u: &str) -> String {
    let u = u.trim();
    let u = u.split_once("://").map_or(u, |(_, rest)| rest);
    let u = u.split_once('@').map_or(u, |(_, rest)| rest);
    let u = u.trim_end_matches('/').trim_end_matches(".git");
    u.replacen(':', "/", 1).to_ascii_lowercase()
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
        self.render_with(input, false)
    }

    /// [`render`](Self::render) for text a shell will parse: every substituted value that
    /// contains anything but `[A-Za-z0-9_./:@%+=,-]` is single-quoted, so a branch name
    /// like `$(cmd)` stays data.
    pub fn render_shell(&self, input: &str) -> String {
        self.render_with(input, true)
    }

    fn render_with(&self, input: &str, shell: bool) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            if !out.ends_with('$')
                && let Some(j) = after.find('}')
                && let Some(v) = self.get(&after[..j])
            {
                if shell {
                    out.push_str(&shell_quote(&v));
                } else {
                    out.push_str(&v);
                }
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
    /// invalid names dropped, port offsets outside the lease dropped.
    pub fn env_pairs(&self, tf: &TaskFile) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = Vec::new();
        if let Some(base) = self.port_base {
            let end = self.port_end.unwrap_or(base);
            for (k, off) in &tf.ports.env {
                let p = u32::from(base) + u32::from(*off);
                if valid_env_name(k) && p <= u32::from(end) {
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
    fn shell_rendering_quotes_values_not_the_template() {
        let mut v = vars();
        v.branch = "x$(touch pwned)'y".into();
        assert_eq!(
            v.render_shell("echo {branch} {slug} > {worktree}/out"),
            "echo 'x$(touch pwned)'\\''y' fix-login > /w/out"
        );
        assert_eq!(v.render("{branch}"), "x$(touch pwned)'y");
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
