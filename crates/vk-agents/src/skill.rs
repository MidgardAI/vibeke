//! The Vibeke agent skill (`crates/vibeke/skill/SKILL.md`), installed next to each harness's
//! hooks or extension so agents learn the `vibeke` CLI (panes, agents, tasks, previews, the
//! agent browser, images) without the MCP server.
//!
//! * **Claude Code** reads user skills from `<claude>/skills/<name>/SKILL.md`.
//! * **Codex, pi, omp, OpenCode and Gemini CLI** all read the shared Agent Skills directory
//!   `~/.agents/skills/<name>/SKILL.md`. They share one file: Codex lists two skills with the
//!   same name twice, so a second copy in a harness's own directory would show up as a duplicate.
//!   (omp and OpenCode also read `~/.claude/skills`; both keep the first skill of a name.)
//!
//! * **Hermes Agent** reads `<HERMES_HOME>/skills/<name>/SKILL.md` (not `~/.agents`).
//!
//! A line after the front matter marks the file as ours (`<!-- managed by vibeke
//! (vibeke-skill@<ver>) … -->`). A `vibeke` skill without it belongs to the user and is left
//! alone. Uninstalling one harness keeps the shared file while another harness that reads it
//! still has its Vibeke integration.

use crate::install::{Dirs, FileChange, Harness, read_file, resolve_symlink};
use anyhow::Result;
use std::path::PathBuf;

/// The skill text `vibeke --skill` prints.
pub const SKILL: &str = include_str!("../../vibeke/skill/SKILL.md");

const MARKER_PREFIX: &str = "<!-- managed by vibeke (vibeke-skill@";

/// Where `h` reads the Vibeke skill.
pub fn skill_file(h: Harness, dirs: &Dirs) -> PathBuf {
    match h {
        Harness::Claude => dirs.claude.join("skills/vibeke/SKILL.md"),
        Harness::Hermes => dirs.hermes.join("skills/vibeke/SKILL.md"),
        _ => dirs.agents.join("skills/vibeke/SKILL.md"),
    }
}

/// The installed file: the front matter, our marker line, then the body.
pub fn skill_content() -> String {
    let marker = format!(
        "{MARKER_PREFIX}{}); reinstall overwrites -->\n",
        crate::install::VERSION
    );
    match split_front_matter(SKILL) {
        Some((front, body)) => format!("{front}{marker}{body}"),
        None => format!("{marker}{SKILL}"),
    }
}

/// `("---\n…\n---\n", rest)` when `text` starts with YAML front matter.
fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")? + "---\n".len() + "\n---\n".len();
    Some(text.split_at(end))
}

/// The version in our marker line, or `None` when the file is not ours.
fn marker_version(text: &str) -> Option<String> {
    let rest = text.lines().find_map(|l| l.strip_prefix(MARKER_PREFIX))?;
    rest.split_once(')').map(|(v, _)| v.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillState {
    Missing,
    /// Ours, from this build.
    Current,
    /// Ours, from another version (or edited).
    Outdated(String),
    /// A `vibeke` skill that Vibeke did not write; never touched.
    Foreign,
    /// The file cannot be read.
    Unreadable(String),
}

pub fn skill_state(h: Harness, dirs: &Dirs) -> (PathBuf, SkillState) {
    let path = skill_file(h, dirs);
    let state = match read_file(&resolve_symlink(&path)) {
        Ok(None) => SkillState::Missing,
        Ok(Some((text, _))) => match marker_version(&text) {
            None => SkillState::Foreign,
            Some(_) if text == skill_content() => SkillState::Current,
            Some(v) => SkillState::Outdated(v),
        },
        Err(e) => SkillState::Unreadable(format!("{e:#}")),
    };
    (path, state)
}

/// `h` reads the shared `~/.agents/skills` (Claude Code and Hermes have their own).
fn shared(h: Harness) -> bool {
    !matches!(h, Harness::Claude | Harness::Hermes)
}

/// Other harnesses that read the same skill file as `h`.
fn sharing(h: Harness) -> impl Iterator<Item = Harness> {
    Harness::ALL
        .into_iter()
        .filter(move |&o| o != h && shared(o) && shared(h))
}

/// The skill's part of an install or uninstall plan for `h`. `None` when there is nothing to
/// do; a note explains a skipped foreign file or a shared file that stays.
pub(crate) fn plan_skill(
    h: Harness,
    dirs: &Dirs,
    install: bool,
    notes: &mut Vec<String>,
) -> Result<Option<FileChange>> {
    let path = resolve_symlink(&skill_file(h, dirs));
    let existing = read_file(&path)?;
    let ours = existing
        .as_ref()
        .is_some_and(|(t, _)| marker_version(t).is_some());
    if existing.is_some() && !ours {
        if install {
            notes.push(format!(
                "{} exists and is not managed by Vibeke; the Vibeke skill was not written",
                path.display()
            ));
        }
        return Ok(None);
    }
    let (before, stamp) = match existing {
        Some((t, s)) => (Some(t), Some(s)),
        None if install => (None, None),
        None => return Ok(None),
    };
    if !install {
        let users: Vec<&str> = sharing(h)
            .filter(|&o| crate::install::hooks_present(o, dirs))
            .map(Harness::id)
            .collect();
        if !users.is_empty() {
            notes.push(format!(
                "{} stays: {} also read it",
                path.display(),
                users.join(", ")
            ));
            return Ok(None);
        }
    }
    Ok(Some(FileChange {
        path,
        before,
        after: install.then(skill_content),
        stamp,
        remove: !install,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_follows_the_front_matter() {
        let c = skill_content();
        assert!(c.starts_with("---\nname: vibeke\n"), "{c}");
        let (front, body) = split_front_matter(&c).unwrap();
        assert!(front.ends_with("---\n"));
        assert!(body.starts_with(MARKER_PREFIX), "{body}");
        assert_eq!(marker_version(&c).as_deref(), Some(crate::install::VERSION));
        assert_eq!(marker_version(SKILL), None);
    }

    #[test]
    fn claude_has_its_own_file_and_the_others_share_one() {
        let d = Dirs {
            claude: "/c".into(),
            codex: "/x".into(),
            pi: "/p".into(),
            omp: "/o".into(),
            opencode: "/oc".into(),
            gemini: "/g".into(),
            agents: "/a".into(),
            hermes: "/h".into(),
        };
        assert_eq!(
            skill_file(Harness::Claude, &d),
            PathBuf::from("/c/skills/vibeke/SKILL.md")
        );
        assert_eq!(
            skill_file(Harness::Hermes, &d),
            PathBuf::from("/h/skills/vibeke/SKILL.md")
        );
        assert_eq!(sharing(Harness::Hermes).count(), 0);
        for h in Harness::ALL.into_iter().filter(|&h| shared(h)) {
            assert_eq!(
                skill_file(h, &d),
                PathBuf::from("/a/skills/vibeke/SKILL.md")
            );
        }
        assert_eq!(sharing(Harness::Claude).count(), 0);
        assert!(sharing(Harness::Codex).any(|h| h == Harness::Pi));
        assert!(!sharing(Harness::Codex).any(|h| h == Harness::Claude));
    }
}
