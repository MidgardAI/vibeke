//! Relay accounts on the gateway side (spec 16 §6.6): which control plane a host logs in to,
//! where the credential is kept, whether a relay requires tickets, and the interactive device-code
//! login shared by `vibeke login` and `vibeke gateway pair`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use vk_account::{Account, Client, Credential, CredentialStore, KeychainStore, Prompt};

use crate::state::Config;

/// What a relay's `/v1/status` says about admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAuth {
    /// `auth == "tickets"`: hosts need an account token, devices a host-signed ticket.
    pub tickets: bool,
    pub account_url: Option<String>,
}

impl RelayAuth {
    /// Hosts need an account token: the relay names an account server. A relay that only
    /// requires tickets (`--require-tickets` without `--account-url`) needs none.
    pub fn needs_account(&self) -> bool {
        self.account_url.is_some()
    }
}

/// The relay's canonical `http(s)://` origin.
pub fn relay_origin(relay: &str) -> Result<String> {
    Ok(vk_e2e::relay::canonical_origin(
        &crate::relay_client::ws_base(relay),
    )?)
}

/// Ask the relay how it admits hosts and devices. An older relay without the field is open.
pub async fn relay_auth(relay: &str, host_id: &str) -> Result<RelayAuth> {
    let url = format!("{}/v1/status?host={host_id}", relay_origin(relay)?);
    let v: serde_json::Value = {
        let r = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        anyhow::ensure!(r.status().is_success(), "GET {url}: HTTP {}", r.status());
        serde_json::from_slice(&r.bytes().await?).context("relay status")?
    };
    Ok(RelayAuth {
        tickets: v.get("auth").and_then(|a| a.as_str()) == Some("tickets"),
        account_url: v
            .get("account_url")
            .and_then(|a| a.as_str())
            .map(str::to_string),
    })
}

/// The control plane for `cfg`: `account_url`, else the relay's origin, else the hosted relay.
pub fn account_server(cfg: &Config) -> String {
    cfg.account_url
        .clone()
        .or_else(|| cfg.relay.as_deref().and_then(|r| relay_origin(r).ok()))
        .unwrap_or_else(|| vk_account::DEFAULT_SERVER.to_string())
}

/// Credential storage for a gateway state dir: the OS keychain, falling back to
/// `<dir>/account.json` (0600). `VIBEKE_ACCOUNT_STORE=file` uses only the file.
pub fn store(dir: &Path) -> Arc<dyn CredentialStore> {
    let file = dir.join("account.json");
    // Unit tests never touch the OS keychain.
    if cfg!(test) || std::env::var("VIBEKE_ACCOUNT_STORE").as_deref() == Ok("file") {
        Arc::new(KeychainStore::file_only(file))
    } else {
        Arc::new(KeychainStore::new(file))
    }
}

/// The account for `server` with credentials kept for the gateway in `dir`.
pub fn account(server: &str, dir: &Path) -> Result<Account> {
    Ok(Account::new(Client::new(server)?, store(dir)))
}

/// Whether this host must log in to its relay, and whether it has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginNeed {
    /// The relay requires an account token (it names an account server).
    pub needs_account: bool,
    /// The account server the relay names.
    pub account_url: Option<String>,
    /// A credential is stored for [`account_server`] (only read when `needs_account`).
    pub logged_in: bool,
    pub login: Option<String>,
}

/// The account server `cfg`'s relay requires, `None` when it needs none: no relay, a private
/// relay's static `relay_token`, or an open relay. `Err` when the relay can't be asked.
async fn relay_account_url(cfg: &Config, host_id: &str) -> Result<Option<String>> {
    match cfg.relay.as_deref() {
        Some(relay) if cfg.relay_token.is_none() => {
            let a = relay_auth(relay, host_id).await?;
            Ok(a.needs_account().then_some(a.account_url).flatten())
        }
        _ => Ok(None),
    }
}

/// [`LoginNeed`] given the relay's answer; reads the credential only when an account is needed.
async fn need_for(cfg: &Config, dir: &Path, account_url: Option<String>) -> Result<LoginNeed> {
    let Some(url) = account_url else {
        return Ok(LoginNeed::default());
    };
    let acct = account(&account_server(cfg), dir)?;
    let cred = tokio::task::spawn_blocking(move || acct.credential()).await??;
    Ok(LoginNeed {
        needs_account: true,
        account_url: Some(url),
        logged_in: cred.is_some(),
        login: cred.map(|c| c.login),
    })
}

/// Whether this host must log in for its relay. An unreachable relay counts as open (as the
/// relay client treats it); only a credential store failure is an error.
pub async fn login_needed(cfg: &Config, dir: &Path, host_id: &str) -> Result<LoginNeed> {
    let url = relay_account_url(cfg, host_id).await.unwrap_or_else(|e| {
        tracing::debug!("relay status: {e:#}");
        None
    });
    need_for(cfg, dir, url).await
}

/// How long `account.status` trusts the relay's last `/v1/status` answer.
const STATUS_TTL: Duration = Duration::from_secs(60);
/// Lifetime of a device code when the server names none.
/// Longest a login job may stay pending, whatever the server says.
const MAX_EXPIRES_S: u64 = 3600;
const DEFAULT_EXPIRES_S: u64 = 600;

/// Where a TUI-started login stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    Pending,
    Done(String),
    Expired,
    Denied,
    Error(String),
}

struct LoginJob {
    id: String,
    prompt: Prompt,
    expires: Instant,
    status: Arc<Mutex<LoginStatus>>,
    handle: tokio::task::JoinHandle<()>,
}

impl LoginJob {
    fn status(&self) -> LoginStatus {
        let s = self.status.lock().unwrap().clone();
        match s {
            LoginStatus::Pending if Instant::now() > self.expires => LoginStatus::Expired,
            s => s,
        }
    }
}

/// The gateway's device-code logins started from the TUI (`account.*` on the server bridge):
/// at most one, kept in memory until replaced.
#[derive(Default)]
pub struct Logins {
    job: Mutex<Option<LoginJob>>,
    /// Serialises `start`, so two calls can't both begin a login.
    starting: tokio::sync::Mutex<()>,
    /// The relay's last answer: when, and the account server it named.
    relay: Mutex<Option<(Instant, Option<String>)>>,
    /// A login finished: the relay client stops waiting in `login_required`.
    pub done: tokio::sync::Notify,
}

impl Logins {
    /// [`login_needed`] with the relay's answer cached for a minute; an unreachable relay
    /// reports the last known answer (none: open).
    pub async fn status(&self, cfg: &Config, dir: &Path, host_id: &str) -> Result<LoginNeed> {
        let fresh = self
            .relay
            .lock()
            .unwrap()
            .clone()
            .filter(|(at, _)| at.elapsed() < STATUS_TTL);
        let url = match fresh {
            Some((_, url)) => url,
            None => match relay_account_url(cfg, host_id).await {
                Ok(url) => {
                    *self.relay.lock().unwrap() = Some((Instant::now(), url.clone()));
                    url
                }
                Err(e) => {
                    tracing::debug!("relay status: {e:#}");
                    self.relay.lock().unwrap().clone().and_then(|(_, url)| url)
                }
            },
        };
        need_for(cfg, dir, url).await
    }

    /// The pending login's id and prompt, if one is still pending.
    fn pending(&self) -> Option<(String, Prompt)> {
        let job = self.job.lock().unwrap();
        job.as_ref()
            .filter(|j| j.status() == LoginStatus::Pending)
            .map(|j| (j.id.clone(), j.prompt.clone()))
    }

    /// Start a device-code login with `acct` for `host_id`, or return the pending one. Returns
    /// once the server has issued the code; the poll runs in the background, saves the
    /// credential and wakes [`done`](Self::done).
    pub async fn start(
        self: &Arc<Self>,
        acct: Account,
        host_id: &str,
    ) -> vk_account::Result<(String, Prompt)> {
        let _starting = self.starting.lock().await;
        if let Some(p) = self.pending() {
            return Ok(p);
        }
        let status = Arc::new(Mutex::new(LoginStatus::Pending));
        let (tx, rx) = tokio::sync::oneshot::channel::<Prompt>();
        let handle = tokio::spawn({
            let status = status.clone();
            let host = host_id.to_string();
            let logins = Arc::downgrade(self);
            async move {
                let mut tx = Some(tx);
                let r = acct
                    .client()
                    .login_with(Some(&host), |p| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(p.clone());
                        }
                    })
                    .await;
                let r = match r {
                    Ok(cred) => {
                        let a = acct.clone();
                        let login = cred.login.clone();
                        match tokio::task::spawn_blocking(move || a.save(&cred)).await {
                            Ok(Ok(())) => Ok(login),
                            Ok(Err(e)) => Err(vk_account::Error::Store(e.to_string())),
                            Err(e) => Err(vk_account::Error::Store(e.to_string())),
                        }
                    }
                    Err(e) => Err(e),
                };
                *status.lock().unwrap() = match r {
                    Ok(login) => LoginStatus::Done(login),
                    Err(vk_account::Error::Expired) => LoginStatus::Expired,
                    Err(vk_account::Error::Denied) => LoginStatus::Denied,
                    Err(e) => LoginStatus::Error(e.to_string()),
                };
                if let Some(l) = logins.upgrade()
                    && matches!(*status.lock().unwrap(), LoginStatus::Done(_))
                {
                    l.done.notify_one();
                }
            }
        });
        let prompt = match rx.await {
            Ok(p) => p,
            // The task ended before the server issued a code.
            Err(_) => {
                let _ = handle.await;
                let s = status.lock().unwrap().clone();
                return Err(match s {
                    LoginStatus::Error(m) => vk_account::Error::Http(m),
                    _ => vk_account::Error::Http("the login did not start".into()),
                });
            }
        };
        let id = format!("login-{:016x}", rand::random::<u64>());
        // A server cannot keep a job alive forever with a huge `expires_in`.
        let expires_in = match prompt.expires_in {
            0 => DEFAULT_EXPIRES_S,
            n => n.min(MAX_EXPIRES_S),
        };
        let job = LoginJob {
            id: id.clone(),
            prompt: prompt.clone(),
            expires: Instant::now() + Duration::from_secs(expires_in),
            status,
            handle,
        };
        if let Some(old) = self.job.lock().unwrap().replace(job) {
            old.handle.abort();
        }
        Ok((id, prompt))
    }

    /// Where login `id` stands; an unknown or replaced id reads as expired.
    pub fn login_status(&self, id: &str) -> LoginStatus {
        match self.job.lock().unwrap().as_ref().filter(|j| j.id == id) {
            Some(j) => j.status(),
            None => LoginStatus::Expired,
        }
    }

    /// Abort login `id` if it is still pending (it then reads as expired).
    /// Abort whatever login is pending (a logout must not be undone by a later approval).
    pub fn cancel_all(&self) {
        if let Some(job) = self.job.lock().unwrap().take() {
            job.handle.abort();
        }
    }

    pub fn cancel(&self, id: &str) {
        let mut job = self.job.lock().unwrap();
        if job
            .as_ref()
            .is_some_and(|j| j.id == id && j.status() == LoginStatus::Pending)
            && let Some(j) = job.take()
        {
            j.handle.abort();
        }
    }
}

impl Drop for Logins {
    fn drop(&mut self) {
        if let Some(j) = self.job.get_mut().ok().and_then(|j| j.take()) {
            j.handle.abort();
        }
    }
}

/// The login prompt, as `vibeke login` prints it.
pub fn prompt_text(p: &Prompt) -> String {
    let mins = p.expires_in.div_ceil(60);
    format!(
        "Open {} on any device and enter the code:\n\n    {}\n\n(or open {})\nWaiting for approval… (expires in {mins} min)",
        p.verification_uri, p.user_code, p.verification_uri_complete
    )
}

/// Whether opening a browser here is likely to reach the user: never over SSH; on macOS
/// otherwise always; elsewhere only with a display.
pub fn display_likely() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return false;
    }
    cfg!(target_os = "macos") || set("DISPLAY") || set("WAYLAND_DISPLAY")
}

/// Best effort: `open` / `xdg-open`, output discarded, failures ignored.
pub fn open_browser(url: &str) {
    use std::process::{Command, Stdio};
    let tool = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(tool)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Run the device-code login on a terminal: print the prompt, maybe open a browser, poll until
/// approved, store the credential and print `Logged in as <login>`. A running gateway notices the
/// new credential on its own (it checks every few seconds while it waits for a login).
pub async fn login_interactive(
    acct: &Account,
    host_id: Option<&str>,
    browser: bool,
) -> Result<Credential> {
    let cred = acct
        .client()
        .login_with(host_id, |p| {
            println!("{}", prompt_text(p));
            if browser && display_likely() {
                open_browser(&p.verification_uri_complete);
            }
        })
        .await?;
    acct.save(&cred)?;
    println!("Logged in as {}", cred.login);
    Ok(cred)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_and_prompt() {
        let mut cfg = Config::default();
        assert_eq!(account_server(&cfg), "https://cloud.vibeke.dev");
        cfg.relay = Some("wss://relay.example/".into());
        assert_eq!(account_server(&cfg), "https://relay.example");
        cfg.account_url = Some("https://accounts.example".into());
        assert_eq!(account_server(&cfg), "https://accounts.example");
        let p = Prompt {
            verification_uri: "https://r/login/device".into(),
            verification_uri_complete: "https://r/login/device?user_code=WDJB-MJHT".into(),
            user_code: "WDJB-MJHT".into(),
            expires_in: 600,
        };
        assert_eq!(
            prompt_text(&p),
            "Open https://r/login/device on any device and enter the code:\n\n    WDJB-MJHT\n\n(or open https://r/login/device?user_code=WDJB-MJHT)\nWaiting for approval… (expires in 10 min)"
        );
    }
}
