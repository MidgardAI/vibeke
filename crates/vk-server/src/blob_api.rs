//! `blob.get` and `blob.stat` (07 §2.15): read content-addressed blobs by their blake3 hash.
//! Blobs live in two places today: the session blob store (`<state>/blobs/<h2>/<hash>.<ext>`,
//! with an optional `<hash>.json` metadata file: screenshots, pane screenshots) and the pane
//! inbox (`<state root>/inbox/<hash12>/<name>`: `blob.put`, chunked uploads). An inbox file only
//! counts when its content really hashes to the requested hash.

use crate::Server;
use crate::api::{R, err, internal, invalid, req, u};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[("blob.get", false), ("blob.stat", false)];

/// Largest `blob.get` response payload (decoded); bigger blobs are read with `range`.
pub const MAX_GET: u64 = 16 << 20;

pub fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("html" | "htm") => "text/html",
        Some("txt" | "log" | "md") => "text/plain",
        Some("ans") => "text/x-ansi",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("tar") => "application/x-tar",
        _ => "application/octet-stream",
    }
}

pub struct Found {
    pub path: PathBuf,
    pub mime: String,
    pub size: u64,
    pub created_at_ms: i64,
    /// Number of stored files with this content.
    pub refs: usize,
}

fn hash_file(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = blake3::Hasher::new();
    std::io::copy(&mut f, &mut h).ok()?;
    Some(h.finalize().to_hex().to_string())
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Locate a blob by hash.
pub fn find(server: &Server, hash: &str) -> Option<Found> {
    let mut hits: Vec<(PathBuf, Option<String>, i64)> = vec![];
    // Session blob store.
    let dir = server.paths.blobs().join(&hash[..2]);
    let meta: Option<Value> = std::fs::read(dir.join(format!("{hash}.json")))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{hash}.")) && !name.ends_with(".json") {
                let md = e.metadata().ok();
                let mime = meta
                    .as_ref()
                    .and_then(|m| m.get("mime"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let created = meta
                    .as_ref()
                    .and_then(|m| m.get("created_at_ms").or(m.get("at_ms")))
                    .and_then(Value::as_i64)
                    .or(md.as_ref().map(mtime_ms))
                    .unwrap_or(0);
                hits.push((e.path(), mime, created));
            }
        }
    }
    // Pane inbox (blob.put, chunked uploads).
    if let Ok(rd) = std::fs::read_dir(crate::paths::Paths::inbox().join(&hash[..12])) {
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if md.is_file() && hash_file(&p).as_deref() == Some(hash) {
                hits.push((p, None, mtime_ms(&md)));
            }
        }
    }
    let refs = hits.len();
    let (path, mime, created_at_ms) = hits.into_iter().min_by_key(|h| h.2)?;
    let size = std::fs::metadata(&path).ok()?.len();
    let mime = mime.unwrap_or_else(|| mime_for(&path).to_string());
    Some(Found {
        path,
        mime,
        size,
        created_at_ms,
        refs,
    })
}

fn valid_hash(h: &str) -> bool {
    h.len() == 64
        && h.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn lookup(server: &Server, p: &Value) -> Result<(String, Found), vk_proto::rpc::RpcError> {
    let hash = req(p, "hash")?;
    if !valid_hash(hash) {
        return Err(invalid("hash must be 64 lowercase hex characters (blake3)"));
    }
    let f = find(server, hash).ok_or_else(|| crate::api::not_found("blob", hash))?;
    Ok((hash.to_string(), f))
}

fn blob_stat(server: &Server, p: &Value) -> R {
    let (hash, f) = lookup(server, p)?;
    Ok(json!({
        "hash": hash,
        "mime": f.mime,
        "size": f.size,
        "created_at": f.created_at_ms,
        "refs": f.refs,
        "path": f.path,
    }))
}

fn blob_get(server: &Server, p: &Value) -> R {
    use base64::Engine;
    use std::io::{Read, Seek, SeekFrom};
    let (hash, f) = lookup(server, p)?;
    let range = p.get("range");
    let offset = range.and_then(|r| u(r, "offset")).unwrap_or(0).min(f.size);
    let rest = f.size - offset;
    let length = match range.and_then(|r| u(r, "length")) {
        Some(l) => l.min(rest),
        None => rest,
    };
    if length > MAX_GET {
        return Err(err(
            ErrorKind::InvalidParams,
            format!(
                "blob is {} bytes; read it in ranges of at most {MAX_GET}",
                f.size
            ),
        )
        .details(json!({"size": f.size, "max": MAX_GET})));
    }
    let mut file = std::fs::File::open(&f.path).map_err(internal)?;
    file.seek(SeekFrom::Start(offset)).map_err(internal)?;
    let mut buf = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut buf).map_err(internal)?;
    Ok(json!({
        "hash": hash,
        "mime": f.mime,
        "size": f.size,
        "offset": offset,
        "length": buf.len(),
        "data_b64": base64::engine::general_purpose::STANDARD.encode(&buf),
    }))
}

/// Dispatch hook for `blob.get` / `blob.stat`.
pub fn api(server: &Server, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "blob.get" => blob_get(server, p),
        "blob.stat" => blob_stat(server, p),
        _ => return None,
    })
}
