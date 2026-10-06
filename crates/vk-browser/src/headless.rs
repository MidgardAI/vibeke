//! The agents' headless Chromium (spec 06 B5): binary discovery and launch flags.
//!
//! One process per machine, owned by that machine's server, on a Vibeke profile directory under
//! the server's state (never a user profile), driven over `--remote-debugging-pipe`. Network
//! isolation is layered:
//!
//! - the default browser context gets `--proxy-server` pointing at a dead loopback port, so a
//!   page opened outside a Vibeke session can't load anything;
//! - each session's context gets its own filtering proxy ([`crate::proxy`]) with
//!   `proxyBypassList: "<-loopback>"` (Chromium otherwise bypasses proxies for loopback);
//! - `--host-resolver-rules` maps every name to `~NOTFOUND`, so the browser never resolves
//!   destinations itself (the proxy does, and checks the resolved addresses);
//! - QUIC is off and WebRTC may not use non-proxied UDP.

use std::path::{Path, PathBuf};

/// A discovered headless-capable Chromium binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessBin {
    pub path: PathBuf,
    /// `config`, `env`, `installed`, `playwright-shell`, `playwright`, `chromium`, `chrome`.
    pub kind: String,
}

impl HeadlessBin {
    /// `chrome-headless-shell` builds are always headless; full Chromium needs `--headless=new`.
    pub fn is_shell(&self) -> bool {
        is_headless_shell(&self.path)
    }
}

pub fn is_headless_shell(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "chrome-headless-shell" || n == "headless_shell")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn playwright_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(p) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH") {
        roots.push(PathBuf::from(p));
    }
    roots.push(home().join("Library/Caches/ms-playwright"));
    roots.push(home().join(".cache/ms-playwright"));
    roots
}

/// Playwright builds on disk, newest revision first: (`shell?`, path).
pub fn playwright_builds() -> Vec<(bool, PathBuf)> {
    let mut found: Vec<(u32, bool, PathBuf)> = Vec::new();
    for root in playwright_roots() {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let (rev, shell, subs): (Option<u32>, bool, &[&str]) = if let Some(r) =
                name.strip_prefix("chromium_headless_shell-")
            {
                (
                    r.parse().ok(),
                    true,
                    &[
                        "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                        "chrome-headless-shell-mac-x64/chrome-headless-shell",
                        "chrome-headless-shell-linux64/chrome-headless-shell",
                        "chrome-linux/headless_shell",
                    ],
                )
            } else if let Some(r) = name.strip_prefix("chromium-") {
                (
                    r.parse().ok(),
                    false,
                    &[
                        "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                        "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                        "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                        "chrome-linux64/chrome",
                        "chrome-linux/chrome",
                    ],
                )
            } else {
                (None, false, &[])
            };
            let Some(rev) = rev else { continue };
            for s in subs {
                let p = e.path().join(s);
                if p.is_file() {
                    found.push((rev, shell, p));
                }
            }
        }
    }
    // Newest first; at equal revision the headless shell first.
    found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    found.into_iter().map(|(_, s, p)| (s, p)).collect()
}

fn system_chromium() -> Vec<(PathBuf, &'static str)> {
    let mut v = Vec::new();
    if cfg!(target_os = "macos") {
        for base in [PathBuf::from("/Applications"), home().join("Applications")] {
            v.push((
                base.join("Chromium.app/Contents/MacOS/Chromium"),
                "chromium",
            ));
            v.push((
                base.join("Google Chrome.app/Contents/MacOS/Google Chrome"),
                "chrome",
            ));
        }
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for (name, kind) in [
            ("chromium", "chromium"),
            ("chromium-browser", "chromium"),
            ("google-chrome", "chrome"),
            ("google-chrome-stable", "chrome"),
        ] {
            for d in std::env::split_paths(&path) {
                v.push((d.join(name), kind));
            }
        }
    }
    v
}

/// Find the headless browser: `configured` (`preview.browser_path`), `$VIBEKE_HEADLESS_BROWSER`,
/// a build installed by `vibeke browser install` under `install_root`, Playwright's cache
/// (headless shell preferred), then a system Chromium/Chrome. Only the binary is used; the
/// profile directory is always Vibeke's own.
pub fn discover(configured: Option<&str>, install_root: &Path) -> Option<HeadlessBin> {
    if let Some(c) = configured.filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        return p.is_file().then(|| HeadlessBin {
            path: p,
            kind: "config".into(),
        });
    }
    if let Some(e) = std::env::var_os("VIBEKE_HEADLESS_BROWSER").filter(|e| !e.is_empty()) {
        let p = PathBuf::from(e);
        if p.is_file() {
            return Some(HeadlessBin {
                path: p,
                kind: "env".into(),
            });
        }
    }
    if let Some(p) = crate::install::installed(install_root) {
        return Some(HeadlessBin {
            path: p,
            kind: "installed".into(),
        });
    }
    if let Some((shell, p)) = playwright_builds().into_iter().next() {
        return Some(HeadlessBin {
            path: p,
            kind: if shell {
                "playwright-shell"
            } else {
                "playwright"
            }
            .into(),
        });
    }
    system_chromium()
        .into_iter()
        .find(|(p, _)| p.is_file())
        .map(|(p, k)| HeadlessBin {
            path: p,
            kind: k.into(),
        })
}

/// WebRTC may not use UDP that bypasses the proxy (06 B5, 09). The headless shell reads the
/// policy from the **value** of `--force-webrtc-ip-handling-policy` (a bare switch means the
/// default: UDP to any address, i.e. STUN/data channels to IP literals around the filtering
/// proxy), while full Chromium/Chrome reads `--webrtc-ip-handling-policy` and treats the force
/// switch as a flag. Both are passed; the gated `tests/webrtc_containment.rs` checks each
/// binary with a UDP sentinel.
pub const WEBRTC_UDP_POLICY: [&str; 2] = [
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    "--webrtc-ip-handling-policy=disable_non_proxied_udp",
];

/// Address of the default context's proxy: nothing listens on port 1, so a page in the default
/// context gets `ERR_PROXY_CONNECTION_FAILED` for every request.
pub const DEAD_PROXY: &str = "http://127.0.0.1:1";

/// Flags for the agents' headless browser on top of [`crate::cdp::LaunchOptions::args`].
pub fn isolation_args() -> Vec<String> {
    let mut a = vec![
        format!("--proxy-server={DEAD_PROXY}"),
        "--proxy-bypass-list=<-loopback>".into(),
        "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1".into(),
        "--disable-quic".into(),
        "--dns-prefetch-disable".into(),
        "--no-pings".into(),
        "--disable-breakpad".into(),
        "--disable-domain-reliability".into(),
        "--disable-client-side-phishing-detection".into(),
        "--deny-permission-prompts".into(),
        "--hide-scrollbars".into(),
    ];
    a.extend(WEBRTC_UDP_POLICY.iter().map(|s| s.to_string()));
    a
}

/// Launch options for the agents' browser.
pub fn launch_options(
    bin: &HeadlessBin,
    profile_dir: &Path,
    stderr_log: Option<PathBuf>,
) -> crate::cdp::LaunchOptions {
    let mut o = crate::cdp::LaunchOptions::new(&bin.path, profile_dir);
    o.headless_new = !bin.is_shell();
    o.extra_args = isolation_args();
    o.stderr_log = stderr_log;
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_isolate_network() {
        let bin = HeadlessBin {
            path: "/x/chrome-headless-shell".into(),
            kind: "playwright-shell".into(),
        };
        let o = launch_options(&bin, Path::new("/state/agent-browser/profile"), None);
        let a = o.args();
        assert!(a.contains(&"--remote-debugging-pipe".to_string()));
        assert!(a.contains(&"--user-data-dir=/state/agent-browser/profile".to_string()));
        assert!(a.contains(&"--proxy-server=http://127.0.0.1:1".to_string()));
        assert!(a.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
        assert!(
            a.iter()
                .any(|x| x.starts_with("--host-resolver-rules=MAP * ~NOTFOUND"))
        );
        assert!(a.contains(&"--disable-quic".to_string()));
        assert!(
            a.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string())
        );
        assert!(a.contains(&"--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()));
        assert!(
            !a.iter().any(|x| x == "--force-webrtc-ip-handling-policy"),
            "the headless shell reads the force switch's value: {a:?}"
        );
        assert!(
            !a.contains(&"--headless=new".to_string()),
            "shell is headless already"
        );
        assert!(!a.iter().any(|x| x.starts_with("--remote-debugging-port")));
        let full = HeadlessBin {
            path: "/x/Chromium".into(),
            kind: "chromium".into(),
        };
        assert!(
            launch_options(&full, Path::new("/p"), None)
                .args()
                .contains(&"--headless=new".to_string())
        );
    }

    #[test]
    fn configured_path_wins_and_must_exist() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("chrome-headless-shell");
        std::fs::write(&f, "").unwrap();
        let b = discover(Some(f.to_str().unwrap()), t.path()).unwrap();
        assert_eq!(b.kind, "config");
        assert!(b.is_shell());
        assert_eq!(discover(Some("/nonexistent/chrome"), t.path()), None);
    }
}
