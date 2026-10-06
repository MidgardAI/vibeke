//! Learned policy (04 §7.7, 12 "Learned policy"): suggest approval rules from repeated human
//! decisions.
//!
//! Each answered approval is a [`DecisionRecord`]. Records reduce to a [`Pattern`] (a command
//! prefix, a path glob inside the workspace, or a read-only tool) and a fingerprint
//! `hash(harness, tool, pattern, workspace)`. A fingerprint a person approved at least
//! `min_approvals` times and denied at most `max_denials` times becomes a [`Suggestion`] with a
//! ready-to-paste rule. Suggestions are never applied automatically: accepting one is an
//! explicit user act that goes through the normal policy API (`policy.add`, audited), or writes
//! to the repository's `.vibeke/policy.toml` so the rule is reviewed like code.
//!
//! Safety rules, all enforced here:
//! - decisions already made by policy are not evidence (they would feed back on themselves);
//! - any `high`-risk decision in a group, or a command that is compound (`;`, `|`, `&`, `>`,
//!   `$(`, backticks, newlines) or names a destructive program, is never suggested as an allow;
//! - a command rule matches the prefix plus arguments **without shell metacharacters**, so
//!   `npm test && curl x | sh` does not match a learned `npm test` rule;
//! - path rules cover a directory inside the workspace and never sensitive paths (`.git`,
//!   `.env*`, keys, credentials);
//! - `deny` suggestions (opt-in) come only from fingerprints that were denied and never approved.

use crate::config::LearnedPolicyConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub interaction: String,
    pub harness: String,
    pub tool: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    /// Repository root, or the run's working directory when there is none.
    pub workspace: String,
    pub verdict: Verdict,
    /// Who decided: `user`, a client kind, `policy` for an automatic answer.
    pub by: String,
    /// `low` | `medium` | `high` | `unknown`.
    pub risk: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pattern {
    /// `tool` running a command whose shell words start with `prefix`.
    Command { tool: String, prefix: String },
    /// `tool` on paths below `dir` (absolute, no trailing slash) or exactly `file`.
    Paths { tool: String, glob: String },
    /// A read-only tool, anywhere in the workspace.
    Tool { tool: String },
}

impl Pattern {
    pub fn tool(&self) -> &str {
        match self {
            Pattern::Command { tool, .. }
            | Pattern::Paths { tool, .. }
            | Pattern::Tool { tool } => tool,
        }
    }
    fn key(&self) -> String {
        match self {
            Pattern::Command { tool, prefix } => format!("cmd:{tool}:{prefix}"),
            Pattern::Paths { tool, glob } => format!("path:{tool}:{glob}"),
            Pattern::Tool { tool } => format!("tool:{tool}"),
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Pattern::Command { tool, prefix } => format!("{tool}: `{prefix} ...`"),
            Pattern::Paths { tool, glob } => format!("{tool} on {glob}"),
            Pattern::Tool { tool } => format!("{tool} (read-only tool)"),
        }
    }
}

const EDIT_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "apply_patch",
    "file_change",
    "edit",
    "write",
    "str_replace_editor",
];
const READ_TOOLS: &[&str] = &["Read", "Glob", "Grep", "LS", "read", "grep", "find", "ls"];
/// Programs that are never suggested as allow rules, whatever the user approved so far.
const DANGEROUS_PROGRAMS: &[&str] = &[
    "rm",
    "sudo",
    "su",
    "doas",
    "dd",
    "mkfs",
    "chmod",
    "chown",
    "chgrp",
    "curl",
    "wget",
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "nc",
    "ncat",
    "eval",
    "exec",
    "sh",
    "bash",
    "zsh",
    "fish",
    "kill",
    "killall",
    "pkill",
    "shutdown",
    "reboot",
    "mv",
    "ln",
    "crontab",
    "launchctl",
    "systemctl",
    "xargs",
    "find",
    "python",
    "python3",
    "node",
    "ruby",
    "perl",
    "osascript",
    "open",
    "env",
    "nohup",
    "tee",
    "truncate",
    "shred",
    "mount",
    "umount",
    "iptables",
    "pfctl",
    "defaults",
];
/// (program, subcommand) pairs that are destructive or leave the machine.
const DANGEROUS_SUBCOMMANDS: &[(&str, &str)] = &[
    ("git", "push"),
    ("git", "reset"),
    ("git", "clean"),
    ("git", "rebase"),
    ("git", "filter-branch"),
    ("git", "update-ref"),
    ("git", "gc"),
    ("git", "config"),
    ("git", "remote"),
    ("cargo", "publish"),
    ("cargo", "login"),
    ("cargo", "install"),
    ("npm", "publish"),
    ("npm", "login"),
    ("npm", "install"),
    ("pnpm", "publish"),
    ("yarn", "publish"),
    ("bun", "publish"),
    ("gh", "api"),
    ("gh", "auth"),
    ("gh", "secret"),
    ("docker", "rm"),
    ("docker", "rmi"),
    ("docker", "system"),
    ("docker", "volume"),
    ("docker", "push"),
    ("docker", "login"),
    ("kubectl", "delete"),
    ("kubectl", "apply"),
    ("kubectl", "exec"),
    ("pip", "install"),
    ("pip3", "install"),
    ("brew", "install"),
    ("brew", "uninstall"),
];
/// Programs whose first non-flag argument is part of the command's identity.
const TWO_LEVEL: &[&str] = &[
    "git",
    "cargo",
    "go",
    "docker",
    "kubectl",
    "gh",
    "mise",
    "uv",
    "pip",
    "pip3",
    "brew",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "make",
    "just",
    "rustup",
    "task",
    "poetry",
    "pytest",
    "tsc",
    "gradle",
    "mvn",
    "dotnet",
    "swift",
    "zig",
    "terraform",
];
const SCRIPT_RUNNERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];
const SENSITIVE_PARTS: &[&str] = &[
    ".git",
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".npmrc",
    ".netrc",
    ".pypirc",
    "credentials",
    "secrets",
    "secret",
    "id_rsa",
    "id_ed25519",
    ".vibeke",
];

/// Shell words of `cmd`, or `None` when it is compound, unbalanced or uses substitution.
pub fn simple_words(cmd: &str) -> Option<Vec<String>> {
    if cmd.contains("$(") || cmd.contains('`') || cmd.contains('\n') || cmd.contains('\r') {
        return None;
    }
    let mut words: Vec<String> = vec![];
    let mut cur = String::new();
    let mut have = false;
    let mut chars = cmd.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            Some(_) => cur.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    have = true;
                }
                '\\' => {
                    cur.push(chars.next()?);
                    have = true;
                }
                ';' | '|' | '&' | '<' | '>' | '(' | ')' | '{' | '}' => return None,
                c if c.is_whitespace() => {
                    if have {
                        words.push(std::mem::take(&mut cur));
                        have = false;
                    }
                }
                c => {
                    cur.push(c);
                    have = true;
                }
            },
        }
    }
    if quote.is_some() {
        return None;
    }
    if have {
        words.push(cur);
    }
    (!words.is_empty()).then_some(words)
}

/// The identifying command prefix, or `None` when no safe allow rule can cover this command.
pub fn command_prefix(cmd: &str) -> Option<String> {
    let w = simple_words(cmd)?;
    // Leading VAR=value assignments change what runs: do not learn them.
    if w[0].contains('=') {
        return None;
    }
    let prog = w[0].as_str();
    let base = prog.rsplit('/').next().unwrap_or(prog);
    if DANGEROUS_PROGRAMS.contains(&base) {
        return None;
    }
    if prog.contains('/') && !prog.starts_with("./") {
        // A command named by absolute path is a specific binary; keep the path as typed.
    }
    let second = w.get(1).filter(|a| !a.starts_with('-')).map(String::as_str);
    if TWO_LEVEL.contains(&base) {
        if let Some(sub) = second {
            if DANGEROUS_SUBCOMMANDS.contains(&(base, sub)) {
                return None;
            }
            if SCRIPT_RUNNERS.contains(&base)
                && matches!(sub, "run" | "exec" | "dlx" | "x")
                && let Some(script) = w.get(2).filter(|a| !a.starts_with('-'))
            {
                if sub == "dlx" || sub == "x" || sub == "exec" {
                    return None;
                }
                return Some(format!("{prog} {sub} {script}"));
            }
            return Some(format!("{prog} {sub}"));
        }
        // `git` with only flags is too broad.
        return None;
    }
    Some(prog.to_string())
}

fn sensitive(rel: &str) -> bool {
    rel.split('/').any(|part| {
        let l = part.to_ascii_lowercase();
        SENSITIVE_PARTS.contains(&l.as_str())
            || l.starts_with(".env")
            || l.ends_with(".pem")
            || l.ends_with(".key")
            || l.ends_with(".p12")
            || l.contains("credential")
            || l.contains("password")
    })
}

fn rel_to(workspace: &str, p: &str) -> Option<String> {
    let ws = workspace.trim_end_matches('/');
    let rel = p.strip_prefix(ws)?.strip_prefix('/')?;
    if rel.is_empty() || rel.split('/').any(|c| c == ".." || c.is_empty()) {
        return None;
    }
    Some(rel.to_string())
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// The pattern a decision generalizes to, if any.
pub fn pattern_of(r: &DecisionRecord) -> Option<Pattern> {
    let tool = r.tool.clone();
    if let Some(cmd) = &r.command {
        return command_prefix(cmd).map(|prefix| Pattern::Command { tool, prefix });
    }
    if EDIT_TOOLS.contains(&r.tool.as_str()) {
        if r.paths.is_empty() {
            return None;
        }
        let rels: Vec<String> = r
            .paths
            .iter()
            .map(|p| rel_to(&r.workspace, p))
            .collect::<Option<_>>()?;
        if rels.iter().any(|x| sensitive(x)) {
            return None;
        }
        let ws = r.workspace.trim_end_matches('/');
        let mut dirs: BTreeSet<&str> = rels.iter().map(|x| dir_of(x)).collect();
        let glob = if rels.len() == 1 && dirs.iter().all(|d| d.is_empty()) {
            // A single top-level file: only that file.
            format!("{ws}/{}", rels[0])
        } else {
            // Common directory prefix of every path.
            let first = dirs.pop_first()?;
            let mut common: Vec<&str> = first.split('/').filter(|c| !c.is_empty()).collect();
            for d in dirs {
                let parts: Vec<&str> = d.split('/').filter(|c| !c.is_empty()).collect();
                let n = common
                    .iter()
                    .zip(&parts)
                    .take_while(|(a, b)| a == b)
                    .count();
                common.truncate(n);
            }
            if common.is_empty() {
                return None;
            }
            format!("{ws}/{}/**", common.join("/"))
        };
        return Some(Pattern::Paths { tool, glob });
    }
    if READ_TOOLS.contains(&r.tool.as_str()) {
        if r.paths
            .iter()
            .any(|p| rel_to(&r.workspace, p).is_none_or(|x| sensitive(&x)))
        {
            return None;
        }
        return Some(Pattern::Tool { tool });
    }
    None
}

/// `hash(harness, tool, pattern, workspace)` as 16 hex digits.
pub fn fingerprint(harness: &str, workspace: &str, p: &Pattern) -> String {
    let mut h = blake3::Hasher::new();
    for part in [harness, p.tool(), &p.key(), workspace] {
        h.update(part.as_bytes());
        h.update(b"\0");
    }
    h.finalize().to_hex()[..16].to_string()
}

fn risk_rank(r: &str) -> u8 {
    match r {
        "low" => 0,
        "medium" => 1,
        "high" => 3,
        _ => 2,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub fingerprint: String,
    pub harness: String,
    pub workspace: String,
    pub pattern: Pattern,
    pub approvals: u32,
    pub denials: u32,
    pub first_at_ms: i64,
    pub last_at_ms: i64,
    pub max_risk: String,
    pub samples: Vec<String>,
    /// A representative record (for the "is a rule already deciding this" check).
    pub sample: DecisionRecord,
}

/// Group decisions by fingerprint. Decisions made by policy are skipped; `since_ms` bounds the
/// window.
pub fn aggregate(records: &[DecisionRecord], since_ms: i64) -> Vec<Aggregate> {
    let mut m: BTreeMap<String, Aggregate> = BTreeMap::new();
    for r in records {
        if r.at_ms < since_ms || r.by == "policy" {
            continue;
        }
        let Some(pattern) = pattern_of(r) else {
            continue;
        };
        let fp = fingerprint(&r.harness, &r.workspace, &pattern);
        let e = m.entry(fp.clone()).or_insert_with(|| Aggregate {
            fingerprint: fp,
            harness: r.harness.clone(),
            workspace: r.workspace.clone(),
            pattern: pattern.clone(),
            approvals: 0,
            denials: 0,
            first_at_ms: r.at_ms,
            last_at_ms: r.at_ms,
            max_risk: r.risk.clone(),
            samples: vec![],
            sample: r.clone(),
        });
        match r.verdict {
            Verdict::Allow => e.approvals += 1,
            Verdict::Deny => e.denials += 1,
        }
        e.first_at_ms = e.first_at_ms.min(r.at_ms);
        e.last_at_ms = e.last_at_ms.max(r.at_ms);
        if risk_rank(&r.risk) > risk_rank(&e.max_risk) {
            e.max_risk = r.risk.clone();
        }
        let sample = r
            .command
            .clone()
            .or_else(|| r.paths.first().cloned())
            .unwrap_or_else(|| r.tool.clone());
        if e.samples.len() < 3 && !e.samples.contains(&sample) {
            e.samples.push(sample.chars().take(200).collect());
        }
    }
    m.into_values().collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    /// The fingerprint; stable across runs, used to accept or dismiss.
    pub id: String,
    /// `allow` | `deny`.
    pub effect: String,
    pub harness: String,
    pub tool: String,
    pub workspace: String,
    pub pattern: String,
    /// A policy rule as `policy.add` takes it.
    pub rule: Value,
    /// The same rule as a `[[policy.rule]]` block for `config.toml` (or `[[rule]]` in a repo file).
    pub toml: String,
    pub approvals: u32,
    pub denials: u32,
    pub first_at_ms: i64,
    pub last_at_ms: i64,
    pub max_risk: String,
    pub samples: Vec<String>,
    pub reason: String,
}

/// Regex for a command prefix: the prefix, then optionally arguments without shell
/// metacharacters.
pub fn command_regex(prefix: &str) -> String {
    format!(
        "^{}(?:[ \\t][^;&|<>`$(){{}}\\n\\r]*)?$",
        regex_escape(prefix)
    )
}

fn regex_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

fn rule_for(a: &Aggregate, effect: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("tool".into(), json!(a.pattern.tool()));
    match &a.pattern {
        Pattern::Command { prefix, .. } => {
            m.insert("command_regex".into(), json!(command_regex(prefix)));
        }
        Pattern::Paths { glob, .. } => {
            m.insert("path_glob".into(), json!(glob));
        }
        Pattern::Tool { .. } => {}
    }
    json!({
        "match": Value::Object(m),
        "effect": effect,
        "scope": a.workspace,
        "note": format!("learned: {} approval(s), {} denial(s), last {}", a.approvals, a.denials, a.last_at_ms),
    })
}

/// A `[[policy.rule]]` (or, for `repo_file`, `[[rule]]`) block with the match keys flat.
pub fn rule_toml(rule: &Value, repo_file: bool) -> String {
    let mut s = String::new();
    s.push_str(if repo_file {
        "[[rule]]\n"
    } else {
        "[[policy.rule]]\n"
    });
    let q = |v: &Value| serde_json::to_string(v).unwrap_or_default();
    let m = &rule["match"];
    for k in ["tool", "command_regex", "path_glob", "url_glob"] {
        if let Some(v) = m.get(k).filter(|v| v.is_string()) {
            s.push_str(&format!("{k} = {}\n", toml_string(v.as_str().unwrap())));
        }
    }
    s.push_str(&format!("effect = {}\n", q(&rule["effect"])));
    if let Some(v) = rule.get("scope").filter(|v| v.is_string()) {
        s.push_str(&format!("scope = {}\n", toml_string(v.as_str().unwrap())));
    }
    if let Some(v) = rule.get("note").filter(|v| v.is_string()) {
        s.push_str(&format!("note = {}\n", toml_string(v.as_str().unwrap())));
    }
    s
}

/// A TOML literal string when possible (regexes read better), else a basic string.
fn toml_string(s: &str) -> String {
    if !s.contains('\'') && !s.chars().any(|c| c.is_control()) {
        format!("'{s}'")
    } else {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '\\' => o.push_str("\\\\"),
                '"' => o.push_str("\\\""),
                '\n' => o.push_str("\\n"),
                '\r' => o.push_str("\\r"),
                '\t' => o.push_str("\\t"),
                c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
                c => o.push(c),
            }
        }
        o.push('"');
        o
    }
}

/// Suggestions for the aggregates that pass the thresholds. `dismissed` holds fingerprints the
/// user dismissed; `covered` says whether a rule already decides this kind of action (the
/// server answers with the policy engine).
pub fn suggest(
    aggs: &[Aggregate],
    cfg: &LearnedPolicyConfig,
    dismissed: &HashSet<String>,
    covered: &dyn Fn(&DecisionRecord) -> bool,
) -> Vec<Suggestion> {
    let mut out = vec![];
    for a in aggs {
        if dismissed.contains(&a.fingerprint) || covered(&a.sample) {
            continue;
        }
        let (effect, reason) = if a.approvals >= cfg.min_approvals.max(1)
            && a.denials <= cfg.max_denials
            && cfg.allow_risk.contains(&a.max_risk)
            && a.max_risk != "high"
        {
            (
                "allow",
                format!(
                    "approved {} time(s) with {} denial(s), highest risk {}",
                    a.approvals, a.denials, a.max_risk
                ),
            )
        } else if cfg.suggest_deny && a.approvals == 0 && a.denials >= cfg.min_approvals.max(1) {
            (
                "deny",
                format!("denied {} time(s) and never approved", a.denials),
            )
        } else {
            continue;
        };
        let rule = rule_for(a, effect);
        out.push(Suggestion {
            id: a.fingerprint.clone(),
            effect: effect.into(),
            harness: a.harness.clone(),
            tool: a.pattern.tool().into(),
            workspace: a.workspace.clone(),
            pattern: a.pattern.describe(),
            toml: rule_toml(&rule, false),
            rule,
            approvals: a.approvals,
            denials: a.denials,
            first_at_ms: a.first_at_ms,
            last_at_ms: a.last_at_ms,
            max_risk: a.max_risk.clone(),
            samples: a.samples.clone(),
            reason,
        });
    }
    out.sort_by(|a, b| {
        b.approvals
            .max(b.denials)
            .cmp(&a.approvals.max(a.denials))
            .then(b.last_at_ms.cmp(&a.last_at_ms))
            .then(a.id.cmp(&b.id))
    });
    out
}

/// `existing` (a repository's `.vibeke/policy.toml`, possibly empty) with the suggestion's rule
/// appended as a `[[rule]]` block, unless a block with the same matchers is already there.
/// Returns `None` when nothing changed.
pub fn append_repo_rule(existing: &str, s: &Suggestion) -> Option<String> {
    let block = rule_toml(&s.rule, true);
    // Same rule already present (matchers compared textually).
    let key_lines: Vec<&str> = block
        .lines()
        .filter(|l| {
            l.starts_with("tool =")
                || l.starts_with("command_regex =")
                || l.starts_with("path_glob =")
                || l.starts_with("effect =")
        })
        .collect();
    let present = existing
        .split("[[rule]]")
        .skip(1)
        .any(|b| key_lines.iter().all(|k| b.lines().any(|l| l.trim() == *k)));
    if present {
        return None;
    }
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!(
        "# learned by vibeke (fingerprint {}); review like code\n",
        s.id
    ));
    out.push_str(&block);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        tool: &str,
        cmd: Option<&str>,
        paths: &[&str],
        verdict: Verdict,
        risk: &str,
        at: i64,
    ) -> DecisionRecord {
        DecisionRecord {
            interaction: format!("i{at}"),
            harness: "claude".into(),
            tool: tool.into(),
            command: cmd.map(str::to_string),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            workspace: "/work/app".into(),
            verdict,
            by: "user".into(),
            risk: risk.into(),
            at_ms: at,
        }
    }

    #[test]
    fn shell_words_reject_compound_commands() {
        assert_eq!(
            simple_words("npm test --watch").unwrap(),
            vec!["npm", "test", "--watch"]
        );
        assert_eq!(simple_words("git commit -m \"fix: a b\"").unwrap().len(), 4);
        assert_eq!(simple_words("echo 'a; b'").unwrap(), vec!["echo", "a; b"]);
        for bad in [
            "a && b",
            "a | b",
            "a; b",
            "a > f",
            "echo $(x)",
            "echo `x`",
            "a\nb",
            "echo \"unterminated",
            "(a)",
            "{ a; }",
            "a &",
        ] {
            assert!(simple_words(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn command_prefixes() {
        assert_eq!(command_prefix("npm test").as_deref(), Some("npm test"));
        assert_eq!(
            command_prefix("npm run lint -- --fix").as_deref(),
            Some("npm run lint")
        );
        assert_eq!(
            command_prefix("pnpm test:unit").as_deref(),
            Some("pnpm test:unit")
        );
        assert_eq!(
            command_prefix("cargo test -p vk-tasks").as_deref(),
            Some("cargo test")
        );
        assert_eq!(command_prefix("git status").as_deref(), Some("git status"));
        assert_eq!(
            command_prefix("git -C /x status").as_deref(),
            None,
            "flag first: too broad"
        );
        assert_eq!(command_prefix("cargo --version").as_deref(), None);
        assert_eq!(
            command_prefix("./scripts/check.sh --fast").as_deref(),
            Some("./scripts/check.sh")
        );
        assert_eq!(command_prefix("ls -la").as_deref(), Some("ls"));
        // never
        for bad in [
            "rm -rf build",
            "sudo make install",
            "curl http://x",
            "git push origin main",
            "git reset --hard",
            "cargo publish",
            "npm install left-pad",
            "docker rm x",
            "FOO=1 make",
            "bash -c ls",
            "pnpm dlx evil",
            "npm exec foo",
            "python3 x.py",
            "kubectl delete pod x",
            "mv a b",
        ] {
            assert_eq!(command_prefix(bad), None, "{bad}");
        }
    }

    #[test]
    fn command_regex_does_not_cover_compound_commands() {
        let re = regex::Regex::new(&command_regex("npm test")).unwrap();
        assert!(re.is_match("npm test"));
        assert!(re.is_match("npm test --watch"));
        assert!(re.is_match("npm test src/a.test.ts"));
        assert!(!re.is_match("npm testing"));
        assert!(!re.is_match("npm test && curl x | sh"));
        assert!(!re.is_match("npm test; rm -rf /"));
        assert!(!re.is_match("npm test $(whoami)"));
        assert!(!re.is_match("npm test > /etc/passwd"));
        assert!(!re.is_match("npm test\nrm -rf /"));
        let re2 = regex::Regex::new(&command_regex("./scripts/check.sh")).unwrap();
        assert!(re2.is_match("./scripts/check.sh --fast"));
        assert!(!re2.is_match("xscripts/check.sh"));
    }

    #[test]
    fn patterns_for_paths_and_tools() {
        let e = rec(
            "Edit",
            None,
            &["/work/app/src/auth/a.rs", "/work/app/src/auth/b.rs"],
            Verdict::Allow,
            "low",
            1,
        );
        assert_eq!(
            pattern_of(&e),
            Some(Pattern::Paths {
                tool: "Edit".into(),
                glob: "/work/app/src/auth/**".into()
            })
        );
        let two = rec(
            "Edit",
            None,
            &["/work/app/src/a/x.rs", "/work/app/src/b/y.rs"],
            Verdict::Allow,
            "low",
            1,
        );
        assert_eq!(
            pattern_of(&two),
            Some(Pattern::Paths {
                tool: "Edit".into(),
                glob: "/work/app/src/**".into()
            })
        );
        let top = rec(
            "Write",
            None,
            &["/work/app/README.md"],
            Verdict::Allow,
            "low",
            1,
        );
        assert_eq!(
            pattern_of(&top),
            Some(Pattern::Paths {
                tool: "Write".into(),
                glob: "/work/app/README.md".into()
            })
        );
        // root-level mix of dirs: no common dir -> nothing
        let mix = rec(
            "Edit",
            None,
            &["/work/app/src/a.rs", "/work/app/docs/b.md"],
            Verdict::Allow,
            "low",
            1,
        );
        assert_eq!(pattern_of(&mix), None);
        // sensitive and outside
        for bad in [
            "/work/app/.env",
            "/work/app/.git/config",
            "/work/app/keys/server.pem",
            "/etc/hosts",
            "/work/app/../x",
            "/work/application/x",
        ] {
            assert_eq!(
                pattern_of(&rec("Edit", None, &[bad], Verdict::Allow, "low", 1)),
                None,
                "{bad}"
            );
        }
        let r = rec(
            "Read",
            None,
            &["/work/app/src/a.rs"],
            Verdict::Allow,
            "low",
            1,
        );
        assert_eq!(
            pattern_of(&r),
            Some(Pattern::Tool {
                tool: "Read".into()
            })
        );
        assert_eq!(
            pattern_of(&rec(
                "Read",
                None,
                &["/work/app/.env"],
                Verdict::Allow,
                "low",
                1
            )),
            None
        );
        // unknown tools without a command never generalize
        assert_eq!(
            pattern_of(&rec("mcp__x__do", None, &[], Verdict::Allow, "low", 1)),
            None
        );
    }

    #[test]
    fn fingerprints_are_stable_and_distinguish_inputs() {
        let p = Pattern::Command {
            tool: "Bash".into(),
            prefix: "npm test".into(),
        };
        let a = fingerprint("claude", "/w", &p);
        assert_eq!(a, fingerprint("claude", "/w", &p));
        assert_eq!(a.len(), 16);
        assert_ne!(a, fingerprint("codex", "/w", &p));
        assert_ne!(a, fingerprint("claude", "/other", &p));
        assert_ne!(
            a,
            fingerprint(
                "claude",
                "/w",
                &Pattern::Command {
                    tool: "Bash".into(),
                    prefix: "npm run x".into()
                }
            )
        );
    }

    fn many(n: u32, cmd: &str, verdict: Verdict, risk: &str) -> Vec<DecisionRecord> {
        (0..n)
            .map(|i| rec("Bash", Some(cmd), &[], verdict, risk, 1000 + i64::from(i)))
            .collect()
    }

    fn cfg() -> LearnedPolicyConfig {
        LearnedPolicyConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn none_covered(_: &DecisionRecord) -> bool {
        false
    }

    #[test]
    fn suggests_after_enough_approvals_and_no_denials() {
        let mut rs = many(5, "npm test --watch", Verdict::Allow, "low");
        rs.extend(many(4, "cargo build", Verdict::Allow, "low"));
        rs.extend(many(5, "npm run lint", Verdict::Allow, "low"));
        rs.push(rec(
            "Bash",
            Some("npm run lint"),
            &[],
            Verdict::Deny,
            "low",
            5000,
        ));
        let aggs = aggregate(&rs, 0);
        let s = suggest(&aggs, &cfg(), &HashSet::new(), &none_covered);
        assert_eq!(s.len(), 1, "{s:#?}");
        assert_eq!(s[0].effect, "allow");
        assert!(s[0].pattern.contains("npm test"));
        assert_eq!(s[0].approvals, 5);
        assert_eq!(s[0].rule["scope"], "/work/app");
        assert_eq!(s[0].rule["match"]["tool"], "Bash");
        let re = regex::Regex::new(s[0].rule["match"]["command_regex"].as_str().unwrap()).unwrap();
        assert!(re.is_match("npm test -- --coverage"));
        assert!(s[0].toml.starts_with("[[policy.rule]]"));
        assert!(s[0].toml.contains("effect = \"allow\""));
        // the TOML parses back to the same matchers
        let parsed: toml::Value = toml::from_str(&s[0].toml).unwrap();
        let rule = &parsed["policy"]["rule"][0];
        assert_eq!(
            rule["command_regex"].as_str(),
            s[0].rule["match"]["command_regex"].as_str()
        );
        assert_eq!(rule["scope"].as_str(), Some("/work/app"));
    }

    #[test]
    fn high_risk_policy_decisions_dismissed_and_covered_are_excluded() {
        let mut rs = many(6, "make build", Verdict::Allow, "medium");
        rs.extend(many(6, "make deploy", Verdict::Allow, "high"));
        let mut by_policy = many(9, "just fmt", Verdict::Allow, "low");
        for r in &mut by_policy {
            r.by = "policy".into();
        }
        rs.extend(by_policy);
        rs.extend(many(6, "just test", Verdict::Allow, "low"));
        let aggs = aggregate(&rs, 0);
        assert_eq!(aggs.len(), 3, "policy-made decisions are not evidence");
        let s = suggest(&aggs, &cfg(), &HashSet::new(), &none_covered);
        let names: Vec<_> = s.iter().map(|x| x.pattern.clone()).collect();
        assert!(names.iter().any(|n| n.contains("make build")));
        assert!(names.iter().any(|n| n.contains("just test")));
        assert!(
            !names.iter().any(|n| n.contains("deploy")),
            "high risk never"
        );
        // dismissed
        let mut d = HashSet::new();
        d.insert(s[0].id.clone());
        let s2 = suggest(&aggs, &cfg(), &d, &none_covered);
        assert_eq!(s2.len(), s.len() - 1);
        // covered by an existing rule
        let s3 = suggest(&aggs, &cfg(), &HashSet::new(), &|r: &DecisionRecord| {
            r.command.as_deref() == Some("make build")
        });
        assert!(!s3.iter().any(|x| x.pattern.contains("make build")));
        // risk class not allowed by config
        let strict = LearnedPolicyConfig {
            allow_risk: vec!["low".into()],
            ..cfg()
        };
        let s4 = suggest(&aggs, &strict, &HashSet::new(), &none_covered);
        assert!(!s4.iter().any(|x| x.pattern.contains("make build")));
    }

    #[test]
    fn window_bounds_the_evidence() {
        let rs = many(5, "npm test", Verdict::Allow, "low");
        assert_eq!(aggregate(&rs, 2000).len(), 0);
        assert_eq!(aggregate(&rs, 0).len(), 1);
        let a = aggregate(&rs, 1002);
        assert_eq!(a[0].approvals, 3);
        assert_eq!(a[0].first_at_ms, 1002);
    }

    #[test]
    fn deny_suggestions_are_opt_in_and_need_zero_approvals() {
        let mut rs = many(5, "terraform apply", Verdict::Deny, "medium");
        rs.extend(many(5, "npm test", Verdict::Deny, "low"));
        rs.extend(many(1, "npm test", Verdict::Allow, "low"));
        let aggs = aggregate(&rs, 0);
        assert!(suggest(&aggs, &cfg(), &HashSet::new(), &none_covered).is_empty());
        let c = LearnedPolicyConfig {
            suggest_deny: true,
            ..cfg()
        };
        let s = suggest(&aggs, &c, &HashSet::new(), &none_covered);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].effect, "deny");
        assert!(s[0].pattern.contains("terraform apply"));
    }

    #[test]
    fn path_rules_become_suggestions() {
        let rs: Vec<_> = (0..5)
            .map(|i| {
                rec(
                    "Edit",
                    None,
                    &[&format!("/work/app/src/ui/f{i}.tsx")],
                    Verdict::Allow,
                    "low",
                    i,
                )
            })
            .collect();
        let s = suggest(&aggregate(&rs, 0), &cfg(), &HashSet::new(), &none_covered);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].rule["match"]["path_glob"], "/work/app/src/ui/**");
        assert_eq!(s[0].rule["match"]["tool"], "Edit");
        assert!(s[0].samples.len() <= 3);
    }

    #[test]
    fn repo_file_append_is_idempotent_and_valid_toml() {
        let rs = many(5, "npm test", Verdict::Allow, "low");
        let s = suggest(&aggregate(&rs, 0), &cfg(), &HashSet::new(), &none_covered).remove(0);
        let first = append_repo_rule("", &s).unwrap();
        assert!(first.contains("[[rule]]"));
        assert!(first.contains(&s.id));
        let parsed: toml::Value = toml::from_str(&first).unwrap();
        assert_eq!(parsed["rule"][0]["effect"].as_str(), Some("allow"));
        assert!(
            append_repo_rule(&first, &s).is_none(),
            "second append is a no-op"
        );
        let existing = "[[rule]]\ntool = 'Bash'\ncommand_regex = '^rm'\neffect = \"deny\"\n";
        let both = append_repo_rule(existing, &s).unwrap();
        let parsed: toml::Value = toml::from_str(&both).unwrap();
        assert_eq!(parsed["rule"].as_array().unwrap().len(), 2);
        assert!(both.starts_with(existing));
    }

    #[test]
    fn toml_strings_survive_awkward_characters() {
        let v = json!({"match": {"tool": "Bash", "command_regex": "it's\\d"}, "effect": "allow", "scope": "/w p", "note": "a \"quoted\" note"});
        let t = rule_toml(&v, false);
        let parsed: toml::Value = toml::from_str(&t).unwrap();
        let r = &parsed["policy"]["rule"][0];
        assert_eq!(r["command_regex"].as_str(), Some("it's\\d"));
        assert_eq!(r["note"].as_str(), Some("a \"quoted\" note"));
    }
}
