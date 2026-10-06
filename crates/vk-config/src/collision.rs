//! Typed `[collision]` keys (05 §10): the shared-cwd collision tracker.
//!
//! Like `[events]`, the section stays external (`Config::extra["collision"]`, read by the
//! server); this module gives every key one type and default and validates it at load time. A
//! bad value is a located warning and keeps that key's default, so one typo never silences the
//! tracker or turns on claim enforcement.

use crate::load::Warning;
use crate::units::Dur;
use std::time::Duration;

/// `fs_attribution`: how file-system changes are attributed to a run (05 §10 signal 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FsAttribution {
    /// Watcher events are attributed by in-flight tool calls, then by the runs working in that
    /// directory (ambiguous when several).
    #[default]
    Auto,
    /// No watcher and no `git status` poll: adapter `file_change` items only.
    Off,
    /// `auto`, plus best-effort open-file sampling (`/proc/*/fd` on Linux; fanotify where
    /// permitted) to name the writer.
    Aggressive,
}

impl FsAttribution {
    pub fn as_str(self) -> &'static str {
        match self {
            FsAttribution::Auto => "auto",
            FsAttribution::Off => "off",
            FsAttribution::Aggressive => "aggressive",
        }
    }
}

/// `[collision]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Collision {
    /// Master switch (default on). Off: nothing is recorded or raised, claims are still stored.
    pub enabled: bool,
    /// Rolling window per repo root in which touches count (default 30 minutes).
    pub window: Duration,
    /// How long a read makes a later edit by another run "medium" (default 10 minutes).
    pub read_window: Duration,
    pub fs_attribution: FsAttribution,
    /// Cooperating adapters deny *reported* edit tools inside a foreign claim (default off).
    pub enforce_claims: bool,
    /// Directory depth of the "same directory/module" rule: files whose directories share their
    /// first `dir_depth` components are `low` (default 2; 0 turns the rule off).
    pub dir_depth: usize,
    /// `git status --porcelain` poll period while an agent in the repo is working (default 5 s).
    pub poll_interval: Duration,
    /// Settle time before a watcher event is attributed (default 150 ms), so the adapter's own
    /// report of the same edit lands first.
    pub settle: Duration,
    /// `os` (the platform file watcher) or `none` (no watcher; tests feed events directly).
    pub watcher: String,
    /// Raise a notification for `high` and `medium` collisions (default on).
    pub notify: bool,
    /// Path globs (repo relative) never tracked, besides `.git`, `node_modules` and gitignored.
    pub ignore: Vec<String>,
    /// Touches kept per repo root (default 5000).
    pub max_touches: usize,
}

impl Default for Collision {
    fn default() -> Self {
        Collision {
            enabled: true,
            window: Duration::from_secs(30 * 60),
            read_window: Duration::from_secs(10 * 60),
            fs_attribution: FsAttribution::Auto,
            enforce_claims: false,
            dir_depth: 2,
            poll_interval: Duration::from_secs(5),
            settle: Duration::from_millis(150),
            watcher: "os".into(),
            notify: true,
            ignore: Vec::new(),
            max_touches: 5000,
        }
    }
}

pub const COLLISION_KEYS: &[&str] = &[
    "enabled",
    "window",
    "read_window",
    "fs_attribution",
    "enforce_claims",
    "dir_depth",
    "poll_interval",
    "settle",
    "watcher",
    "notify",
    "ignore",
    "max_touches",
];

fn dur(v: &toml::Value, min_ms: u128) -> Result<Duration, String> {
    let d = match v {
        toml::Value::String(s) => Dur::parse(s)?.0,
        toml::Value::Integer(i) if *i >= 0 => Duration::from_secs(*i as u64),
        _ => return Err("expected a duration like \"30m\" or a number of seconds".into()),
    };
    if d.as_millis() < min_ms {
        return Err(format!("must be at least {min_ms} ms"));
    }
    Ok(d)
}

impl Collision {
    /// Read `[collision]` from its raw value; every bad or unknown key is a warning and keeps
    /// its default.
    pub fn from_value(v: Option<&toml::Value>) -> (Collision, Vec<Warning>) {
        let mut out = Collision::default();
        let mut warns = Vec::new();
        let Some(t) = v.and_then(toml::Value::as_table) else {
            if v.is_some() {
                warns.push(Warning::new("collision", "`collision` must be a table"));
            }
            return (out, warns);
        };
        for (k, val) in t {
            let key = format!("collision.{k}");
            let mut bad = |why: &str| warns.push(Warning::new(&key, format!("{key}: {why}")));
            match k.as_str() {
                "enabled" | "enforce_claims" | "notify" => match val.as_bool() {
                    Some(b) => match k.as_str() {
                        "enabled" => out.enabled = b,
                        "enforce_claims" => out.enforce_claims = b,
                        _ => out.notify = b,
                    },
                    None => bad("expected true or false"),
                },
                "window" | "read_window" | "poll_interval" | "settle" => {
                    let min = if k == "settle" { 0 } else { 1000 };
                    match dur(val, min) {
                        Ok(d) => match k.as_str() {
                            "window" => out.window = d,
                            "read_window" => out.read_window = d,
                            "poll_interval" => out.poll_interval = d,
                            _ => out.settle = d,
                        },
                        Err(e) => bad(&e),
                    }
                }
                "fs_attribution" => match val.as_str() {
                    Some("auto") => out.fs_attribution = FsAttribution::Auto,
                    Some("off") => out.fs_attribution = FsAttribution::Off,
                    Some("aggressive") => out.fs_attribution = FsAttribution::Aggressive,
                    _ => bad("expected \"auto\", \"off\" or \"aggressive\""),
                },
                "watcher" => match val.as_str() {
                    Some(w @ ("os" | "none")) => out.watcher = w.into(),
                    _ => bad("expected \"os\" or \"none\""),
                },
                "dir_depth" => match val.as_integer() {
                    Some(n) if (0..=16).contains(&n) => out.dir_depth = n as usize,
                    _ => bad("expected an integer from 0 to 16"),
                },
                "max_touches" => match val.as_integer() {
                    Some(n) if (100..=1_000_000).contains(&n) => out.max_touches = n as usize,
                    _ => bad("expected an integer from 100 to 1000000"),
                },
                "ignore" => match val.as_array() {
                    Some(a) if a.iter().all(toml::Value::is_str) => {
                        out.ignore = a
                            .iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    }
                    _ => bad("expected a list of path globs"),
                },
                _ => warns.push(Warning::new(&key, format!("unknown key `{key}`"))),
            }
        }
        (out, warns)
    }
}

impl crate::Config {
    /// The typed `[collision]` view.
    pub fn collision(&self) -> Collision {
        Collision::from_value(self.extra.get("collision")).0
    }
}
