//! Relay accounts on the gateway side (spec 16 §6.6): which control plane a host logs in to,
//! where the credential is kept, whether a relay requires tickets, and the interactive device-code
//! login shared by `vibeke login` and `vibeke gateway pair`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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
    if std::env::var("VIBEKE_ACCOUNT_STORE").as_deref() == Ok("file") {
        Arc::new(KeychainStore::file_only(file))
    } else {
        Arc::new(KeychainStore::new(file))
    }
}

/// The account for `server` with credentials kept for the gateway in `dir`.
pub fn account(server: &str, dir: &Path) -> Result<Account> {
    Ok(Account::new(Client::new(server)?, store(dir)))
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
        assert_eq!(account_server(&cfg), "https://relay.vibeke.dev");
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
