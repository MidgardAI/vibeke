//! Repo-local configuration, `.vibeke/config.toml` in a repository root (08 §11.1, 09 §4).
//!
//! A repo file may set `[tasks]`, `[preview]`, `[[policy.rule]]` (tighten only: `deny`/`ask`;
//! `allow` rules are dropped with a warning, 09 §4 rule 2) and `[[keys.command]]`. Every other
//! section is ignored with a warning. Nothing from it applies until the repo's `.vibeke/` tree is
//! trusted (`vibeke trust` → `policy.trust`, which records a digest of the tree, so any edit needs
//! a new review). This module only parses and merges; the trust decision is the caller's.

use std::path::{Path, PathBuf};

use crate::types::{Config, KeyCommand, PolicyEffect, PolicyRule};

/// The repo-local file, relative to a repository root.
pub const REPO_CONFIG: &str = ".vibeke/config.toml";

/// Top-level sections a repo file may set.
pub const REPO_SECTIONS: &[&str] = &["tasks", "preview", "policy", "keys"];

/// A parsed repo-local config.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoConfig {
    /// Repository root (the directory holding `.vibeke/`).
    pub root: PathBuf,
    /// The file's text, shown when the user reviews it.
    pub text: String,
    pub tasks: Option<toml::Table>,
    pub preview: Option<toml::Table>,
    /// `deny`/`ask` rules only.
    pub policy_rules: Vec<PolicyRule>,
    pub commands: Vec<KeyCommand>,
    /// Ignored sections and dropped rules, for the review screen.
    pub warnings: Vec<String>,
}

/// The nearest repo-local config at or above `dir`: the first ancestor with
/// `.vibeke/config.toml`. The walk stops at a git root (a directory with `.git`) or `$HOME`.
pub fn find(dir: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for d in dir.ancestors() {
        if d.join(REPO_CONFIG).is_file() {
            return Some(d.to_path_buf());
        }
        if d.join(".git").exists() || home.as_deref() == Some(d) {
            return None;
        }
    }
    None
}

/// Parse a repo file's text; `root` is the repository root it belongs to.
pub fn parse(text: &str, root: &Path) -> Result<RepoConfig, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("{REPO_CONFIG}: {}", e.message()))?;
    let mut out = RepoConfig {
        root: root.to_path_buf(),
        text: text.to_string(),
        ..Default::default()
    };
    for (k, v) in &table {
        match (k.as_str(), v) {
            ("tasks", toml::Value::Table(t)) => out.tasks = Some(t.clone()),
            ("preview", toml::Value::Table(t)) => out.preview = Some(t.clone()),
            ("policy", toml::Value::Table(t)) => {
                for (pk, pv) in t {
                    if pk != "rule" {
                        out.warnings
                            .push(format!("policy.{pk}: ignored (only policy.rule)"));
                        continue;
                    }
                    let rules: Vec<PolicyRule> = pv
                        .clone()
                        .try_into()
                        .map_err(|e: toml::de::Error| format!("policy.rule: {}", e.message()))?;
                    for (i, r) in rules.into_iter().enumerate() {
                        if r.effect == PolicyEffect::Allow {
                            out.warnings.push(format!(
                                "policy.rule[{i}]: allow rules from a repo are ignored (repo policy can only tighten)"
                            ));
                        } else {
                            out.policy_rules.push(r);
                        }
                    }
                }
            }
            ("keys", toml::Value::Table(t)) => {
                for (kk, kv) in t {
                    if kk != "command" {
                        out.warnings
                            .push(format!("keys.{kk}: ignored (only keys.command)"));
                        continue;
                    }
                    out.commands = kv
                        .clone()
                        .try_into()
                        .map_err(|e: toml::de::Error| format!("keys.command: {}", e.message()))?;
                }
            }
            (k, _) => out.warnings.push(format!(
                "{k}: ignored (a repo may set {})",
                REPO_SECTIONS.join(", ")
            )),
        }
    }
    Ok(out)
}

/// Read and parse `<root>/.vibeke/config.toml`; `None` when there is no file.
pub fn load(root: &Path) -> Option<Result<RepoConfig, String>> {
    let text = std::fs::read_to_string(root.join(REPO_CONFIG)).ok()?;
    Some(parse(&text, root))
}

fn merge(base: &mut toml::Table, over: &toml::Table) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

impl Config {
    /// This config with a (trusted) repo config layered on top: `[tasks]` and `[preview]`
    /// keys override, repo rules come after the user's (the user's rules match first), repo
    /// commands are appended. Invalid repo values keep the user's config and return the error.
    pub fn with_repo(&self, repo: &RepoConfig) -> Result<Config, String> {
        let mut v = self.to_value();
        let toml::Value::Table(t) = &mut v else {
            return Err("config is not a table".into());
        };
        for (name, over) in [("tasks", &repo.tasks), ("preview", &repo.preview)] {
            if let Some(o) = over {
                let entry = t
                    .entry(name.to_string())
                    .or_insert_with(|| toml::Value::Table(Default::default()));
                if let toml::Value::Table(b) = entry {
                    merge(b, o);
                }
            }
        }
        let text = toml::to_string(&v).map_err(|e| e.to_string())?;
        let (mut c, _) = Config::parse(&text, Path::new(REPO_CONFIG)).map_err(|e| e.to_string())?;
        c.policy.rule.extend(repo.policy_rules.iter().cloned());
        c.keys.command.extend(repo.commands.iter().cloned());
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
[tasks]
branch_template = "repo/{slug}"
port_block = 20
[preview]
default_viewport = "800x600"
[[policy.rule]]
match = { tool = "Bash", command_regex = "^rm " }
effect = "deny"
[[policy.rule]]
match = { tool = "Bash" }
effect = "allow"
[[keys.command]]
key = "prefix+alt+t"
type = "popup"
command = "make test"
title = "tests"
[keys]
prefix = "ctrl+a"
[ui]
animate = false
"#;

    #[test]
    fn parses_allowed_sections_and_warns_about_the_rest() {
        let r = parse(SRC, Path::new("/r")).unwrap();
        assert_eq!(
            r.tasks.as_ref().unwrap()["branch_template"].as_str(),
            Some("repo/{slug}")
        );
        assert_eq!(r.policy_rules.len(), 1, "allow dropped");
        assert_eq!(r.policy_rules[0].effect, PolicyEffect::Deny);
        assert_eq!(r.commands.len(), 1);
        assert_eq!(r.commands[0].title.as_deref(), Some("tests"));
        let w = r.warnings.join("\n");
        assert!(w.contains("allow rules from a repo are ignored"), "{w}");
        assert!(w.contains("keys.prefix: ignored"), "{w}");
        assert!(w.contains("ui: ignored"), "{w}");
    }

    #[test]
    fn merges_over_the_user_config() {
        let r = parse(SRC, Path::new("/r")).unwrap();
        let mut user = Config::default();
        user.tasks.default_agent = "codex".into();
        let c = user.with_repo(&r).unwrap();
        assert_eq!(c.tasks.branch_template, "repo/{slug}");
        assert_eq!(c.tasks.port_block, 20);
        assert_eq!(c.tasks.default_agent, "codex", "user keys kept");
        assert_eq!(c.keys.prefix, "ctrl+b", "repo can't touch keys.prefix");
        assert!(c.ui.animate, "repo can't touch ui");
        assert_eq!(c.keys.command.len(), 1);
        assert_eq!(c.policy.rule.len(), 1);
    }

    #[test]
    fn invalid_repo_values_are_errors() {
        let r = parse("[tasks]\nport_block = \"many\"\n", Path::new("/r")).unwrap();
        assert!(Config::default().with_repo(&r).is_err());
        assert!(parse("[tasks\n", Path::new("/r")).is_err());
    }

    #[test]
    fn find_walks_up_to_the_git_root() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        assert_eq!(find(&repo.join("src/deep")), None);
        std::fs::write(repo.join(REPO_CONFIG), "[tasks]\n").unwrap();
        assert_eq!(find(&repo.join("src/deep")), Some(repo.clone()));
        // A nested git repo without its own file stops the walk.
        std::fs::create_dir_all(repo.join("sub/.git")).unwrap();
        assert_eq!(find(&repo.join("sub")), None);
        assert!(load(&repo).unwrap().is_ok());
    }
}
