//! Secrets and privacy settings (09 §9.1–9.2, lane 3E):
//!
//! - `security.encrypt_state`: seal scrollback segments and session blobs with a key from the
//!   OS keychain (`[security] keychain = "os" | "file:<path>"`, vk-store `crypt`/`keychain`).
//!   Blob writes are sealed inside the unified store (`blob_store::store`, `vk_store::blobs`).
//!   Existing files keep their mode; `security.encryption.migrate` rewrites them. A sealed blob
//!   handed to an agent by path is decrypted into the runtime dir (`<runtime>/plain/`, 0700,
//!   pruned hourly and on restart) so tools can open it.
//! - `security.redact_scrollback_index` (default true): archive rows are indexed redacted
//!   (vk-redact plus `[security.redact] patterns`), and `search.query` results are redacted;
//!   results for remote clients are always redacted (09 §9.1 table). `pane.read` paging and the
//!   segments themselves stay as the terminal showed them.
//! - The OS keychain backend for assistant credential references ([`keychain`]).
//!
//! Settings are read from the user's config (never a repository file), cached for a few
//! seconds; a change to the encryption settings is applied within 30 s by [`start`]'s loop.

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, s};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vk_proto::rpc::ErrorKind;
use vk_store::crypt::{self, StateCipher};
use vk_store::keychain::Keychain;

pub const METHODS: &[(&str, bool)] = &[
    ("state.forget", true),
    ("security.encryption.status", false),
    ("security.encryption.migrate", true),
];

/// Full scope only (09 §5.2): purging state and inspecting or changing encryption are user
/// actions.
pub const PANE_FORBIDDEN: &[&str] = &[
    "state.forget",
    "security.encryption.status",
    "security.encryption.migrate",
];

const SETTINGS_TTL: Duration = Duration::from_secs(5);
const PLAIN_VIEW_TTL: Duration = Duration::from_secs(3600);

/// The `[security]` keys this module reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub encrypt_state: bool,
    /// `[security] keychain`, parsed (`Err`: the setting is invalid).
    pub keychain: Result<Keychain, String>,
    pub redact_scrollback_index: bool,
    /// `[security.redact] patterns`.
    pub patterns: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            encrypt_state: false,
            keychain: Ok(Keychain::Os),
            redact_scrollback_index: true,
            patterns: vec![],
        }
    }
}

impl Settings {
    pub fn from_config(cfg: &vk_config::Config) -> Settings {
        let d = Settings::default();
        let Some(t) = cfg.extra.get("security").and_then(|v| v.as_table()) else {
            return d;
        };
        Settings {
            encrypt_state: t
                .get("encrypt_state")
                .and_then(|v| v.as_bool())
                .unwrap_or(d.encrypt_state),
            keychain: match t.get("keychain") {
                Some(v) => match v.as_str() {
                    Some(x) => Keychain::from_setting(x),
                    None => Err("keychain must be a string".into()),
                },
                None => Ok(Keychain::Os),
            },
            redact_scrollback_index: t
                .get("redact_scrollback_index")
                .and_then(|v| v.as_bool())
                .unwrap_or(d.redact_scrollback_index),
            patterns: t
                .get("redact")
                .and_then(|r| r.get("patterns"))
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// From the user's config file (defaults when it can't be read).
    pub fn load() -> Settings {
        vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| Settings::from_config(&c))
            .unwrap_or_default()
    }
}

/// Encryption state as last applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EncStatus {
    pub requested: bool,
    pub active: bool,
    pub key_id: Option<String>,
    pub keychain: Option<String>,
    pub error: Option<String>,
    /// A key for previously sealed files is unlocked (also with encryption off).
    pub readable: bool,
    /// Requested but not active: scrollback archiving and blob writes are paused (fail
    /// closed) until the key unlocks; the unlock is retried every 30 s.
    pub writes_paused: bool,
}

#[derive(Default)]
pub struct State {
    cache: Mutex<Option<(Instant, Settings)>>,
    redactor: Mutex<Option<(Vec<String>, Arc<vk_redact::Redactor>)>>,
    status: Mutex<EncStatus>,
    /// The settings the encryption state was last applied from (`encrypt_state`, keychain).
    applied: Mutex<Option<(bool, Result<Keychain, String>)>>,
    /// Test override of the settings (no config file involved).
    #[cfg(test)]
    pub(crate) forced: Mutex<Option<Settings>>,
}

/// Current settings (cached for a few seconds).
pub fn settings(server: &Server) -> Settings {
    #[cfg(test)]
    if let Some(s) = server.privacy.forced.lock().unwrap().clone() {
        return s;
    }
    let mut c = server.privacy.cache.lock().unwrap();
    if let Some((t, s)) = c.as_ref()
        && t.elapsed() < SETTINGS_TTL
    {
        return s.clone();
    }
    let s = Settings::load();
    *c = Some((Instant::now(), s.clone()));
    s
}

/// The keychain backend the user configured (an invalid setting falls back to the OS one).
pub fn keychain(server: &Server) -> Keychain {
    settings(server).keychain.unwrap_or(Keychain::Os)
}

fn redactor(server: &Server, patterns: &[String]) -> Arc<vk_redact::Redactor> {
    let mut r = server.privacy.redactor.lock().unwrap();
    if let Some((p, red)) = r.as_ref()
        && p == patterns
    {
        return red.clone();
    }
    // Invalid user patterns are skipped one by one rather than disabling redaction.
    let valid: Vec<String> = patterns
        .iter()
        .filter(|p| regex::Regex::new(p).is_ok())
        .cloned()
        .collect();
    let red =
        Arc::new(vk_redact::Redactor::new(&valid).unwrap_or_else(|_| {
            vk_redact::Redactor::new(&[]).expect("empty pattern set is valid")
        }));
    *r = Some((patterns.to_vec(), red.clone()));
    red
}

/// Redact `text` with the built-in and user patterns.
pub fn redact_text(server: &Server, text: &str) -> String {
    let s = settings(server);
    redactor(server, &s.patterns).redact(text).into_owned()
}

// ---- encryption ---------------------------------------------------------------------------

/// Called once from `Server::new`, before anything is archived.
pub fn init(server: &Arc<Server>) {
    let _ = std::fs::remove_dir_all(plain_dir(server));
    let s = settings(server);
    apply(server, &s);
}

/// Bring the archive cipher and the status in line with `s` (no-op when unchanged and in
/// force). Requested-but-locked is never cached: every call retries the unlock.
pub fn apply(server: &Server, s: &Settings) {
    let key = (s.encrypt_state, s.keychain.clone());
    {
        let mut a = server.privacy.applied.lock().unwrap();
        if a.as_ref() == Some(&key) && !status(server).writes_paused {
            return;
        }
        *a = Some(key);
    }
    let state_dir = server.paths.state.clone();
    let mut st = EncStatus {
        requested: s.encrypt_state,
        ..Default::default()
    };
    let mut cipher: Option<Arc<StateCipher>> = None;
    if s.encrypt_state {
        match &s.keychain {
            Err(e) => st.error = Some(format!("[security] {e}")),
            Ok(kc) => {
                st.keychain = Some(kc.describe());
                match crypt::unlock(&state_dir, kc, true) {
                    Ok(Some(u)) => {
                        st.key_id = Some(u.cipher.id_hex());
                        st.active = true;
                        st.readable = true;
                        cipher = Some(u.cipher);
                    }
                    Ok(None) => st.error = Some("no state key".into()),
                    Err(e) => st.error = Some(e.to_string()),
                }
            }
        }
    } else if crypt::read_marker(&state_dir).is_some() {
        // Off, but files sealed earlier must stay readable.
        match crypt::unlock_for_reading(&state_dir) {
            Ok(Some(c)) => {
                st.key_id = Some(c.id_hex());
                st.readable = true;
            }
            Ok(None) => {}
            Err(e) => st.error = Some(e.to_string()),
        }
    }
    // Fail closed (09 §9.1): encryption asked for but no key means nothing new is persisted
    // in plaintext — the archive and blob writes pause until the unlock succeeds.
    st.writes_paused = s.encrypt_state && cipher.is_none();
    {
        let mut a = server.archive.lock().unwrap();
        if let Err(e) = a.set_paused(st.writes_paused) {
            st.error.get_or_insert(e.to_string());
        }
        if let Err(e) = a.set_cipher(cipher) {
            st.error.get_or_insert(e.to_string());
        }
    }
    if let Some(e) = &st.error {
        tracing::error!(error = %e, "state encryption");
    }
    *server.privacy.status.lock().unwrap() = st;
}

/// The cipher new blobs and segments are sealed with.
pub fn cipher(server: &Server) -> Option<Arc<StateCipher>> {
    server.archive.lock().unwrap().cipher().cloned()
}

pub fn status(server: &Server) -> EncStatus {
    server.privacy.status.lock().unwrap().clone()
}

/// Encryption is requested but locked: archive and blob writes are paused.
pub fn writes_paused(server: &Server) -> bool {
    server.archive.lock().unwrap().paused()
}

/// Background loop: re-apply changed encryption settings (notifying the user when encryption
/// was asked for but could not start) and prune decrypted blob views.
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    tokio::spawn(async move {
        let mut last_error: Option<String> = None;
        let mut prune_at = Instant::now();
        loop {
            let s = settings(&srv);
            let srv2 = srv.clone();
            // Keychain tools may block (an unlock prompt): keep them off the runtime threads.
            let _ = tokio::task::spawn_blocking(move || apply(&srv2, &s)).await;
            let st = status(&srv);
            if st.requested && !st.active && st.error != last_error {
                srv.notify(
                    "security",
                    None,
                    "State encryption is not active",
                    "security.encrypt_state is on but the state key could not be unlocked; new scrollback and blobs are not saved (nothing is written unencrypted) until it unlocks. Retrying every 30 s. See `vibeke security status`.",
                    "high",
                );
            }
            last_error = st.error.clone();
            if prune_at.elapsed() >= Duration::from_secs(600) {
                prune_plain_views(&srv, PLAIN_VIEW_TTL);
                prune_at = Instant::now();
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

fn plain_dir(server: &Server) -> PathBuf {
    server.paths.runtime.join("plain")
}

/// Remove decrypted views older than `ttl`.
pub fn prune_plain_views(server: &Server, ttl: Duration) -> usize {
    let Ok(rd) = std::fs::read_dir(plain_dir(server)) else {
        return 0;
    };
    let mut n = 0;
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= ttl);
        if old && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
        }
    }
    n
}

/// Read a blob file, decrypting a sealed one.
pub fn read_blob(path: &Path) -> std::io::Result<Vec<u8>> {
    crypt::read_file(path)
}

/// Plaintext size of a blob file.
pub fn blob_len(path: &Path) -> u64 {
    crypt::plain_len(path).unwrap_or(0)
}

/// A path a tool can open: the file itself, or for a sealed blob a decrypted copy in the
/// runtime dir (0600 in a 0700 dir; pruned after an hour and at restart). Falls back to the
/// stored path when the copy can't be made.
pub fn readable_path(server: &Server, path: &Path) -> PathBuf {
    if !crypt::file_is_sealed(path) {
        return path.to_path_buf();
    }
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let dir = plain_dir(server);
    let out = dir.join(name);
    if out.exists() {
        return out;
    }
    let made = crate::paths::ensure_private_dir(&dir)
        .and_then(|()| crypt::read_file(path))
        .and_then(|data| crypt::write_file(&out, &data, None));
    match made {
        Ok(()) => out,
        Err(e) => {
            tracing::warn!(error = %e, "could not decrypt a blob view");
            path.to_path_buf()
        }
    }
}

// ---- scrollback index redaction -------------------------------------------------------------

/// Archive rows on their way into `scrollback_fts` (called from `Server::housekeeping`).
pub fn index_rows(
    server: &Server,
    mut rows: Vec<(String, u64, i64, String)>,
) -> Vec<(String, u64, i64, String)> {
    let s = settings(server);
    if !s.redact_scrollback_index || rows.is_empty() {
        return rows;
    }
    let red = redactor(server, &s.patterns);
    for r in &mut rows {
        if let std::borrow::Cow::Owned(t) = red.redact(&r.3) {
            r.3 = t;
        }
    }
    rows
}

/// The text transform `doctor --rebuild-index` applies (same settings as the live indexer).
pub fn index_transform(cfg: &vk_config::Config) -> impl Fn(&str) -> String {
    let s = Settings::from_config(cfg);
    let valid: Vec<String> = s
        .patterns
        .iter()
        .filter(|p| regex::Regex::new(p).is_ok())
        .cloned()
        .collect();
    let red = vk_redact::Redactor::new(&valid).ok();
    move |t: &str| match (&red, s.redact_scrollback_index) {
        (Some(r), true) => r.redact(t).into_owned(),
        _ => t.to_string(),
    }
}

/// Redact `search.query` hits: when the index is redacted (so live and archive hits agree)
/// and always for remote clients.
pub fn redact_search(server: &Server, ctx: &Ctx, v: &mut Value) {
    let s = settings(server);
    if !s.redact_scrollback_index && !ctx.remote {
        return;
    }
    let red = redactor(server, &s.patterns);
    let fix = |x: &mut Value| {
        if let Some(t) = x.as_str()
            && let std::borrow::Cow::Owned(n) = red.redact(t)
        {
            *x = Value::String(n);
        }
    };
    let Some(hits) = v.get_mut("hits").and_then(Value::as_array_mut) else {
        return;
    };
    for h in hits {
        for k in ["text", "title"] {
            if let Some(x) = h.get_mut(k) {
                fix(x);
            }
        }
        if let Some(c) = h.get_mut("context") {
            for k in ["before", "after"] {
                if let Some(a) = c.get_mut(k).and_then(Value::as_array_mut) {
                    a.iter_mut().for_each(fix);
                }
            }
        }
    }
    v["redacted"] = json!(true);
}

// ---- server.log ------------------------------------------------------------------------------

/// A stderr writer that redacts each record (the server's tracing output, which `server.log`
/// captures) with the built-in vk-redact patterns (09 §9.2: logs are always redacted).
pub struct LogRedactor;

/// `tracing_subscriber::fmt().with_writer(log_writer)`.
pub fn log_writer() -> LogRedactor {
    LogRedactor
}

impl std::io::Write for LogRedactor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let out = vk_redact::redact(&text);
        std::io::stderr().write_all(out.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

// ---- API ------------------------------------------------------------------------------------

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "search.query" => crate::search::query(server, ctx, p).map(|mut v| {
            redact_search(server, ctx, &mut v);
            v
        }),
        "state.forget" => Box::pin(crate::forget_scope::forget(server, ctx, p))
            .await
            .map(|mut v| {
                // Pre-migration backups are whole copies of state.db made before this forget:
                // they still hold what was forgotten, so they go too (not on a dry run).
                if p.get("dry_run").and_then(Value::as_bool) != Some(true) {
                    let n = vk_store::backup::forget_backups(&server.paths.db());
                    if let Some(o) = v.as_object_mut() {
                        o.insert("backups_removed".into(), json!(n));
                    }
                }
                v
            }),
        "security.encryption.status" => Ok(status_json(server)),
        "security.encryption.migrate" => migrate(server, p).await,
        _ => return None,
    })
}

fn status_json(server: &Server) -> Value {
    let s = settings(server);
    let st = status(server);
    let (mut sealed, mut plain) = (0u64, 0u64);
    for f in crypt::sealable_files(&server.paths.state) {
        if crypt::file_is_sealed(&f) {
            sealed += 1;
        } else {
            plain += 1;
        }
    }
    json!({
        "encrypt_state": s.encrypt_state,
        "active": st.active,
        "key_id": st.key_id,
        "keychain": st.keychain.or_else(|| s.keychain.as_ref().ok().map(Keychain::describe)),
        "readable": st.readable,
        "writes_paused": st.writes_paused,
        "error": st.error,
        "files": {"sealed": sealed, "plain": plain},
        "covers": ["scrollback segments", "session blobs (screenshots, pane screenshots, diffs)"],
        "not_covered": ["state.db (entities, events, search index)", "pane inbox uploads (handed to agents by path)", "desk.db", "logs and audit.jsonl"],
        "redact_scrollback_index": s.redact_scrollback_index,
        "note": "protects backups and disk images, not against processes running as you",
    })
}

/// `security.encryption.migrate {to: sealed|plain, dry_run?}`: rewrite existing scrollback
/// segments and blobs into the requested mode. Open segments are closed first; each file is
/// replaced atomically.
async fn migrate(server: &Arc<Server>, p: &Value) -> R {
    let to = s(p, "to").unwrap_or("sealed");
    let seal = match to {
        "sealed" | "encrypted" => true,
        "plain" | "decrypted" => false,
        _ => return Err(invalid("to must be `sealed` or `plain`")),
    };
    let dry = p.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let srv = server.clone();
    tokio::task::spawn_blocking(move || -> R {
        let mut a = srv.archive.lock().unwrap();
        let c = a.cipher().cloned();
        if seal && c.is_none() {
            return Err(err(
                ErrorKind::Conflict,
                "encryption is not active: set security.encrypt_state = true (and check `vibeke security status`) first",
            ));
        }
        // Close open segments so no writer appends to a file being rewritten.
        a.set_cipher(None).map_err(internal)?;
        let files = crypt::sealable_files(&srv.paths.state);
        let (mut changed, mut kept, mut failed) = (0u64, 0u64, Vec::<String>::new());
        for f in &files {
            if crypt::file_is_sealed(f) == seal {
                kept += 1;
                continue;
            }
            if dry {
                changed += 1;
                continue;
            }
            let r = crypt::read_file(f)
                .and_then(|data| crypt::write_file(f, &data, if seal { c.as_deref() } else { None }));
            match r {
                Ok(()) => changed += 1,
                Err(e) => failed.push(format!(
                    "{}: {e}",
                    f.strip_prefix(&srv.paths.state).unwrap_or(f).display()
                )),
            }
        }
        a.set_cipher(c).map_err(internal)?;
        drop(a);
        if !dry && changed > 0 {
            let _ = std::fs::remove_dir_all(plain_dir(&srv));
            let mut core = srv.core.lock().unwrap();
            let mut tx = crate::core::Tx::new();
            tx.event(
                "security.encryption_migrated",
                json!({}),
                json!({"to": if seal { "sealed" } else { "plain" }, "files": changed, "failed": failed.len()}),
            );
            srv.commit(&mut core, tx).map_err(internal)?;
        }
        Ok(json!({
            "to": if seal { "sealed" } else { "plain" },
            "dry_run": dry,
            "files": files.len(),
            "changed": changed,
            "unchanged": kept,
            "failed": failed,
        }))
    })
    .await
    .map_err(internal)?
}

#[cfg(test)]
#[path = "privacy_tests.rs"]
mod tests;
