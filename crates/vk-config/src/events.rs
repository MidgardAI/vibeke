//! Typed `[events]` keys (02 §2.3): how long the event log keeps what, and the row cap.
//!
//! Like `[preview]`, the section stays external (`Config::extra["events"]`, read by the server's
//! storage sweep); this module gives every key one type and default and validates it at load
//! time: a bad value is a located warning and keeps that key's default, so one typo never
//! disables retention or empties the log.

use crate::load::Warning;
use crate::units::Dur;

/// `[events]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Events {
    /// `sync`-tier events (catch-up and live UI) are pruned after this many days (default 7).
    pub sync_days: i64,
    /// `history`-tier events (interactions, policy, task status, agent start/exit) are kept this
    /// many days (default 365).
    pub history_days: i64,
    /// Unreferenced uploads and tool-output payloads in the blob store are collected after this
    /// many days (default 30).
    pub blob_days: i64,
    /// Hard cap on event rows (default 2 000 000); the oldest `sync` rows go first, `history` only
    /// when no `sync` row is left. `0` disables the cap.
    pub max_rows: i64,
}

impl Default for Events {
    fn default() -> Self {
        Events {
            sync_days: 7,
            history_days: 365,
            blob_days: 30,
            max_rows: 2_000_000,
        }
    }
}

const KEYS: &[&str] = &[
    "sync_retention",
    "history_retention",
    "blob_retention",
    "max_rows",
];

/// A duration string (`"7d"`, `"36h"`) or a bare integer (days) as whole days, at least 1.
fn days(v: &toml::Value) -> Result<i64, String> {
    let d = match v {
        toml::Value::String(s) => {
            let secs = Dur::parse(s)?.0.as_secs() as i64;
            (secs + 86_399) / 86_400
        }
        toml::Value::Integer(i) => *i,
        _ => return Err("expected a duration like \"7d\" or a number of days".into()),
    };
    if d >= 1 {
        Ok(d)
    } else {
        Err("must be at least one day".into())
    }
}

impl Events {
    /// Read `[events]` from its raw value; every bad or unknown key is a warning and keeps its
    /// default.
    pub fn from_value(v: Option<&toml::Value>) -> (Events, Vec<Warning>) {
        let mut out = Events::default();
        let mut warns = Vec::new();
        let Some(t) = v.and_then(toml::Value::as_table) else {
            if v.is_some() {
                warns.push(Warning::new("events", "`events` must be a table"));
            }
            return (out, warns);
        };
        for (k, val) in t {
            let key = format!("events.{k}");
            if !KEYS.contains(&k.as_str()) {
                warns.push(Warning::new(&key, format!("unknown key `{key}`")));
                continue;
            }
            if k == "max_rows" {
                match val.as_integer() {
                    Some(n) if n >= 0 => out.max_rows = n,
                    _ => warns.push(Warning::new(
                        &key,
                        format!("{key}: expected a non-negative integer (0 = no cap)"),
                    )),
                }
                continue;
            }
            match days(val) {
                Ok(d) => match k.as_str() {
                    "sync_retention" => out.sync_days = d,
                    "history_retention" => out.history_days = d,
                    _ => out.blob_days = d,
                },
                Err(e) => warns.push(Warning::new(&key, format!("{key}: {e}"))),
            }
        }
        (out, warns)
    }
}

impl crate::Config {
    /// The typed `[events]` view.
    pub fn events(&self) -> Events {
        Events::from_value(self.extra.get("events")).0
    }
}
