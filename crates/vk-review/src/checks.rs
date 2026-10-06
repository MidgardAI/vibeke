//! Check definitions, per-candidate authorization, disposable-checkout execution and observed
//! agent commands (§6.2, §6.3).
//!
//! **Unchanged argv ≠ unchanged code.** A [`CheckDefinition`]'s `definition_digest` covers the
//! command plus the resolved script text it runs: `npm|pnpm|yarn|bun run X` → `scripts.X` (and
//! npm-style `preX`/`postX`) from the relevant `package.json`; `make X` → the whole Makefile
//! (variables and includes change recipes) with the target's body recorded for display; a
//! relative script path (`./ci/test.sh`, `bash ci/test.sh`, `node x.js`, …) → that file's content.
//! Resolve definitions against the candidate's tree ([`resolve_definition_at`]) so a task that
//! edits `package.json` produces a different digest and needs renewed authorization.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::gitcmd::{self, GitError};
use crate::subject::ChangeSubject;
use crate::{Actor, ActorKind, FieldHasher, new_id, now_ms};

/// Label shown on the per-candidate host-run action (§6.3).
pub const HOST_RUN_LABEL: &str = "Runs code modified by this task";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CheckCommand {
    /// Executed directly (no shell).
    Argv(Vec<String>),
    /// Executed with `/bin/sh -c`.
    Shell(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckTrust {
    /// Defined by the user.
    User,
    /// From a trusted project recipe at a trusted baseline.
    ProjectRecipe,
    /// The resolved definition differs from the trusted baseline: supplied by the task's
    /// changes. Separate execution does not make it independent in design.
    TaskModified,
}

/// A check as configured by the user or a recipe, before resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub id: String,
    pub name: String,
    pub command: CheckCommand,
    /// Relative to the repository root; `""` for the root. Absolute or `..` paths are refused.
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
    /// Declared trust (`user` or `project_recipe`); [`classify_trust`] may downgrade it.
    pub trust: CheckTrust,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    PackageScript,
    MakeTarget,
    Makefile,
    ScriptFile,
}

/// Executable text a check resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedScript {
    pub kind: ScriptKind,
    /// Repository-relative file it came from.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `None` when the file/script/target is missing (recorded so that adding it later changes
    /// the digest). For `makefile`/`script_file` this is a blake3 digest, not the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// A resolved, digest-bearing check definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDefinition {
    pub id: String,
    pub name: String,
    pub command: CheckCommand,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub trust: CheckTrust,
    pub resolved: Vec<ResolvedScript>,
    /// False when the command could not be fully resolved (e.g. shell metacharacters); the
    /// digest then covers only the literal command and the UI should say so.
    pub resolution_complete: bool,
    pub definition_digest: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("invalid check: {0}")]
    Invalid(String),
    #[error("check is not authorized for this candidate: {0:?}")]
    NotAuthorized(AuthRequirement),
    #[error("resolved definition in the disposable checkout differs from the authorized digest")]
    DefinitionMismatch,
    #[error("could not create a disposable checkout: {0}")]
    Checkout(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn validate_rel(cwd: &str) -> Result<(), CheckError> {
    let p = Path::new(cwd);
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(CheckError::Invalid(format!(
            "cwd must be a relative path inside the repository: {cwd:?}"
        )));
    }
    Ok(())
}

fn rel_join(cwd: &str, file: &str) -> String {
    let joined = Path::new(cwd).join(file);
    let mut parts: Vec<String> = Vec::new();
    for c in joined.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    parts.join("/")
}

const SHELL_META: &[char] = &[
    '|', '&', ';', '<', '>', '(', ')', '$', '`', '"', '\'', '*', '?', '[', ']', '{', '}', '~',
    '\n', '\\', '#',
];

fn tokens(cmd: &CheckCommand) -> Option<Vec<String>> {
    match cmd {
        CheckCommand::Argv(v) => Some(v.clone()),
        CheckCommand::Shell(s) => {
            if s.contains(SHELL_META) {
                None
            } else {
                Some(s.split_whitespace().map(str::to_string).collect())
            }
        }
    }
}

fn program_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Resolve a [`CheckSpec`] using `read(path)` to fetch repository-relative files (from the
/// candidate's tree, a checkout, or a test map). Pure apart from `read`.
pub fn resolve_definition(
    spec: &CheckSpec,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Result<CheckDefinition, CheckError> {
    validate_rel(&spec.cwd)?;
    match &spec.command {
        CheckCommand::Argv(v) if v.is_empty() => {
            return Err(CheckError::Invalid("empty argv".into()));
        }
        CheckCommand::Shell(s) if s.trim().is_empty() => {
            return Err(CheckError::Invalid("empty shell command".into()));
        }
        _ => {}
    }
    let mut resolved = Vec::new();
    let mut complete = true;
    match tokens(&spec.command) {
        None => complete = false,
        Some(argv) => resolve_argv(&argv, &spec.cwd, read, &mut resolved),
    }
    let digest = definition_digest(spec, &resolved);
    Ok(CheckDefinition {
        id: spec.id.clone(),
        name: spec.name.clone(),
        command: spec.command.clone(),
        cwd: spec.cwd.clone(),
        env: spec.env.clone(),
        timeout_ms: spec.timeout_ms,
        trust: spec.trust,
        resolved,
        resolution_complete: complete,
        definition_digest: digest,
    })
}

fn definition_digest(spec: &CheckSpec, resolved: &[ResolvedScript]) -> String {
    let mut h = FieldHasher::new("vk-review/check-definition/v1");
    match &spec.command {
        CheckCommand::Argv(v) => {
            h.str("argv");
            for a in v {
                h.str(a);
            }
        }
        CheckCommand::Shell(s) => {
            h.str("shell").str(s);
        }
    }
    h.str("cwd").str(&spec.cwd);
    for (k, v) in &spec.env {
        h.str(k).str(v);
    }
    h.str(&spec.timeout_ms.to_string());
    for r in resolved {
        h.str(&format!("{:?}", r.kind))
            .str(&r.file)
            .opt(r.name.as_deref())
            .opt(r.body.as_deref());
    }
    h.finish()
}

fn resolve_argv(
    argv: &[String],
    cwd: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    out: &mut Vec<ResolvedScript>,
) {
    let Some(prog) = argv.first() else { return };
    let name = program_name(prog);
    let rest: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    match name {
        "npm" | "pnpm" | "yarn" | "bun" => resolve_package_script(name, &rest, cwd, read, out),
        "make" | "gmake" => resolve_make(&rest, cwd, read, out),
        "sh" | "bash" | "zsh" | "node" | "python" | "python3" | "ruby" | "perl" | "deno" => {
            if let Some(file) = rest.iter().find(|a| !a.starts_with('-')) {
                push_script_file(file, cwd, read, out);
            }
        }
        _ => {
            if prog.contains('/') && !Path::new(prog).is_absolute() {
                push_script_file(prog, cwd, read, out);
            }
        }
    }
}

fn push_script_file(
    file: &str,
    cwd: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    out: &mut Vec<ResolvedScript>,
) {
    if Path::new(file).is_absolute() {
        return;
    }
    let rel = rel_join(cwd, file);
    out.push(ResolvedScript {
        kind: ScriptKind::ScriptFile,
        body: read(&rel).map(|b| blake3::hash(&b).to_hex().to_string()),
        file: rel,
        name: None,
    });
}

fn resolve_package_script(
    pm: &str,
    args: &[&str],
    cwd: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    out: &mut Vec<ResolvedScript>,
) {
    let positional: Vec<&str> = args
        .iter()
        .copied()
        .filter(|a| !a.starts_with('-'))
        .collect();
    let script = match positional.as_slice() {
        ["run" | "run-script", s, ..] => Some(*s),
        ["test" | "t", ..] if pm != "bun" => Some("test"),
        [s, ..] if pm == "yarn" && !is_yarn_builtin(s) => Some(*s),
        _ => None,
    };
    let Some(script) = script else { return };
    let file = rel_join(cwd, "package.json");
    let scripts: Option<serde_json::Map<String, serde_json::Value>> = read(&file)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("scripts").and_then(|s| s.as_object()).cloned());
    let names: Vec<String> = if pm == "npm" || pm == "pnpm" {
        vec![
            format!("pre{script}"),
            script.to_string(),
            format!("post{script}"),
        ]
    } else {
        vec![script.to_string()]
    };
    for n in &names {
        let body = scripts
            .as_ref()
            .and_then(|s| s.get(n))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Hooks are only recorded when present; the main script is always recorded.
        if body.is_none() && n != script {
            continue;
        }
        out.push(ResolvedScript {
            kind: ScriptKind::PackageScript,
            file: file.clone(),
            name: Some(n.clone()),
            body,
        });
    }
}

fn is_yarn_builtin(s: &str) -> bool {
    matches!(
        s,
        "install"
            | "add"
            | "remove"
            | "upgrade"
            | "why"
            | "info"
            | "init"
            | "dlx"
            | "exec"
            | "workspace"
            | "workspaces"
            | "config"
            | "cache"
            | "set"
            | "plugin"
            | "node"
            | "bin"
            | "link"
            | "unlink"
            | "pack"
            | "publish"
            | "version"
            | "up"
            | "global"
    )
}

fn resolve_make(
    args: &[&str],
    cwd: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    out: &mut Vec<ResolvedScript>,
) {
    let mut dir = cwd.to_string();
    let mut makefile: Option<String> = None;
    let mut targets = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        match a {
            "-C" | "--directory" => {
                if let Some(d) = args.get(i + 1) {
                    dir = rel_join(&dir, d);
                }
                i += 1;
            }
            "-f" | "--file" | "--makefile" => {
                makefile = args.get(i + 1).map(|s| s.to_string());
                i += 1;
            }
            _ if a.starts_with("-C") && a.len() > 2 => dir = rel_join(&dir, &a[2..]),
            _ if a.starts_with('-') || a.contains('=') => {}
            _ => targets.push(a.to_string()),
        }
        i += 1;
    }
    let candidates: Vec<String> = match makefile {
        Some(f) => vec![rel_join(&dir, &f)],
        None => ["GNUmakefile", "makefile", "Makefile"]
            .iter()
            .map(|f| rel_join(&dir, f))
            .collect(),
    };
    let found = candidates
        .iter()
        .find_map(|f| read(f).map(|b| (f.clone(), b)));
    let Some((file, bytes)) = found else {
        out.push(ResolvedScript {
            kind: ScriptKind::Makefile,
            file: candidates.last().cloned().unwrap_or_default(),
            name: None,
            body: None,
        });
        return;
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    out.push(ResolvedScript {
        kind: ScriptKind::Makefile,
        file: file.clone(),
        name: None,
        body: Some(blake3::hash(&bytes).to_hex().to_string()),
    });
    if targets.is_empty()
        && let Some(first) = default_make_target(&text)
    {
        targets.push(first);
    }
    let mut seen = BTreeSet::new();
    for t in targets {
        collect_make_target(&text, &t, &file, 0, &mut seen, out);
    }
}

fn parse_rule_line(line: &str) -> Option<(Vec<&str>, &str)> {
    if line.starts_with('\t') || line.trim_start().starts_with('#') {
        return None;
    }
    let colon = line.find(':')?;
    let after = &line[colon + 1..];
    if after.starts_with('=') || line[..colon].ends_with(['?', '+', '!']) {
        return None; // `:=`, `::=`, `?=` style assignment
    }
    if line[..colon].contains('=') {
        return None;
    }
    let targets: Vec<&str> = line[..colon].split_whitespace().collect();
    if targets.is_empty() {
        return None;
    }
    let prereqs = after.trim_start_matches(':');
    let prereqs = prereqs.split(';').next().unwrap_or("");
    Some((targets, prereqs))
}

fn default_make_target(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (targets, _) = parse_rule_line(l)?;
        targets
            .into_iter()
            .find(|t| !t.starts_with('.') && !t.contains('%'))
            .map(str::to_string)
    })
}

fn collect_make_target(
    text: &str,
    target: &str,
    file: &str,
    depth: usize,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<ResolvedScript>,
) {
    if depth > 8 || !seen.insert(target.to_string()) {
        return;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut body = String::new();
    let mut prereqs = Vec::new();
    let mut found = false;
    let mut i = 0;
    while i < lines.len() {
        if let Some((targets, pre)) = parse_rule_line(lines[i])
            && targets.contains(&target)
        {
            found = true;
            body.push_str(lines[i]);
            body.push('\n');
            prereqs.extend(pre.split_whitespace().map(str::to_string));
            i += 1;
            while i < lines.len() && (lines[i].starts_with('\t') || lines[i].trim().is_empty()) {
                if lines[i].starts_with('\t') {
                    body.push_str(lines[i]);
                    body.push('\n');
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out.push(ResolvedScript {
        kind: ScriptKind::MakeTarget,
        file: file.to_string(),
        name: Some(target.to_string()),
        body: found.then_some(body),
    });
    for p in prereqs {
        // Only follow prerequisites that are themselves rules (not plain files).
        if lines
            .iter()
            .any(|l| parse_rule_line(l).is_some_and(|(t, _)| t.contains(&p.as_str())))
        {
            collect_make_target(text, &p, file, depth + 1, seen, out);
        }
    }
}

/// Resolve against the candidate's immutable tree (`git show <head_sha>:<path>`).
pub fn resolve_definition_at(
    subject: &ChangeSubject,
    spec: &CheckSpec,
) -> Result<CheckDefinition, CheckError> {
    let repo = PathBuf::from(&subject.repo.root);
    let sha = subject.content_sha().to_string();
    let read = move |rel: &str| -> Option<Vec<u8>> {
        let obj = format!("{sha}:{rel}");
        let out = gitcmd::run_raw(&repo, &["cat-file", "blob", &obj], gitcmd::GIT_TIMEOUT).ok()?;
        (out.code == Some(0)).then_some(out.stdout)
    };
    resolve_definition(spec, &read)
}

/// Resolve against files in a directory (e.g. the disposable checkout).
pub fn resolve_definition_in_dir(
    dir: &Path,
    spec: &CheckSpec,
) -> Result<CheckDefinition, CheckError> {
    let dir = dir.to_path_buf();
    resolve_definition(spec, &move |rel: &str| std::fs::read(dir.join(rel)).ok())
}

impl CheckDefinition {
    /// The spec this definition was resolved from.
    pub fn spec(&self) -> CheckSpec {
        CheckSpec {
            id: self.id.clone(),
            name: self.name.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            timeout_ms: self.timeout_ms,
            trust: self.trust,
        }
    }
}

/// Downgrade declared trust to `task_modified` when the definition resolved at the candidate
/// differs from the one resolved at the trusted baseline (or none existed there). A repo file
/// or agent suggestion cannot authorize itself.
pub fn classify_trust(
    declared: CheckTrust,
    at_baseline: Option<&str>,
    at_candidate: &str,
) -> CheckTrust {
    match declared {
        CheckTrust::TaskModified => CheckTrust::TaskModified,
        CheckTrust::User | CheckTrust::ProjectRecipe => {
            if at_baseline == Some(at_candidate) {
                declared
            } else {
                CheckTrust::TaskModified
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// One explicit action for one candidate subject and one definition digest.
    PerCandidate,
    /// A remembered recipe grant. Never authorizes host execution of task code.
    Recipe,
}

/// A recorded authorization decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckGrant {
    pub id: String,
    pub check_id: String,
    pub definition_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    pub scope: GrantScope,
    pub authorized_by: Actor,
    pub authorized_at_ms: i64,
    /// The user saw and confirmed [`HOST_RUN_LABEL`].
    pub acknowledged_task_code: bool,
}

/// Record the user's per-candidate host-run action.
pub fn grant_per_candidate(
    def: &CheckDefinition,
    subject: &ChangeSubject,
    actor: Actor,
    now_ms: i64,
) -> Result<CheckGrant, CheckError> {
    if actor.kind != ActorKind::User {
        return Err(CheckError::Invalid(
            "only a user can authorize verification".into(),
        ));
    }
    Ok(CheckGrant {
        id: new_id(),
        check_id: def.id.clone(),
        definition_digest: def.definition_digest.clone(),
        subject_id: Some(subject.id.clone()),
        scope: GrantScope::PerCandidate,
        authorized_by: actor,
        authorized_at_ms: now_ms,
        acknowledged_task_code: true,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AuthReason {
    /// No per-candidate action for this subject yet.
    NewCandidate,
    /// A grant exists for this subject but the definition digest changed since.
    DefinitionChanged { previous_digest: String },
    /// Only a remembered recipe grant exists; it cannot authorize task code on the host.
    RecipeGrantInsufficient,
    /// The resolved definition was supplied by the task's changes.
    TaskModifiedDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthRequirement {
    Authorized {
        grant_id: String,
    },
    /// Show command, machine, environment, scope and the [`HOST_RUN_LABEL`] action.
    Required {
        reasons: Vec<AuthReason>,
        confirmation_label: String,
    },
    /// Cannot run at all (e.g. not a committed candidate in T2).
    Unavailable {
        reason: String,
    },
}

/// Host runs always need a per-candidate grant naming this subject and this definition digest,
/// given by a user. (Contained auto-run is a later capability and is not modelled here.)
pub fn authorization_required(
    def: &CheckDefinition,
    subject: &ChangeSubject,
    prior_grants: &[CheckGrant],
) -> AuthRequirement {
    if !subject.is_immutable() {
        return AuthRequirement::Unavailable {
            reason: "Select a committed revision to verify".into(),
        };
    }
    if let Some(g) = prior_grants.iter().find(|g| {
        g.scope == GrantScope::PerCandidate
            && g.authorized_by.kind == ActorKind::User
            && g.acknowledged_task_code
            && g.check_id == def.id
            && g.subject_id.as_deref() == Some(subject.id.as_str())
            && g.definition_digest == def.definition_digest
    }) {
        return AuthRequirement::Authorized {
            grant_id: g.id.clone(),
        };
    }
    let mut reasons = Vec::new();
    if let Some(g) = prior_grants.iter().find(|g| {
        g.scope == GrantScope::PerCandidate
            && g.check_id == def.id
            && g.subject_id.as_deref() == Some(subject.id.as_str())
            && g.definition_digest != def.definition_digest
    }) {
        reasons.push(AuthReason::DefinitionChanged {
            previous_digest: g.definition_digest.clone(),
        });
    } else {
        reasons.push(AuthReason::NewCandidate);
    }
    if prior_grants
        .iter()
        .any(|g| g.scope == GrantScope::Recipe && g.check_id == def.id)
    {
        reasons.push(AuthReason::RecipeGrantInsufficient);
    }
    if def.trust == CheckTrust::TaskModified {
        reasons.push(AuthReason::TaskModifiedDefinition);
    }
    AuthRequirement::Required {
        reasons,
        confirmation_label: HOST_RUN_LABEL.into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
    Interrupted,
    Unknown,
}

impl CheckState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, CheckState::Queued | CheckState::Running)
    }
    /// `queued -> running -> passed|failed|cancelled|interrupted|unknown`; queued may also be
    /// cancelled/interrupted before starting. Terminal states never change.
    pub fn can_transition(self, to: CheckState) -> bool {
        use CheckState::*;
        matches!(
            (self, to),
            (Queued, Running)
                | (Queued, Cancelled | Interrupted)
                | (Running, Passed | Failed | Cancelled | Interrupted | Unknown)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutMethod {
    DisposableWorktree,
    ArchiveExport,
}

/// Explicit environment identity for a check run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentManifest {
    pub os: String,
    pub arch: String,
    pub runner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<CheckoutMethod>,
    /// Tool versions supplied by the caller (e.g. `node`, `cargo`).
    #[serde(default)]
    pub tool_versions: BTreeMap<String, String>,
    /// Names (never values) of environment variables visible to the check.
    #[serde(default)]
    pub env_var_names: Vec<String>,
    /// blake3 over os, arch, runner and tool versions. Env var names are recorded but excluded
    /// so that unrelated shell exports do not make evidence stale.
    pub digest: String,
}

impl EnvironmentManifest {
    pub fn new(
        runner: &str,
        checkout: Option<CheckoutMethod>,
        tool_versions: BTreeMap<String, String>,
        mut env_var_names: Vec<String>,
    ) -> Self {
        env_var_names.sort();
        env_var_names.dedup();
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let mut h = FieldHasher::new("vk-review/environment/v1");
        h.str(&os).str(&arch).str(runner);
        for (k, v) in &tool_versions {
            h.str(k).str(v);
        }
        EnvironmentManifest {
            digest: h.finish(),
            os,
            arch,
            runner: runner.to_string(),
            checkout,
            tool_versions,
            env_var_names,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckAuthorization {
    PerCandidate {
        subject_id: String,
        grant_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: String,
    pub check_id: String,
    pub definition_digest: String,
    pub definition_trust: CheckTrust,
    pub subject_id: String,
    pub head_sha: String,
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(default)]
    pub log_bytes: u64,
    #[serde(default)]
    pub log_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentManifest>,
    pub authorized_by: Actor,
    pub authorization: CheckAuthorization,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid check state transition {from:?} -> {to:?}")]
pub struct InvalidTransition {
    pub from: CheckState,
    pub to: CheckState,
}

impl CheckRun {
    /// A queued run for an authorized grant.
    pub fn queued(
        def: &CheckDefinition,
        subject: &ChangeSubject,
        grant: &CheckGrant,
        idempotency_key: impl Into<String>,
    ) -> Self {
        CheckRun {
            id: new_id(),
            check_id: def.id.clone(),
            definition_digest: def.definition_digest.clone(),
            definition_trust: def.trust,
            subject_id: subject.id.clone(),
            head_sha: subject.head_sha.clone(),
            state: CheckState::Queued,
            exit_code: None,
            signal: None,
            timed_out: false,
            started_at_ms: None,
            ended_at_ms: None,
            log_path: None,
            log_bytes: 0,
            log_truncated: false,
            environment: None,
            authorized_by: grant.authorized_by.clone(),
            authorization: CheckAuthorization::PerCandidate {
                subject_id: subject.id.clone(),
                grant_id: grant.id.clone(),
            },
            idempotency_key: idempotency_key.into(),
            error: None,
        }
    }

    pub fn transition(&mut self, to: CheckState) -> Result<(), InvalidTransition> {
        if !self.state.can_transition(to) {
            return Err(InvalidTransition {
                from: self.state,
                to,
            });
        }
        self.state = to;
        Ok(())
    }

    /// State to record for a run found non-terminal after a server restart when the runner
    /// cannot be reconciled: a never-started run is `interrupted`; a running one is `unknown`
    /// (it may have had side effects). Never relaunched automatically.
    pub fn state_after_unreconciled_restart(&self) -> CheckState {
        match self.state {
            CheckState::Queued => CheckState::Interrupted,
            CheckState::Running => CheckState::Unknown,
            s => s,
        }
    }
}

/// Cooperative cancellation for [`run_in_disposable_checkout`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Caller-scoped idempotency key recorded on the run.
    pub idempotency_key: String,
    /// Log bytes kept; further output is drained and discarded (`log_truncated`).
    pub max_log_bytes: u64,
    /// Tool versions for the environment manifest.
    pub tool_versions: BTreeMap<String, String>,
    /// Grace between SIGTERM and SIGKILL to the process group.
    pub kill_grace: Duration,
    /// Where to create the disposable checkout (default: the system temp dir).
    pub work_root: Option<PathBuf>,
    /// Skip `git worktree add` and export with `git archive` (diagnostics/tests).
    pub force_archive: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            idempotency_key: new_id(),
            max_log_bytes: 4 * 1024 * 1024,
            tool_versions: BTreeMap::new(),
            kill_grace: Duration::from_secs(2),
            work_root: None,
            force_archive: false,
        }
    }
}

/// A disposable checkout that removes itself on drop (also on panic). Never touches the user's
/// checkout: removal targets only the created path and its own worktree metadata.
struct Disposable {
    repo: PathBuf,
    path: PathBuf,
    method: CheckoutMethod,
}

impl Drop for Disposable {
    fn drop(&mut self) {
        if self.method == CheckoutMethod::DisposableWorktree {
            let p = self.path.to_string_lossy().into_owned();
            let _ = gitcmd::run_raw(
                &self.repo,
                &["worktree", "remove", "--force", "--force", &p],
                gitcmd::GIT_TIMEOUT,
            );
        }
        if self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn make_checkout(repo: &Path, sha: &str, opts: &RunOptions) -> Result<Disposable, CheckError> {
    let root = opts.work_root.clone().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root)?;
    let path = root.join(format!("vk-check-{}", new_id().to_lowercase()));
    if !opts.force_archive {
        let p = path.to_string_lossy().into_owned();
        let out = gitcmd::run_raw(
            repo,
            &["worktree", "add", "--detach", "--quiet", &p, sha],
            Duration::from_secs(300),
        )?;
        if out.code == Some(0) {
            return Ok(Disposable {
                repo: repo.to_path_buf(),
                path,
                method: CheckoutMethod::DisposableWorktree,
            });
        }
        // A failed add may leave a partial directory behind.
        let _ = std::fs::remove_dir_all(&path);
    }
    std::fs::create_dir_all(&path)?;
    let guard = Disposable {
        repo: repo.to_path_buf(),
        path: path.clone(),
        method: CheckoutMethod::ArchiveExport,
    };
    let mut archive = gitcmd::command(repo)
        .args(["archive", "--format=tar", sha])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = archive.stdout.take().expect("piped");
    let tar = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(&path)
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let a = archive.wait()?;
    if !a.success() || !tar.success() {
        return Err(CheckError::Checkout(format!(
            "git archive {sha} | tar failed (archive {a}, tar {tar})"
        )));
    }
    Ok(guard)
}

struct LogSink {
    file: std::fs::File,
    written: u64,
    max: u64,
    truncated: bool,
}

fn pump(mut src: impl Read + Send + 'static, sink: Arc<Mutex<LogSink>>, done: mpsc::Sender<()>) {
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut s = sink.lock().unwrap_or_else(|e| e.into_inner());
                    let room = s.max.saturating_sub(s.written) as usize;
                    let take = n.min(room);
                    if take > 0 && s.file.write_all(&buf[..take]).is_ok() {
                        s.written += take as u64;
                    }
                    if take < n {
                        s.truncated = true;
                    }
                }
            }
        }
        let _ = done.send(());
    });
}

fn kill_group(pgid: i32, sig: i32) {
    // SAFETY: kill(2) on the process group we created for the check.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

/// Run an authorized check against `subject` in a disposable checkout.
///
/// Blocking: call it from a blocking thread (e.g. `spawn_blocking`). Steps: verify the grant
/// (per-candidate, this subject, this digest) → `git worktree add --detach <tmp> <head_sha>`
/// (falling back to a `git archive` export) → re-resolve the definition inside the checkout and
/// refuse on digest mismatch → run in its own process group with the definition's timeout,
/// bounded log capture to `log_dir/<run id>.log`, `VIBEKE_*` variables removed → kill the whole
/// group on timeout/cancel and after exit → remove the disposable checkout.
///
/// Terminal states: exit 0 → `passed`; non-zero exit, crash signal, timeout or spawn failure →
/// `failed` (with `timed_out`/`error`); cancellation → `cancelled` (partial log kept, never a
/// fabricated pass/fail); killed from outside by SIGTERM/SIGKILL/SIGHUP/SIGINT → `interrupted`.
pub fn run_in_disposable_checkout(
    repo: &Path,
    subject: &ChangeSubject,
    def: &CheckDefinition,
    grant: &CheckGrant,
    log_dir: &Path,
    cancel: &CancelToken,
    opts: &RunOptions,
) -> Result<CheckRun, CheckError> {
    match authorization_required(def, subject, std::slice::from_ref(grant)) {
        AuthRequirement::Authorized { .. } => {}
        other => return Err(CheckError::NotAuthorized(other)),
    }
    validate_rel(&def.cwd)?;
    let mut run = CheckRun::queued(def, subject, grant, opts.idempotency_key.clone());
    if cancel.is_cancelled() {
        run.transition(CheckState::Cancelled)
            .expect("queued→cancelled");
        return Ok(run);
    }

    let checkout = make_checkout(repo, subject.content_sha(), opts)?;
    let check_dir = checkout.path.clone();
    let resolved_here = resolve_definition_in_dir(&check_dir, &def.spec())?;
    if resolved_here.definition_digest != def.definition_digest {
        return Err(CheckError::DefinitionMismatch);
    }

    std::fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join(format!("{}.log", run.id));
    let sink = Arc::new(Mutex::new(LogSink {
        file: std::fs::File::create(&log_path)?,
        written: 0,
        max: opts.max_log_bytes,
        truncated: false,
    }));
    run.log_path = Some(log_path.to_string_lossy().into_owned());

    let cwd = check_dir.join(&def.cwd);
    let mut cmd = match &def.command {
        CheckCommand::Argv(v) => {
            let prog = &v[0];
            let mut c = if prog.contains('/') && !Path::new(prog).is_absolute() {
                Command::new(cwd.join(prog))
            } else {
                Command::new(prog)
            };
            c.args(&v[1..]);
            c
        }
        CheckCommand::Shell(s) => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(s);
            c
        }
    };
    let mut env_names: Vec<String> = Vec::new();
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if k.starts_with("VIBEKE_") || k.starts_with("HERDR_") {
            cmd.env_remove(&k);
        } else {
            env_names.push(k);
        }
    }
    cmd.envs(&def.env);
    env_names.extend(def.env.keys().cloned());
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    run.environment = Some(EnvironmentManifest::new(
        "host",
        Some(checkout.method),
        opts.tool_versions.clone(),
        env_names,
    ));

    run.transition(CheckState::Running).expect("queued→running");
    run.started_at_ms = Some(now_ms());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            run.transition(CheckState::Failed).expect("running→failed");
            run.error = Some(format!("failed to start: {e}"));
            run.ended_at_ms = Some(now_ms());
            return Ok(run);
        }
    };
    let pgid = child.id() as i32;
    let (done_tx, done_rx) = mpsc::channel();
    pump(
        child.stdout.take().expect("piped"),
        sink.clone(),
        done_tx.clone(),
    );
    pump(child.stderr.take().expect("piped"), sink.clone(), done_tx);

    let deadline = Instant::now() + Duration::from_millis(def.timeout_ms.max(1));
    let mut cancelled = false;
    let mut timed_out = false;
    let mut term_sent: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                run.error = Some(format!("wait failed: {e}"));
                kill_group(pgid, libc::SIGKILL);
                break child.wait().ok();
            }
        }
        if term_sent.is_none() {
            if cancel.is_cancelled() {
                cancelled = true;
            } else if Instant::now() >= deadline {
                timed_out = true;
            }
            if cancelled || timed_out {
                kill_group(pgid, libc::SIGTERM);
                term_sent = Some(Instant::now());
            }
        } else if term_sent.is_some_and(|t| t.elapsed() >= opts.kill_grace) {
            kill_group(pgid, libc::SIGKILL);
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Reap stragglers left in the group (background servers etc.).
    kill_group(pgid, libc::SIGKILL);
    // Readers end when every writer closed; don't hang on a process that escaped the group.
    let wait_until = Instant::now() + Duration::from_secs(2);
    for _ in 0..2 {
        let left = wait_until.saturating_duration_since(Instant::now());
        if done_rx.recv_timeout(left).is_err() {
            break;
        }
    }
    {
        let s = sink.lock().unwrap_or_else(|e| e.into_inner());
        run.log_bytes = s.written;
        run.log_truncated = s.truncated;
        let _ = s.file.sync_all();
    }
    run.ended_at_ms = Some(now_ms());
    run.timed_out = timed_out;
    let state = match status {
        None => CheckState::Unknown,
        Some(_) if cancelled => CheckState::Cancelled,
        Some(_) if timed_out => {
            run.error = Some(format!("timed out after {} ms", def.timeout_ms));
            CheckState::Failed
        }
        Some(st) => {
            run.exit_code = st.code();
            run.signal = st.signal();
            match (st.code(), st.signal()) {
                (Some(0), _) => CheckState::Passed,
                (Some(_), _) => CheckState::Failed,
                (None, Some(sig))
                    if [libc::SIGTERM, libc::SIGKILL, libc::SIGHUP, libc::SIGINT]
                        .contains(&sig) =>
                {
                    CheckState::Interrupted
                }
                (None, Some(_)) => CheckState::Failed,
                (None, None) => CheckState::Unknown,
            }
        }
    };
    run.transition(state).expect("running→terminal");
    drop(checkout);
    Ok(run)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    /// Prose or an unconfirmed report; never a passed check.
    AgentClaim,
    /// A bound tool/process execution observed through the adapter or pane tracking.
    Observed,
    /// A configured check launched by Vibeke against a stable captured subject.
    VibekeVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecordSource {
    /// Structured tool-call record from the harness adapter (Turn/Item).
    NativeTool,
    /// Process observed by pane process tracking.
    PaneProcess,
    /// The agent's prose (e.g. "tests pass").
    Prose,
}

/// Adapter input for the observed-command table.
///
/// One record per tool call / observed process / prose claim. `command` is the exact command
/// line for shell-like tools (`None` for non-command tools such as file edits, which produce no
/// row). Missing native fields stay `None` and are shown as unknown. `established_subject` is set
/// **only** when the collector/runner itself established the stable execution subject for the
/// whole execution interval (T4); quiet file watchers or equal start/end hashes do not qualify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecord {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    pub item_id: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    pub source: ToolRecordSource,
    /// For prose: the claim text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub established_subject: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutcome {
    Passed,
    Failed,
    Unknown,
}

/// A row of the observed-command table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedCommand {
    pub id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// Subject established for the execution; `None` = code binding unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    pub category: CommandCategory,
    pub outcome: CommandOutcome,
}

impl ObservedCommand {
    /// Build a row from an adapter record. Returns `None` for non-command tool calls.
    pub fn from_tool_record(rec: &ToolRecord) -> Option<ObservedCommand> {
        let (command, category) = match rec.source {
            ToolRecordSource::Prose => (
                rec.text.clone().or_else(|| rec.command.clone())?,
                CommandCategory::AgentClaim,
            ),
            ToolRecordSource::NativeTool | ToolRecordSource::PaneProcess => {
                (rec.command.clone()?, CommandCategory::Observed)
            }
        };
        let outcome = match (category, rec.exit_code) {
            (CommandCategory::AgentClaim, _) => CommandOutcome::Unknown,
            (_, Some(0)) => CommandOutcome::Passed,
            (_, Some(_)) => CommandOutcome::Failed,
            (_, None) => CommandOutcome::Unknown,
        };
        Some(ObservedCommand {
            id: rec.item_id.clone(),
            command,
            cwd: rec.cwd.clone(),
            exit_code: if category == CommandCategory::AgentClaim {
                None
            } else {
                rec.exit_code
            },
            started_at_ms: rec.started_at_ms,
            ended_at_ms: rec.ended_at_ms,
            run_id: rec.run_id.clone(),
            turn: rec.turn,
            subject_id: if category == CommandCategory::AgentClaim {
                None
            } else {
                rec.established_subject.clone()
            },
            category,
            outcome,
        })
    }

    /// A Vibeke verification row from a terminal check run.
    pub fn from_check_run(run: &CheckRun, def: &CheckDefinition) -> ObservedCommand {
        let command = match &def.command {
            CheckCommand::Argv(v) => v.join(" "),
            CheckCommand::Shell(s) => s.clone(),
        };
        ObservedCommand {
            id: run.id.clone(),
            command,
            cwd: Some(def.cwd.clone()),
            exit_code: run.exit_code,
            started_at_ms: run.started_at_ms,
            ended_at_ms: run.ended_at_ms,
            run_id: run.id.clone(),
            turn: None,
            subject_id: Some(run.subject_id.clone()),
            category: CommandCategory::VibekeVerification,
            outcome: match run.state {
                CheckState::Passed => CommandOutcome::Passed,
                CheckState::Failed => CommandOutcome::Failed,
                _ => CommandOutcome::Unknown,
            },
        }
    }

    /// Display label for the table's result column.
    pub fn label(&self) -> String {
        match (self.category, self.outcome, self.subject_id.is_some()) {
            (CommandCategory::AgentClaim, _, _) => "Agent claim · not a check".into(),
            (_, CommandOutcome::Passed, false) => "Command passed · code binding unverified".into(),
            (_, CommandOutcome::Failed, false) => "Command failed · code binding unverified".into(),
            (_, CommandOutcome::Unknown, false) => {
                "Result unknown · code binding unverified".into()
            }
            (CommandCategory::VibekeVerification, CommandOutcome::Passed, true) => {
                "Vibeke verification passed".into()
            }
            (CommandCategory::VibekeVerification, CommandOutcome::Failed, true) => {
                "Vibeke verification failed".into()
            }
            (_, CommandOutcome::Passed, true) => "Command passed".into(),
            (_, CommandOutcome::Failed, true) => "Command failed".into(),
            (_, CommandOutcome::Unknown, true) => "Result unknown".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::testrepo::TestRepo;
    use crate::subject::{DirtyState, RepoIdentity, SubjectKind, capture_committed};

    fn spec(cmd: CheckCommand) -> CheckSpec {
        CheckSpec {
            id: "test".into(),
            name: "Unit tests".into(),
            command: cmd,
            cwd: String::new(),
            env: BTreeMap::new(),
            timeout_ms: 60_000,
            trust: CheckTrust::ProjectRecipe,
        }
    }

    fn argv(s: &str) -> CheckCommand {
        CheckCommand::Argv(s.split_whitespace().map(str::to_string).collect())
    }

    fn files(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<Vec<u8>> + use<> {
        let m: BTreeMap<String, Vec<u8>> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect();
        move |p: &str| m.get(p).cloned()
    }

    #[test]
    fn package_script_change_changes_digest_with_same_argv() {
        let a = files(&[("package.json", r#"{"scripts":{"test":"vitest run"}}"#)]);
        let b = files(&[("package.json", r#"{"scripts":{"test":"exit 0"}}"#)]);
        for cmd in [
            "npm run test",
            "pnpm run test",
            "yarn test",
            "bun run test",
            "npm test",
        ] {
            let da = resolve_definition(&spec(argv(cmd)), &a).unwrap();
            let db = resolve_definition(&spec(argv(cmd)), &b).unwrap();
            assert_ne!(da.definition_digest, db.definition_digest, "{cmd}");
            assert!(
                da.resolved
                    .iter()
                    .any(|r| r.body.as_deref() == Some("vitest run")),
                "{cmd}: {:?}",
                da.resolved
            );
        }
        // Unrelated package.json changes (deps) do not change the definition digest.
        let c = files(&[(
            "package.json",
            r#"{"dependencies":{"x":"1"},"scripts":{"test":"vitest run"}}"#,
        )]);
        assert_eq!(
            resolve_definition(&spec(argv("npm run test")), &a)
                .unwrap()
                .definition_digest,
            resolve_definition(&spec(argv("npm run test")), &c)
                .unwrap()
                .definition_digest
        );
        // Adding a pretest hook changes it.
        let d = files(&[(
            "package.json",
            r#"{"scripts":{"pretest":"curl evil","test":"vitest run"}}"#,
        )]);
        assert_ne!(
            resolve_definition(&spec(argv("npm run test")), &a)
                .unwrap()
                .definition_digest,
            resolve_definition(&spec(argv("npm run test")), &d)
                .unwrap()
                .definition_digest
        );
    }

    #[test]
    fn package_json_in_cwd_and_missing_script() {
        let mut s = spec(argv("npm run lint"));
        s.cwd = "web".into();
        let r = files(&[("web/package.json", r#"{"scripts":{"lint":"eslint ."}}"#)]);
        let d = resolve_definition(&s, &r).unwrap();
        assert_eq!(d.resolved[0].file, "web/package.json");
        assert_eq!(d.resolved[0].body.as_deref(), Some("eslint ."));
        let missing = resolve_definition(&s, &files(&[])).unwrap();
        assert_eq!(missing.resolved[0].body, None);
        assert_ne!(missing.definition_digest, d.definition_digest);
    }

    #[test]
    fn make_target_resolution() {
        let mk = "CC=cc\n\nall: build\n\nbuild: deps\n\t$(CC) main.c\n\ndeps:\n\techo deps\n\ntest: build\n\t./run-tests\n\t@echo done\n";
        let r = files(&[("Makefile", mk)]);
        let d = resolve_definition(&spec(argv("make test")), &r).unwrap();
        let target = d
            .resolved
            .iter()
            .find(|x| x.kind == ScriptKind::MakeTarget && x.name.as_deref() == Some("test"))
            .unwrap();
        assert_eq!(
            target.body.as_deref(),
            Some("test: build\n\t./run-tests\n\t@echo done\n")
        );
        // Prerequisite rules are followed recursively.
        assert!(
            d.resolved
                .iter()
                .any(|x| x.name.as_deref() == Some("build"))
        );
        assert!(d.resolved.iter().any(|x| x.name.as_deref() == Some("deps")));
        // A variable change elsewhere in the Makefile changes the digest.
        let r2 = files(&[("Makefile", &mk.replace("CC=cc", "CC=evil"))]);
        assert_ne!(
            d.definition_digest,
            resolve_definition(&spec(argv("make test")), &r2)
                .unwrap()
                .definition_digest
        );
        // Default goal.
        let d = resolve_definition(&spec(argv("make")), &r).unwrap();
        assert!(d.resolved.iter().any(|x| x.name.as_deref() == Some("all")));
        // -C dir
        let r3 = files(&[("sub/Makefile", "t:\n\techo hi\n")]);
        let d = resolve_definition(&spec(argv("make -C sub t")), &r3).unwrap();
        assert_eq!(d.resolved[0].file, "sub/Makefile");
        assert_eq!(d.resolved[1].body.as_deref(), Some("t:\n\techo hi\n"));
    }

    #[test]
    fn script_files_and_shell_commands() {
        let a = files(&[("ci/test.sh", "cargo test")]);
        let b = files(&[("ci/test.sh", "true")]);
        for cmd in ["./ci/test.sh", "bash ci/test.sh"] {
            assert_ne!(
                resolve_definition(&spec(argv(cmd)), &a)
                    .unwrap()
                    .definition_digest,
                resolve_definition(&spec(argv(cmd)), &b)
                    .unwrap()
                    .definition_digest,
                "{cmd}"
            );
        }
        let simple = resolve_definition(
            &spec(CheckCommand::Shell("npm run test".into())),
            &files(&[("package.json", r#"{"scripts":{"test":"x"}}"#)]),
        )
        .unwrap();
        assert!(simple.resolution_complete);
        assert_eq!(simple.resolved.len(), 1);
        let complex = resolve_definition(
            &spec(CheckCommand::Shell("npm test && make".into())),
            &files(&[]),
        )
        .unwrap();
        assert!(!complex.resolution_complete);
        // Bad cwd.
        let mut s = spec(argv("true"));
        s.cwd = "../outside".into();
        assert!(resolve_definition(&s, &files(&[])).is_err());
        s.cwd = "/etc".into();
        assert!(resolve_definition(&s, &files(&[])).is_err());
    }

    #[test]
    fn trust_downgrades_on_task_modified_definition() {
        assert_eq!(
            classify_trust(CheckTrust::ProjectRecipe, Some("a"), "a"),
            CheckTrust::ProjectRecipe
        );
        assert_eq!(
            classify_trust(CheckTrust::ProjectRecipe, Some("a"), "b"),
            CheckTrust::TaskModified
        );
        assert_eq!(
            classify_trust(CheckTrust::User, None, "b"),
            CheckTrust::TaskModified
        );
    }

    fn subj(head: &str, kind: SubjectKind) -> ChangeSubject {
        ChangeSubject::new(
            RepoIdentity {
                root: "/r".into(),
                origin_url: None,
            },
            "base".into(),
            head.into(),
            None,
            DirtyState::Clean,
            kind,
            0,
        )
    }

    #[test]
    fn host_runs_need_per_candidate_auth_and_renewal_on_change() {
        let r = files(&[("package.json", r#"{"scripts":{"test":"vitest"}}"#)]);
        let def = resolve_definition(&spec(argv("npm test")), &r).unwrap();
        let s1 = subj("h1", SubjectKind::Committed);
        let s2 = subj("h2", SubjectKind::Committed);
        // Nothing granted.
        assert!(matches!(
            authorization_required(&def, &s1, &[]),
            AuthRequirement::Required { ref reasons, ref confirmation_label }
                if reasons == &[AuthReason::NewCandidate] && confirmation_label == HOST_RUN_LABEL
        ));
        // Agents can't grant.
        assert!(grant_per_candidate(&def, &s1, Actor::agent("run1"), 0).is_err());
        let g = grant_per_candidate(&def, &s1, Actor::user("demo"), 0).unwrap();
        assert!(matches!(
            authorization_required(&def, &s1, std::slice::from_ref(&g)),
            AuthRequirement::Authorized { .. }
        ));
        // Grant does not carry over to a new candidate.
        assert!(matches!(
            authorization_required(&def, &s2, std::slice::from_ref(&g)),
            AuthRequirement::Required { .. }
        ));
        // Same argv, edited script → renewed authorization.
        let r2 = files(&[("package.json", r#"{"scripts":{"test":"true"}}"#)]);
        let mut def2 = resolve_definition(&spec(argv("npm test")), &r2).unwrap();
        def2.trust = classify_trust(
            def2.trust,
            Some(&def.definition_digest),
            &def2.definition_digest,
        );
        match authorization_required(&def2, &s1, std::slice::from_ref(&g)) {
            AuthRequirement::Required { reasons, .. } => {
                assert!(reasons.contains(&AuthReason::DefinitionChanged {
                    previous_digest: def.definition_digest.clone()
                }));
                assert!(reasons.contains(&AuthReason::TaskModifiedDefinition));
            }
            other => panic!("{other:?}"),
        }
        // A remembered recipe grant never authorizes host task code.
        let recipe = CheckGrant {
            scope: GrantScope::Recipe,
            subject_id: None,
            ..g.clone()
        };
        match authorization_required(&def, &s2, &[recipe]) {
            AuthRequirement::Required { reasons, .. } => {
                assert!(reasons.contains(&AuthReason::RecipeGrantInsufficient))
            }
            other => panic!("{other:?}"),
        }
        // Live subjects cannot be verified in T2.
        assert!(matches!(
            authorization_required(&def, &subj("h1", SubjectKind::CheckoutLive), &[g]),
            AuthRequirement::Unavailable { .. }
        ));
    }

    #[test]
    fn state_machine() {
        use CheckState::*;
        assert!(Queued.can_transition(Running));
        assert!(Running.can_transition(Passed));
        assert!(!Queued.can_transition(Passed));
        assert!(!Passed.can_transition(Failed));
        assert!(!Failed.can_transition(Passed));
        assert!(Cancelled.is_terminal() && !Running.is_terminal());
    }

    #[test]
    fn observed_commands_categories_and_labels() {
        let base = ToolRecord {
            run_id: "run1".into(),
            turn: Some(3),
            item_id: "i1".into(),
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            cwd: Some("/r".into()),
            exit_code: Some(0),
            started_at_ms: Some(1),
            ended_at_ms: Some(2),
            source: ToolRecordSource::NativeTool,
            text: None,
            established_subject: None,
        };
        let o = ObservedCommand::from_tool_record(&base).unwrap();
        assert_eq!(o.category, CommandCategory::Observed);
        assert_eq!(o.outcome, CommandOutcome::Passed);
        assert_eq!(o.label(), "Command passed · code binding unverified");
        let claim = ObservedCommand::from_tool_record(&ToolRecord {
            source: ToolRecordSource::Prose,
            text: Some("All tests pass".into()),
            established_subject: Some("s".into()),
            ..base.clone()
        })
        .unwrap();
        assert_eq!(claim.category, CommandCategory::AgentClaim);
        assert_eq!(claim.outcome, CommandOutcome::Unknown);
        assert_eq!(claim.subject_id, None);
        assert!(
            ObservedCommand::from_tool_record(&ToolRecord {
                command: None,
                tool: "Edit".into(),
                ..base.clone()
            })
            .is_none()
        );
        let missing = ObservedCommand::from_tool_record(&ToolRecord {
            exit_code: None,
            ..base
        })
        .unwrap();
        assert_eq!(missing.outcome, CommandOutcome::Unknown);
    }

    fn committed_repo(script: &str) -> (TestRepo, ChangeSubject) {
        let r = TestRepo::new();
        r.write("README", "x");
        r.commit("base");
        r.write("ci/check.sh", script);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            r.root().join("ci/check.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        r.commit("head");
        let s = capture_committed(r.root(), "HEAD~1", "HEAD").unwrap();
        (r, s)
    }

    fn run_check(
        r: &TestRepo,
        s: &ChangeSubject,
        cmd: CheckCommand,
        timeout_ms: u64,
        cancel: &CancelToken,
        opts: RunOptions,
    ) -> CheckRun {
        let mut sp = spec(cmd);
        sp.timeout_ms = timeout_ms;
        let def = resolve_definition_at(s, &sp).unwrap();
        let g = grant_per_candidate(&def, s, Actor::user("demo"), 0).unwrap();
        let logs = r.dir.path().join(".vk-logs");
        run_in_disposable_checkout(r.root(), s, &def, &g, &logs, cancel, &opts).unwrap()
    }

    fn worktree_count(r: &TestRepo) -> usize {
        r.git(&["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }

    #[test]
    fn disposable_run_passes_and_cleans_up_without_touching_checkout() {
        let (r, s) = committed_repo("#!/bin/sh\necho hello from $(pwd)\ntest -f README\n");
        // Dirty the user's checkout; the run must not see or touch it.
        r.write("README", "user edit");
        r.write("untracked.txt", "mine");
        let work = tempfile::tempdir().unwrap();
        let opts = RunOptions {
            work_root: Some(work.path().to_path_buf()),
            tool_versions: [("sh".to_string(), "posix".to_string())].into(),
            ..Default::default()
        };
        let run = run_check(
            &r,
            &s,
            argv("./ci/check.sh"),
            30_000,
            &CancelToken::new(),
            opts,
        );
        assert_eq!(run.state, CheckState::Passed, "{run:?}");
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.subject_id, s.id);
        let env = run.environment.as_ref().unwrap();
        assert_eq!(env.checkout, Some(CheckoutMethod::DisposableWorktree));
        assert!(env.env_var_names.iter().any(|n| n == "PATH"));
        let log = std::fs::read_to_string(run.log_path.as_ref().unwrap()).unwrap();
        assert!(log.contains("hello from"), "{log}");
        assert!(
            log.contains("vk-check-"),
            "ran in the disposable checkout: {log}"
        );
        // Cleaned up: only the main worktree remains and the temp root is empty.
        assert_eq!(worktree_count(&r), 1);
        assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
        // User's checkout untouched.
        assert_eq!(
            std::fs::read_to_string(r.root().join("README")).unwrap(),
            "user edit"
        );
        assert!(r.root().join("untracked.txt").exists());
    }

    #[test]
    fn failing_check_and_archive_fallback() {
        let (r, s) = committed_repo("#!/bin/sh\necho failing >&2\nexit 3\n");
        let work = tempfile::tempdir().unwrap();
        let run = run_check(
            &r,
            &s,
            argv("./ci/check.sh"),
            30_000,
            &CancelToken::new(),
            RunOptions {
                work_root: Some(work.path().to_path_buf()),
                force_archive: true,
                ..Default::default()
            },
        );
        assert_eq!(run.state, CheckState::Failed);
        assert_eq!(run.exit_code, Some(3));
        assert_eq!(
            run.environment.unwrap().checkout,
            Some(CheckoutMethod::ArchiveExport)
        );
        assert_eq!(std::fs::read_dir(work.path()).unwrap().count(), 0);
    }

    #[test]
    fn timeout_kills_process_group_and_log_is_bounded() {
        let (r, s) =
            committed_repo("#!/bin/sh\nhead -c 100000 /dev/zero | tr '\\0' x\nsleep 30 &\nwait\n");
        let started = Instant::now();
        let run = run_check(
            &r,
            &s,
            argv("./ci/check.sh"),
            // Long enough for the 100 kB of output to land first, even on a loaded host.
            5000,
            &CancelToken::new(),
            RunOptions {
                max_log_bytes: 1000,
                kill_grace: Duration::from_millis(200),
                ..Default::default()
            },
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(run.state, CheckState::Failed);
        assert!(run.timed_out);
        assert!(run.log_truncated);
        assert_eq!(run.log_bytes, 1000);
        assert_eq!(worktree_count(&r), 1);
    }

    #[test]
    fn cancel_records_cancelled_not_failure() {
        let (r, s) = committed_repo("#!/bin/sh\necho partial\nsleep 30\n");
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let logs = r.dir.path().join(".vk-logs");
        // Cancel once the check has demonstrably produced output.
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let seen = std::fs::read_dir(&logs)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|e| {
                        std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains("partial"))
                    });
                if seen {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            c2.cancel();
        });
        let run = run_check(
            &r,
            &s,
            argv("./ci/check.sh"),
            60_000,
            &cancel,
            RunOptions {
                kill_grace: Duration::from_millis(200),
                ..Default::default()
            },
        );
        assert_eq!(run.state, CheckState::Cancelled);
        assert_eq!(run.exit_code, None);
        let log = std::fs::read_to_string(run.log_path.unwrap()).unwrap();
        assert!(log.contains("partial"));
        assert_eq!(worktree_count(&r), 1);
    }

    #[test]
    fn unauthorized_or_mismatched_definition_refused() {
        let (r, s) = committed_repo("#!/bin/sh\nexit 0\n");
        let def = resolve_definition_at(&s, &spec(argv("./ci/check.sh"))).unwrap();
        let other = subj("zzz", SubjectKind::Committed);
        let g = grant_per_candidate(&def, &other, Actor::user("demo"), 0).unwrap();
        let logs = r.dir.path().join(".vk-logs");
        assert!(matches!(
            run_in_disposable_checkout(
                r.root(),
                &s,
                &def,
                &g,
                &logs,
                &CancelToken::new(),
                &RunOptions::default()
            ),
            Err(CheckError::NotAuthorized(_))
        ));
        // A definition digest computed from a different tree than the subject's is refused.
        let mut forged = def.clone();
        forged.definition_digest = "forged".into();
        let g = grant_per_candidate(&forged, &s, Actor::user("demo"), 0).unwrap();
        assert!(matches!(
            run_in_disposable_checkout(
                r.root(),
                &s,
                &forged,
                &g,
                &logs,
                &CancelToken::new(),
                &RunOptions::default()
            ),
            Err(CheckError::DefinitionMismatch)
        ));
        assert_eq!(worktree_count(&r), 1);
    }

    #[test]
    fn vibeke_verification_row() {
        let (r, s) = committed_repo("#!/bin/sh\nexit 0\n");
        let sp = spec(argv("./ci/check.sh"));
        let def = resolve_definition_at(&s, &sp).unwrap();
        let run = run_check(
            &r,
            &s,
            argv("./ci/check.sh"),
            30_000,
            &CancelToken::new(),
            RunOptions::default(),
        );
        let row = ObservedCommand::from_check_run(&run, &def);
        assert_eq!(row.category, CommandCategory::VibekeVerification);
        assert_eq!(row.subject_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(row.label(), "Vibeke verification passed");
    }
}
