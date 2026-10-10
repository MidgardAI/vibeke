//! Cloud sandbox providers (spec 17): Fly.io Sprites, E2B and a local fake for tests, behind one
//! [`Provider`] trait. A provider creates, lists and destroys remote boxes and runs commands in
//! them, with or without a terminal. Everything else (the `cloud` isolation level, the pane
//! shim, moves and the box lifecycle) is built on these few operations.
//!
//! - [`auth`]: credential references, the keychain items `vibeke/cloud/<provider>` and the
//!   [`AuthMethod`]s every client renders as the sign-in prompt.
//! - [`config`]: the `[cloud]` config table.
//! - [`exec_cli`]: `vibeke cloud exec`, a `docker exec`-shaped command over [`Provider::exec`]
//!   that the server puts in pane, link and git-service argv.
//! - [`sprites`], [`e2b`], [`fake`]: the providers.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub mod auth;
pub mod config;
pub mod e2b;
pub mod exec_cli;
pub mod fake;
pub mod naming;
pub mod sprites;

pub use auth::{AuthMethod, AuthState, Secret};
pub use config::{CloudConfig, ProviderConfig};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type Result<T> = std::result::Result<T, CloudError>;

/// What went wrong talking to a provider. Messages never contain a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct CloudError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// No credential, or the provider rejected it (401/403 on auth). Clients show the sign-in
    /// prompt and retry.
    NeedsAuth,
    /// The account can't do this (billing, quota, plan limit). The message says what to do.
    Account,
    NotFound,
    Conflict,
    Unsupported,
    RateLimited,
    InvalidParams,
    /// Network or provider failure; may be retried.
    Unavailable,
    Internal,
}

impl CloudError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        CloudError {
            kind,
            message: message.into(),
        }
    }
    pub fn needs_auth(provider: &str) -> Self {
        Self::new(
            ErrorKind::NeedsAuth,
            format!("sign in to {provider} first (vibeke cloud login {provider})"),
        )
    }
    pub fn unsupported(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, m)
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unavailable, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, m)
    }
    pub fn internal(m: impl std::fmt::Display) -> Self {
        Self::new(ErrorKind::Internal, m.to_string())
    }
}

/// What a provider can do; clients hide actions a provider lacks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// Terminal resize of a running session.
    pub resize: bool,
    /// A session survives a dropped connection and can be attached again by id.
    pub reattach: bool,
    /// `suspend` is an explicit call (E2B pause). Otherwise the provider sleeps idle boxes itself
    /// (Sprites) and `suspend` is a no-op.
    pub explicit_suspend: bool,
    /// Suspend keeps memory, so running processes continue after resume.
    pub keeps_memory: bool,
    /// Filesystem checkpoints.
    pub checkpoints: bool,
    /// Each box has HTTPS URLs for its ports.
    pub port_urls: bool,
    /// Maximum continuous runtime in seconds (0 = none), shown in the UI.
    pub max_runtime_s: u64,
}

/// Observed box state, as normalized from the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxState {
    Creating,
    Running,
    /// Sleeping with memory kept (Sprites warm, E2B paused).
    Warm,
    /// Sleeping without memory (Sprites cold).
    Cold,
    Paused,
    Stopped,
    #[default]
    Unknown,
}

impl BoxState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BoxState::Creating => "creating",
            BoxState::Running => "running",
            BoxState::Warm => "warm",
            BoxState::Cold => "cold",
            BoxState::Paused => "paused",
            BoxState::Stopped => "stopped",
            BoxState::Unknown => "unknown",
        }
    }
}

/// A box as the provider reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBox {
    pub provider: String,
    /// Provider id used in API paths (Sprites: the name; E2B: the sandbox id).
    pub id: String,
    /// Display name. Vibeke boxes are named by [`naming::box_name`].
    pub name: String,
    pub state: BoxState,
    /// Unix seconds, 0 when unknown.
    pub created_at: u64,
    pub last_active_at: u64,
    /// Public or org URL of the box, if any.
    pub url: Option<String>,
    /// Owner tags parsed from the name or metadata ([`naming::Tags`]); `None` for boxes Vibeke
    /// did not create (never listed by [`Provider::list`]).
    pub tags: Option<naming::Tags>,
}

/// What to create.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSpec {
    /// From [`naming::box_name`].
    pub name: String,
    pub tags: naming::Tags,
    /// Provider template or image (E2B template id); `None` = provider default.
    pub template: Option<String>,
    /// Lifetime hint in seconds (E2B timeout); 0 = provider default.
    pub timeout_s: u64,
    /// Pause instead of kill when the timeout hits (E2B `autoPause`).
    pub auto_pause: bool,
    /// Non-secret env set for every process in the box.
    pub env: BTreeMap<String, String>,
}

/// One command to run in a box.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecReq {
    /// argv[0] is resolved on the box's PATH.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    /// Allocate a terminal (`cols` x `rows`). Without one, stdout/stderr/exit are separate.
    pub tty: bool,
    pub cols: u16,
    pub rows: u16,
    /// Keep the command running after the connection drops, so it can be attached again
    /// ([`Caps::reattach`]). Pane sessions set this; link and git-service sessions don't.
    pub detachable: bool,
}

/// Client to box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum In {
    Data(Vec<u8>),
    /// End of stdin (non-terminal sessions).
    Eof,
    Resize {
        cols: u16,
        rows: u16,
    },
    Signal(String),
}

/// Box to client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// Terminal output (tty sessions) or stdout.
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command ended.
    Exit(i32),
    /// A process in the box started listening (`url` when the provider gives one).
    PortOpened {
        port: u16,
        url: Option<String>,
    },
    /// The connection ended without an exit. A detachable session keeps running; attach again.
    Lost(String),
}

/// A running command. Dropping `input` detaches (the command keeps running when detachable);
/// the provider task ends when `output` is dropped.
pub struct Session {
    /// Provider session id for [`Provider::attach`] (Sprites exec session id, E2B pid).
    pub id: String,
    pub tty: bool,
    pub input: mpsc::Sender<In>,
    pub output: mpsc::Receiver<Out>,
}

/// A command session as listed by the provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub command: String,
    pub tty: bool,
    /// The session still runs and can be attached. This is liveness, not recent output: a
    /// quiet session is active. `last_activity_at` carries the activity time.
    pub active: bool,
    pub last_activity_at: u64,
}

/// Account facts shown after sign-in (never the secret).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Org, team or user label.
    pub label: String,
    /// Extra non-secret facts (limits, plan).
    #[serde(default)]
    pub details: BTreeMap<String, String>,
}

/// A cloud sandbox provider. Every method that talks to the provider takes the resolved
/// credential; a missing or rejected one is [`ErrorKind::NeedsAuth`].
pub trait Provider: Send + Sync {
    /// Stable id used in config, params and keychain items: `sprites`, `e2b`, `fake`.
    fn id(&self) -> &'static str;
    /// Human label: "Fly.io Sprites".
    fn label(&self) -> &'static str;
    fn caps(&self) -> Caps;
    /// Ways to sign in, in the order clients should offer them.
    fn auth_methods(&self) -> Vec<AuthMethod>;
    /// Check a credential and return the account it belongs to.
    fn verify<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Account>>;
    /// Turn an import source (see [`AuthMethod::Import`]) into a credential, or `None` when that
    /// source has nothing (no local CLI login).
    fn import<'a>(&'a self, source: &'a str) -> BoxFut<'a, Result<Option<Secret>>>;

    fn create<'a>(
        &'a self,
        cred: &'a Secret,
        spec: &'a CreateSpec,
    ) -> BoxFut<'a, Result<RemoteBox>>;
    fn get<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<RemoteBox>>;
    /// Boxes Vibeke created (name or metadata carries [`naming::Tags`]).
    fn list<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Vec<RemoteBox>>>;
    /// Delete the box and everything in it. Not finding it is success.
    fn destroy<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>>;
    /// Ask the box to sleep (no-op without [`Caps::explicit_suspend`]).
    fn suspend<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>>;
    /// Wake a suspended box (connect/resume); a no-op where any request wakes it.
    fn resume<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>>;
    /// Filesystem checkpoint; returns its id ([`Caps::checkpoints`]).
    fn checkpoint<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        note: &'a str,
    ) -> BoxFut<'a, Result<String>>;

    fn exec<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        req: ExecReq,
    ) -> BoxFut<'a, Result<Session>>;
    /// Attach to a detachable session started by [`Provider::exec`].
    fn attach<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFut<'a, Result<Session>>;
    fn sessions<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
    ) -> BoxFut<'a, Result<Vec<SessionInfo>>>;
    /// Write a file in the box (parents created).
    fn write_file<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        path: &'a str,
        data: Vec<u8>,
        mode: u32,
    ) -> BoxFut<'a, Result<()>>;
    /// HTTPS URL that reaches `port` in the box, when the provider has one.
    fn port_url<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        port: u16,
    ) -> BoxFut<'a, Result<Option<String>>>;
    /// Prefix of absolute in-box paths (`/workspace` is `<box_root>/workspace`). Empty for real
    /// boxes; the fake provider's boxes are host directories, so their paths live below one.
    fn box_root(&self, _id: &str) -> String {
        String::new()
    }
}

/// Every provider this build knows, in the order clients list them. `fake` only appears when
/// `VIBEKE_CLOUD_FAKE_DIR` is set (tests).
pub fn providers(cfg: &CloudConfig) -> Vec<Arc<dyn Provider>> {
    let mut v: Vec<Arc<dyn Provider>> = vec![
        Arc::new(sprites::Sprites::new(cfg.provider("sprites"))),
        Arc::new(e2b::E2b::new(cfg.provider("e2b"))),
    ];
    if let Some(dir) = std::env::var_os(fake::DIR_ENV).filter(|d| !d.is_empty()) {
        v.push(Arc::new(fake::Fake::new(dir.into())));
    }
    v
}

pub fn provider(cfg: &CloudConfig, id: &str) -> Option<Arc<dyn Provider>> {
    providers(cfg).into_iter().find(|p| p.id() == id)
}

/// Unix seconds now.
pub fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A provider whose methods all fail with `unsupported` (scaffolding for lanes in progress).
#[macro_export]
macro_rules! unimplemented_provider {
    ($t:ty, $id:expr, $label:expr) => {
        impl $crate::Provider for $t {
            fn id(&self) -> &'static str {
                $id
            }
            fn label(&self) -> &'static str {
                $label
            }
            fn caps(&self) -> $crate::Caps {
                $crate::Caps::default()
            }
            fn auth_methods(&self) -> Vec<$crate::AuthMethod> {
                vec![]
            }
            fn verify<'a>(
                &'a self,
                _c: &'a $crate::Secret,
            ) -> $crate::BoxFut<'a, $crate::Result<$crate::Account>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn import<'a>(
                &'a self,
                _s: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<Option<$crate::Secret>>> {
                Box::pin(async { Ok(None) })
            }
            fn create<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _s: &'a $crate::CreateSpec,
            ) -> $crate::BoxFut<'a, $crate::Result<$crate::RemoteBox>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn get<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<$crate::RemoteBox>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn list<'a>(
                &'a self,
                _c: &'a $crate::Secret,
            ) -> $crate::BoxFut<'a, $crate::Result<Vec<$crate::RemoteBox>>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn destroy<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<()>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn suspend<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn resume<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<()>> {
                Box::pin(async { Ok(()) })
            }
            fn checkpoint<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
                _n: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<String>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn exec<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
                _r: $crate::ExecReq,
            ) -> $crate::BoxFut<'a, $crate::Result<$crate::Session>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn attach<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
                _s: &'a str,
                _w: u16,
                _h: u16,
            ) -> $crate::BoxFut<'a, $crate::Result<$crate::Session>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn sessions<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
            ) -> $crate::BoxFut<'a, $crate::Result<Vec<$crate::SessionInfo>>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn write_file<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
                _p: &'a str,
                _d: Vec<u8>,
                _m: u32,
            ) -> $crate::BoxFut<'a, $crate::Result<()>> {
                Box::pin(async { Err($crate::CloudError::unsupported("not implemented")) })
            }
            fn port_url<'a>(
                &'a self,
                _c: &'a $crate::Secret,
                _i: &'a str,
                _p: u16,
            ) -> $crate::BoxFut<'a, $crate::Result<Option<String>>> {
                Box::pin(async { Ok(None) })
            }
        }
    };
}
