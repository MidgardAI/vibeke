//! Copy delivery (03 §11.1, 08 §13 D#748): how text the user copied (a mouse selection, a
//! copy-mode yank, a hint, a page's clipboard write) reaches the clipboard of the machine the
//! user sits at.
//!
//! - **OSC 52** to the host terminal, always (wrapped in a tmux DCS passthrough when `TMUX` is
//!   set). Writes larger than `clipboard.remote_write_max_bytes` are not sent this way.
//! - **The platform clipboard tool** too when the TUI runs on the machine with the display (no
//!   `SSH_CONNECTION` / `SSH_TTY`): `pbcopy` on macOS, else `wl-copy` / `xclip` / `xsel`
//!   (PRIMARY: `wl-copy --primary` / `xclip -selection primary` / `xsel -p`; macOS has none).
//!   `VIBEKE_CLIPBOARD_CMD` replaces the tool (run with `sh -c`, the text on stdin and
//!   `VIBEKE_CLIPBOARD_SELECTION=clipboard|primary` in the environment) — a test hook that
//!   keeps tests off the real clipboard.
//! - Over SSH from iTerm2 (`LC_TERMINAL=iTerm2`), the first copy also shows a one-time hint:
//!   iTerm2 drops OSC 52 unless "Applications in terminal may access clipboard" is on.

use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Command, Stdio};

/// What the TUI knows about where it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEnv {
    /// Running over SSH (`SSH_CONNECTION` or `SSH_TTY`): the platform clipboard here is not
    /// the user's.
    pub ssh: bool,
    /// The host terminal is iTerm2 (`LC_TERMINAL=iTerm2`, which iTerm2 sends over SSH, or
    /// `TERM_PROGRAM=iTerm.app` locally).
    pub iterm2: bool,
    /// Inside tmux (`TMUX`): OSC 52 goes through a DCS passthrough.
    pub tmux: bool,
}

impl HostEnv {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let set = |k: &str| get(k).is_some_and(|v| !v.is_empty());
        HostEnv {
            ssh: set("SSH_CONNECTION") || set("SSH_TTY"),
            iterm2: get("LC_TERMINAL").as_deref() == Some("iTerm2")
                || get("TERM_PROGRAM").as_deref() == Some("iTerm.app"),
            tmux: set("TMUX"),
        }
    }
}

pub const ITERM2_HINT: &str = "If paste doesn't work: iTerm2 → Settings → General → Selection → enable 'Applications in terminal may access clipboard'";

/// Per-client copy delivery state.
#[derive(Debug, Clone, Default)]
pub struct Delivery {
    pub env: HostEnv,
    /// Replaces the platform clipboard tool (`VIBEKE_CLIPBOARD_CMD`, a `sh -c` command line).
    pub native_cmd: Option<String>,
    /// The iTerm2-over-SSH hint was shown (once per TUI run).
    pub hinted: bool,
}

impl Delivery {
    pub fn from_env() -> Self {
        Delivery {
            env: HostEnv::from_env(),
            native_cmd: std::env::var("VIBEKE_CLIPBOARD_CMD")
                .ok()
                .filter(|c| !c.trim().is_empty()),
            hinted: false,
        }
    }

    /// The hint to show after a successful clipboard copy, once.
    pub fn take_hint(&mut self) -> Option<&'static str> {
        if self.env.ssh && self.env.iterm2 && !self.hinted {
            self.hinted = true;
            return Some(ITERM2_HINT);
        }
        None
    }
}

/// How one copy is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Bytes to write to the host terminal (OSC 52, maybe tmux-wrapped); `None` = skip.
    pub osc52: Option<Vec<u8>>,
    /// Also run the platform clipboard tool.
    pub native: bool,
    /// Why OSC 52 was skipped, when it was.
    pub osc52_skipped: Option<String>,
}

/// Wrap an escape sequence in tmux's DCS passthrough (`ESC P tmux; … ESC \`, inner ESCs doubled).
pub fn tmux_wrap(seq: &[u8]) -> Vec<u8> {
    let mut out = b"\x1bPtmux;".to_vec();
    for &b in seq {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

pub fn plan(data: &[u8], primary: bool, env: &HostEnv, max_osc52: u64) -> Plan {
    let native = !env.ssh;
    if data.len() as u64 > max_osc52 {
        return Plan {
            osc52: None,
            native,
            osc52_skipped: Some(format!(
                "{} is over the OSC 52 limit ({})",
                crate::upload::human(data.len() as u64),
                crate::upload::human(max_osc52)
            )),
        };
    }
    let seq = crate::clipboard::osc52_set(data, primary);
    Plan {
        osc52: Some(if env.tmux { tmux_wrap(&seq) } else { seq }),
        native,
        osc52_skipped: None,
    }
}

/// Run `plan`: write OSC 52 to `out`, then the platform tool via `native`. `Ok` when at least
/// one way was taken; the error names why nothing worked.
pub fn deliver(
    data: &[u8],
    primary: bool,
    plan: &Plan,
    out: &mut dyn Write,
    native: &mut dyn FnMut(&[u8], bool) -> anyhow::Result<()>,
) -> Result<(), String> {
    let mut sent = false;
    let mut why: Vec<String> = Vec::new();
    if let Some(seq) = &plan.osc52 {
        match out.write_all(seq).and_then(|_| out.flush()) {
            Ok(()) => sent = true,
            Err(e) => why.push(format!("terminal write failed: {e}")),
        }
    } else if let Some(s) = &plan.osc52_skipped {
        why.push(s.clone());
    }
    if plan.native {
        match native(data, primary) {
            Ok(()) => sent = true,
            Err(e) => why.push(format!("{e:#}")),
        }
    } else if plan.osc52.is_none() {
        why.push("no local clipboard over SSH".into());
    }
    if sent { Ok(()) } else { Err(why.join("; ")) }
}

/// The platform clipboard tool, or the `VIBEKE_CLIPBOARD_CMD` override.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_copy(cmd: Option<&str>, data: &[u8], primary: bool) -> anyhow::Result<()> {
    let Some(cmd) = cmd else {
        return if primary {
            crate::clipboard::os_copy_primary(data)
        } else {
            crate::clipboard::os_copy(data)
        };
    };
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env(
            "VIBEKE_CLIPBOARD_SELECTION",
            if primary { "primary" } else { "clipboard" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let w = stdin.write_all(data);
    drop(stdin);
    let st = child.wait()?;
    w?;
    if !st.success() {
        anyhow::bail!("VIBEKE_CLIPBOARD_CMD exited with {st}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(ssh: bool, iterm2: bool, tmux: bool) -> HostEnv {
        HostEnv { ssh, iterm2, tmux }
    }

    #[test]
    fn host_env_from_variables() {
        let e = HostEnv::from_lookup(|k| match k {
            "SSH_CONNECTION" => Some("1.2.3.4 5 6.7.8.9 22".into()),
            "LC_TERMINAL" => Some("iTerm2".into()),
            _ => None,
        });
        assert_eq!(e, env(true, true, false));
        let e = HostEnv::from_lookup(|k| match k {
            "SSH_TTY" => Some("/dev/pts/1".into()),
            "TMUX" => Some("/tmp/tmux-1/default,1,0".into()),
            "TERM_PROGRAM" => Some("ghostty".into()),
            _ => None,
        });
        assert_eq!(e, env(true, false, true));
        let e = HostEnv::from_lookup(|k| (k == "SSH_TTY").then(String::new));
        assert_eq!(e, env(false, false, false), "empty values don't count");
    }

    #[test]
    fn local_copies_go_to_osc52_and_the_platform_tool() {
        let p = plan(b"hi", false, &env(false, false, false), 1 << 20);
        assert_eq!(p.osc52.as_deref(), Some(&b"\x1b]52;c;aGk=\x1b\\"[..]));
        assert!(p.native);
        let p = plan(b"hi", false, &env(true, true, false), 1 << 20);
        assert!(!p.native, "over SSH the platform clipboard is the server's");
        assert!(p.osc52.is_some());
    }

    #[test]
    fn tmux_gets_a_dcs_passthrough() {
        assert_eq!(
            tmux_wrap(b"\x1b]52;c;aGk=\x1b\\"),
            b"\x1bPtmux;\x1b\x1b]52;c;aGk=\x1b\x1b\\\x1b\\"
        );
        let p = plan(b"hi", true, &env(true, false, true), 1 << 20);
        assert_eq!(
            p.osc52.as_deref(),
            Some(&b"\x1bPtmux;\x1b\x1b]52;p;aGk=\x1b\x1b\\\x1b\\"[..])
        );
    }

    #[test]
    fn the_size_limit_skips_osc52() {
        let big = vec![b'x'; 2048];
        let p = plan(&big, false, &env(true, false, false), 1024);
        assert!(p.osc52.is_none() && !p.native);
        let mut out = Vec::new();
        let err = deliver(&big, false, &p, &mut out, &mut |_, _| Ok(())).unwrap_err();
        assert!(err.contains("over the OSC 52 limit"), "{err}");
        assert!(err.contains("no local clipboard over SSH"), "{err}");
        assert!(out.is_empty());
        // Locally the platform tool still takes it.
        let p = plan(&big, false, &env(false, false, false), 1024);
        let mut got = Vec::new();
        deliver(&big, false, &p, &mut out, &mut |d, _| {
            got = d.to_vec();
            Ok(())
        })
        .unwrap();
        assert_eq!(got.len(), 2048);
        assert!(out.is_empty());
    }

    #[test]
    fn deliver_reports_success_when_either_path_works() {
        let p = plan(b"hey", false, &env(false, false, false), 1 << 20);
        let mut out = Vec::new();
        let mut calls = 0;
        deliver(b"hey", false, &p, &mut out, &mut |_, _| {
            calls += 1;
            anyhow::bail!("pbcopy failed")
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(out, b"\x1b]52;c;aGV5\x1b\\");
    }

    #[test]
    fn iterm2_over_ssh_hints_once() {
        let mut d = Delivery {
            env: env(true, true, false),
            ..Default::default()
        };
        assert_eq!(d.take_hint(), Some(ITERM2_HINT));
        assert_eq!(d.take_hint(), None);
        let mut local = Delivery {
            env: env(false, true, false),
            ..Default::default()
        };
        assert_eq!(local.take_hint(), None, "pbcopy works locally");
    }

    #[test]
    fn the_command_hook_replaces_the_platform_tool() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("clip");
        let cmd = format!(
            "printf '%s:' \"$VIBEKE_CLIPBOARD_SELECTION\" >> {0}; cat >> {0}",
            f.display()
        );
        native_copy(Some(&cmd), b"abc", false).unwrap();
        native_copy(Some(&cmd), b"def", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "clipboard:abcprimary:def"
        );
        assert!(native_copy(Some("exit 3"), b"x", false).is_err());
    }
}

#[cfg(target_arch = "wasm32")]
pub fn native_copy(_cmd: Option<&str>, _data: &[u8], _primary: bool) -> anyhow::Result<()> {
    anyhow::bail!("Use the browser clipboard controls")
}
