//! `vibeke login|logout|whoami` (spec 16 §6.6): the account a hosted relay requires. The login is
//! an OAuth device-code flow that works without a browser on this machine; the credential goes to
//! the OS keychain (or `<gateway dir>/account.json`, 0600, where there is none). A running
//! gateway waiting for a login (`login_required`) picks it up within a few seconds.

use vk_cli::{EXIT_OK, EXIT_USAGE};
use vk_gateway::state::{StateDir, default_dir};

pub const USAGE: &str =
    "vibeke login [--server URL] [--no-browser]   log in to the relay's account service
vibeke logout [--server URL]                 sign out and delete the stored credential
vibeke whoami [--server URL]                 show the logged-in account (exit 1 if none)";

struct Opts {
    server: Option<String>,
    no_browser: bool,
}

fn parse(args: &[String], allow_browser_flag: bool) -> Result<Opts, String> {
    let mut o = Opts {
        server: None,
        no_browser: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--server" => {
                o.server = Some(it.next().ok_or("--server needs a URL")?.clone());
            }
            s if s.starts_with("--server=") => o.server = Some(s["--server=".len()..].into()),
            "--no-browser" if allow_browser_flag => o.no_browser = true,
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(o)
}

/// Server: `--server`, else the gateway's `account_url`, else its relay's origin, else the
/// hosted relay.
fn resolve(state: &StateDir, flag: Option<String>) -> anyhow::Result<String> {
    let server = match flag {
        Some(s) => s,
        None => vk_gateway::account::account_server(&state.config()?),
    };
    Ok(vk_account::server_origin(&server)?)
}

pub async fn run(cmd: &str, args: &[String]) -> i32 {
    let opts = match parse(args, cmd == "login") {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{e}");
            }
            eprintln!("{USAGE}");
            return if e.is_empty() { EXIT_OK } else { EXIT_USAGE };
        }
    };
    match run_inner(cmd, opts).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("vibeke {cmd}: {e:#}");
            1
        }
    }
}

async fn run_inner(cmd: &str, opts: Opts) -> anyhow::Result<i32> {
    let state = StateDir::open(default_dir())?;
    let server = resolve(&state, opts.server)?;
    let acct = vk_gateway::account::account(&server, &state.dir)?;
    match cmd {
        "login" => {
            // Tell the server which host logs in, when this machine already has a gateway identity.
            let host = state
                .dir
                .join("host.json")
                .exists()
                .then(|| state.host_keys().map(|k| k.host_id()))
                .transpose()?;
            vk_gateway::account::login_interactive(&acct, host.as_deref(), !opts.no_browser)
                .await?;
            Ok(EXIT_OK)
        }
        "logout" => {
            if acct.logout().await? {
                println!("Logged out of {server}.");
            } else {
                println!("Not logged in to {server}.");
            }
            Ok(EXIT_OK)
        }
        "whoami" => match acct.credential()? {
            Some(c) => {
                println!("{} ({server})", c.login);
                Ok(EXIT_OK)
            }
            None => {
                eprintln!("Not logged in to {server}. Run: vibeke login");
                Ok(1)
            }
        },
        _ => unreachable!("dispatch routes only login, logout and whoami here"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags() {
        let o = parse(&v(&["--server", "relay.example", "--no-browser"]), true).unwrap();
        assert_eq!(o.server.as_deref(), Some("relay.example"));
        assert!(o.no_browser);
        let o = parse(&v(&["--server=https://r.example"]), false).unwrap();
        assert_eq!(o.server.as_deref(), Some("https://r.example"));
        assert!(parse(&v(&["--no-browser"]), false).is_err());
        assert!(parse(&v(&["--server"]), true).is_err());
        assert_eq!(parse(&v(&["-h"]), true).err().as_deref(), Some(""));
    }
}
