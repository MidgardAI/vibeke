//! Encrypted blobs on the account server (spec 20 §4.4): a continue link's bundle, encrypted by
//! the sending host, uploaded with its account and fetched by the receiving host with its own.
//! The server only ever sees ciphertext; it checks who may fetch (`access`) and keeps a blob for
//! its `ttl_s`.
//!
//! - `POST /v1/blobs {size, ttl_s?, access: "github"|"any", github?: [id]}` → `{id, expires_at}`
//! - `PUT /v1/blobs/{id}` (body: exactly `size` bytes)
//! - `GET /v1/blobs/{id}/meta` → `{size, created_at, expires_at, from: {id, login}}`
//! - `GET /v1/blobs/{id}`
//! - `DELETE /v1/blobs/{id}`
//!
//! Errors are `{error: <code>}`: `quota`, `too_large`, `not_found`, `forbidden`, `not_uploaded`,
//! `already_uploaded`, `size_mismatch`. They surface as [`Error::Http`] (`HTTP <status>: <code>`);
//! [`error_code`] reads the code back.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::attest::GithubUser;
use crate::{Account, Client, Error, Result};

/// Uploads and downloads may take long; the client's 20 s timeout is for small calls.
pub const TRANSFER_TIMEOUT: Duration = Duration::from_secs(3600);
const READ_BUF: usize = 256 * 1024;

/// Called with the bytes moved so far.
pub type Progress = Arc<dyn Fn(u64) + Send + Sync>;

/// Who may fetch a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobAccess {
    /// These GitHub user ids.
    Github(Vec<u64>),
    /// Any signed-in account (the key in the link is the secret).
    Any,
}

impl BlobAccess {
    /// `{access: "github", github: [ids]}` or `{access: "any"}`.
    pub fn to_json(&self) -> Value {
        match self {
            BlobAccess::Github(ids) => json!({"access": "github", "github": ids}),
            BlobAccess::Any => json!({"access": "any"}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobCreated {
    pub id: String,
    /// Unix seconds.
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
pub struct BlobMeta {
    /// Ciphertext bytes.
    pub size: u64,
    pub created_at: u64,
    pub expires_at: u64,
    /// The account that created the blob (authenticated by the server, not taken from a link).
    pub from: GithubUser,
}

/// The server's error code of a blob call (`not_found`, `forbidden`, `quota`, ...), if it sent one.
pub fn error_code(e: &Error) -> Option<&str> {
    match e {
        Error::Http(m) => m
            .strip_prefix("HTTP ")
            .and_then(|r| r.split_once(": "))
            .map(|(_, code)| code),
        _ => None,
    }
}

fn valid_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(Error::Protocol(format!("invalid blob id {id:?}")))
    }
}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Http(format!("{}: {e}", path.display()))
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

fn u64_of(v: &Value, k: &str) -> Result<u64> {
    v.get(k)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Protocol(format!("missing {k}")))
}

impl Client {
    fn blob_url(&self, id: &str, tail: &str) -> String {
        format!("{}/v1/blobs/{id}{tail}", self.base)
    }

    fn refusal(status: u16, v: &Value) -> Error {
        if status == 401 {
            Error::Unauthorized
        } else {
            Self::unexpected(status, v)
        }
    }

    /// Reserve a blob of `size` ciphertext bytes.
    pub async fn blob_create(
        &self,
        access_token: &str,
        size: u64,
        ttl_s: Option<u64>,
        access: &BlobAccess,
    ) -> Result<BlobCreated> {
        let mut body = access.to_json();
        body["size"] = json!(size);
        if let Some(t) = ttl_s {
            body["ttl_s"] = json!(t);
        }
        match self.post("/v1/blobs", Some(access_token), body).await? {
            (200 | 201, v) => {
                let id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Protocol("missing id".into()))?
                    .to_string();
                valid_id(&id)?;
                Ok(BlobCreated {
                    id,
                    expires_at: u64_of(&v, "expires_at")?,
                })
            }
            (s, v) => Err(Self::refusal(s, &v)),
        }
    }

    /// Stream the file at `path` as the blob's content (`Content-Length` = file size).
    pub async fn blob_upload(
        &self,
        access_token: &str,
        id: &str,
        path: &Path,
        progress: Option<Progress>,
    ) -> Result<()> {
        valid_id(id)?;
        let file = tokio::fs::File::open(path).await.map_err(|e| io(path, e))?;
        let len = file.metadata().await.map_err(|e| io(path, e))?.len();
        let stream =
            futures::stream::unfold((file, progress, 0u64), |(mut f, p, done)| async move {
                let mut buf = vec![0u8; READ_BUF];
                match f.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        let done = done + n as u64;
                        if let Some(p) = &p {
                            p(done);
                        }
                        Some((Ok::<_, std::io::Error>(buf), (f, p, done)))
                    }
                    Err(e) => Some((Err(e), (f, None, done))),
                }
            });
        let resp = self
            .http
            .put(self.blob_url(id, ""))
            .header("authorization", format!("Bearer {access_token}"))
            .header("content-type", "application/octet-stream")
            .header("content-length", len.to_string())
            .timeout(TRANSFER_TIMEOUT)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.unwrap_or_default();
        match status {
            200..=299 => Ok(()),
            s => Err(Self::refusal(s, &json_of(&bytes))),
        }
    }

    pub async fn blob_meta(&self, access_token: &str, id: &str) -> Result<BlobMeta> {
        valid_id(id)?;
        match self
            .send(
                reqwest::Method::GET,
                &format!("/v1/blobs/{id}/meta"),
                Some(access_token),
                None,
            )
            .await?
        {
            (200, v) => {
                let from = v
                    .get("from")
                    .ok_or_else(|| Error::Protocol("missing from".into()))?;
                Ok(BlobMeta {
                    size: u64_of(&v, "size")?,
                    created_at: v.get("created_at").and_then(Value::as_u64).unwrap_or(0),
                    expires_at: u64_of(&v, "expires_at")?,
                    from: GithubUser {
                        id: u64_of(from, "id")?,
                        login: from
                            .get("login")
                            .and_then(Value::as_str)
                            .ok_or_else(|| Error::Protocol("missing from.login".into()))?
                            .to_string(),
                    },
                })
            }
            (s, v) => Err(Self::refusal(s, &v)),
        }
    }

    /// Stream the blob into a new file `dest` (0600). Fails with `HTTP 200: too_large` (and
    /// removes `dest`) when the server sends more than `max` bytes. Returns the bytes written.
    pub async fn blob_download(
        &self,
        access_token: &str,
        id: &str,
        dest: &Path,
        max: u64,
        progress: Option<Progress>,
    ) -> Result<u64> {
        valid_id(id)?;
        let mut resp = self
            .http
            .get(self.blob_url(id, ""))
            .header("authorization", format!("Bearer {access_token}"))
            .timeout(TRANSFER_TIMEOUT)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let bytes = resp.bytes().await.unwrap_or_default();
            return Err(Self::refusal(status, &json_of(&bytes)));
        }
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut out = opts.open(dest).await.map_err(|e| io(dest, e))?;
        let r = async {
            let mut n = 0u64;
            while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Http(e.to_string()))? {
                n += chunk.len() as u64;
                if n > max {
                    return Err(Error::Http("HTTP 200: too_large".into()));
                }
                out.write_all(&chunk).await.map_err(|e| io(dest, e))?;
                if let Some(p) = &progress {
                    p(n);
                }
            }
            out.flush().await.map_err(|e| io(dest, e))?;
            out.sync_all().await.map_err(|e| io(dest, e))?;
            Ok(n)
        }
        .await;
        if r.is_err() {
            let _ = tokio::fs::remove_file(dest).await;
        }
        r
    }

    pub async fn blob_delete(&self, access_token: &str, id: &str) -> Result<()> {
        valid_id(id)?;
        match self
            .send(
                reqwest::Method::DELETE,
                &format!("/v1/blobs/{id}"),
                Some(access_token),
                None,
            )
            .await?
        {
            (200..=299, _) => Ok(()),
            (s, v) => Err(Self::refusal(s, &v)),
        }
    }
}

impl Account {
    /// Run `f` with an access token; a refused token is refreshed once, refused again →
    /// [`Error::LoginRequired`].
    async fn with_access<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let access = self.access_token(false).await?;
        match f(access).await {
            Err(Error::Unauthorized) => {
                let access = self.access_token(true).await?;
                f(access).await.map_err(|e| match e {
                    Error::Unauthorized => Error::LoginRequired,
                    e => e,
                })
            }
            r => r,
        }
    }

    pub async fn blob_create(
        &self,
        size: u64,
        ttl_s: Option<u64>,
        access: BlobAccess,
    ) -> Result<BlobCreated> {
        let access = &access;
        self.with_access(|t| async move { self.client.blob_create(&t, size, ttl_s, access).await })
            .await
    }

    /// Streaming PUT of the file at `path`.
    pub async fn blob_upload(&self, id: &str, path: &Path) -> Result<()> {
        self.blob_upload_with(id, path, None).await
    }

    /// [`blob_upload`](Self::blob_upload), reporting the bytes sent.
    pub async fn blob_upload_with(
        &self,
        id: &str,
        path: &Path,
        progress: Option<Progress>,
    ) -> Result<()> {
        self.with_access(|t| {
            let progress = progress.clone();
            async move { self.client.blob_upload(&t, id, path, progress).await }
        })
        .await
    }

    pub async fn blob_meta(&self, id: &str) -> Result<BlobMeta> {
        self.with_access(|t| async move { self.client.blob_meta(&t, id).await })
            .await
    }

    /// Streaming GET into a new file `dest`.
    pub async fn blob_download(&self, id: &str, dest: &Path) -> Result<()> {
        self.blob_download_with(id, dest, u64::MAX, None)
            .await
            .map(|_| ())
    }

    /// [`blob_download`](Self::blob_download) of at most `max` bytes, reporting the bytes
    /// received. Returns the bytes written.
    pub async fn blob_download_with(
        &self,
        id: &str,
        dest: &Path,
        max: u64,
        progress: Option<Progress>,
    ) -> Result<u64> {
        self.with_access(|t| {
            let progress = progress.clone();
            async move { self.client.blob_download(&t, id, dest, max, progress).await }
        })
        .await
    }

    pub async fn blob_delete(&self, id: &str) -> Result<()> {
        self.with_access(|t| async move { self.client.blob_delete(&t, id).await })
            .await
    }
}

/// Blob routes for the fake control plane ([`crate::fake::FakeServer`]): in memory, one signed-in
/// identity (whoever holds the fake's current access token).
#[cfg(any(test, feature = "fake-server"))]
pub mod fake {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{DefaultBodyLimit, Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use serde_json::{Value, json};

    use crate::fake::Shared;

    #[derive(Debug, Clone)]
    pub struct FakeBlob {
        /// GitHub `(id, login)` of the account that created it.
        pub owner: (u64, String),
        pub size: u64,
        pub created_at: u64,
        pub expires_at: u64,
        /// `None`: any signed-in account.
        pub access: Option<Vec<u64>>,
        pub data: Option<Vec<u8>>,
    }

    #[derive(Debug)]
    pub struct BlobState {
        pub blobs: HashMap<String, FakeBlob>,
        /// GitHub identity of the signed-in account.
        pub github: (u64, String),
        pub max_size: u64,
        pub max_ttl_s: u64,
        pub next: u32,
        pub uploads: u32,
        pub downloads: u32,
    }

    impl Default for BlobState {
        fn default() -> Self {
            BlobState {
                blobs: HashMap::new(),
                github: (4242, "octo".into()),
                max_size: 256 * 1024 * 1024,
                max_ttl_s: 30 * 24 * 3600,
                next: 0,
                uploads: 0,
                downloads: 0,
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct FakeBlobs(pub Arc<Mutex<BlobState>>);

    impl FakeBlobs {
        pub fn with<T>(&self, f: impl FnOnce(&mut BlobState) -> T) -> T {
            f(&mut self.0.lock().unwrap())
        }
    }

    type St = (Shared, FakeBlobs);

    pub fn router(auth: Shared, blobs: FakeBlobs) -> Router {
        Router::new()
            .route("/v1/blobs", post(create))
            .route("/v1/blobs/{id}", get(download).put(upload).delete(remove))
            .route("/v1/blobs/{id}/meta", get(meta))
            .layer(DefaultBodyLimit::max(300 * 1024 * 1024))
            .with_state((auth, blobs))
    }

    fn now_s() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn err(status: StatusCode, e: &str) -> Response {
        (status, axum::Json(json!({"error": e}))).into_response()
    }

    /// The caller's GitHub identity, if its bearer token is the current access token.
    fn caller(st: &St, h: &HeaderMap) -> Option<(u64, String)> {
        let token = h
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?
            .to_string();
        if st.0.lock().unwrap().access.as_deref() != Some(token.as_str()) {
            return None;
        }
        Some(st.1.0.lock().unwrap().github.clone())
    }

    async fn create(State(st): State<St>, h: HeaderMap, b: String) -> Response {
        let Some(who) = caller(&st, &h) else {
            return err(StatusCode::UNAUTHORIZED, "invalid_token");
        };
        let b: Value = serde_json::from_str(&b).unwrap_or(Value::Null);
        let Some(size) = b["size"].as_u64() else {
            return err(StatusCode::BAD_REQUEST, "invalid_request");
        };
        let access = match b["access"].as_str() {
            Some("any") => None,
            Some("github") => match b["github"].as_array() {
                Some(ids) if !ids.is_empty() => {
                    Some(ids.iter().filter_map(Value::as_u64).collect())
                }
                _ => return err(StatusCode::BAD_REQUEST, "invalid_request"),
            },
            _ => return err(StatusCode::BAD_REQUEST, "invalid_request"),
        };
        let mut s = st.1.0.lock().unwrap();
        if size > s.max_size {
            return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
        }
        let ttl = b["ttl_s"]
            .as_u64()
            .unwrap_or(7 * 24 * 3600)
            .min(s.max_ttl_s);
        s.next += 1;
        let id = format!("blob{}", s.next);
        let now = now_s();
        s.blobs.insert(
            id.clone(),
            FakeBlob {
                owner: who,
                size,
                created_at: now,
                expires_at: now + ttl,
                access,
                data: None,
            },
        );
        axum::Json(json!({"id": id, "expires_at": now + ttl})).into_response()
    }

    /// The live blob `id` the caller may read.
    fn readable(st: &St, h: &HeaderMap, id: &str) -> Result<FakeBlob, Response> {
        let Some(who) = caller(st, h) else {
            return Err(err(StatusCode::UNAUTHORIZED, "invalid_token"));
        };
        let s = st.1.0.lock().unwrap();
        let Some(blob) = s.blobs.get(id).filter(|b| b.expires_at > now_s()) else {
            return Err(err(StatusCode::NOT_FOUND, "not_found"));
        };
        if let Some(ids) = &blob.access
            && !ids.contains(&who.0)
        {
            return Err(err(StatusCode::FORBIDDEN, "forbidden"));
        }
        Ok(blob.clone())
    }

    async fn upload(
        State(st): State<St>,
        Path(id): Path<String>,
        h: HeaderMap,
        body: Bytes,
    ) -> Response {
        let Some(who) = caller(&st, &h) else {
            return err(StatusCode::UNAUTHORIZED, "invalid_token");
        };
        let declared = h
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let mut s = st.1.0.lock().unwrap();
        s.uploads += 1;
        let Some(blob) = s.blobs.get_mut(&id) else {
            return err(StatusCode::NOT_FOUND, "not_found");
        };
        if blob.owner.0 != who.0 {
            return err(StatusCode::FORBIDDEN, "forbidden");
        }
        if blob.data.is_some() {
            return err(StatusCode::CONFLICT, "already_uploaded");
        }
        if declared != Some(blob.size) || body.len() as u64 != blob.size {
            return err(StatusCode::BAD_REQUEST, "size_mismatch");
        }
        blob.data = Some(body.to_vec());
        StatusCode::NO_CONTENT.into_response()
    }

    async fn meta(State(st): State<St>, Path(id): Path<String>, h: HeaderMap) -> Response {
        match readable(&st, &h, &id) {
            Ok(b) => axum::Json(json!({
                "size": b.size,
                "created_at": b.created_at,
                "expires_at": b.expires_at,
                "from": {"id": b.owner.0, "login": b.owner.1},
            }))
            .into_response(),
            Err(r) => r,
        }
    }

    async fn download(State(st): State<St>, Path(id): Path<String>, h: HeaderMap) -> Response {
        match readable(&st, &h, &id) {
            Ok(b) => match b.data {
                Some(d) => {
                    st.1.0.lock().unwrap().downloads += 1;
                    (StatusCode::OK, d).into_response()
                }
                None => err(StatusCode::CONFLICT, "not_uploaded"),
            },
            Err(r) => r,
        }
    }

    async fn remove(State(st): State<St>, Path(id): Path<String>, h: HeaderMap) -> Response {
        let Some(who) = caller(&st, &h) else {
            return err(StatusCode::UNAUTHORIZED, "invalid_token");
        };
        let mut s = st.1.0.lock().unwrap();
        match s.blobs.get(&id) {
            None => err(StatusCode::NOT_FOUND, "not_found"),
            Some(b) if b.owner.0 != who.0 => err(StatusCode::FORBIDDEN, "forbidden"),
            Some(_) => {
                s.blobs.remove(&id);
                StatusCode::NO_CONTENT.into_response()
            }
        }
    }
}

#[cfg(test)]
#[path = "blobs_tests.rs"]
mod tests;
