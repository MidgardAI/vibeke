//! Screenshots as evidence (spec 06 B6, 15 §6.4; Goal 03 Stage 4).
//!
//! Every screenshot — from the agents' headless browser (`browser.screenshot`) or from a
//! human's browser pane (Stage 2, via [`record_screenshot`] with `environment.kind =
//! local_pane`) — becomes a content-addressed PNG blob plus a `screenshot` entity in the store
//! ([`ScreenshotMeta`]) that answers *what produced it* (environment) and *what code it shows*:
//!
//! - **Checkout identity** ([`CodeState`]): head SHA + dirty digest of the preview's checkout,
//!   captured at screenshot time off the state actor (`spawn_blocking`, read-only git).
//! - **Running-build identity** ([`RuntimeIdentity`]): what the serving app reports at
//!   `/__vibeke_build` (JSON, e.g. the output of `vibeke screenshot code-state --json` captured
//!   by its dev script) or in an `X-Vibeke-Build` header on that endpoint; probed only on ports
//!   of this machine's previews. Otherwise `unknown` ("Build not verified").
//! - **Document identity** ([`DocumentIdentity`]): read from the page right before and right
//!   after the pixels (`location.href`/`origin`, `performance.timeOrigin`, and the build the
//!   page itself reports in `window.__VIBEKE_BUILD__` or `<meta name="vibeke-build">`). A
//!   navigation in between makes the capture `illustrative`. A page-reported build is the
//!   document's own identity; otherwise the probe asks **exactly the captured origin** (no
//!   host rewriting, `Host` as captured) and binds only if that build started before the
//!   document was loaded (else the server may have moved on since: loaded A, serving B).
//! - **Binding**: `bound` only when the captured document's build reports exactly the
//!   captured checkout state; any uncertainty is `illustrative`. Review packages list screenshots as `browser` evidence;
//!   they never satisfy a check criterion, and a bound one supports a human criterion only
//!   through an explicit human review.
//!
//! Also here: retention (`[screenshots] keep_days`, `max_per_task`, `referenced_keep_days`)
//! with a cleanup job, visual diff (`browser.diff`), and `screenshot.list|get|open|delete`.

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, s, u};
use crate::core::{Tx, ulid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vk_proto::model::PreviewStatus;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_review::intent::{Evaluation, TaskIntent};
use vk_review::readiness::Evidence;
use vk_review::screenshot::{
    Binding, CodeState, RuntimeIdentity, RuntimeStatus, capture_code_state, decide_binding,
};
use vk_review::subject::ChangeSubject;

pub use vk_review::screenshot::Binding as ScreenshotBinding;

pub const METHODS: &[(&str, bool)] = &[
    ("screenshot.list", false),
    ("screenshot.get", false),
    ("screenshot.open", false),
    ("screenshot.delete", true),
    ("screenshot.add", true),
    ("browser.diff", true),
];

/// Store entity kind.
pub const KIND: &str = "screenshot";
const DAY_MS: i64 = 24 * 3600 * 1000;
/// Screenshots/diffs above this are not inlined; the path on the machine still works.
const MAX_INLINE: usize = 8 << 20;
/// `/__vibeke_build` probe: total time budget and response size bound.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const PROBE_MAX: usize = 64 * 1024;
/// Path probed on the preview's origin for the running-build identity.
pub const BUILD_PATH: &str = "/__vibeke_build";

// ---- config ---------------------------------------------------------------------------------

/// `[screenshots]` (06 Part C).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScreenshotConfig {
    /// Unreferenced screenshots older than this are deleted (0 = never by age).
    pub keep_days: u32,
    /// At most this many screenshots per task; older unreferenced ones go first (0 = no cap).
    pub max_per_task: usize,
    /// Screenshots referenced by a review acceptance are kept this long (15 §11: references
    /// pin blobs only within the declared retention policy).
    pub referenced_keep_days: u32,
    /// Probe `/__vibeke_build` on the preview for the running-build identity.
    pub probe_build: bool,
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        ScreenshotConfig {
            keep_days: 30,
            max_per_task: 200,
            referenced_keep_days: 365,
            probe_build: true,
        }
    }
}

impl ScreenshotConfig {
    pub fn load() -> Self {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        Self::from_config(&cfg)
    }

    pub fn from_config(cfg: &vk_config::Config) -> Self {
        cfg.extra
            .get("screenshots")
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

// ---- types ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvKind {
    /// The agents' headless browser on the dev server's machine (B5), fresh context.
    #[serde(alias = "headless")]
    RemoteHeadless,
    /// The human's browser pane (B3.2).
    LocalPane,
    /// The external headful window with the Vibeke profile (B3.3).
    #[serde(alias = "local_profile")]
    Window,
    /// The user's normal browser through the authenticated reverse proxy (B4).
    LocalProxy,
    /// An image file an agent attached (`screenshot.add`); no browser involved.
    Agent,
}

impl EnvKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EnvKind::RemoteHeadless => "remote_headless",
            EnvKind::LocalPane => "local_pane",
            EnvKind::Window => "window",
            EnvKind::LocalProxy => "local_proxy",
            EnvKind::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// Which browser produced the pixels (06 B6 `BrowserEnv`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub kind: EnvKind,
    /// Machine the browser ran on.
    pub machine: String,
    /// Runner level of the browser process (`host`, `sandbox`, …).
    pub runner: String,
    /// Product string (`HeadlessChrome/153.0.8010.12`).
    pub browser: String,
    pub browser_version: Option<String>,
    pub viewport: Viewport,
    pub dpr: f64,
    pub color_scheme: Option<String>,
    pub device: Option<String>,
    /// A fresh browser context (no cookies/logins) — not proof of what a logged-in profile
    /// shows.
    pub fresh_context: bool,
    /// Vibeke profile the pane/window used (`devbox`), if any.
    pub profile: Option<String>,
}

impl Environment {
    /// "devbox · headless · fresh context" vs "your browser pane · profile devbox".
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match self.kind {
            EnvKind::RemoteHeadless => {
                parts.push(self.machine.clone());
                parts.push("headless".into());
            }
            EnvKind::LocalPane => parts.push("your browser pane".into()),
            EnvKind::Window => parts.push("your browser window".into()),
            EnvKind::LocalProxy => parts.push("your browser via proxy".into()),
            EnvKind::Agent => return "attached by agent".into(),
        }
        if let Some(p) = &self.profile {
            parts.push(format!("profile {p}"));
        }
        if self.fresh_context {
            parts.push("fresh context".into());
        }
        if let Some(d) = &self.device {
            parts.push(d.clone());
        }
        parts.join(" · ")
    }
}

/// Who asked for the screenshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Requester {
    /// `agent` | `user` | `plugin`.
    pub kind: String,
    pub pane: Option<String>,
    pub run: Option<String>,
    pub client: Option<String>,
}

/// The stored record (entity kind `screenshot`). Field names are part of the API (07).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotMeta {
    pub id: String,
    /// `s<N>`.
    pub handle: String,
    /// Always `"screenshot"` (the blob sidecar uses the same document).
    pub kind: String,
    /// blake3 of the PNG; the blob lives at `<state>/blobs/<h2>/<blob>.png`.
    pub blob: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub created_at_ms: i64,
    /// Same as `created_at_ms` (Stage 3 name).
    pub taken_at: i64,
    pub environment: Environment,
    /// Environment label for UIs.
    pub label: String,
    /// The page's URL as tracked by the session / pane.
    pub url: String,
    /// `location.href` at capture time (after redirects), if known.
    pub final_url: Option<String>,
    pub title: Option<String>,
    /// Preview handle (`v4`) and id.
    pub preview: Option<String>,
    pub preview_id: Option<String>,
    /// Browser session handle (`b1`) for headless screenshots.
    pub session: Option<String>,
    pub full_page: bool,
    pub selector: Option<String>,
    pub taken_by: Requester,
    /// Requesting pane / run, the task the screenshot belongs to, and the workspace that
    /// scopes pane-token reads.
    pub pane: Option<String>,
    pub run: Option<String>,
    pub task: Option<String>,
    pub workspace: Option<String>,
    /// Checkout identity at capture time; `None` when the preview isn't tied to a checkout.
    pub code: Option<CodeState>,
    /// Why `code` is missing (no checkout found, git failed …).
    pub code_note: Option<String>,
    /// Running-build identity (15 §6.4).
    pub runtime: RuntimeIdentity,
    pub binding: Binding,
    pub binding_reason: String,
    /// What the captured document said about itself at capture time (Codex review follow-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<DocumentIdentity>,
    /// What the agent said the image shows (`screenshot.add`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// Base name of the file an agent attached (`screenshot.add`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
}

/// The captured document's identity, read from the page itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentIdentity {
    pub href: String,
    /// `location.origin` (`scheme://host[:port]`, exactly as the browser connected).
    pub origin: String,
    #[serde(default)]
    pub title: Option<String>,
    /// `performance.timeOrigin` (ms since the epoch): when this document's navigation began.
    #[serde(default)]
    pub time_origin_ms: Option<f64>,
    /// `window.__VIBEKE_BUILD__` (a build report object or an `X-Vibeke-Build` string) or the
    /// `<meta name="vibeke-build">` content.
    #[serde(default)]
    pub build: Option<Value>,
}

/// Evaluated in the page right before and right after the pixels are captured.
pub const DOCUMENT_IDENTITY_JS: &str = r#"(() => {
  let b = null;
  try { const w = window.__VIBEKE_BUILD__; if (w !== undefined && w !== null) b = JSON.parse(JSON.stringify(w)); } catch (e) {}
  if (b === null) { const m = document.querySelector('meta[name="vibeke-build"]'); if (m) b = m.getAttribute('content'); }
  return {href: location.href, origin: location.origin, title: document.title, time_origin_ms: performance.timeOrigin, build: b};
})()"#;

impl DocumentIdentity {
    pub fn from_value(v: &Value) -> Option<Self> {
        let d: DocumentIdentity = serde_json::from_value(v.clone()).ok()?;
        (!d.href.is_empty()).then_some(d)
    }

    /// Same document (no navigation, reload or redirect in between).
    pub fn same_document(&self, other: &DocumentIdentity) -> bool {
        self.href == other.href
            && self.origin == other.origin
            && self.time_origin_ms == other.time_origin_ms
    }

    /// The build the page reports about itself, if any.
    pub fn page_build(&self, now: i64) -> Option<RuntimeIdentity> {
        let mut r = match self.build.as_ref()? {
            Value::String(s) => RuntimeIdentity::from_header(s, now).or_else(|| {
                serde_json::from_str::<Value>(s)
                    .ok()
                    .and_then(|v| RuntimeIdentity::from_report(&v, "page", now))
            })?,
            v @ Value::Object(_) => RuntimeIdentity::from_report(v, "page", now)?,
            _ => return None,
        };
        r.source = "page".into();
        Some(r)
    }
}

/// The document identity around a capture: `Same` when the reads before and after the pixels
/// agree, `Changed` (with why) when they don't, `None` when the page couldn't be read.
#[derive(Debug, Clone, PartialEq)]
pub enum DocumentCapture {
    Same(DocumentIdentity),
    Changed(String),
}

impl DocumentCapture {
    pub fn from_reads(before: Option<&Value>, after: Option<&Value>) -> Option<Self> {
        let b = before.and_then(DocumentIdentity::from_value);
        let a = after.and_then(DocumentIdentity::from_value);
        match (b, a) {
            (Some(b), Some(a)) if b.same_document(&a) => Some(DocumentCapture::Same(a)),
            (Some(b), Some(a)) => Some(DocumentCapture::Changed(format!(
                "the page navigated during the capture ({} → {})",
                b.href, a.href
            ))),
            (None, None) => None,
            _ => Some(DocumentCapture::Changed(
                "the page could not be read on both sides of the capture".into(),
            )),
        }
    }

    pub fn identity(&self) -> Option<&DocumentIdentity> {
        match self {
            DocumentCapture::Same(d) => Some(d),
            DocumentCapture::Changed(_) => None,
        }
    }
}

impl ScreenshotMeta {
    pub fn path(&self, server: &Server) -> PathBuf {
        blob_path(server, &self.blob)
    }
}

/// Everything the caller of [`record_screenshot`] knows about a capture.
#[derive(Debug, Clone)]
pub struct ShotInputs {
    pub environment: Environment,
    pub url: String,
    pub final_url: Option<String>,
    pub title: Option<String>,
    /// Preview handle or id.
    pub preview: Option<String>,
    pub session: Option<String>,
    pub taken_by: Requester,
    pub full_page: bool,
    pub selector: Option<String>,
    /// Checkout to capture the code state from, overriding the preview/pane lookup.
    pub checkout: Option<PathBuf>,
    /// Running-build identity the caller already knows (skips the probe).
    pub runtime: Option<RuntimeIdentity>,
    /// Probe `/__vibeke_build` when `runtime` is `None` (also gated by config).
    pub probe_runtime: bool,
    /// The captured document's identity (read around the pixels). Without it, a probed
    /// identity can't be tied to these pixels and the screenshot is `illustrative`.
    pub document: Option<DocumentCapture>,
}

pub fn blob_path(server: &Server, hash: &str) -> PathBuf {
    let h2 = hash.get(..2).unwrap_or("00");
    server.paths.blobs().join(h2).join(format!("{hash}.png"))
}

// ---- capture --------------------------------------------------------------------------------

/// What the server knows about where a screenshot came from (resolved under the core lock).
#[derive(Debug, Default)]
struct Origin {
    preview_handle: Option<String>,
    preview_id: Option<String>,
    preview_port: Option<u16>,
    pane: Option<String>,
    run: Option<String>,
    task: Option<String>,
    workspace: Option<String>,
    checkouts: Vec<PathBuf>,
    preview_ports: BTreeSet<u16>,
}

fn resolve_origin(server: &Server, inputs: &ShotInputs) -> Origin {
    server.with_core(|c| {
        let mut o = Origin::default();
        let pv = inputs.preview.as_deref().and_then(|t| {
            c.model
                .previews
                .iter()
                .find(|p| p.id == t || p.handle == t)
                .cloned()
        });
        o.preview_ports = c
            .model
            .previews
            .iter()
            .filter(|p| {
                p.machine == server.opts.machine
                    && matches!(
                        p.status,
                        PreviewStatus::Declared | PreviewStatus::Up | PreviewStatus::Down
                    )
            })
            .map(|p| p.port)
            .collect();
        let req_pane = inputs
            .taken_by
            .pane
            .as_deref()
            .and_then(|p| c.pane(p).cloned());
        let pv_pane = pv
            .as_ref()
            .and_then(|p| p.pane.as_deref())
            .and_then(|p| c.pane(p).cloned());
        let run = inputs
            .taken_by
            .run
            .as_deref()
            .and_then(|r| c.run(r).cloned())
            .or_else(|| {
                req_pane
                    .as_ref()
                    .and_then(|p| c.run_for_pane(&p.id))
                    .filter(|r| r.ended_at_ms.is_none())
                    .cloned()
            });
        let ws_of = |id: &str| c.model.workspaces.iter().find(|w| w.id == id).cloned();
        let workspace = req_pane
            .as_ref()
            .or(pv_pane.as_ref())
            .map(|p| p.workspace.clone());
        let task = pv
            .as_ref()
            .and_then(|p| p.task.clone())
            .or_else(|| run.as_ref().and_then(|r| r.task.clone()))
            .or_else(|| {
                workspace
                    .as_deref()
                    .and_then(ws_of)
                    .and_then(|w| w.task.clone())
            });
        let task_rec = task.as_deref().and_then(|t| c.task(t).cloned());
        let workspace = workspace.or_else(|| task_rec.as_ref().and_then(|t| t.workspace.clone()));
        if let Some(p) = &inputs.checkout {
            o.checkouts.push(p.clone());
        }
        if let Some(t) = &task_rec {
            o.checkouts.push(PathBuf::from(
                t.worktree_path.as_deref().unwrap_or(&t.repo_root),
            ));
        }
        for p in [&pv_pane, &req_pane].into_iter().flatten() {
            if let Some(cwd) = &p.cwd {
                o.checkouts.push(PathBuf::from(cwd));
            }
        }
        if let Some(w) = workspace.as_deref().and_then(ws_of)
            && !w.root_path.is_empty()
        {
            o.checkouts.push(PathBuf::from(w.root_path));
        }
        o.preview_handle = pv
            .as_ref()
            .map(|p| p.handle.clone())
            .or_else(|| inputs.preview.clone());
        o.preview_id = pv.as_ref().map(|p| p.id.clone());
        o.preview_port = pv.as_ref().map(|p| p.port);
        o.pane = req_pane.map(|p| p.id);
        o.run = run.map(|r| r.id);
        o.task = task_rec.map(|t| t.id).or(task);
        o.workspace = workspace;
        o
    })
}

/// Capture the code state from the first candidate directory that is a git checkout.
fn code_state_blocking(candidates: &[PathBuf]) -> (Option<CodeState>, Option<String>) {
    let mut last_err = None;
    for p in candidates {
        if !p.is_dir() {
            continue;
        }
        match capture_code_state(p) {
            Ok(c) => return (Some(c), None),
            Err(e) => last_err = Some(format!("{}: {e}", p.display())),
        }
    }
    (
        None,
        Some(last_err.unwrap_or_else(|| {
            "no checkout: the preview isn't tied to a task, pane directory or workspace".into()
        })),
    )
}

/// Record a screenshot: store the PNG blob, capture the checkout's code state (off the state
/// actor), determine the running-build identity and binding, persist the `screenshot` entity
/// and emit `screenshot.captured`. The headless browser calls this from `browser.screenshot`;
/// the browser pane (Stage 2) calls it with `environment.kind = local_pane`.
pub async fn record_screenshot(
    server: &Arc<Server>,
    png: &[u8],
    inputs: ShotInputs,
) -> Result<ScreenshotMeta, RpcError> {
    let origin = resolve_origin(server, &inputs);
    let candidates = origin.checkouts.clone();
    let (code, code_note) = tokio::task::spawn_blocking(move || code_state_blocking(&candidates))
        .await
        .unwrap_or((None, Some("code state capture panicked".into())));
    let now = vk_store::now_ms();
    let probe = inputs.probe_runtime && ScreenshotConfig::load().probe_build;
    let doc = inputs.document.clone();
    let runtime = match (inputs.runtime.clone(), &doc) {
        (Some(r), _) => r,
        (None, Some(DocumentCapture::Changed(why))) => RuntimeIdentity::unknown(why.clone(), now),
        (None, Some(DocumentCapture::Same(d))) => match d.page_build(now) {
            Some(r) => r,
            // Exactly the origin the captured document came from.
            None if probe => probe_runtime(&format!("{}/", d.origin), &origin.preview_ports).await,
            None => RuntimeIdentity::unknown("runtime probe disabled", now),
        },
        (None, None) if probe => {
            let target = inputs.final_url.as_deref().unwrap_or(&inputs.url);
            probe_runtime(target, &origin.preview_ports).await
        }
        (None, None) => RuntimeIdentity::unknown("runtime probe disabled", now),
    };
    let (mut binding, mut binding_reason) = decide_binding(code.as_ref(), &runtime);
    // The identity must be the captured document's (a caller-supplied identity is the
    // caller's claim). Anything uncertain is illustrative.
    if binding == Binding::Bound && inputs.runtime.is_none() {
        let doc_id = doc.as_ref().and_then(DocumentCapture::identity);
        let ok = match doc_id {
            Some(_) if runtime.source == "page" => true,
            Some(d) => runtime
                .started_at_ms
                .zip(d.time_origin_ms)
                .is_some_and(|(started, loaded)| (started as f64) <= loaded),
            None => false,
        };
        if !ok {
            binding = Binding::Illustrative;
            binding_reason = if doc_id.is_some() {
                "Build not verified for this page: the server's build may have changed since the page was loaded (expose window.__VIBEKE_BUILD__ in the page, or report started_at_ms)".into()
            } else {
                "Build not verified for this page: the captured document's identity is unknown"
                    .into()
            };
        }
    }
    let (width, height) = crate::agent_browser::png_size(png);
    let id = ulid();
    let meta = ScreenshotMeta {
        id: id.clone(),
        handle: String::new(),
        kind: "screenshot".into(),
        blob: blake3::hash(png).to_hex().to_string(),
        mime: "image/png".into(),
        width,
        height,
        bytes: png.len() as u64,
        created_at_ms: now,
        taken_at: now,
        label: inputs.environment.label(),
        environment: inputs.environment,
        url: inputs.url,
        final_url: inputs.final_url,
        title: inputs.title,
        preview: origin.preview_handle,
        preview_id: origin.preview_id,
        session: inputs.session,
        full_page: inputs.full_page,
        selector: inputs.selector,
        taken_by: inputs.taken_by,
        pane: origin.pane,
        run: origin.run,
        task: origin.task,
        workspace: origin.workspace,
        code,
        code_note,
        runtime,
        binding,
        binding_reason,
        document: doc.as_ref().and_then(|d| d.identity().cloned()),
        caption: None,
        source_name: None,
    };
    persist_screenshot(server, png, meta)
}

/// Allocate the handle, write the entity and event in one commit, store the blob and enforce
/// the per-task cap. Shared by [`record_screenshot`] and `screenshot.add`.
fn persist_screenshot(
    server: &Arc<Server>,
    png: &[u8],
    mut meta: ScreenshotMeta,
) -> Result<ScreenshotMeta, RpcError> {
    // Handle allocation and the entity write under one core lock.
    let srv = server.clone();
    let meta = {
        let mut c = srv.core.lock().unwrap();
        let n: u64 = c
            .store
            .kv_get("screenshots", "seq")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
            + 1;
        meta.handle = format!("s{n}");
        let mut tx = Tx::new();
        tx.m.kv("screenshots", "seq", Some(n.to_string()));
        tx.m.put(KIND, &meta.id, Some(&meta.handle), &meta);
        tx.event(
            "screenshot.captured",
            json!({"screenshot": meta.id, "handle": meta.handle, "pane": meta.pane, "task": meta.task, "workspace": meta.workspace, "machine": server.opts.machine}),
            json!({
                "id": meta.id,
                "handle": meta.handle,
                "task": meta.task,
                "binding": meta.binding,
                "environment": meta.environment.kind,
                "label": meta.label,
                "blob": meta.blob,
                "preview": meta.preview,
                "pane": meta.pane,
                "caption": meta.caption,
                "head_sha": meta.code.as_ref().and_then(|c| c.head_sha.clone()),
            }),
        );
        srv.commit(&mut c, tx).map_err(crate::api::internal)?;
        meta
    };
    // Blob + JSON sidecar (0600, content-addressed).
    crate::agent_browser::store_blob(
        server,
        png,
        "png",
        &serde_json::to_value(&meta).unwrap_or_default(),
    )
    .map_err(crate::api::internal)?;
    // Enforce the per-task cap right away (cheap: one task's records).
    if let Some(t) = meta.task.clone() {
        let srv = server.clone();
        tokio::task::spawn_blocking(move || {
            let cfg = ScreenshotConfig::load();
            cleanup_with(&srv, vk_store::now_ms(), &cfg, Some(&t));
        });
    }
    Ok(meta)
}

// ---- attached images (screenshot.add) ------------------------------------------------------

/// Largest image file `screenshot.add` accepts.
const ADD_MAX_BYTES: usize = 16 << 20;
/// Largest width or height `screenshot.add` accepts.
const ADD_MAX_DIM: u32 = 16384;
const CAPTION_MAX_CHARS: usize = 500;
const NAME_MAX_CHARS: usize = 200;

/// Validate an attached image and return PNG bytes with the pixel size. PNG is kept as-is;
/// JPEG is decoded and re-encoded as PNG. Anything else is refused.
fn normalize_image(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), RpcError> {
    use image::{ImageFormat, ImageReader};
    use std::io::Cursor;
    let unsupported = || invalid("only PNG and JPEG images are supported");
    if bytes.len() > ADD_MAX_BYTES {
        return Err(invalid(format!(
            "image is larger than {} MiB",
            ADD_MAX_BYTES >> 20
        )));
    }
    let format = image::guess_format(bytes).map_err(|_| unsupported())?;
    let too_big = |w: u32, h: u32| {
        invalid(format!(
            "image is {w}x{h}; the largest side allowed is {ADD_MAX_DIM}"
        ))
    };
    match format {
        ImageFormat::Png => {
            let (w, h) = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png)
                .into_dimensions()
                .map_err(|e| invalid(format!("not a valid PNG image: {e}")))?;
            if w == 0 || h == 0 || w > ADD_MAX_DIM || h > ADD_MAX_DIM {
                return Err(too_big(w, h));
            }
            Ok((bytes.to_vec(), w, h))
        }
        ImageFormat::Jpeg => {
            let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg);
            #[allow(clippy::field_reassign_with_default)]
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(ADD_MAX_DIM);
            limits.max_image_height = Some(ADD_MAX_DIM);
            reader.limits(limits);
            let img = reader
                .decode()
                .map_err(|e| invalid(format!("not a valid JPEG image: {e}")))?;
            let (w, h) = (img.width(), img.height());
            if w == 0 || h == 0 || w > ADD_MAX_DIM || h > ADD_MAX_DIM {
                return Err(too_big(w, h));
            }
            let mut out = Vec::new();
            img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
                .map_err(crate::api::internal)?;
            Ok((out, w, h))
        }
        _ => Err(unsupported()),
    }
}

/// Control characters removed, trimmed, cut to `max` characters; `None` when nothing is left.
fn clean_text(s: &str, max: usize) -> Option<String> {
    let cleaned: String = s.chars().filter(|c| !c.is_control()).collect();
    let cleaned: String = cleaned.trim().chars().take(max).collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// `screenshot.add {data_b64, caption?, name?, pane?}`: store an image file an agent (or the
/// user) attached, as a `screenshot` record in the `agent` environment. The caller reads the
/// file; the server never opens caller-supplied paths.
async fn add(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    use base64::Engine as _;
    let data_b64 = crate::api::req(p, "data_b64")?;
    // Cheap bound before decoding: base64 is 4 bytes per 3.
    if data_b64.len() > ADD_MAX_BYTES / 3 * 4 + 8 {
        return Err(invalid(format!(
            "image is larger than {} MiB",
            ADD_MAX_BYTES >> 20
        )));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data_b64.trim())
        .map_err(|e| invalid(format!("data_b64 is not valid base64: {e}")))?;
    // A pane-scoped caller always attaches to its own pane.
    let pane = match (&ctx.pane_scope, s(p, "pane").filter(|t| !t.is_empty())) {
        (Some(own), target) => {
            let pane = crate::api::resolve_pane(server, ctx, target.or(Some(own.as_str())))?;
            if &pane.id != own {
                return Err(err(
                    ErrorKind::PermissionDenied,
                    "an agent can attach images to its own pane only",
                )
                .details(json!({"scope": "pane"})));
            }
            Some(pane.id)
        }
        (None, Some(target)) => Some(crate::api::resolve_pane(server, ctx, Some(target))?.id),
        (None, None) => None,
    };
    let caption = s(p, "caption").and_then(|c| clean_text(c, CAPTION_MAX_CHARS));
    let source_name = s(p, "name")
        .and_then(|n| n.rsplit(['/', '\\']).next())
        .and_then(|n| clean_text(n, NAME_MAX_CHARS));
    let (png, width, height) = tokio::task::spawn_blocking(move || normalize_image(&raw))
        .await
        .map_err(crate::api::internal)??;
    let blob = blake3::hash(&png).to_hex().to_string();
    if let Some(existing) = load_all(server)
        .into_iter()
        .find(|m| m.blob == blob && m.pane == pane)
    {
        let mut v = with_path(server, &existing);
        v["duplicate"] = json!(true);
        return Ok(v);
    }
    let agent = ctx.pane_scope.is_some();
    let taken_by = Requester {
        kind: if agent { "agent" } else { "user" }.into(),
        pane: pane.clone(),
        run: None,
        client: (!agent).then(|| ctx.client_id.clone()),
    };
    let environment = Environment {
        kind: EnvKind::Agent,
        machine: server.opts.machine.clone(),
        runner: "host".into(),
        browser: String::new(),
        browser_version: None,
        viewport: Viewport { width, height },
        dpr: 1.0,
        color_scheme: None,
        device: None,
        fresh_context: false,
        profile: None,
    };
    let inputs = ShotInputs {
        environment: environment.clone(),
        url: String::new(),
        final_url: None,
        title: None,
        preview: None,
        session: None,
        taken_by: taken_by.clone(),
        full_page: false,
        selector: None,
        checkout: None,
        runtime: None,
        probe_runtime: false,
        document: None,
    };
    let origin = resolve_origin(server, &inputs);
    let now = vk_store::now_ms();
    let meta = ScreenshotMeta {
        id: ulid(),
        handle: String::new(),
        kind: "screenshot".into(),
        blob,
        mime: "image/png".into(),
        width,
        height,
        bytes: png.len() as u64,
        created_at_ms: now,
        taken_at: now,
        label: environment.label(),
        environment,
        url: String::new(),
        final_url: None,
        title: None,
        preview: None,
        preview_id: None,
        session: None,
        full_page: false,
        selector: None,
        taken_by,
        pane: origin.pane,
        run: origin.run,
        task: origin.task,
        workspace: origin.workspace,
        code: None,
        code_note: Some("attached file".into()),
        runtime: RuntimeIdentity::unknown("attached file", now),
        binding: Binding::Illustrative,
        binding_reason: "Attached from a file by an agent; not tied to a running build".into(),
        document: None,
        caption,
        source_name,
    };
    let meta = persist_screenshot(server, &png, meta)?;
    let mut v = with_path(server, &meta);
    v["duplicate"] = json!(false);
    Ok(v)
}

// ---- running-build probe --------------------------------------------------------------------

fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_line = String::from_utf8_lossy(&rest[..pos]);
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        rest = &rest[pos + 2..];
        if size == 0 || rest.len() < size {
            out.extend_from_slice(&rest[..size.min(rest.len())]);
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
    out
}

/// Probe `<origin>/__vibeke_build` (loopback, a declared preview's port only) for the
/// running-build identity. The probe connects to exactly the origin's address (`127.0.0.2`
/// stays `127.0.0.2`; `localhost` names go to the loopback addresses) and sends the origin's
/// own `Host` (virtual hosts such as `app.localhost` answer for themselves). Everything that
/// isn't a usable report yields `unknown` with the reason.
pub async fn probe_runtime(url: &str, preview_ports: &BTreeSet<u16>) -> RuntimeIdentity {
    let now = vk_store::now_ms();
    let Some(t) = vk_browser::policy::parse_target(url) else {
        return RuntimeIdentity::unknown("not an http(s) page", now);
    };
    let literal = t
        .host
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| ip.to_canonical());
    let loopback = vk_browser::policy::is_localhost_name(&t.host)
        || literal.is_some_and(|ip| ip.is_loopback());
    if !loopback || !preview_ports.contains(&t.port) {
        return RuntimeIdentity::unknown(
            "page is not served by a preview on this machine; build not verified",
            now,
        );
    }
    if t.scheme != "http" {
        return RuntimeIdentity::unknown("https previews are not probed; build not verified", now);
    }
    let addrs: Vec<std::net::SocketAddr> = match literal {
        Some(ip) => vec![std::net::SocketAddr::new(ip, t.port)],
        None => vec![
            std::net::SocketAddr::from(([127, 0, 0, 1], t.port)),
            std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, t.port)),
        ],
    };
    let host_header = if t.host.contains(':') {
        format!("[{}]:{}", t.host, t.port)
    } else {
        format!("{}:{}", t.host, t.port)
    };
    let fut = async {
        let mut s = tokio::net::TcpStream::connect(&addrs[..]).await?;
        let req = format!(
            "GET {BUILD_PATH} HTTP/1.1\r\nHost: {host_header}\r\nAccept: application/json\r\nUser-Agent: vibeke-build-probe\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > PROBE_MAX {
                break;
            }
        }
        Ok::<Vec<u8>, std::io::Error>(buf)
    };
    let raw = match tokio::time::timeout(PROBE_TIMEOUT, fut).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return RuntimeIdentity::unknown(format!("build probe failed: {e}"), now),
        Err(_) => return RuntimeIdentity::unknown("build probe timed out", now),
    };
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return RuntimeIdentity::unknown("build probe: malformed response", now);
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body = raw[split + 4..].to_vec();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut header_identity = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "x-vibeke-build" {
                header_identity = RuntimeIdentity::from_header(v.trim(), now);
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    if chunked {
        body = dechunk(&body);
    }
    if status == 200
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
        && let Some(r) = RuntimeIdentity::from_report(&v, "probe", now)
    {
        return r;
    }
    if let Some(h) = header_identity {
        return h;
    }
    RuntimeIdentity::unknown(
        format!("the app exposes no build identity at {BUILD_PATH} (HTTP {status})"),
        now,
    )
}

// ---- reads & scope --------------------------------------------------------------------------

pub(crate) fn load_all(server: &Server) -> Vec<ScreenshotMeta> {
    server.with_core(|c| c.store.load::<ScreenshotMeta>(KIND).unwrap_or_default())
}

pub fn find(server: &Server, t: &str) -> Option<ScreenshotMeta> {
    server.with_core(|c| c.store.find::<ScreenshotMeta>(KIND, t).ok().flatten())
}

/// The caller's workspace for pane-scoped reads (`None` = full scope).
fn scope_workspace(server: &Server, ctx: &Ctx) -> Result<Option<String>, RpcError> {
    let Some(pane) = &ctx.pane_scope else {
        return Ok(None);
    };
    server
        .with_core(|c| c.pane(pane).map(|p| p.workspace.clone()))
        .map(Some)
        .ok_or_else(|| {
            err(
                ErrorKind::PermissionDenied,
                "the caller's pane no longer exists",
            )
            .details(json!({"scope": "pane"}))
        })
}

/// A pane sees screenshots from its own workspace only.
fn visible(ws: &Option<String>, m: &ScreenshotMeta) -> bool {
    match ws {
        None => true,
        Some(w) => m.workspace.as_deref() == Some(w.as_str()),
    }
}

fn find_visible(server: &Server, ctx: &Ctx, t: &str) -> Result<ScreenshotMeta, RpcError> {
    let ws = scope_workspace(server, ctx)?;
    match find(server, t) {
        Some(m) if visible(&ws, &m) => Ok(m),
        // Don't reveal other workspaces' screenshots.
        _ => Err(not_found("screenshot", t)),
    }
}

fn since_ms(p: &Value) -> Option<i64> {
    if let Some(n) = p.get("since").and_then(Value::as_i64) {
        return Some(n);
    }
    u(p, "since_ms")
        .map(|ms| vk_store::now_ms() - ms as i64)
        .or_else(|| {
            s(p, "since")
                .and_then(|d| vk_config::Dur::parse(d).ok())
                .map(|d| vk_store::now_ms() - d.0.as_millis() as i64)
        })
}

fn with_path(server: &Server, m: &ScreenshotMeta) -> Value {
    let mut v = serde_json::to_value(m).unwrap_or_default();
    let path = m.path(server);
    v["exists"] = json!(path.exists());
    v["path_on_machine"] = json!(crate::privacy::readable_path(server, &path));
    v
}

fn inline_into(v: &mut Value, path: &Path) {
    use base64::Engine as _;
    match crate::privacy::read_blob(path) {
        Ok(data) if data.len() <= MAX_INLINE => {
            v["data_b64"] = json!(base64::engine::general_purpose::STANDARD.encode(&data));
            v["mime"] = json!("image/png");
        }
        Ok(data) => v["inline_skipped"] = json!(format!("{} bytes > {MAX_INLINE}", data.len())),
        Err(e) => v["inline_skipped"] = json!(format!("blob unreadable: {e}")),
    }
}

fn list(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let ws = scope_workspace(server, ctx)?;
    let task = s(p, "task").map(|t| {
        crate::tracking::find_task(server, t)
            .map(|t| t.id)
            .unwrap_or_else(|_| t.to_string())
    });
    let preview = s(p, "preview");
    let run = s(p, "run");
    // A deleted pane or workspace can still have screenshots: fall back to the literal id.
    let pane = s(p, "pane").filter(|t| !t.is_empty()).map(|t| {
        crate::api::resolve_pane(server, ctx, Some(t))
            .map(|p| p.id)
            .unwrap_or_else(|_| t.to_string())
    });
    let workspace = s(p, "workspace").filter(|t| !t.is_empty()).map(|t| {
        crate::api::resolve_ws(server, ctx, Some(t))
            .map(|w| w.id)
            .unwrap_or_else(|_| t.to_string())
    });
    let environment = s(p, "environment").filter(|t| !t.is_empty());
    let since = since_ms(p);
    let limit = u(p, "limit").unwrap_or(50).clamp(1, 1000) as usize;
    let mut v: Vec<ScreenshotMeta> = load_all(server)
        .into_iter()
        .filter(|m| visible(&ws, m))
        .filter(|m| task.as_ref().is_none_or(|t| m.task.as_deref() == Some(t)))
        .filter(|m| {
            preview.is_none_or(|pv| {
                m.preview.as_deref() == Some(pv) || m.preview_id.as_deref() == Some(pv)
            })
        })
        .filter(|m| {
            run.is_none_or(|r| m.run.as_deref() == Some(r) || m.taken_by.run.as_deref() == Some(r))
        })
        .filter(|m| pane.as_ref().is_none_or(|t| m.pane.as_deref() == Some(t)))
        .filter(|m| {
            workspace
                .as_ref()
                .is_none_or(|w| m.workspace.as_deref() == Some(w))
        })
        .filter(|m| environment.is_none_or(|e| m.environment.kind.as_str() == e))
        .filter(|m| since.is_none_or(|t| m.created_at_ms >= t))
        .collect();
    v.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then(b.id.cmp(&a.id)));
    let total = v.len();
    v.truncate(limit);
    Ok(json!({
        "screenshots": v.iter().map(|m| with_path(server, m)).collect::<Vec<_>>(),
        "count": v.len(),
        "total": total,
    }))
}

fn get(server: &Server, ctx: &Ctx, p: &Value, inline: bool) -> R {
    let t = s(p, "id")
        .or_else(|| s(p, "screenshot"))
        .ok_or_else(|| invalid("missing param `id`"))?;
    let m = find_visible(server, ctx, t)?;
    let mut v = with_path(server, &m);
    if inline || b(p, "inline").unwrap_or(false) {
        inline_into(&mut v, &m.path(server));
    }
    Ok(v)
}

fn delete(server: &Server, ctx: &Ctx, p: &Value) -> R {
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "screenshot.delete is a human action (evidence can't be deleted by agents)",
        )
        .details(json!({"scope": "pane"})));
    }
    let t = s(p, "id")
        .or_else(|| s(p, "screenshot"))
        .ok_or_else(|| invalid("missing param `id`"))?;
    let m = find(server, t).ok_or_else(|| not_found("screenshot", t))?;
    if !b(p, "force").unwrap_or(false) && referenced_by_acceptance(server, &m) {
        return Err(err(
            ErrorKind::Conflict,
            "this screenshot is part of an accepted review; pass force to delete it anyway",
        )
        .details(json!({"reason": "referenced_by_acceptance", "id": m.id})));
    }
    let blobs = remove_records(server, std::slice::from_ref(&m), "deleted");
    Ok(json!({"id": m.id, "handle": m.handle, "deleted": true, "blob_removed": blobs > 0}))
}

// ---- retention ------------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed: Vec<String>,
    pub blobs_removed: usize,
    pub kept_referenced: Vec<String>,
    pub diff_blobs_removed: usize,
}

/// Accepted review subjects referencing a screenshot: an acceptance of the screenshot's task
/// on the head the screenshot shows, recorded after it was taken (it was in that package).
fn acceptance_heads(server: &Server, task: &str) -> Vec<(String, i64)> {
    crate::review::acceptances_for(server, task)
        .into_iter()
        .map(|a| (a.head_sha, a.accepted_at_ms))
        .collect()
}

fn is_referenced(m: &ScreenshotMeta, heads: &[(String, i64)]) -> bool {
    let Some(h) = m.code.as_ref().and_then(|c| c.head_sha.as_deref()) else {
        return false;
    };
    heads
        .iter()
        .any(|(sha, at)| sha == h && *at >= m.created_at_ms)
}

fn referenced_by_acceptance(server: &Server, m: &ScreenshotMeta) -> bool {
    m.task
        .as_deref()
        .is_some_and(|t| is_referenced(m, &acceptance_heads(server, t)))
}

/// Delete records (one transaction + `screenshot.deleted` event) and every blob no remaining
/// record references. Returns the number of blobs removed.
pub(crate) fn remove_records(server: &Server, gone: &[ScreenshotMeta], reason: &str) -> usize {
    if gone.is_empty() {
        return 0;
    }
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for m in gone {
            tx.m.delete(KIND, &m.id);
        }
        tx.event(
            "screenshot.deleted",
            json!({"machine": server.opts.machine}),
            json!({"ids": gone.iter().map(|m| &m.id).collect::<Vec<_>>(), "reason": reason}),
        );
        if server.commit(&mut c, tx).is_err() {
            return 0;
        }
    }
    let still: HashSet<String> = load_all(server).into_iter().map(|m| m.blob).collect();
    let mut n = 0;
    let mut done = HashSet::new();
    for m in gone {
        if still.contains(&m.blob) || !done.insert(m.blob.clone()) {
            continue;
        }
        let p = blob_path(server, &m.blob);
        if std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
        let _ = std::fs::remove_file(p.with_extension("json"));
    }
    n
}

/// Apply the retention policy at `now_ms` (injectable for tests). `only_task` limits the run
/// to one task's screenshots (used right after a capture for the per-task cap).
pub fn cleanup_with(
    server: &Server,
    now_ms: i64,
    cfg: &ScreenshotConfig,
    only_task: Option<&str>,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    let all: Vec<ScreenshotMeta> = load_all(server)
        .into_iter()
        .filter(|m| only_task.is_none_or(|t| m.task.as_deref() == Some(t)))
        .collect();
    let mut heads: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
    for t in all.iter().filter_map(|m| m.task.clone()) {
        if let std::collections::btree_map::Entry::Vacant(v) = heads.entry(t) {
            let h = acceptance_heads(server, v.key());
            v.insert(h);
        }
    }
    let referenced = |m: &ScreenshotMeta| {
        m.task
            .as_ref()
            .and_then(|t| heads.get(t))
            .is_some_and(|h| is_referenced(m, h))
    };
    let mut gone: Vec<ScreenshotMeta> = Vec::new();
    let mut gone_ids: HashSet<String> = HashSet::new();
    for m in &all {
        let age = now_ms - m.created_at_ms;
        let refd = referenced(m);
        let limit_days = if refd {
            cfg.referenced_keep_days
        } else {
            cfg.keep_days
        };
        if limit_days > 0 && age > limit_days as i64 * DAY_MS {
            gone_ids.insert(m.id.clone());
            gone.push(m.clone());
        }
    }
    // Per-task cap: newest first; referenced ones are kept (and still count).
    if cfg.max_per_task > 0 {
        let mut by_task: BTreeMap<&str, Vec<&ScreenshotMeta>> = BTreeMap::new();
        for m in all.iter().filter(|m| !gone_ids.contains(&m.id)) {
            if let Some(t) = m.task.as_deref() {
                by_task.entry(t).or_default().push(m);
            }
        }
        for (_, mut v) in by_task {
            v.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then(b.id.cmp(&a.id)));
            for m in v.into_iter().skip(cfg.max_per_task) {
                if referenced(m) {
                    report.kept_referenced.push(m.id.clone());
                } else if gone_ids.insert(m.id.clone()) {
                    gone.push(m.clone());
                }
            }
        }
    }
    for m in &all {
        if !gone_ids.contains(&m.id)
            && referenced(m)
            && cfg.keep_days > 0
            && now_ms - m.created_at_ms > cfg.keep_days as i64 * DAY_MS
            && !report.kept_referenced.contains(&m.id)
        {
            report.kept_referenced.push(m.id.clone());
        }
    }
    report.removed = gone.iter().map(|m| m.id.clone()).collect();
    report.blobs_removed = remove_records(server, &gone, "retention");
    if only_task.is_none() {
        report.diff_blobs_removed = prune_diff_blobs(server, now_ms, cfg);
    }
    report
}

/// Diff images are blobs with a `screenshot_diff` sidecar; they follow `keep_days`.
fn prune_diff_blobs(server: &Server, now_ms: i64, cfg: &ScreenshotConfig) -> usize {
    if cfg.keep_days == 0 {
        return 0;
    }
    let cutoff = now_ms - cfg.keep_days as i64 * DAY_MS;
    let mut n = 0;
    let Ok(dirs) = std::fs::read_dir(server.paths.blobs()) else {
        return 0;
    };
    for d in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(v) = std::fs::read(&p).map(|b| serde_json::from_slice::<Value>(&b)) else {
                continue;
            };
            let Ok(v) = v else { continue };
            if v["kind"] == "screenshot_diff"
                && v["created_at_ms"].as_i64().is_some_and(|t| t < cutoff)
            {
                let _ = std::fs::remove_file(p.with_extension("png"));
                let _ = std::fs::remove_file(&p);
                n += 1;
            }
        }
    }
    n
}

/// Start the hourly retention job (also runs once at startup).
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let s2 = srv.clone();
            let r = tokio::task::spawn_blocking(move || {
                cleanup_with(&s2, vk_store::now_ms(), &ScreenshotConfig::load(), None)
            })
            .await;
            if let Ok(r) = r
                && (!r.removed.is_empty() || r.diff_blobs_removed > 0)
            {
                tracing::info!(
                    removed = r.removed.len(),
                    blobs = r.blobs_removed,
                    diffs = r.diff_blobs_removed,
                    "screenshot retention"
                );
            }
        }
    });
}

// ---- visual diff ----------------------------------------------------------------------------

fn resolve_image(
    server: &Server,
    ctx: &Ctx,
    t: &str,
) -> Result<(Option<ScreenshotMeta>, PathBuf), RpcError> {
    if let Ok(m) = find_visible(server, ctx, t) {
        let p = m.path(server);
        return Ok((Some(m), p));
    }
    // A raw blob hash (full scope only).
    let is_hash = t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit());
    if ctx.pane_scope.is_none() && is_hash {
        let p = blob_path(server, t);
        if p.exists() {
            return Ok((None, p));
        }
    }
    Err(not_found("screenshot", t))
}

async fn diff(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    use base64::Engine as _;
    let a = s(p, "a").ok_or_else(|| invalid("missing param `a`"))?;
    let b_id = s(p, "b").ok_or_else(|| invalid("missing param `b`"))?;
    let threshold = p
        .get("threshold")
        .and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0.1);
    if !(0.0..=1.0).contains(&threshold) {
        return Err(invalid("threshold must be between 0 and 1"));
    }
    let (ma, pa) = resolve_image(server, ctx, a)?;
    let (mb, pb) = resolve_image(server, ctx, b_id)?;
    let force = b(p, "force").unwrap_or(false);
    if let (Some(x), Some(y)) = (&ma, &mb)
        && x.environment.kind != y.environment.kind
        && !force
    {
        return Err(invalid(format!(
            "screenshots come from different environments ({} vs {}); pass force to diff anyway",
            x.environment.kind.as_str(),
            y.environment.kind.as_str()
        ))
        .details(json!({"reason": "environment_mismatch", "a": x.environment.kind, "b": y.environment.kind})));
    }
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let da = crate::privacy::read_blob(&pa)?;
        let db = crate::privacy::read_blob(&pb)?;
        vk_browser::diff::diff_png(&da, &db, threshold)
    })
    .await
    .map_err(crate::api::internal)?
    .map_err(|e| invalid(format!("diff failed: {e:#}")))?;
    let now = vk_store::now_ms();
    let side = json!({
        "kind": "screenshot_diff",
        "created_at_ms": now,
        "a": ma.as_ref().map(|m| &m.id).map(|x| x.as_str()).unwrap_or(a),
        "b": mb.as_ref().map(|m| &m.id).map(|x| x.as_str()).unwrap_or(b_id),
        "threshold": threshold,
        "changed_ratio": r.changed_ratio,
    });
    let (hash, path) = crate::agent_browser::store_blob(server, &r.diff_png, "png", &side)
        .map_err(crate::api::internal)?;
    let summary = |m: &Option<ScreenshotMeta>, t: &str| match m {
        Some(m) => {
            json!({"id": m.id, "handle": m.handle, "blob": m.blob, "environment": m.environment.kind, "label": m.label, "binding": m.binding, "head_sha": m.code.as_ref().and_then(|c| c.head_sha.clone()), "width": m.width, "height": m.height})
        }
        None => json!({"blob": t}),
    };
    let mut out = json!({
        "a": summary(&ma, a),
        "b": summary(&mb, b_id),
        "threshold": threshold,
        "channel_threshold": r.channel_threshold,
        "width": r.width,
        "height": r.height,
        "size_mismatch": r.size_mismatch,
        "a_size": {"width": r.a_size.0, "height": r.a_size.1},
        "b_size": {"width": r.b_size.0, "height": r.b_size.1},
        "changed_pixels": r.changed_pixels,
        "total_pixels": r.total_pixels,
        "changed_ratio": r.changed_ratio,
        "regions": r.regions.iter().map(|g| json!({"x": g.x, "y": g.y, "width": g.width, "height": g.height, "pixels": g.pixels})).collect::<Vec<_>>(),
        "regions_total": r.regions_total,
        "forced": force && ma.as_ref().zip(mb.as_ref()).is_some_and(|(x, y)| x.environment.kind != y.environment.kind),
        "blob": hash,
        "path_on_machine": crate::privacy::readable_path(server, &path),
        "bytes": r.diff_png.len(),
    });
    if b(p, "inline").unwrap_or(false) {
        if r.diff_png.len() <= MAX_INLINE {
            out["data_b64"] = json!(base64::engine::general_purpose::STANDARD.encode(&r.diff_png));
            out["mime"] = json!("image/png");
        } else {
            out["inline_skipped"] = json!(format!("{} bytes > {MAX_INLINE}", r.diff_png.len()));
        }
    }
    Ok(out)
}

// ---- review integration (15 §6.4) -----------------------------------------------------------

/// Screenshots of a task for its review package: as `browser` [`Evidence`] items and as
/// display rows. A screenshot is linked to the reviewed `subject` only when it is `bound` and
/// its code state is exactly that subject; then it may support the intent's *human* criteria
/// (still needing an explicit human review). Everything else is illustrative.
pub fn review_evidence(
    server: &Server,
    task: &str,
    runs: &[String],
    subject: Option<&ChangeSubject>,
    intent: Option<&TaskIntent>,
) -> (Vec<Evidence>, Vec<Value>) {
    let mut shots: Vec<ScreenshotMeta> = server.with_core(|c| {
        let mut v: Vec<ScreenshotMeta> = c.store.load_by_task(KIND, task).unwrap_or_default();
        for r in runs {
            for m in c
                .store
                .load_by_field::<ScreenshotMeta>(KIND, "$.run", r)
                .unwrap_or_default()
            {
                if !v.iter().any(|x| x.id == m.id) {
                    v.push(m);
                }
            }
        }
        v
    });
    shots.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then(a.id.cmp(&b.id)));
    let human: Vec<String> = intent
        .map(|i| {
            i.criteria
                .iter()
                .filter(|c| c.evaluation == Evaluation::Human)
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut evidence = Vec::new();
    let mut rows = Vec::new();
    for m in &shots {
        let matches = match (&m.code, subject) {
            (Some(c), Some(s)) => Some(c.matches_subject(s)),
            _ => None,
        };
        let bound_here = m.binding == Binding::Bound && matches == Some(true);
        let summary = format!(
            "Screenshot {} · {} · {}",
            m.handle,
            m.label,
            m.code
                .as_ref()
                .map(|c| c.label())
                .unwrap_or_else(|| "no code state".into())
        );
        evidence.push(Evidence::from_screenshot(
            &m.id,
            if bound_here {
                subject.map(|s| s.id.as_str())
            } else {
                None
            },
            if bound_here { human.clone() } else { vec![] },
            m.created_at_ms,
            summary,
        ));
        let note = if bound_here {
            "Bound to this revision: can support a human criterion through your review; never a check result"
        } else if m.binding == Binding::Bound {
            "Bound to another revision than the one under review: illustrative here"
        } else if m.runtime.status == RuntimeStatus::Unknown {
            "Build not verified: illustrative, cannot satisfy a check"
        } else {
            "Illustrative, cannot satisfy a check"
        };
        rows.push(json!({
            "id": m.id,
            "handle": m.handle,
            "category": "browser",
            "blob": m.blob,
            "created_at_ms": m.created_at_ms,
            "url": m.url,
            "final_url": m.final_url,
            "environment": m.environment.kind,
            "label": m.label,
            "binding": m.binding,
            "binding_here": if bound_here { "bound" } else { "illustrative" },
            "binding_reason": m.binding_reason,
            "code": m.code.as_ref().map(|c| json!({"head_sha": c.head_sha, "dirty_state": c.dirty_state, "dirty_digest": c.dirty_digest})),
            "subject_match": match matches { Some(true) => "this_revision", Some(false) => "other_revision", None => "unknown" },
            "runtime": {"status": m.runtime.status, "build_id": m.runtime.build_id, "head_sha": m.runtime.head_sha, "source": m.runtime.source},
            "supports": if bound_here { "human_review_only" } else { "nothing" },
            "note": note,
        }));
    }
    (evidence, rows)
}

// ---- API ------------------------------------------------------------------------------------

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "screenshot.list" => list(server, ctx, p),
        "screenshot.get" => get(server, ctx, p, false),
        // `vibeke screenshot open`: the image inline so the CLI can open it locally.
        "screenshot.open" => get(server, ctx, p, true),
        "screenshot.delete" => delete(server, ctx, p),
        "screenshot.add" => add(server, ctx, p).await,
        "browser.diff" => diff(server, ctx, p).await,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "screenshots_tests.rs"]
mod tests;
