//! The system `ssh` binary as transport (06 A2): the user's `~/.ssh/config` (ProxyJump, agents,
//! Tailscale SSH…) keeps working, and a ControlMaster makes repeated connections cheap.

use crate::mux::Mux;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    /// `user@host`, `host`, or an ssh config alias.
    pub address: String,
    pub port: Option<u16>,
    pub identity: Option<String>,
    pub jump: Option<String>,
    pub extra: Vec<String>,
}

impl Target {
    pub fn parse(label: &str, address: &str) -> Target {
        // user@host:port
        let (addr, port) = match address.rsplit_once(':') {
            Some((a, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (a.to_string(), p.parse().ok())
            }
            _ => (address.to_string(), None),
        };
        Target {
            label: label.to_string(),
            address: addr,
            port,
            identity: None,
            jump: None,
            extra: vec![],
        }
    }

    fn control_dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join(".cache/vibeke/ssh")
    }

    pub fn args(&self) -> Vec<String> {
        let dir = Self::control_dir();
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut a: Vec<String> = vec![
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            "ControlPersist=60".into(),
            "-o".into(),
            format!("ControlPath={}/%C", dir.display()),
        ];
        if let Some(p) = self.port {
            a.extend(["-p".into(), p.to_string()]);
        }
        if let Some(i) = &self.identity {
            a.extend(["-i".into(), i.clone()]);
        }
        if let Some(j) = &self.jump {
            a.extend(["-J".into(), j.clone()]);
        }
        a.extend(self.extra.iter().cloned());
        a
    }

    fn command(&self, remote: &str, batch: bool) -> Command {
        let mut c = Command::new(std::env::var("VIBEKE_SSH").unwrap_or_else(|_| "ssh".into()));
        c.args(self.args());
        if batch {
            c.args(["-o", "BatchMode=yes"]);
        }
        c.arg("-T").arg(&self.address).arg(remote);
        c.kill_on_drop(true);
        c
    }

    /// Run a remote shell command, optionally feeding stdin; returns stdout. Interactive auth
    /// (password, 2FA) is allowed here so later connections reuse the ControlMaster.
    pub async fn run(&self, remote: &str, stdin: Option<&[u8]>) -> Result<String> {
        let mut c = self.command(remote, false);
        c.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = c.spawn().context("spawn ssh")?;
        if let Some(data) = stdin {
            let mut si = child.stdin.take().context("ssh stdin")?;
            si.write_all(data).await?;
            si.shutdown().await?;
            drop(si);
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!(
                "ssh {} `{}` failed: {}",
                self.address,
                remote,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Start `<remote_bin> bridge` and a client mux over its stdio.
    pub async fn bridge(&self, remote_bin: &str, session: &str) -> Result<(Mux, Child)> {
        let cmd = format!(
            "{} bridge --session {}",
            sh_quote(remote_bin),
            sh_quote(session)
        );
        let mut c = self.command(&cmd, true);
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = c.spawn().context("spawn ssh bridge")?;
        let si = child.stdin.take().context("stdin")?;
        let so = child.stdout.take().context("stdout")?;
        let mux = Mux::start(so, si, "client", None);
        Ok((mux, child))
    }
}

/// Quote for a POSIX shell, keeping `~/` expandable at the start.
pub fn sh_quote(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", sh_quote(rest));
    }
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_quote() {
        let t = Target::parse("devbox", "demo@devbox:2222");
        assert_eq!(t.address, "demo@devbox");
        assert_eq!(t.port, Some(2222));
        assert_eq!(Target::parse("x", "host").port, None);
        assert_eq!(
            sh_quote("~/.local/share/vibeke/current/vibeke"),
            "\"$HOME\"/.local/share/vibeke/current/vibeke"
        );
        assert_eq!(sh_quote("a b'c"), "'a b'\\''c'");
    }
}
