//! Loading, validation, unknown-key warnings, paths, and hot-reload diffing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::binding::{parse_binding, parse_prefix_key};
use crate::keys::{
    ACTION_ALIASES, canonical_action, check_keys, default_bindings, is_known_action,
};
use crate::types::{Config, default_harnesses};

/// A source position (1-based line and column).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// A located parse or validation error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.file.display(),
            self.line,
            self.col,
            self.message
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Syntax error or wrong value type.
    #[error("{0}")]
    Parse(Diagnostic),
    /// Well-formed TOML that fails validation (all problems are listed).
    #[error("{}", .0.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<Diagnostic>),
}

impl ConfigError {
    /// The first diagnostic, if the error is located.
    pub fn first_diagnostic(&self) -> Option<&Diagnostic> {
        match self {
            ConfigError::Parse(d) => Some(d),
            ConfigError::Invalid(v) => v.first(),
            ConfigError::Io { .. } => None,
        }
    }
}

/// A non-fatal finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// Dotted key path the warning is about (`ui.sidebar.foo`).
    pub key: String,
    pub pos: Option<Pos>,
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pos {
            Some(p) => write!(f, "{}:{}: {}", p.line, p.col, self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl Warning {
    pub(crate) fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Warning {
            key: key.into(),
            pos: None,
            message: message.into(),
        }
    }
}

/// Top-level sections owned by other crates; preserved in `Config::extra`.
pub const EXTERNAL_SECTIONS: &[&str] = &[
    "assistant",
    "assist",
    "collision",
    "isolation",
    "preview",
    "browser",
    "screenshots",
    "security",
    "plugins",
    "desk",
];

const BUILTIN_SEGMENTS: &[&str] = &[
    "machine",
    "session",
    "workspace",
    "task",
    "branch",
    "ports",
    "attention",
    "agents_summary",
    "cpu",
    "clock",
    "prefix_indicator",
    "mode",
    "sync_input",
];

fn offset_to_pos(src: &str, off: usize) -> Pos {
    let off = off.min(src.len());
    let before = &src[..off];
    let line = before.matches('\n').count() + 1;
    let col = before
        .rsplit('\n')
        .next()
        .map(|l| l.chars().count())
        .unwrap_or(0)
        + 1;
    Pos { line, col }
}

/// Locates dotted paths (with optional `[n]` indices) in the source text.
struct Locator<'a> {
    src: &'a str,
    doc: toml_edit::Document<&'a str>,
}

impl<'a> Locator<'a> {
    fn new(src: &'a str) -> Option<Self> {
        let doc = toml_edit::Document::parse(src).ok()?;
        Some(Locator { src, doc })
    }

    fn locate(&self, path: &str) -> Option<Pos> {
        let mut segs: Vec<(String, Option<usize>)> = Vec::new();
        for part in path.split('.') {
            if let Some((name, rest)) = part.split_once('[') {
                let idx = rest.trim_end_matches(']').parse().ok();
                segs.push((name.to_string(), idx));
            } else {
                segs.push((part.to_string(), None));
            }
        }
        // Try the full path, then progressively shorter ones.
        for n in (1..=segs.len()).rev() {
            let mut item: &toml_edit::Item = self.doc.as_item();
            let mut ok = true;
            for (name, idx) in &segs[..n] {
                match item.get(name.as_str()) {
                    Some(i) => item = i,
                    None => {
                        ok = false;
                        break;
                    }
                }
                if let Some(i) = idx {
                    match item.get(*i) {
                        Some(x) => item = x,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if ok && let Some(span) = item.span() {
                return Some(offset_to_pos(self.src, span.start));
            }
        }
        None
    }
}

struct Problem {
    path: String,
    message: String,
}

fn problem(path: impl Into<String>, message: impl Into<String>) -> Problem {
    Problem {
        path: path.into(),
        message: message.into(),
    }
}

fn check_binding(path: String, raw: &str, errs: &mut Vec<Problem>, warns: &mut Vec<Warning>) {
    if raw.trim().is_empty() {
        return;
    }
    match parse_binding(raw) {
        Err(e) => errs.push(problem(path, format!("invalid binding `{raw}`: {e}"))),
        Ok(b) => {
            if b.is_direct()
                && let Some(first) = b.chords.first()
                && matches!(first.as_str(), "ctrl+c" | "ctrl+d" | "ctrl+r" | "esc")
            {
                warns.push(Warning::new(
                    path.clone(),
                    format!("direct binding `{raw}` shadows a key that terminal apps commonly use"),
                ));
            }
            if b.uses_super() {
                warns.push(Warning::new(
                    path,
                    format!(
                        "`{raw}` uses cmd/super, which needs a host terminal with the kitty keyboard protocol"
                    ),
                ));
            }
        }
    }
}

impl Config {
    /// Load a config file. A missing file yields the defaults. Unknown keys are warnings;
    /// syntax and validation errors carry `file:line:col`.
    pub fn load(path: impl AsRef<Path>) -> Result<(Config, Vec<Warning>), ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(src) => Config::parse(&src, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), vec![])),
            Err(e) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source: e,
            }),
        }
    }

    /// Parse config text; `origin` is only used in diagnostics.
    pub fn parse(src: &str, origin: &Path) -> Result<(Config, Vec<Warning>), ConfigError> {
        let diag = |span: Option<std::ops::Range<usize>>, message: String| {
            let pos = span
                .map(|s| offset_to_pos(src, s.start))
                .unwrap_or(Pos { line: 1, col: 1 });
            Diagnostic {
                file: origin.to_path_buf(),
                line: pos.line,
                col: pos.col,
                message,
            }
        };

        if let Err(e) = toml_edit::Document::parse(src) {
            return Err(ConfigError::Parse(diag(e.span(), e.message().to_string())));
        }
        let raw: toml::Table = toml::from_str(src)
            .map_err(|e| ConfigError::Parse(diag(e.span(), e.message().to_string())))?;
        let mut cfg: Config = toml::from_str(src)
            .map_err(|e| ConfigError::Parse(diag(e.span(), e.message().to_string())))?;

        let mut warnings = Vec::new();
        cfg.keys.finish(&raw, &mut warnings);
        fill_harness_defaults(&mut cfg);
        for name in EXTERNAL_SECTIONS {
            if let Some(v) = raw.get(*name) {
                cfg.extra.insert((*name).to_string(), v.clone());
            }
        }
        warn_unknown(&raw, &mut warnings);
        // Typed `[preview]` (06 Part C): bad values and unknown keys are located warnings.
        warnings.extend(crate::preview::Preview::from_value(raw.get("preview")).1);

        let (problems, mut vwarn) = cfg.validate();
        warnings.append(&mut vwarn);

        let loc = Locator::new(src);
        let locate = |p: &str| loc.as_ref().and_then(|l| l.locate(p));
        if !problems.is_empty() {
            let diags = problems
                .into_iter()
                .map(|p| {
                    let pos = locate(&p.path).unwrap_or(Pos { line: 1, col: 1 });
                    Diagnostic {
                        file: origin.to_path_buf(),
                        line: pos.line,
                        col: pos.col,
                        message: format!("{}: {}", p.path, p.message),
                    }
                })
                .collect();
            return Err(ConfigError::Invalid(diags));
        }
        for w in &mut warnings {
            if w.pos.is_none() {
                w.pos = locate(&w.key);
            }
        }
        Ok((cfg, warnings))
    }

    /// Validate semantic constraints. Returns `(errors, warnings)`.
    fn validate(&self) -> (Vec<Problem>, Vec<Warning>) {
        let mut errs: Vec<Problem> = Vec::new();
        let mut warns: Vec<Warning> = Vec::new();
        let k = &self.keys;

        if let Err(e) = parse_prefix_key(&k.prefix) {
            errs.push(problem("keys.prefix", e));
        }
        if k.prefix_timeout_ms == 0 {
            errs.push(problem("keys.prefix_timeout_ms", "must be greater than 0"));
        }

        for (action, raw) in &k.bindings {
            check_binding(format!("keys.{action}"), raw, &mut errs, &mut warns);
        }
        for (i, c) in k.command.iter().enumerate() {
            if c.key.trim().is_empty() {
                errs.push(problem(format!("keys.command[{i}].key"), "missing key"));
            } else {
                check_binding(
                    format!("keys.command[{i}].key"),
                    &c.key,
                    &mut errs,
                    &mut warns,
                );
            }
            if c.command.trim().is_empty() {
                errs.push(problem(
                    format!("keys.command[{i}].command"),
                    "missing command",
                ));
            }
            if let Some(w) = &c.when
                && !w.starts_with("agent:")
            {
                errs.push(problem(
                    format!("keys.command[{i}].when"),
                    "expected `agent:<harness>`",
                ));
            }
        }

        // `[keys.copy_mode]` overrides: one plain key → a copy-mode action ("" unbinds). Bad
        // entries are ignored with a warning; copy mode keeps working with the base table.
        for (key, action) in &k.copy_mode.overrides {
            let path = format!("keys.copy_mode.{key}");
            match parse_binding(key) {
                Ok(b) if !b.prefix && b.chords.len() == 1 && b.range.is_none() => {}
                Ok(_) => warns.push(Warning::new(
                    path.clone(),
                    format!("`{key}` must be a single key (no prefix or sequence); ignoring"),
                )),
                Err(e) => warns.push(Warning::new(
                    path.clone(),
                    format!("invalid key `{key}`: {e}; ignoring"),
                )),
            }
            if !action.is_empty() && !crate::keys::is_copy_mode_action(action) {
                warns.push(Warning::new(
                    path,
                    format!(
                        "unknown copy-mode action `{action}` (one of: {}); ignoring",
                        crate::keys::COPY_MODE_ACTIONS.join(", ")
                    ),
                ));
            }
        }

        let sb = &self.ui.sidebar;
        if sb.min_width > sb.max_width {
            errs.push(problem("ui.sidebar.min_width", "greater than max_width"));
        }
        if sb.width < sb.min_width || sb.width > sb.max_width {
            warns.push(Warning::new(
                "ui.sidebar.width",
                "outside min_width..max_width; it will be clamped",
            ));
        }
        if self.ui.max_fps == 0 {
            errs.push(problem("ui.max_fps", "must be greater than 0"));
        }
        for (name, list) in [
            ("left", &self.ui.status_bar.left),
            ("center", &self.ui.status_bar.center),
            ("right", &self.ui.status_bar.right),
        ] {
            for (i, seg) in list.iter().enumerate() {
                if !BUILTIN_SEGMENTS.contains(&seg.as_str()) && !seg.starts_with("plugin:") {
                    warns.push(Warning::new(
                        format!("ui.status_bar.{name}[{i}]"),
                        format!("unknown status bar segment `{seg}`"),
                    ));
                }
            }
        }
        for (i, t) in sb.token.iter().enumerate() {
            let m = &t.matcher;
            if m.harness.is_none() && m.state.is_none() && m.regex.is_none() {
                errs.push(problem(
                    format!("ui.sidebar.token[{i}].match"),
                    "must set at least one of harness, state, regex",
                ));
            }
            if let Some(r) = &m.regex
                && let Err(e) = regex::Regex::new(r)
            {
                errs.push(problem(
                    format!("ui.sidebar.token[{i}].match.regex"),
                    format!("invalid regex: {e}"),
                ));
            }
        }

        let q = &self.notifications.quiet_hours;
        if !q.is_empty() && !valid_quiet_hours(q) {
            errs.push(problem(
                "notifications.quiet_hours",
                "expected `HH:MM-HH:MM`",
            ));
        }

        for (i, r) in self.policy.rule.iter().enumerate() {
            let m = &r.matcher;
            if m.tool.is_none() && m.command_regex.is_none() && m.path_glob.is_none() {
                errs.push(problem(
                    format!("policy.rule[{i}].match"),
                    "must set at least one of tool, command_regex, path_glob",
                ));
            }
            if let Some(re) = &m.command_regex
                && let Err(e) = regex::Regex::new(re)
            {
                errs.push(problem(
                    format!("policy.rule[{i}].match.command_regex"),
                    format!("invalid regex: {e}"),
                ));
            }
        }

        let t = &self.tasks;
        if t.port_block == 0 || t.port_block as u32 > t.port_pool.len() {
            errs.push(problem(
                "tasks.port_block",
                "must be between 1 and the size of tasks.port_pool",
            ));
        }
        if t.root.trim().is_empty() {
            errs.push(problem("tasks.root", "must not be empty"));
        }
        for (key, o) in &t.repos {
            let at = format!("tasks.repos.{key}");
            if key.trim().is_empty() {
                errs.push(problem("tasks.repos", "empty repo key"));
            }
            if let Some(c) = o.ports.count
                && (c == 0 || u32::from(c) > t.port_pool.len())
            {
                errs.push(problem(
                    format!("{at}.ports.count"),
                    "must be between 1 and the size of tasks.port_pool",
                ));
            }
            if let Some(tm) = &o.setup.timeout
                && !valid_duration(tm)
            {
                errs.push(problem(
                    format!("{at}.setup.timeout"),
                    "expected a duration like \"10m\"",
                ));
            }
        }

        let mut labels = BTreeSet::new();
        for (i, m) in self.remote.machine.iter().enumerate() {
            if m.label.trim().is_empty() {
                errs.push(problem(
                    format!("remote.machine[{i}].label"),
                    "missing label",
                ));
            } else if !labels.insert(m.label.clone()) {
                errs.push(problem(
                    format!("remote.machine[{i}].label"),
                    format!("duplicate machine label `{}`", m.label),
                ));
            }
            if m.address.trim().is_empty() {
                errs.push(problem(
                    format!("remote.machine[{i}].address"),
                    "missing address",
                ));
            }
        }

        if self.render.max_unacked == 0 {
            errs.push(problem("render.max_unacked", "must be greater than 0"));
        }

        if errs.is_empty() {
            for c in check_keys(self) {
                warns.push(Warning::new(
                    "keys",
                    format!(
                        "binding `{}` conflicts between {} ({})",
                        c.binding,
                        c.actions.join(", "),
                        match c.reason {
                            crate::keys::ConflictReason::Duplicate => "duplicate",
                            crate::keys::ConflictReason::ShadowedSequence => "shadowed sequence",
                        }
                    ),
                ));
            }
        }
        (errs, warns)
    }

    /// Serialise to a TOML value tree with `extra` sections merged to the top level.
    pub fn to_value(&self) -> toml::Value {
        let mut v = toml::Value::try_from(self).expect("Config is always serialisable");
        if let toml::Value::Table(t) = &mut v {
            t.remove("extra");
            for (k, val) in &self.extra {
                t.insert(k.clone(), val.clone());
            }
        }
        v
    }

    /// Dotted keys whose values differ between two configs (leaf scalars/arrays; map entries
    /// individually). Sorted. Used for `session.config_reloaded { changed_keys }`.
    pub fn diff(old: &Config, new: &Config) -> Vec<String> {
        let a = flatten(&old.to_value());
        let b = flatten(&new.to_value());
        let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        keys.into_iter()
            .filter(|k| a.get(*k) != b.get(*k))
            .cloned()
            .collect()
    }
}

/// `true` when a changed key only applies to newly created panes (08 §11.2).
pub fn requires_new_panes(key: &str) -> bool {
    matches!(
        key,
        "terminal.default_shell" | "terminal.shell_mode" | "terminal.term"
    ) || key == "terminal.env"
        || key.starts_with("terminal.env.")
}

fn flatten(v: &toml::Value) -> BTreeMap<String, toml::Value> {
    fn go(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, toml::Value>) {
        match v {
            toml::Value::Table(t) if !t.is_empty() => {
                for (k, x) in t {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    go(&p, x, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    go("", v, &mut out);
    out
}

fn valid_quiet_hours(s: &str) -> bool {
    fn hm(p: &str) -> bool {
        let Some((h, m)) = p.split_once(':') else {
            return false;
        };
        matches!((h.parse::<u8>(), m.parse::<u8>()), (Ok(h), Ok(m)) if h < 24 && m < 60)
            && m.len() == 2
    }
    s.split_once('-').is_some_and(|(a, b)| hm(a) && hm(b))
}

fn fill_harness_defaults(cfg: &mut Config) {
    let defaults = default_harnesses();
    for (id, d) in defaults {
        let e = cfg.agents.harness.entry(id).or_insert_with(|| d.clone());
        if e.integration.is_none() {
            e.integration = d.integration.clone();
        }
        if e.shim.is_none() {
            e.shim = d.shim;
        }
        if e.headless_shared.is_none() {
            e.headless_shared = d.headless_shared;
        }
    }
}

impl crate::types::Keys {
    /// Move `action = "binding"` entries out of the raw flattened table, filling defaults.
    pub(crate) fn finish(&mut self, raw: &toml::Table, warnings: &mut Vec<Warning>) {
        self.bindings = default_bindings();
        let other = std::mem::take(&mut self.raw_other);
        // Canonical names first so an explicit canonical entry beats an alias.
        let mut aliased: Vec<(String, String)> = Vec::new();
        for (name, val) in other {
            let canon = canonical_action(&name).to_string();
            if !is_known_action(&canon) {
                continue; // reported by the unknown-key walker
            }
            let toml::Value::String(b) = val else {
                warnings.push(Warning::new(
                    format!("keys.{name}"),
                    "expected a binding string; ignoring",
                ));
                continue;
            };
            if canon != name {
                aliased.push((canon, b));
            } else {
                self.bindings.insert(canon, b);
            }
        }
        for (canon, b) in aliased {
            let explicit = raw
                .get("keys")
                .and_then(|k| k.as_table())
                .is_some_and(|t| t.contains_key(&canon));
            if !explicit {
                self.bindings.insert(canon, b);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Unknown-key detection
// ---------------------------------------------------------------------------------------

/// Tables whose keys are user-defined.
fn is_free_table(path: &str) -> bool {
    matches!(
        path,
        "theme.custom"
            | "layouts"
            | "theme.pane"
            | "terminal.env"
            | "terminal.host_overrides"
            | "keys.navigate"
            | "keys.resize"
            | "keys.card"
            | "keys.copy_mode"
    )
}

/// Allowed keys for elements of array-of-tables (and nested inline tables) by schema path.
fn element_schema(path: &str) -> Option<&'static [&'static str]> {
    Some(match path {
        "keys.command" => &[
            "key",
            "type",
            "command",
            "width",
            "height",
            "cwd",
            "env",
            "title",
            "when",
            "description",
        ],
        "ui.sidebar.token" => &["match", "label", "color", "hide"],
        "ui.sidebar.token.match" => &["harness", "state", "regex"],
        "policy.rule" => &["match", "effect", "scope"],
        "policy.rule.match" => &["tool", "command_regex", "path_glob"],
        "remote.machine" => &[
            "label",
            "address",
            "transport",
            "keybindings",
            "auto_connect",
            "auto_upgrade",
            "bootstrap",
        ],
        "agents.harness.*" => &[
            "enabled",
            "integration",
            "extra_args",
            "shim",
            "headless_shared",
        ],
        _ => return None,
    })
}

fn warn_unknown(raw: &toml::Table, out: &mut Vec<Warning>) {
    let mut known = match toml::Value::try_from(Config::default()) {
        Ok(toml::Value::Table(t)) => t,
        _ => return,
    };
    known.remove("extra");
    walk(raw, &known, "", out);
}

fn join(prefix: &str, k: &str) -> String {
    if prefix.is_empty() {
        k.to_string()
    } else {
        format!("{prefix}.{k}")
    }
}

fn check_elements(val: &toml::Value, schema_path: &str, key_path: &str, out: &mut Vec<Warning>) {
    let Some(schema) = element_schema(schema_path) else {
        return;
    };
    let Some(t) = val.as_table() else { return };
    for (k, v) in t {
        let p = join(key_path, k);
        if !schema.contains(&k.as_str()) {
            out.push(Warning::new(&p, format!("unknown key `{p}`")));
            continue;
        }
        let sub = format!("{schema_path}.{k}");
        if element_schema(&sub).is_some() {
            check_elements(v, &sub, &p, out);
        }
    }
}

/// `[tasks.repos."<key>"]` mirrors `.vibeke/task.toml`. `Option` keys are absent from the
/// default serialization, so the shape is spelled out here.
fn check_repo_override(raw: &toml::Table, path: &str, out: &mut Vec<Warning>) {
    fn keys(rel: &str) -> Option<&'static [&'static str]> {
        Some(match rel {
            "" => &["files", "deps", "setup", "ports", "env"],
            "files" => &["copy", "link", "clone", "ignore_missing"],
            "deps" => &["strategy", "install"],
            "setup" => &[
                "script",
                "run",
                "timeout",
                "start_agents_on_failure",
                "parallel_agent",
            ],
            "ports" => &["count", "env"],
            _ => return None,
        })
    }
    fn go(raw: &toml::Table, rel: &str, path: &str, out: &mut Vec<Warning>) {
        let Some(allowed) = keys(rel) else { return };
        for (k, v) in raw {
            let p = join(path, k);
            if !allowed.contains(&k.as_str()) {
                out.push(Warning::new(&p, format!("unknown key `{p}`")));
            } else if let toml::Value::Table(t) = v {
                go(t, k, &p, out);
            }
        }
    }
    go(raw, "", path, out);
}

fn walk(raw: &toml::Table, known: &toml::Table, path: &str, out: &mut Vec<Warning>) {
    for (k, v) in raw {
        let p = join(path, k);

        if path.is_empty() && EXTERNAL_SECTIONS.contains(&k.as_str()) {
            continue;
        }
        if path == "keys" && ACTION_ALIASES.iter().any(|(f, _)| f == k) {
            continue;
        }
        if path == "agents.harness" {
            // Any harness id is allowed; its keys are checked against the harness schema.
            check_elements(v, "agents.harness.*", &p, out);
            continue;
        }
        if path == "tasks.repos" {
            // Keys are repo URLs or paths; the value has the task.toml shape.
            if let toml::Value::Table(t) = v {
                check_repo_override(t, &p, out);
            }
            continue;
        }
        let Some(kv) = known.get(k) else {
            out.push(Warning::new(&p, format!("unknown key `{p}`")));
            continue;
        };
        if is_free_table(&p) {
            continue;
        }
        match (v, kv) {
            (toml::Value::Table(rt), toml::Value::Table(kt)) => walk(rt, kt, &p, out),
            (toml::Value::Array(items), _) if element_schema(&p).is_some() => {
                for (i, item) in items.iter().enumerate() {
                    check_elements(item, &p, &format!("{p}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------

/// Config file location: `VIBEKE_CONFIG`, else `$XDG_CONFIG_HOME/vibeke/config.toml`, else
/// `~/.config/vibeke/config.toml` (the same on macOS).
pub fn config_path() -> PathBuf {
    config_path_with(|k| std::env::var(k).ok())
}

/// [`config_path`] with an injectable environment lookup.
pub fn config_path_with(env: impl Fn(&str) -> Option<String>) -> PathBuf {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(p) = get("VIBEKE_CONFIG") {
        return PathBuf::from(p);
    }
    if let Some(x) = get("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("vibeke/config.toml");
    }
    let home = get("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".config/vibeke/config.toml")
}

// ---------------------------------------------------------------------------------------
// Default config file
// ---------------------------------------------------------------------------------------

const DEFAULT_TEMPLATE: &str = include_str!("default_config.toml");

/// The default configuration as a TOML document with every key present and commented out,
/// plus explanations. Uncommenting any line leaves it equal to the built-in default.
pub fn default_config_toml() -> String {
    let mut out = String::new();
    for line in DEFAULT_TEMPLATE.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            out.push_str(line);
        } else {
            out.push_str("# ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The same document with nothing commented out (used by tests; also handy for `--print`).
pub fn default_config_toml_uncommented() -> &'static str {
    DEFAULT_TEMPLATE
}

/// `"90s"`, `"10m"`, `"2h"`, `"1d"` or bare seconds; non-zero.
fn valid_duration(s: &str) -> bool {
    let s = s.trim();
    let digits = s.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(s);
    digits.parse::<u64>().is_ok_and(|n| n > 0)
}
