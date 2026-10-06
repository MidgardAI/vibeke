//! Hook installers for Claude Code and Codex, plus the Codex PATH shim.
//! Normative rules: spec 04 §11 (idempotent, merge-only, backup before first
//! modification, atomic write with change detection, stable command paths).
//!
//! ## File formats and assumptions
//!
//! * **Claude** `<claude>/settings.json`:
//!   `{"hooks": {"<Event>": [ {"matcher"?: "*", "hooks": [ {"type":"command",
//!   "command": "...", "timeout"?: N} ], "_vibeke": "vibeke-integration=claude@<ver>"} ]}}`.
//!   The marker lives on the matcher group. Claude's settings schema ignores
//!   unknown keys (assumption, documented in the lead report); if that proves
//!   wrong, switch [`CLAUDE_MARKER_KEY`] handling to command-based detection
//!   like Codex.
//! * **Codex** `<codex>/hooks.json`: ASSUMED to have the same shape as Claude's
//!   (`{"hooks": {"<Event>": [{"matcher"?, "hooks": [{type, command, timeout}]}]}}`).
//!   No marker key is written: Codex trust is keyed on the exact hook
//!   definition and an unknown key could change or invalidate it. Vibeke
//!   entries are recognised by their command (`<bin> hook codex <Event>`).
//!   Trust is read from `<codex>/config.toml` `[hooks.state."<file>:<event>:<i>:<j>"]`;
//!   an entry (table) that does not set `trusted = false`/`enabled = false`
//!   counts as trusted. `<event>` is tried as given and in snake_case.

use crate::fingerprint::Fnv64;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CLAUDE_MARKER_KEY: &str = "_vibeke";
pub const GATE_TIMEOUT_SECS: u64 = 1800;
pub const CODEX_TRUST_INSTRUCTION: &str = "run /hooks in Codex once to trust the Vibeke hooks";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Harness {
    Claude,
    Codex,
    /// pi (`@earendil-works/pi-coding-agent`): extension file, see [`EXTENSION_BUNDLE`].
    Pi,
    /// omp (`@oh-my-pi/pi-coding-agent`): extension file.
    Omp,
}

impl Harness {
    pub fn id(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Omp => "omp",
        }
    }

    /// Installed as a single extension file rather than merged into a JSON config.
    fn is_extension(self) -> bool {
        matches!(self, Harness::Pi | Harness::Omp)
    }
}

#[derive(Debug, Clone)]
pub struct Dirs {
    pub claude: PathBuf,
    pub codex: PathBuf,
    /// pi root (`~/.pi`); the agent dir is `<pi>/agent`.
    pub pi: PathBuf,
    /// omp root (`~/.omp`); the agent dir is `<omp>/agent`.
    pub omp: PathBuf,
}

impl Dirs {
    /// `CLAUDE_CONFIG_DIR` (default `~/.claude`), `CODEX_HOME` (default `~/.codex`),
    /// `PI_CODING_AGENT_DIR` (pi's agent dir, default `~/.pi/agent`; honoured when it ends in
    /// `agent`, in which case its parent is the pi root), omp `~/.omp`.
    pub fn from_env() -> Dirs {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let pick = |var: &str, default: &str| match std::env::var_os(var) {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => home.join(default),
        };
        Dirs {
            claude: pick("CLAUDE_CONFIG_DIR", ".claude"),
            codex: pick("CODEX_HOME", ".codex"),
            // VIBEKE_PI_HOME / VIBEKE_OMP_HOME redirect to scratch copies (tests, pre-switch-over).
            pi: match (
                std::env::var_os("VIBEKE_PI_HOME"),
                std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from),
            ) {
                (Some(root), _) => PathBuf::from(root),
                (None, Some(a))
                    if a.file_name().is_some_and(|n| n == "agent") && a.parent().is_some() =>
                {
                    a.parent().map(Path::to_path_buf).unwrap_or_default()
                }
                _ => home.join(".pi"),
            },
            omp: std::env::var_os("VIBEKE_OMP_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".omp")),
        }
    }
    pub fn config_file(&self, h: Harness) -> PathBuf {
        match h {
            Harness::Claude => self.claude.join("settings.json"),
            Harness::Codex => self.codex.join("hooks.json"),
            Harness::Pi => self.pi.join("agent/extensions/vibeke/index.js"),
            Harness::Omp => self.omp.join("agent/extensions/vibeke.js"),
        }
    }
}

// ---------------------------------------------------------------------------
// Hook definitions
// ---------------------------------------------------------------------------

struct EventSpec {
    name: &'static str,
    matcher: Option<&'static str>,
    timeout: Option<u64>,
}

const fn ev(name: &'static str, matcher: Option<&'static str>, timeout: Option<u64>) -> EventSpec {
    EventSpec {
        name,
        matcher,
        timeout,
    }
}

const G: Option<u64> = Some(GATE_TIMEOUT_SECS);
const STAR: Option<&str> = Some("*");

const CLAUDE_EVENTS: &[EventSpec] = &[
    ev("SessionStart", None, None),
    ev("UserPromptSubmit", None, None),
    ev("PreToolUse", STAR, G),
    ev("PermissionRequest", STAR, G),
    ev("PermissionDenied", None, None),
    ev("PostToolUse", STAR, None),
    ev("PostToolUseFailure", STAR, None),
    ev("Notification", None, None),
    ev("Stop", None, None),
    ev("StopFailure", None, None),
    ev("SubagentStart", None, None),
    ev("SubagentStop", None, None),
    ev("PreCompact", None, None),
    ev("PostCompact", None, None),
    ev("SessionEnd", None, None),
];

const CODEX_EVENTS: &[EventSpec] = &[
    ev("SessionStart", None, None),
    ev("UserPromptSubmit", None, None),
    ev("PreToolUse", Some("Bash"), G),
    ev("PermissionRequest", None, G),
    ev("PostToolUse", None, None),
    ev("Stop", None, None),
    ev("SessionEnd", None, None),
    ev("SubagentStart", None, None),
    ev("SubagentStop", None, None),
    ev("PreCompact", None, None),
    ev("PostCompact", None, None),
];

fn events(h: Harness) -> &'static [EventSpec] {
    match h {
        Harness::Claude => CLAUDE_EVENTS,
        Harness::Codex => CODEX_EVENTS,
        Harness::Pi | Harness::Omp => &[],
    }
}

fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-+@:=,~".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// `<bin> hook <harness> <Event>`. No version in it: Codex trust is keyed on
/// the exact definition.
fn command_for(h: Harness, bin: &Path, event: &str) -> String {
    format!(
        "{} hook {} {}",
        shell_quote(&bin.to_string_lossy()),
        h.id(),
        event
    )
}

fn marker(h: Harness) -> String {
    format!("vibeke-integration={}@{}", h.id(), VERSION)
}

fn desired_group(h: Harness, bin: &Path, spec: &EventSpec) -> Value {
    let mut hook = Map::new();
    hook.insert("type".into(), json!("command"));
    hook.insert("command".into(), json!(command_for(h, bin, spec.name)));
    if let Some(t) = spec.timeout {
        hook.insert("timeout".into(), json!(t));
    }
    let mut g = Map::new();
    if let Some(m) = spec.matcher {
        g.insert("matcher".into(), json!(m));
    }
    g.insert("hooks".into(), json!([Value::Object(hook)]));
    if h == Harness::Claude {
        g.insert(CLAUDE_MARKER_KEY.into(), json!(marker(h)));
    }
    Value::Object(g)
}

fn hook_commands(group: &Value) -> impl Iterator<Item = &str> {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
}

fn is_ours_command(h: Harness, cmd: &str) -> bool {
    cmd.contains(&format!(" hook {} ", h.id()))
}

/// Group carries the Claude marker.
fn claude_marked(group: &Value) -> bool {
    group
        .get(CLAUDE_MARKER_KEY)
        .and_then(Value::as_str)
        .is_some_and(|m| m.starts_with("vibeke-integration=claude@"))
}

fn is_ours_group(h: Harness, group: &Value) -> bool {
    match h {
        Harness::Claude => claude_marked(group),
        Harness::Codex => {
            let mut it = hook_commands(group).peekable();
            it.peek().is_some() && it.all(|c| is_ours_command(h, c))
        }
        Harness::Pi | Harness::Omp => false,
    }
}

fn is_foreign_command(cmd: &str) -> bool {
    cmd.contains("herdr-agent-state") || cmd.contains("herdr-omp-agent-state")
}

/// Codex has no marker: remove Vibeke hook objects that sit in a group shared
/// with user hooks (so they are not duplicated on install). Returns whether
/// anything changed.
fn strip_mixed_codex(group: &mut Value) -> bool {
    let h = Harness::Codex;
    let Some(arr) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
        return false;
    };
    let has_ours = arr.iter().any(|x| {
        x.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| is_ours_command(h, c))
    });
    let has_other = arr.iter().any(|x| {
        !x.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| is_ours_command(h, c))
    });
    if !(has_ours && has_other) {
        return false;
    }
    arr.retain(|x| {
        !x.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| is_ours_command(h, c))
    });
    true
}

// ---------------------------------------------------------------------------
// Plan / apply
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanKind {
    Install,
    Uninstall,
}

/// Identity of a file as read: used to detect concurrent modification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: Option<SystemTime>,
    pub len: u64,
    pub hash: u64,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    /// Contents when planned (`None` = file did not exist).
    pub before: Option<String>,
    /// Contents to write (`None` = nothing to write; only for a missing file
    /// on uninstall).
    pub after: Option<String>,
    pub stamp: Option<FileStamp>,
    /// Delete the file (uninstall of a managed single-file extension).
    pub remove: bool,
}

impl FileChange {
    pub fn changed(&self) -> bool {
        self.remove || (self.before.as_deref() != self.after.as_deref() && self.after.is_some())
    }

    /// Plain line diff (`-`/`+`/` ` prefixed) for `--dry-run`.
    pub fn diff(&self) -> String {
        let a: Vec<&str> = self.before.as_deref().unwrap_or("").lines().collect();
        let b: Vec<&str> = self.after.as_deref().unwrap_or("").lines().collect();
        let mut out = format!("--- {}\n+++ {}\n", self.path.display(), self.path.display());
        let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                out.push_str(&format!(" {}\n", a[i]));
                i += 1;
                j += 1;
            } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
                out.push_str(&format!("+{}\n", b[j]));
                j += 1;
            } else {
                out.push_str(&format!("-{}\n", a[i]));
                i += 1;
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub harness: Harness,
    pub kind: PlanKind,
    pub files: Vec<FileChange>,
    /// Instructions for the user (e.g. Codex hook trust).
    pub notes: Vec<String>,
}

impl Plan {
    pub fn changed(&self) -> bool {
        self.files.iter().any(FileChange::changed)
    }
}

fn stamp_of(path: &Path, content: &[u8]) -> FileStamp {
    let mut h = Fnv64::new();
    h.write(content);
    FileStamp {
        mtime: fs::metadata(path).and_then(|m| m.modified()).ok(),
        len: content.len() as u64,
        hash: h.0,
    }
}

pub(crate) fn read_file(path: &Path) -> Result<Option<(String, FileStamp)>> {
    match fs::read(path) {
        Ok(bytes) => {
            let stamp = stamp_of(path, &bytes);
            let s = String::from_utf8(bytes)
                .map_err(|_| anyhow!("{} is not valid UTF-8", path.display()))?;
            Ok(Some((s, stamp)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub(crate) fn parse_root(path: &Path, text: Option<&str>) -> Result<Value> {
    match text {
        None => Ok(Value::Object(Map::new())),
        Some(t) if t.trim().is_empty() => Ok(Value::Object(Map::new())),
        Some(t) => {
            let v: Value = serde_json::from_str(t).with_context(|| {
                format!(
                    "{} is not valid JSON; refusing to modify it",
                    path.display()
                )
            })?;
            if !v.is_object() {
                bail!(
                    "{} is not a JSON object; refusing to modify it",
                    path.display()
                );
            }
            Ok(v)
        }
    }
}

fn detect_indent(text: &str) -> Vec<u8> {
    for line in text.lines().skip(1) {
        let ws: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if !ws.is_empty() && ws.len() < line.len() {
            return ws.into_bytes();
        }
    }
    b"  ".to_vec()
}

pub(crate) fn render(root: &Value, original: Option<&str>) -> String {
    use serde_json::ser::{PrettyFormatter, Serializer};
    let indent = original
        .map(detect_indent)
        .unwrap_or_else(|| b"  ".to_vec());
    let mut buf = Vec::new();
    let fmt = PrettyFormatter::with_indent(&indent);
    let mut ser = Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(root, &mut ser).expect("serialize Value");
    let mut s = String::from_utf8(buf).expect("utf8");
    if original.is_none_or(|o| o.is_empty() || o.ends_with('\n')) {
        s.push('\n');
    }
    s
}

pub fn plan_install(h: Harness, dirs: &Dirs, vibeke_bin: &Path) -> Result<Plan> {
    plan(h, dirs, Some(vibeke_bin))
}

pub fn plan_uninstall(h: Harness, dirs: &Dirs) -> Result<Plan> {
    plan(h, dirs, None)
}

fn plan(h: Harness, dirs: &Dirs, bin: Option<&Path>) -> Result<Plan> {
    if h.is_extension() {
        return plan_extension(h, dirs, bin.is_some());
    }
    let path = resolve_symlink(&dirs.config_file(h));
    let existing = read_file(&path)?;
    let (text, stamp) = match &existing {
        Some((t, s)) => (Some(t.as_str()), Some(s.clone())),
        None => (None, None),
    };
    let original = parse_root(&path, text)?;
    let mut root = original.clone();
    let changed_any = match bin {
        Some(bin) => install_into(h, &mut root, bin, &path)?,
        None => uninstall_from(h, &mut root, &path)?,
    };
    let after = if bin.is_none() && existing.is_none() {
        None
    } else if !changed_any || root == original {
        // Semantically nothing to do: keep the user's bytes (or create the file).
        match text {
            Some(t) => Some(t.to_string()),
            None => Some(render(&root, None)),
        }
    } else {
        Some(render(&root, text))
    };
    let mut notes = vec![];
    if bin.is_some() && h == Harness::Codex {
        notes.push(CODEX_TRUST_INSTRUCTION.to_string());
        if codex_features_hooks_disabled(&dirs.codex.join("config.toml")) {
            notes.push(
                "config.toml sets features.hooks = false; enable it or Codex will ignore hooks.json"
                    .to_string(),
            );
        }
    }
    Ok(Plan {
        harness: h,
        kind: if bin.is_some() {
            PlanKind::Install
        } else {
            PlanKind::Uninstall
        },
        files: vec![FileChange {
            path,
            before: existing.map(|(t, _)| t),
            after,
            stamp,
            remove: false,
        }],
        notes,
    })
}

pub(crate) fn resolve_symlink(p: &Path) -> PathBuf {
    match fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
        }
        _ => p.to_path_buf(),
    }
}

fn hooks_obj<'a>(root: &'a mut Value, path: &Path) -> Result<&'a mut Map<String, Value>> {
    let obj = root.as_object_mut().expect("checked object");
    if !obj.contains_key("hooks") {
        obj.insert("hooks".into(), Value::Object(Map::new()));
    }
    obj.get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("{}: \"hooks\" is not an object", path.display()))
}

fn install_into(h: Harness, root: &mut Value, bin: &Path, path: &Path) -> Result<bool> {
    let before = root.clone();
    let hooks = hooks_obj(root, path)?;
    // Stale managed entries on events we no longer install.
    let stale: Vec<String> = hooks
        .keys()
        .filter(|k| !events(h).iter().any(|e| e.name == k.as_str()))
        .cloned()
        .collect();
    for k in stale {
        remove_ours_from_event(h, hooks, &k, path)?;
    }
    for spec in events(h) {
        let want = desired_group(h, bin, spec);
        let arr = match hooks
            .entry(spec.name)
            .or_insert_with(|| Value::Array(vec![]))
        {
            Value::Array(a) => a,
            _ => bail!("{}: hooks.{} is not an array", path.display(), spec.name),
        };
        if h == Harness::Codex {
            arr.iter_mut().for_each(|g| {
                strip_mixed_codex(g);
            });
        }
        let ours: Vec<usize> = arr
            .iter()
            .enumerate()
            .filter(|(_, g)| is_ours_group(h, g))
            .map(|(i, _)| i)
            .collect();
        match ours.split_first() {
            None => arr.push(want),
            Some((first, rest)) => {
                arr[*first] = want;
                for i in rest.iter().rev() {
                    arr.remove(*i);
                }
            }
        }
    }
    Ok(*root != before)
}

fn remove_ours_from_event(
    h: Harness,
    hooks: &mut Map<String, Value>,
    event: &str,
    path: &Path,
) -> Result<bool> {
    let Some(v) = hooks.get_mut(event) else {
        return Ok(false);
    };
    let Value::Array(arr) = v else {
        return Ok(false); // not ours to complain about on uninstall
    };
    let _ = path;
    let n = arr.len();
    let mut modified = false;
    if h == Harness::Codex {
        for g in arr.iter_mut() {
            modified |= strip_mixed_codex(g);
        }
    }
    arr.retain(|g| !is_ours_group(h, g));
    modified |= arr.len() != n;
    if modified && arr.is_empty() {
        hooks.shift_remove(event);
    }
    Ok(modified)
}

fn uninstall_from(h: Harness, root: &mut Value, path: &Path) -> Result<bool> {
    let obj = root.as_object_mut().expect("checked object");
    let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let keys: Vec<String> = hooks.keys().cloned().collect();
    let mut any = false;
    for k in keys {
        any |= remove_ours_from_event(h, hooks, &k, path)?;
    }
    if any && hooks.is_empty() {
        obj.shift_remove("hooks");
    }
    Ok(any)
}

/// Apply a plan. Returns every path written (backups included).
pub fn apply(plan: &Plan) -> Result<Vec<PathBuf>> {
    let mut written = vec![];
    for f in &plan.files {
        if !f.changed() {
            continue;
        }
        // Abort if the file changed since planning.
        let now = read_file(&f.path)?.map(|(_, s)| s);
        if now != f.stamp {
            bail!(
                "{} changed since it was read; aborting (re-run to retry)",
                f.path.display()
            );
        }
        if f.remove {
            // Managed file: its content is reproducible, no backup needed.
            fs::remove_file(&f.path).with_context(|| format!("removing {}", f.path.display()))?;
            if let Some(parent) = f.path.parent()
                && parent.file_name().is_some_and(|n| n == "vibeke")
            {
                let _ = fs::remove_dir(parent); // pi: only if empty
            }
            written.push(f.path.clone());
            continue;
        }
        let after = f.after.as_deref().expect("changed implies after");
        if let Some(parent) = f.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(before) = &f.before
            && let Some(b) = ensure_backup(&f.path, before)?
        {
            written.push(b);
        }
        atomic_write(&f.path, after.as_bytes(), 0o644)?;
        written.push(f.path.clone());
    }
    Ok(written)
}

/// Write `<file>.vibeke-bak-<ts>` unless a Vibeke backup already exists.
fn ensure_backup(path: &Path, content: &str) -> Result<Option<PathBuf>> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("file name")?;
    let prefix = format!("{name}.vibeke-bak-");
    let dir = path.parent().unwrap_or(Path::new("."));
    for e in fs::read_dir(dir)? {
        if e?.file_name().to_string_lossy().starts_with(&prefix) {
            return Ok(None);
        }
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let b = dir.join(format!("{prefix}{ts}"));
    atomic_write(&b, content.as_bytes(), 0o600)?;
    Ok(Some(b))
}

fn atomic_write(path: &Path, bytes: &[u8], default_mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("file name")?;
    let tmp = dir.join(format!(".{name}.vibeke-tmp-{}", std::process::id()));
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(default_mode);
    let res = (|| -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", path.display()))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    NotInstalled,
    Partial,
    Installed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookStatus {
    pub event: String,
    /// Index of the matcher group within the event array and of the hook
    /// within the group (the `<i>:<j>` of Codex's trust key).
    pub group: usize,
    pub index: usize,
    pub command: String,
    /// Claude: version from the marker. Codex: `None`.
    pub version: Option<String>,
    /// Codex only.
    pub trust: Option<Trust>,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub harness: Harness,
    pub file: PathBuf,
    pub file_exists: bool,
    pub state: InstallState,
    pub hooks: Vec<HookStatus>,
    pub missing_events: Vec<String>,
    /// Herdr (or other foreign) entries found, as `"<Event>: <command>"`. Left alone.
    pub foreign: Vec<String>,
    /// Parse problems and other warnings.
    pub problems: Vec<String>,
    /// Steps the user still has to take.
    pub todo: Vec<String>,
}

pub fn status(h: Harness, dirs: &Dirs) -> Status {
    if h.is_extension() {
        return status_extension(h, dirs);
    }
    let file = dirs.config_file(h);
    let mut st = Status {
        harness: h,
        file: file.clone(),
        file_exists: false,
        state: InstallState::NotInstalled,
        hooks: vec![],
        missing_events: vec![],
        foreign: vec![],
        problems: vec![],
        todo: vec![],
    };
    let root = match read_file(&file) {
        Ok(None) => {
            st.missing_events = events(h).iter().map(|e| e.name.to_string()).collect();
            return st;
        }
        Ok(Some((t, _))) => {
            st.file_exists = true;
            match parse_root(&file, Some(&t)) {
                Ok(v) => v,
                Err(e) => {
                    st.problems.push(format!("{e:#}"));
                    st.missing_events = events(h).iter().map(|e| e.name.to_string()).collect();
                    return st;
                }
            }
        }
        Err(e) => {
            st.problems.push(format!("{e:#}"));
            return st;
        }
    };
    let trust_table =
        (h == Harness::Codex).then(|| read_trust_table(&dirs.codex.join("config.toml")));
    let hooks = root.get("hooks").and_then(Value::as_object);
    for spec in events(h) {
        let groups = hooks
            .and_then(|m| m.get(spec.name))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut found = false;
        for (gi, g) in groups.iter().enumerate() {
            let hs = g
                .get("hooks")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let group_ours = h == Harness::Claude && claude_marked(g);
            for (ji, hook) in hs.iter().enumerate() {
                let cmd = hook.get("command").and_then(Value::as_str).unwrap_or("");
                let ours = match h {
                    Harness::Claude => group_ours,
                    Harness::Codex => is_ours_command(h, cmd),
                    Harness::Pi | Harness::Omp => false,
                };
                if ours {
                    found = true;
                    st.hooks.push(HookStatus {
                        event: spec.name.to_string(),
                        group: gi,
                        index: ji,
                        command: cmd.to_string(),
                        version: g
                            .get(CLAUDE_MARKER_KEY)
                            .and_then(Value::as_str)
                            .and_then(|m| m.split_once('@'))
                            .map(|(_, v)| v.to_string()),
                        trust: trust_table
                            .as_ref()
                            .map(|t| trust_of(t.as_ref(), &file, spec.name, gi, ji)),
                    });
                }
            }
        }
        if !found {
            st.missing_events.push(spec.name.to_string());
        }
    }
    // Foreign entries across every event, ours-excluded.
    if let Some(m) = hooks {
        for (event, v) in m {
            for g in v.as_array().into_iter().flatten() {
                for cmd in hook_commands(g) {
                    if is_foreign_command(cmd) && !is_ours_command(h, cmd) {
                        st.foreign.push(format!("{event}: {cmd}"));
                    }
                }
            }
        }
    }
    st.state = if st.hooks.is_empty() {
        InstallState::NotInstalled
    } else if st.missing_events.is_empty() {
        InstallState::Installed
    } else {
        InstallState::Partial
    };
    if h == Harness::Codex {
        let untrusted = st
            .hooks
            .iter()
            .filter(|x| x.trust == Some(Trust::Untrusted))
            .count();
        if untrusted > 0 {
            st.todo.push(format!(
                "{untrusted} Vibeke hook(s) untrusted: {CODEX_TRUST_INSTRUCTION}"
            ));
        }
        if codex_features_hooks_disabled(&dirs.codex.join("config.toml")) {
            st.problems
                .push("config.toml sets features.hooks = false".to_string());
        }
    }
    st
}

fn read_trust_table(config: &Path) -> Option<toml::Table> {
    let text = fs::read_to_string(config).ok()?;
    let t: toml::Table = text.parse().ok()?;
    t.get("hooks")?.get("state")?.as_table().cloned()
}

fn trust_of(table: Option<&toml::Table>, file: &Path, event: &str, i: usize, j: usize) -> Trust {
    let Some(table) = table else {
        return Trust::Untrusted;
    };
    let mut files = vec![file.to_string_lossy().to_string()];
    if let Ok(c) = fs::canonicalize(file) {
        files.push(c.to_string_lossy().to_string());
    }
    let snake = snake_case(event);
    for f in &files {
        for ev in [event, snake.as_str()] {
            if let Some(entry) = table.get(&format!("{f}:{ev}:{i}:{j}")) {
                let off = |k: &str| entry.get(k).and_then(toml::Value::as_bool) == Some(false);
                return if off("trusted") || off("enabled") {
                    Trust::Untrusted
                } else {
                    Trust::Trusted
                };
            }
        }
    }
    Trust::Untrusted
}

fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn codex_features_hooks_disabled(config: &Path) -> bool {
    fs::read_to_string(config)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("features")?.get("hooks")?.as_bool())
        == Some(false)
}

// ---------------------------------------------------------------------------
// pi / omp extension (integrations/pi-extension)
// ---------------------------------------------------------------------------
//
// ASSUMPTIONS (documented, not verified against a live pi/omp here):
// * pi and omp load plain `.js` (ESM) extension files: pi from
//   `<pi>/agent/extensions/<name>/index.js` (or `<name>.js`), omp from
//   `<omp>/agent/extensions/<name>.js`. The bundle is a single self-contained ES module
//   (`export default function (pi)`), runnable under Node and Bun.
// * The file starts with a one-line managed header; a file without it is never overwritten.
// * Other files in the extensions dir (e.g. Herdr's `herdr-omp-agent-state.ts`) are left alone
//   and listed as foreign in `status`.

/// The built `integrations/pi-extension/dist/vibeke.js` (committed so Rust builds don't need bun).
pub const EXTENSION_BUNDLE: &str =
    include_str!("../../../integrations/pi-extension/dist/vibeke.js");

const EXTENSION_MARKER_PREFIX: &str = "// managed by vibeke (vibeke-integration=";

fn extension_header(h: Harness) -> String {
    format!(
        "{EXTENSION_MARKER_PREFIX}{}@{VERSION}); reinstall overwrites\n",
        h.id()
    )
}

/// `Some(version)` when `text` starts with Vibeke's managed header for `h`.
fn extension_marker_version(h: Harness, text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let rest = first.strip_prefix(EXTENSION_MARKER_PREFIX)?;
    let rest = rest.strip_prefix(&format!("{}@", h.id()))?;
    let (v, tail) = rest.split_once(')')?;
    tail.starts_with(';').then(|| v.to_string())
}

fn extension_content(h: Harness) -> String {
    format!("{}{}", extension_header(h), EXTENSION_BUNDLE)
}

fn plan_extension(h: Harness, dirs: &Dirs, install: bool) -> Result<Plan> {
    let path = resolve_symlink(&dirs.config_file(h));
    let existing = read_file(&path)?;
    let (before, stamp) = match existing {
        Some((t, s)) => (Some(t), Some(s)),
        None => (None, None),
    };
    let ours = before
        .as_deref()
        .is_some_and(|t| extension_marker_version(h, t).is_some());
    if before.is_some() && !ours {
        bail!(
            "{} exists and is not managed by Vibeke; refusing to {} it",
            path.display(),
            if install { "overwrite" } else { "remove" }
        );
    }
    let (after, remove) = if install {
        (Some(extension_content(h)), false)
    } else {
        (None, ours)
    };
    let mut notes = vec![];
    if install && h == Harness::Pi {
        notes.push(
            "pi loads the extension from ~/.pi/agent/extensions/vibeke/index.js (restart pi or /reload)"
                .to_string(),
        );
    }
    if install && h == Harness::Omp {
        notes.push(
            "omp loads the extension from ~/.omp/agent/extensions/vibeke.js (restart omp)"
                .to_string(),
        );
    }
    Ok(Plan {
        harness: h,
        kind: if install {
            PlanKind::Install
        } else {
            PlanKind::Uninstall
        },
        files: vec![FileChange {
            path,
            before,
            after,
            stamp,
            remove,
        }],
        notes,
    })
}

fn status_extension(h: Harness, dirs: &Dirs) -> Status {
    let file = dirs.config_file(h);
    let mut st = Status {
        harness: h,
        file: file.clone(),
        file_exists: false,
        state: InstallState::NotInstalled,
        hooks: vec![],
        missing_events: vec!["extension".to_string()],
        foreign: vec![],
        problems: vec![],
        todo: vec![],
    };
    match read_file(&file) {
        Ok(None) => {}
        Ok(Some((text, _))) => {
            st.file_exists = true;
            match extension_marker_version(h, &text) {
                Some(version) => {
                    st.missing_events.clear();
                    st.state = InstallState::Installed;
                    if version != VERSION {
                        st.todo.push(format!(
                            "integration_outdated: installed {version}, this build bundles {VERSION}; reinstall"
                        ));
                    } else if text != extension_content(h) {
                        st.todo.push(
                            "extension content differs from this build; reinstall".to_string(),
                        );
                    }
                    st.hooks.push(HookStatus {
                        event: "extension".to_string(),
                        group: 0,
                        index: 0,
                        command: file.to_string_lossy().to_string(),
                        version: Some(version),
                        trust: None,
                    });
                }
                None => st.problems.push(format!(
                    "{} exists but is not managed by Vibeke (install will refuse to overwrite it)",
                    file.display()
                )),
            }
        }
        Err(e) => st.problems.push(format!("{e:#}")),
    }
    // Everything else in the extensions dir is foreign (Herdr's herdr-omp-agent-state.ts, user extensions).
    let ext_dir = match h {
        Harness::Pi => dirs.pi.join("agent/extensions"),
        _ => dirs.omp.join("agent/extensions"),
    };
    let own = match h {
        Harness::Pi => "vibeke",
        _ => "vibeke.js",
    };
    if let Ok(rd) = fs::read_dir(&ext_dir) {
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != own && !n.contains(".vibeke-") && !n.starts_with('.'))
            .collect();
        names.sort();
        st.foreign = names
            .into_iter()
            .map(|n| format!("extension: {n}"))
            .collect();
    }
    st
}

// ---------------------------------------------------------------------------
// Codex PATH shim (spec 04 §6.2)
// ---------------------------------------------------------------------------

const CODEX_SHIM: &str = r##"#!/bin/sh
# vibeke-shim=codex
# Generated by `vibeke integration install codex`. Do not edit; it is rewritten
# byte-for-byte on every install.
#
# Runs the real `codex` (found later in PATH) with a per-pane embedded
# app-server so hooks carry VIBEKE_PANE_ID / VIBEKE_PANE_TOKEN.
# Opt out per invocation with CODEX_VIBEKE_SHIM=0.

# Extra arguments injected before the user's arguments (placeholder: the exact
# flag/config key is being verified against Codex 0.157).
VIBEKE_CODEX_EXTRA="--disable daemon_auto_start"

self_dir=$(cd "$(dirname "$0")" 2>/dev/null && pwd -P)

real=""
old_ifs=$IFS
IFS=:
for d in $PATH; do
    [ -n "$d" ] || d=.
    resolved=$(cd "$d" 2>/dev/null && pwd -P) || continue
    [ "$resolved" = "$self_dir" ] && continue
    if [ -f "$d/codex" ] && [ -x "$d/codex" ]; then
        real="$d/codex"
        break
    fi
done
IFS=$old_ifs

if [ -z "$real" ]; then
    echo "vibeke codex shim: real 'codex' not found in PATH" >&2
    exit 127
fi

if [ "${CODEX_VIBEKE_SHIM:-1}" = "0" ]; then
    exec "$real" "$@"
fi

# Leave daemon-related invocations alone: the user already chose a mode, or is
# talking to the daemon / app-server directly.
for a in "$@"; do
    case "$a" in
        --*daemon*|daemon_*|features.daemon*) exec "$real" "$@" ;;
    esac
done

set -f
case "${1:-}" in
    # Subcommands that take the extra flags after the subcommand name.
    exec|resume|fork|review)
        sub=$1
        shift
        exec "$real" "$sub" $VIBEKE_CODEX_EXTRA "$@"
        ;;
    # Management subcommands: pass through untouched.
    app-server|login|logout|mcp|proxy|completion|help|features|sandbox|apply|cloud|debug)
        exec "$real" "$@"
        ;;
    *)
        exec "$real" $VIBEKE_CODEX_EXTRA "$@"
        ;;
esac
"##;

/// Write `<shim_dir>/codex` (mode 0755). Byte-stable and idempotent.
pub fn write_codex_shim(shim_dir: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(shim_dir)?;
    let path = shim_dir.join("codex");
    let up_to_date = fs::read_to_string(&path).is_ok_and(|c| c == CODEX_SHIM)
        && fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if !up_to_date {
        atomic_write(&path, CODEX_SHIM.as_bytes(), 0o755)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const BIN: &str = "/home/u/.local/bin/vibeke";

    fn dirs(t: &tempfile::TempDir) -> Dirs {
        let d = Dirs {
            claude: t.path().join("claude"),
            codex: t.path().join("codex"),
            pi: t.path().join("pi"),
            omp: t.path().join("omp"),
        };
        fs::create_dir_all(&d.claude).unwrap();
        fs::create_dir_all(&d.codex).unwrap();
        d
    }

    /// Re-render a fixture the way the installer would, so byte-identity
    /// round trips can be asserted on hand-written compact JSON.
    fn pretty(text: &str) -> String {
        let v: Value = serde_json::from_str(text).unwrap();
        render(&v, Some(text))
    }

    fn install(h: Harness, d: &Dirs) -> Plan {
        let p = plan_install(h, d, Path::new(BIN)).unwrap();
        apply(&p).unwrap();
        p
    }

    fn json_of(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".vibeke-bak-"))
            .collect()
    }

    const USER_CLAUDE: &str = r#"{
    "model": "opus",
    "permissions": {"allow": ["Bash(ls:*)"], "deny": []},
    "hooks": {
        "Stop": [
            {"hooks": [{"type": "command", "command": "~/.claude/hooks/notify.sh", "weird": 1}]},
            {"hooks": [{"type": "command", "command": "/x/herdr-agent-state.sh stop"}]}
        ],
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "/x/guard.sh"}]}
        ],
        "CustomEvent": [{"hooks": []}]
    },
    "zzz": [3, 2, 1]
}
"#;

    #[test]
    fn claude_fresh_install_shape() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let p = install(Harness::Claude, &d);
        assert!(p.changed());
        let v = json_of(&d.claude.join("settings.json"));
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 15);
        let pre = &hooks["PreToolUse"][0];
        assert_eq!(pre["matcher"], "*");
        assert_eq!(pre["hooks"][0]["timeout"], 1800);
        assert_eq!(pre["hooks"][0]["type"], "command");
        assert_eq!(
            pre["hooks"][0]["command"],
            format!("{BIN} hook claude PreToolUse")
        );
        assert_eq!(
            pre["_vibeke"],
            format!("vibeke-integration=claude@{VERSION}")
        );
        assert_eq!(hooks["PermissionRequest"][0]["hooks"][0]["timeout"], 1800);
        assert!(hooks["PostToolUse"][0].get("matcher").is_some());
        assert!(hooks["PostToolUseFailure"][0].get("matcher").is_some());
        for e in [
            "SessionStart",
            "Stop",
            "SessionEnd",
            "Notification",
            "PermissionDenied",
        ] {
            assert!(hooks[e][0].get("matcher").is_none(), "{e}");
            assert!(hooks[e][0]["hooks"][0].get("timeout").is_none(), "{e}");
        }
    }

    #[test]
    fn claude_merge_preserves_user_and_herdr() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(&f, USER_CLAUDE).unwrap();
        install(Harness::Claude, &d);
        let v = json_of(&f);
        let orig: Value = serde_json::from_str(USER_CLAUDE).unwrap();
        // Top-level key order preserved.
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["model", "permissions", "hooks", "zzz"]);
        assert_eq!(v["model"], orig["model"]);
        assert_eq!(v["permissions"], orig["permissions"]);
        assert_eq!(v["zzz"], orig["zzz"]);
        // User entries intact and first.
        assert_eq!(v["hooks"]["Stop"][0], orig["hooks"]["Stop"][0]);
        assert_eq!(v["hooks"]["Stop"][1], orig["hooks"]["Stop"][1]);
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 3);
        assert_eq!(v["hooks"]["PreToolUse"][0], orig["hooks"]["PreToolUse"][0]);
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["CustomEvent"], orig["hooks"]["CustomEvent"]);
        // Indentation of the original (4 spaces) kept.
        let text = fs::read_to_string(&f).unwrap();
        assert!(text.contains("\n    \"model\""));
        assert!(text.ends_with("}\n"));
        // Status: installed, herdr foreign.
        let st = status(Harness::Claude, &d);
        assert_eq!(st.state, InstallState::Installed);
        assert_eq!(st.foreign.len(), 1);
        assert!(st.foreign[0].starts_with("Stop: /x/herdr-agent-state.sh"));
        assert!(st.missing_events.is_empty());
    }

    #[test]
    fn claude_reinstall_is_byte_identical_and_backup_once() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(&f, USER_CLAUDE).unwrap();
        let p1 = plan_install(Harness::Claude, &d, Path::new(BIN)).unwrap();
        let w1 = apply(&p1).unwrap();
        assert_eq!(w1.len(), 2, "backup + file");
        let b = backups(&d.claude);
        assert_eq!(b.len(), 1);
        assert_eq!(fs::read_to_string(&b[0]).unwrap(), USER_CLAUDE);
        let after1 = fs::read(&f).unwrap();
        let p2 = plan_install(Harness::Claude, &d, Path::new(BIN)).unwrap();
        assert!(!p2.changed());
        assert!(apply(&p2).unwrap().is_empty());
        assert_eq!(fs::read(&f).unwrap(), after1);
        assert_eq!(backups(&d.claude).len(), 1);
        // A later real modification (different bin) does not add a second backup.
        let p3 = plan_install(Harness::Claude, &d, Path::new("/other/vibeke")).unwrap();
        assert!(p3.changed());
        apply(&p3).unwrap();
        assert_eq!(backups(&d.claude).len(), 1);
        assert!(
            fs::read_to_string(&f)
                .unwrap()
                .contains("/other/vibeke hook claude Stop")
        );
        assert!(!fs::read_to_string(&f).unwrap().contains(BIN));
    }

    #[test]
    fn no_backup_for_new_file() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        install(Harness::Claude, &d);
        assert!(backups(&d.claude).is_empty());
    }

    #[test]
    fn claude_uninstall_restores_original_semantics() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        let user = pretty(USER_CLAUDE);
        fs::write(&f, &user).unwrap();
        install(Harness::Claude, &d);
        let p = plan_uninstall(Harness::Claude, &d).unwrap();
        assert!(p.changed());
        apply(&p).unwrap();
        // Byte-identical to the original (indent and newline detection included).
        assert_eq!(fs::read_to_string(&f).unwrap(), user);
        let st = status(Harness::Claude, &d);
        assert_eq!(st.state, InstallState::NotInstalled);
        assert_eq!(st.foreign.len(), 1);
    }

    #[test]
    fn uninstall_noop_keeps_bytes() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(&f, "{ \"a\":1,\n \"hooks\" : {} }").unwrap();
        let p = plan_uninstall(Harness::Claude, &d).unwrap();
        assert!(!p.changed());
        assert_eq!(
            p.files[0].after.as_deref(),
            Some("{ \"a\":1,\n \"hooks\" : {} }")
        );
        // Missing file: nothing to write, nothing created.
        let t2 = tempfile::tempdir().unwrap();
        let d2 = dirs(&t2);
        let p = plan_uninstall(Harness::Claude, &d2).unwrap();
        assert!(!p.changed());
        assert!(apply(&p).unwrap().is_empty());
        assert!(!d2.claude.join("settings.json").exists());
    }

    #[test]
    fn uninstall_removes_only_marked_entries() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        // User hook with a command that merely resembles ours but has no marker.
        fs::write(
            &f,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/me/vibeke hook claude Stop"}]}]}}"#,
        )
        .unwrap();
        install(Harness::Claude, &d);
        assert_eq!(json_of(&f)["hooks"]["Stop"].as_array().unwrap().len(), 2);
        apply(&plan_uninstall(Harness::Claude, &d).unwrap()).unwrap();
        let v = json_of(&f);
        assert_eq!(v["hooks"].as_object().unwrap().len(), 1);
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/me/vibeke hook claude Stop"
        );
    }

    #[test]
    fn install_dedups_and_drops_stale_marked_entries() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(
            &f,
            r#"{"hooks":{
              "Stop":[{"hooks":[{"type":"command","command":"old"}],"_vibeke":"vibeke-integration=claude@0.0.1"},
                      {"hooks":[{"type":"command","command":"user"}]},
                      {"hooks":[{"type":"command","command":"old2"}],"_vibeke":"vibeke-integration=claude@0.0.1"}],
              "CwdChanged":[{"hooks":[{"type":"command","command":"gone"}],"_vibeke":"vibeke-integration=claude@0.0.1"}]}}"#,
        )
        .unwrap();
        install(Harness::Claude, &d);
        let v = json_of(&f);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0]["_vibeke"],
            format!("vibeke-integration=claude@{VERSION}")
        );
        assert_eq!(stop[1]["hooks"][0]["command"], "user");
        assert!(v["hooks"].get("CwdChanged").is_none());
    }

    #[test]
    fn concurrent_change_aborts() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(&f, USER_CLAUDE).unwrap();
        let p = plan_install(Harness::Claude, &d, Path::new(BIN)).unwrap();
        fs::write(&f, USER_CLAUDE.replace("opus", "sonnet")).unwrap();
        let err = apply(&p).unwrap_err().to_string();
        assert!(err.contains("changed since"), "{err}");
        assert!(fs::read_to_string(&f).unwrap().contains("sonnet"));
        assert!(backups(&d.claude).is_empty());
        // File appearing after a plan made against a missing file also aborts.
        let t2 = tempfile::tempdir().unwrap();
        let d2 = dirs(&t2);
        let p = plan_install(Harness::Claude, &d2, Path::new(BIN)).unwrap();
        fs::write(d2.claude.join("settings.json"), "{}").unwrap();
        assert!(apply(&p).is_err());
    }

    #[test]
    fn invalid_json_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(&f, "{ // comment\n}").unwrap();
        assert!(plan_install(Harness::Claude, &d, Path::new(BIN)).is_err());
        fs::write(&f, "[1]").unwrap();
        assert!(plan_install(Harness::Claude, &d, Path::new(BIN)).is_err());
        fs::write(&f, r#"{"hooks": []}"#).unwrap();
        assert!(plan_install(Harness::Claude, &d, Path::new(BIN)).is_err());
        fs::write(&f, r#"{"hooks": {"Stop": {}}}"#).unwrap();
        assert!(plan_install(Harness::Claude, &d, Path::new(BIN)).is_err());
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            r#"{"hooks": {"Stop": {}}}"#
        );
        let st = status(Harness::Claude, &d);
        assert_eq!(st.state, InstallState::NotInstalled);
    }

    #[test]
    fn empty_file_and_permissions_preserved() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.claude.join("settings.json");
        fs::write(&f, "").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        install(Harness::Claude, &d);
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(status(Harness::Claude, &d).state, InstallState::Installed);
    }

    #[test]
    fn symlinked_settings_is_written_through() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let real = t.path().join("dotfiles-settings.json");
        fs::write(&real, "{}\n").unwrap();
        std::os::unix::fs::symlink(&real, d.claude.join("settings.json")).unwrap();
        install(Harness::Claude, &d);
        assert!(
            fs::symlink_metadata(d.claude.join("settings.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(json_of(&real).get("hooks").is_some());
    }

    #[test]
    fn status_missing_partial_installed() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let st = status(Harness::Claude, &d);
        assert_eq!(st.state, InstallState::NotInstalled);
        assert!(!st.file_exists);
        assert_eq!(st.missing_events.len(), 15);
        install(Harness::Claude, &d);
        let f = d.claude.join("settings.json");
        let mut v = json_of(&f);
        v["hooks"].as_object_mut().unwrap().shift_remove("Stop");
        fs::write(&f, serde_json::to_string(&v).unwrap()).unwrap();
        let st = status(Harness::Claude, &d);
        assert_eq!(st.state, InstallState::Partial);
        assert_eq!(st.missing_events, ["Stop"]);
        assert_eq!(st.hooks.len(), 14);
        assert_eq!(st.hooks[0].version.as_deref(), Some(VERSION));
        assert!(st.hooks.iter().all(|h| h.trust.is_none()));
    }

    #[test]
    fn dirs_from_env_honours_vars() {
        // Single test touching process env; no other test reads these vars.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", "/tmp/cc-x");
            std::env::set_var("CODEX_HOME", "/tmp/cx-x");
        }
        let d = Dirs::from_env();
        assert_eq!(d.claude, Path::new("/tmp/cc-x"));
        assert_eq!(
            d.config_file(Harness::Codex),
            Path::new("/tmp/cx-x/hooks.json")
        );
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }

    #[test]
    fn dry_run_diff() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        fs::write(d.claude.join("settings.json"), "{\n  \"a\": 1\n}\n").unwrap();
        let p = plan_install(Harness::Claude, &d, Path::new(BIN)).unwrap();
        let diff = p.files[0].diff();
        assert!(diff.contains("\n+  \"hooks\": {"));
        assert!(diff.contains("\n-}\n") || diff.contains("\n-  \"a\": 1\n"));
        assert!(diff.contains(" {\n"));
        // nothing was written
        assert_eq!(
            fs::read_to_string(d.claude.join("settings.json")).unwrap(),
            "{\n  \"a\": 1\n}\n"
        );
    }

    #[test]
    fn spaces_in_bin_are_quoted() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let p = plan_install(Harness::Claude, &d, Path::new("/a b/it's/vibeke")).unwrap();
        apply(&p).unwrap();
        let v = json_of(&d.claude.join("settings.json"));
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            r"'/a b/it'\''s/vibeke' hook claude Stop"
        );
        assert_eq!(status(Harness::Claude, &d).state, InstallState::Installed);
    }

    // ----- Codex -----

    const USER_CODEX: &str = r#"{
  "hooks": {
    "Stop": [
      {"hooks": [{"type": "command", "command": "/x/herdr-agent-state.sh stop", "timeout": 5}]}
    ],
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "/x/mine.sh"}]}
    ]
  }
}
"#;

    #[test]
    fn codex_install_shape_notes_and_idempotency() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.codex.join("hooks.json");
        let user = pretty(USER_CODEX);
        fs::write(&f, &user).unwrap();
        let p = plan_install(Harness::Codex, &d, Path::new(BIN)).unwrap();
        assert!(
            p.notes
                .iter()
                .any(|n| n == "run /hooks in Codex once to trust the Vibeke hooks")
        );
        apply(&p).unwrap();
        let v = json_of(&f);
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 11);
        assert_eq!(hooks["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(hooks["PreToolUse"][1]["matcher"], "Bash");
        assert_eq!(hooks["PreToolUse"][1]["hooks"][0]["timeout"], 1800);
        assert_eq!(hooks["PermissionRequest"][0]["hooks"][0]["timeout"], 1800);
        assert_eq!(
            hooks["Stop"][1]["hooks"][0]["command"],
            format!("{BIN} hook codex Stop")
        );
        // No marker key and no version anywhere in the definition.
        assert!(!fs::read_to_string(&f).unwrap().contains("_vibeke"));
        assert!(!hooks["Stop"][1].to_string().contains(VERSION));
        assert_eq!(backups(&d.codex).len(), 1);
        let bytes = fs::read(&f).unwrap();
        let p2 = plan_install(Harness::Codex, &d, Path::new(BIN)).unwrap();
        assert!(!p2.changed());
        apply(&p2).unwrap();
        assert_eq!(fs::read(&f).unwrap(), bytes);
        // Uninstall restores the original bytes exactly.
        apply(&plan_uninstall(Harness::Codex, &d).unwrap()).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), user);
    }

    #[test]
    fn codex_mixed_group_is_split_not_duplicated() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.codex.join("hooks.json");
        fs::write(
            &f,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/u/a.sh"},{"type":"command","command":"/b/vibeke hook codex Stop"}]}]}}"#,
        )
        .unwrap();
        install(Harness::Codex, &d);
        install(Harness::Codex, &d);
        let v = json_of(&f);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "/u/a.sh");
    }

    #[test]
    fn codex_status_trust() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let f = d.codex.join("hooks.json");
        fs::write(&f, USER_CODEX).unwrap();
        install(Harness::Codex, &d);
        // No config.toml: everything untrusted.
        let st = status(Harness::Codex, &d);
        assert_eq!(st.state, InstallState::Installed);
        assert!(st.hooks.iter().all(|h| h.trust == Some(Trust::Untrusted)));
        assert!(st.todo[0].contains("/hooks"));
        assert_eq!(st.foreign.len(), 1);
        // Trust Stop (group 1, hook 0) and PreToolUse (group 1) by key; one by snake_case.
        let fp = f.display();
        fs::write(
            d.codex.join("config.toml"),
            format!(
                "[features]\nhooks = true\n\n[hooks.state.\"{fp}:Stop:1:0\"]\ntrusted_hash = \"abc\"\n\n\
                 [hooks.state.\"{fp}:pre_tool_use:1:0\"]\ntrusted_hash = \"def\"\n\n\
                 [hooks.state.\"{fp}:SessionEnd:0:0\"]\ntrusted = false\n"
            ),
        )
        .unwrap();
        let st = status(Harness::Codex, &d);
        let trust = |ev: &str| st.hooks.iter().find(|h| h.event == ev).unwrap().trust;
        assert_eq!(trust("Stop"), Some(Trust::Trusted));
        assert_eq!(trust("PreToolUse"), Some(Trust::Trusted));
        assert_eq!(trust("SessionEnd"), Some(Trust::Untrusted));
        assert_eq!(trust("SessionStart"), Some(Trust::Untrusted));
        assert!(
            st.todo[0].starts_with("9 Vibeke hook(s) untrusted"),
            "{:?}",
            st.todo
        );
    }

    #[test]
    fn codex_install_never_touches_config_toml() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let cfg = d.codex.join("config.toml");
        fs::write(&cfg, "[features]\nhooks = false\n").unwrap();
        let p = plan_install(Harness::Codex, &d, Path::new(BIN)).unwrap();
        assert!(p.notes.iter().any(|n| n.contains("features.hooks = false")));
        assert_eq!(p.files.len(), 1);
        apply(&p).unwrap();
        assert_eq!(
            fs::read_to_string(&cfg).unwrap(),
            "[features]\nhooks = false\n"
        );
        assert!(!status(Harness::Codex, &d).problems.is_empty());
    }

    // ----- Shim -----

    fn fake_codex(dir: &Path, label: &str, out: &Path) {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join("codex");
        fs::write(
            &p,
            format!(
                "#!/bin/sh\necho {label} > '{o}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{o}'; done\n",
                o = out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_shim(
        shim_dir: &Path,
        path: &[&Path],
        env: &[(&str, &str)],
        args: &[&str],
    ) -> std::process::Output {
        let path = std::env::join_paths(path.iter().map(|p| p.to_path_buf())).unwrap();
        let mut c = std::process::Command::new(shim_dir.join("codex"));
        c.env_clear().env("PATH", path).args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn argv(out: &Path) -> Vec<String> {
        fs::read_to_string(out)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn shim_is_idempotent_and_executable() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("shims");
        let p = write_codex_shim(&dir).unwrap();
        assert_eq!(p, dir.join("codex"));
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let a = fs::read(&p).unwrap();
        let m1 = fs::metadata(&p).unwrap().modified().unwrap();
        write_codex_shim(&dir).unwrap();
        assert_eq!(fs::read(&p).unwrap(), a);
        assert_eq!(
            fs::metadata(&p).unwrap().modified().unwrap(),
            m1,
            "not rewritten"
        );
        assert!(
            String::from_utf8(a)
                .unwrap()
                .contains("VIBEKE_CODEX_EXTRA=\"--disable daemon_auto_start\"")
        );
        // Fixes a drifted mode.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        write_codex_shim(&dir).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn shim_injects_args_and_skips_itself() {
        let t = tempfile::tempdir().unwrap();
        let shims = t.path().join("shims");
        let real = t.path().join("real");
        let out = t.path().join("out");
        write_codex_shim(&shims).unwrap();
        fake_codex(&real, "real", &out);
        let base = Path::new("/usr/bin");
        let bin = Path::new("/bin");
        let o = run_shim(
            &shims,
            &[&shims, &real, base, bin],
            &[],
            &["-a", "never", "--flag=x y", "fix the bug"],
        );
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(
            argv(&out),
            [
                "real",
                "--disable",
                "daemon_auto_start",
                "-a",
                "never",
                "--flag=x y",
                "fix the bug"
            ]
        );
        // Shim dir also listed after the real one, and via a trailing slash.
        let shims_slash = PathBuf::from(format!("{}/", shims.display()));
        run_shim(&shims, &[&real, &shims_slash, base, bin], &[], &[]);
        assert_eq!(argv(&out), ["real", "--disable", "daemon_auto_start"]);
    }

    #[test]
    fn shim_subcommands() {
        let t = tempfile::tempdir().unwrap();
        let shims = t.path().join("shims");
        let real = t.path().join("real");
        let out = t.path().join("out");
        write_codex_shim(&shims).unwrap();
        fake_codex(&real, "real", &out);
        let p: Vec<&Path> = vec![&shims, &real, Path::new("/usr/bin"), Path::new("/bin")];
        run_shim(&shims, &p, &[], &["resume", "abc", "--last"]);
        assert_eq!(
            argv(&out),
            [
                "real",
                "resume",
                "--disable",
                "daemon_auto_start",
                "abc",
                "--last"
            ]
        );
        run_shim(&shims, &p, &[], &["exec", "do it"]);
        assert_eq!(
            argv(&out),
            ["real", "exec", "--disable", "daemon_auto_start", "do it"]
        );
        run_shim(&shims, &p, &[], &["app-server", "--listen", "x"]);
        assert_eq!(argv(&out), ["real", "app-server", "--listen", "x"]);
        run_shim(&shims, &p, &[], &["login"]);
        assert_eq!(argv(&out), ["real", "login"]);
    }

    #[test]
    fn shim_opt_outs() {
        let t = tempfile::tempdir().unwrap();
        let shims = t.path().join("shims");
        let real = t.path().join("real");
        let out = t.path().join("out");
        write_codex_shim(&shims).unwrap();
        fake_codex(&real, "real", &out);
        let p: Vec<&Path> = vec![&shims, &real, Path::new("/usr/bin"), Path::new("/bin")];
        run_shim(&shims, &p, &[("CODEX_VIBEKE_SHIM", "0")], &["-a", "never"]);
        assert_eq!(argv(&out), ["real", "-a", "never"]);
        run_shim(&shims, &p, &[], &["--enable", "daemon_auto_start"]);
        assert_eq!(argv(&out), ["real", "--enable", "daemon_auto_start"]);
        run_shim(&shims, &p, &[], &["--disable", "daemon_auto_start", "x"]);
        assert_eq!(argv(&out), ["real", "--disable", "daemon_auto_start", "x"]);
    }

    #[test]
    fn shim_without_real_codex_fails_cleanly() {
        let t = tempfile::tempdir().unwrap();
        let shims = t.path().join("shims");
        write_codex_shim(&shims).unwrap();
        let o = run_shim(
            &shims,
            &[&shims, Path::new("/usr/bin"), Path::new("/bin")],
            &[],
            &["x"],
        );
        assert_eq!(o.status.code(), Some(127));
        assert!(String::from_utf8_lossy(&o.stderr).contains("not found"));
    }

    // ----- pi / omp extension -----

    #[test]
    fn extension_install_paths_header_and_idempotence() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        for (h, rel) in [
            (Harness::Pi, "pi/agent/extensions/vibeke/index.js"),
            (Harness::Omp, "omp/agent/extensions/vibeke.js"),
        ] {
            assert_eq!(status(h, &d).state, InstallState::NotInstalled);
            let p = install(h, &d);
            assert!(p.changed());
            let path = t.path().join(rel);
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.starts_with(&format!(
                "// managed by vibeke (vibeke-integration={}@{VERSION}); reinstall overwrites\n",
                h.id()
            )));
            assert!(text.ends_with(EXTENSION_BUNDLE));
            assert!(EXTENSION_BUNDLE.contains("VIBEKE_PANE_TOKEN"));
            let st = status(h, &d);
            assert_eq!(st.state, InstallState::Installed);
            assert_eq!(st.hooks[0].version.as_deref(), Some(VERSION));
            assert!(st.todo.is_empty());
            // second install: no change, no backup
            let again = plan_install(h, &d, Path::new(BIN)).unwrap();
            assert!(!again.changed());
            apply(&again).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
            assert!(backups(path.parent().unwrap()).is_empty());
        }
    }

    #[test]
    fn extension_reinstall_overwrites_stale_managed_file() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let path = d.config_file(Harness::Omp);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "// managed by vibeke (vibeke-integration=omp@0.0.1); reinstall overwrites\nold();\n",
        )
        .unwrap();
        let st = status(Harness::Omp, &d);
        assert_eq!(st.state, InstallState::Installed);
        assert!(st.todo[0].contains("integration_outdated"));
        install(Harness::Omp, &d);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .ends_with(EXTENSION_BUNDLE)
        );
        assert!(status(Harness::Omp, &d).todo.is_empty());
    }

    #[test]
    fn extension_never_clobbers_a_foreign_file_and_reports_siblings() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let dir = t.path().join("omp/agent/extensions");
        fs::create_dir_all(&dir).unwrap();
        let herdr = dir.join("herdr-omp-agent-state.ts");
        fs::write(&herdr, "// herdr\n").unwrap();
        let st = status(Harness::Omp, &d);
        assert_eq!(st.state, InstallState::NotInstalled);
        assert_eq!(st.foreign, ["extension: herdr-omp-agent-state.ts"]);
        install(Harness::Omp, &d);
        assert_eq!(fs::read_to_string(&herdr).unwrap(), "// herdr\n");
        assert_eq!(
            status(Harness::Omp, &d).foreign,
            ["extension: herdr-omp-agent-state.ts"]
        );

        // a user's own vibeke.js without our header: refuse, untouched
        let t2 = tempfile::tempdir().unwrap();
        let d2 = dirs(&t2);
        let own = d2.config_file(Harness::Omp);
        fs::create_dir_all(own.parent().unwrap()).unwrap();
        fs::write(&own, "mine\n").unwrap();
        assert!(plan_install(Harness::Omp, &d2, Path::new(BIN)).is_err());
        assert!(plan_uninstall(Harness::Omp, &d2).is_err());
        assert_eq!(fs::read_to_string(&own).unwrap(), "mine\n");
        assert!(!status(Harness::Omp, &d2).problems.is_empty());
        // pi: sibling extension directories are foreign
        fs::create_dir_all(t.path().join("pi/agent/extensions/other")).unwrap();
        install(Harness::Pi, &d);
        assert_eq!(status(Harness::Pi, &d).foreign, ["extension: other"]);
    }

    #[test]
    fn extension_uninstall_removes_only_the_managed_file() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        let sib = t.path().join("pi/agent/extensions/other/index.ts");
        fs::create_dir_all(sib.parent().unwrap()).unwrap();
        fs::write(&sib, "x").unwrap();
        install(Harness::Pi, &d);
        let p = plan_uninstall(Harness::Pi, &d).unwrap();
        assert!(p.changed());
        apply(&p).unwrap();
        assert!(!d.config_file(Harness::Pi).exists());
        assert!(!d.config_file(Harness::Pi).parent().unwrap().exists());
        assert!(sib.exists());
        // uninstalling when absent is a no-op
        assert!(!plan_uninstall(Harness::Pi, &d).unwrap().changed());
        assert_eq!(status(Harness::Pi, &d).state, InstallState::NotInstalled);
    }

    #[test]
    fn extension_apply_detects_concurrent_modification() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs(&t);
        install(Harness::Omp, &d);
        let p = plan_uninstall(Harness::Omp, &d).unwrap();
        let path = d.config_file(Harness::Omp);
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("// edited\n");
        fs::write(&path, text).unwrap();
        assert!(apply(&p).is_err());
        assert!(path.exists());
    }
}
