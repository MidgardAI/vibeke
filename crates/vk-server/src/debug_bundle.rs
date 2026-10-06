//! `vibeke debug bundle` (09 §9.5): a redacted diagnostics archive for bug reports.
//!
//! The archive (an uncompressed tar, 0600) contains: a manifest of what is included and what is
//! deliberately left out; the version and platform; the server's logs (the last 1 MiB of each,
//! every line through `vk-redact`); `state.db`'s schema, migration version and per-table row
//! counts (never row content); the configuration with every secret-looking value and every
//! environment variable value removed (keys only); runtime/state directory checks; the audit
//! log's chain verification (not its entries); integration integrity checks; and, from the
//! caller, `vibeke doctor` output and the running server's status and pane summary.
//!
//! Never included: tokens or holder keys, `state.db` content, environment values, credential
//! files, and scrollback or screen text — unless the user asks with `--include-scrollback`
//! (the archived scrollback of every pane, its last 5000 lines each) or `--include-pane <p>`
//! (that pane's current screen), and then still redacted.

use crate::paths::Paths;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What the caller adds and asks for.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub out: Option<PathBuf>,
    pub include_scrollback: bool,
    /// `(pane, screen text)` for `--include-pane`.
    pub panes: Vec<(String, String)>,
    /// `vibeke doctor` output.
    pub doctor: Option<String>,
    /// `server.status` and a pane summary from the running server, if any.
    pub server: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Bundle {
    pub path: PathBuf,
    /// `(archive member, bytes, description)`.
    pub files: Vec<(String, usize, String)>,
    pub excluded: Vec<String>,
}

/// Text redacted for the bundle: the shared patterns plus Vibeke's own token shapes, applied
/// to the whole text at once, so multi-line secrets (PEM private key blocks, whose body lines
/// look harmless one by one) are removed entirely, and JSON-escaped ones (`\n` inside a
/// string) too. A truncated block (no END line) is redacted to the end of the text.
pub fn redact_text(s: &str) -> String {
    vk_redact::redact(s).into_owned()
}

/// Replace every 64-hex run whose blake3 hash is a known pane token hash.
pub fn scrub_tokens(s: &str, hashes: &std::collections::HashSet<String>) -> String {
    static HEX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\b[0-9a-f]{64}\b").expect("valid"));
    if hashes.is_empty() {
        return s.to_string();
    }
    HEX.replace_all(s, |c: &regex::Captures| {
        let h = blake3::hash(c[0].as_bytes()).to_hex().to_string();
        if hashes.contains(&h) {
            vk_redact::REDACTED.to_string()
        } else {
            c[0].to_string()
        }
    })
    .into_owned()
}

fn tail_bytes(p: &Path, max: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0
        && let Some(i) = s.find('\n')
    {
        s = format!("[… {start} earlier bytes omitted]\n{}", &s[i + 1..]);
    }
    Ok(s)
}

/// The configuration as JSON with secrets and environment values removed.
pub fn redacted_config(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return json!({"path": path, "error": e.to_string()}),
    };
    let mut v = match toml::from_str::<toml::Value>(&text) {
        Ok(t) => serde_json::to_value(t).unwrap_or(Value::Null),
        Err(e) => {
            return json!({"path": path, "error": format!("parse: {}", e.message().lines().next().unwrap_or(""))});
        }
    };
    fn blank_env(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if k == "env" || k.ends_with("_env") {
                        if let Value::Object(e) = x {
                            for val in e.values_mut() {
                                *val = json!("[value omitted]");
                            }
                        } else if !x.is_null() {
                            *x = json!("[value omitted]");
                        }
                    } else {
                        blank_env(x);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(blank_env),
            _ => {}
        }
    }
    blank_env(&mut v);
    vk_redact::redact_json(&mut v);
    json!({"path": path, "config": v})
}

fn db_report(db: &Path) -> Value {
    if !db.exists() {
        return json!({"path": db, "exists": false});
    }
    match vk_store::Diagnostics::open(db) {
        Ok(d) => json!({
            "path": db,
            "schema_version": d.schema_version().ok(),
            "tables": d.table_counts().unwrap_or_default().into_iter().map(|(n, c)| json!({"table": n, "rows": c})).collect::<Vec<_>>(),
            "schema": d.schema_sql().unwrap_or_default(),
        }),
        Err(e) => json!({"path": db, "error": format!("{e:#}")}),
    }
}

fn dirs_report(paths: &Paths) -> Value {
    let dirs = [
        ("runtime_root", crate::paths::runtime_root()),
        ("runtime", paths.runtime.clone()),
        ("holders", paths.holders()),
        ("state_root", crate::paths::state_root()),
        ("state", paths.state.clone()),
        ("logs", paths.logs()),
    ];
    let rows: Vec<Value> = dirs
        .iter()
        .map(|(name, d)| {
            json!({"name": name, "path": d, "exists": d.exists(),
                   "problem": d.exists().then(|| crate::paths::private_dir_problem(d)).flatten()})
        })
        .collect();
    json!({"dirs": rows, "socket_exists": paths.socket().exists(),
           "umask": crate::paths::user_umask().map(|m| format!("{m:03o}"))})
}

fn audit_report(paths: &Paths) -> Value {
    let log = paths.audit_log();
    let v = crate::audit::verify_file(&log);
    let mut counts = std::collections::BTreeMap::<String, u64>::new();
    for e in crate::audit::read_entries(&log, &[], None, None, usize::MAX) {
        *counts
            .entry(e["type"].as_str().unwrap_or("?").to_string())
            .or_default() += 1;
    }
    let mut r = v.to_json();
    r["types"] = json!(counts);
    r["path"] = json!(log);
    r
}

fn scrollback_text(paths: &Paths) -> Vec<(String, String)> {
    let mut a = vk_store::archive::Archive::new(&paths.scrollback());
    let mut out = vec![];
    for pane in a.pane_ids() {
        let Ok(Some(last)) = a.last_line(&pane) else {
            continue;
        };
        let first = a.first_line(&pane).ok().flatten().unwrap_or(0);
        let from = last.saturating_sub(5000).max(first);
        let rows = a.read(&pane, from, last + 1).unwrap_or_default();
        let text: Vec<String> = rows.iter().map(|r| r.t.clone()).collect();
        out.push((pane, redact_text(&text.join("\n"))));
    }
    out
}

/// A minimal ustar writer (regular files only, 0600, names < 100 bytes).
struct Tar {
    buf: Vec<u8>,
}

impl Tar {
    fn add(&mut self, name: &str, data: &[u8]) {
        let mut h = [0u8; 512];
        let name = &name.as_bytes()[..name.len().min(99)];
        h[..name.len()].copy_from_slice(name);
        let mut field = |off: usize, len: usize, v: String| {
            let b = v.as_bytes();
            h[off..off + b.len().min(len)].copy_from_slice(&b[..b.len().min(len)]);
        };
        field(100, 8, "0000600\0".into());
        field(108, 8, "0000000\0".into());
        field(116, 8, "0000000\0".into());
        field(124, 12, format!("{:011o}\0", data.len()));
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        field(136, 12, format!("{mtime:011o}\0"));
        field(148, 8, "        ".into());
        field(156, 1, "0".into());
        field(257, 6, "ustar\0".into());
        field(263, 2, "00".into());
        let sum: u32 = h.iter().map(|b| *b as u32).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        self.buf.extend_from_slice(&h);
        self.buf.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.buf.extend(std::iter::repeat_n(0u8, pad));
    }
    fn finish(mut self) -> Vec<u8> {
        self.buf.extend(std::iter::repeat_n(0u8, 1024));
        self.buf
    }
}

/// Build the bundle for `paths` (the session) into `opts.out` (default
/// `./vibeke-debug-<session>-<time>.tar`).
pub fn build(paths: &Paths, opts: &Options) -> Result<Bundle> {
    let mut files: Vec<(String, Vec<u8>, String)> = vec![];
    let mut add =
        |name: &str, data: Vec<u8>, what: &str| files.push((name.into(), data, what.into()));
    let pretty = |v: &Value| serde_json::to_vec_pretty(v).unwrap_or_default();

    add(
        "version.json",
        pretty(
            &json!({"version": vk_proto::VERSION, "api": vk_proto::API_VERSION, "os": std::env::consts::OS,
                       "arch": std::env::consts::ARCH, "session": paths.session, "created_at_ms": vk_store::now_ms()}),
        ),
        "Vibeke version, API version, OS and architecture",
    );
    add(
        "config.json",
        pretty(&redacted_config(&vk_config::config_path())),
        "config.toml with secrets and environment values removed",
    );
    add(
        "db.json",
        pretty(&db_report(&paths.db())),
        "state.db schema, migration version and row counts (no content)",
    );
    add(
        "dirs.json",
        pretty(&dirs_report(paths)),
        "runtime and state directory ownership/mode checks",
    );
    add(
        "audit.json",
        pretty(&audit_report(paths)),
        "audit log chain verification and entry counts by type (no entries)",
    );
    let dirs = vk_agents::Dirs::from_env();
    let checks: Vec<Value> = vk_agents::Harness::ALL
        .iter()
        .map(|h| crate::integrity::check(*h, &dirs).to_json())
        .collect();
    add(
        "integrations.json",
        pretty(&json!({"checks": checks})),
        "integration install and tamper checks",
    );
    if let Ok(rd) = std::fs::read_dir(paths.logs()) {
        let mut logs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        logs.sort();
        for l in logs {
            let name = l
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if let Ok(t) = tail_bytes(&l, 1 << 20) {
                add(
                    &format!("logs/{name}"),
                    redact_text(&t).into_bytes(),
                    "log tail (last 1 MiB), redacted",
                );
            }
        }
    }
    if let Some(d) = &opts.doctor {
        add(
            "doctor.txt",
            redact_text(d).into_bytes(),
            "`vibeke doctor` output, redacted",
        );
    }
    if let Some(s) = &opts.server {
        let mut s = s.clone();
        vk_redact::redact_json(&mut s);
        add(
            "server.json",
            pretty(&s),
            "running server status and pane summary, redacted",
        );
    }
    for (pane, text) in &opts.panes {
        add(
            &format!("panes/{pane}.txt"),
            redact_text(text).into_bytes(),
            "pane screen (--include-pane), redacted",
        );
    }
    if opts.include_scrollback {
        for (pane, text) in scrollback_text(paths) {
            add(
                &format!("scrollback/{pane}.txt"),
                text.into_bytes(),
                "archived scrollback, last 5000 lines (--include-scrollback), redacted",
            );
        }
    }
    let mut excluded = vec![
        "tokens, holder keys and elevated tokens (only hashes exist, none are included)"
            .to_string(),
        "state.db content".into(),
        "environment variable values".into(),
        "credential files (.env, auth.json, SSH keys)".into(),
        "audit log entries".into(),
    ];
    if !opts.include_scrollback {
        excluded.push("scrollback (add --include-scrollback)".into());
    }
    if opts.panes.is_empty() {
        excluded.push("pane screens (add --include-pane <pane>)".into());
    }
    let manifest = json!({
        "files": files.iter().map(|(n, d, w)| json!({"name": n, "bytes": d.len(), "what": w})).collect::<Vec<_>>(),
        "excluded": excluded,
        "redaction": "every text file passed through vk-redact (09 §9.2); pattern redaction is best-effort, review before sharing",
    });
    // A pane token a pane printed (it is in its environment) is a bare 64-hex string the
    // patterns can't know; recognise it by its stored hash.
    let hashes: std::collections::HashSet<String> = vk_store::Diagnostics::open(&paths.db())
        .map(|d| d.token_hashes().into_iter().collect())
        .unwrap_or_default();
    let files: Vec<(String, Vec<u8>, String)> = files
        .into_iter()
        .map(|(n, d, w)| {
            let s = String::from_utf8_lossy(&d);
            (n, scrub_tokens(&s, &hashes).into_bytes(), w)
        })
        .collect();
    let mut tar = Tar { buf: vec![] };
    tar.add("manifest.json", &serde_json::to_vec_pretty(&manifest)?);
    for (n, d, _) in &files {
        tar.add(n, d);
    }
    let out = opts.out.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "vibeke-debug-{}-{}.tar",
            paths.session,
            vk_store::now_ms() / 1000
        ))
    });
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&out)
            .with_context(|| format!("create {}", out.display()))?;
        f.write_all(&tar.finish())?;
    }
    Ok(Bundle {
        path: out,
        files: files.into_iter().map(|(n, d, w)| (n, d.len(), w)).collect(),
        excluded,
    })
}

/// Read back the members of a bundle (tests, inspection): `(name, contents)`.
pub fn read_tar(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![];
    let mut i = 0;
    while i + 512 <= bytes.len() {
        let h = &bytes[i..i + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let name = String::from_utf8_lossy(&h[..100])
            .trim_end_matches('\0')
            .to_string();
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&h[124..135]).trim_matches(|c: char| c == '\0' || c == ' '),
            8,
        )
        .unwrap_or(0);
        let start = i + 512;
        out.push((name, bytes[start..(start + size).min(bytes.len())].to_vec()));
        i = start + size.div_ceil(512) * 512;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_roundtrip() {
        let mut t = Tar { buf: vec![] };
        t.add("a.txt", b"hello");
        t.add("dir/b.json", &[b'x'; 1000]);
        let bytes = t.finish();
        assert_eq!(bytes.len() % 512, 0);
        let m = read_tar(&bytes);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], ("a.txt".into(), b"hello".to_vec()));
        assert_eq!(m[1].1.len(), 1000);
    }

    /// Review batch 2, finding 6: a multi-line PEM private key in a log, a pane screen or the
    /// doctor output leaves none of its body lines in the bundle (also JSON-escaped).
    #[test]
    fn multiline_pem_keys_are_removed_from_every_member() {
        let body = [
            "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj",
            "MzEfYyjiWA4R4/M2bS1GB4t7NXp98C3SC6dVMvDuictGeurT8jNbvJZHtCSuYEvu",
            "NMoSfm76oqFvAp8Gy0iz5sxjZmSnXyCdPEovGhLa0VzMaQ8s+CLOyS56YyCFGeJZ",
        ];
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            body.join("\n")
        );
        let escaped = format!(
            "{{\"private_key\": \"-----BEGIN RSA PRIVATE KEY-----\\n{}\\n-----END RSA PRIVATE KEY-----\\n\"}}",
            body.join("\\n")
        );
        let truncated = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n{}",
            body[0], body[1]
        );
        let root = tempfile::tempdir().unwrap();
        // The bundle includes the (redacted) config: never the user's real one here.
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if std::env::var_os("VIBEKE_CONFIG").is_none() {
                let f = std::env::temp_dir()
                    .join(format!("vk-bundle-test-{}.toml", std::process::id()));
                // SAFETY: set once, before this test reads it; other tests set the same
                // variable only when it is unset.
                unsafe { std::env::set_var("VIBEKE_CONFIG", f) };
            }
        });
        let paths = Paths {
            session: "t".into(),
            runtime: root.path().join("run"),
            state: root.path().join("state"),
        };
        std::fs::create_dir_all(paths.logs()).unwrap();
        std::fs::write(
            paths.logs().join("server.log"),
            format!("before\nkey loaded:\n{pem}\nconfig {escaped}\nafter\n"),
        )
        .unwrap();
        let opts = Options {
            out: Some(root.path().join("b.tar")),
            panes: vec![("p1".into(), format!("$ cat id\n{pem}\n$ "))],
            doctor: Some(format!("doctor\n{truncated}")),
            ..Default::default()
        };
        let b = build(&paths, &opts).unwrap();
        let members = read_tar(&std::fs::read(&b.path).unwrap());
        let all: String = members
            .iter()
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
            .collect();
        for line in body {
            assert!(!all.contains(line), "key body line survived: {line}");
            // Not even a recognizable fragment of it.
            assert!(!all.contains(&line[..24]), "fragment of {line}");
        }
        let log = members
            .iter()
            .find(|(n, _)| n == "logs/server.log")
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
            .unwrap();
        assert!(log.contains("before") && log.contains("after"), "{log}");
        assert!(log.contains(vk_redact::REDACTED), "{log}");
    }

    #[test]
    fn config_redaction_drops_env_values_and_secrets() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(
            &p,
            "[terminal.env]\nDATABASE_URL = \"postgres://u:hunter2@db/x\"\n\n[assistant.connections.a]\napi_key = \"sk-ant-abcdefghijklmnop\"\nendpoint = \"https://api.example.com\"\n",
        )
        .unwrap();
        let v = redacted_config(&p).to_string();
        assert!(!v.contains("hunter2"), "{v}");
        assert!(!v.contains("sk-ant-abcdefghijklmnop"), "{v}");
        assert!(v.contains("DATABASE_URL"), "keys stay: {v}");
        assert!(v.contains("api.example.com"));
    }
}
