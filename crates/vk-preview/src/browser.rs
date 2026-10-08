//! The Vibeke-managed browser for the external window (06 B3.3) and its profiles (B3.4).
//!
//! Profiles live under `<state>/browser-profiles/<name>` and are always passed as
//! `--user-data-dir`, so the user's real browser profile is never touched. Discovery prefers
//! an explicitly configured binary, then Playwright's Chromium, then installed Chromium-family
//! app bundles / binaries.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// A Chromium-family browser binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserBin {
    pub path: PathBuf,
    /// `config`, `env`, `playwright`, `chromium`, `chrome`, `brave`, `edge`.
    pub kind: String,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Playwright Chromium builds on disk, newest revision first.
pub fn playwright_chromium() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(p) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH") {
        roots.push(PathBuf::from(p));
    }
    if cfg!(target_os = "macos") {
        roots.push(home().join("Library/Caches/ms-playwright"));
    } else {
        roots.push(home().join(".cache/ms-playwright"));
    }
    let mut dirs: Vec<(u32, PathBuf)> = Vec::new();
    for r in roots {
        let Ok(rd) = std::fs::read_dir(&r) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // `chromium-1243`, not `chromium_headless_shell-…` (no headful mode).
            if let Some(rev) = name.strip_prefix("chromium-").and_then(|r| r.parse().ok()) {
                dirs.push((rev, e.path()));
            }
        }
    }
    dirs.sort_by_key(|a| std::cmp::Reverse(a.0));
    let rel: &[&str] = if cfg!(target_os = "macos") {
        &[
            "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else {
        &["chrome-linux64/chrome", "chrome-linux/chrome"]
    };
    dirs.into_iter()
        .flat_map(|(_, d)| rel.iter().map(move |r| d.join(r)))
        .filter(|p| p.is_file())
        .collect()
}

fn installed() -> Vec<(PathBuf, &'static str)> {
    let mut v: Vec<(PathBuf, &'static str)> = Vec::new();
    if cfg!(target_os = "macos") {
        for base in [PathBuf::from("/Applications"), home().join("Applications")] {
            for (app, bin, kind) in [
                ("Chromium.app", "Chromium", "chromium"),
                ("Google Chrome.app", "Google Chrome", "chrome"),
                ("Brave Browser.app", "Brave Browser", "brave"),
                ("Microsoft Edge.app", "Microsoft Edge", "edge"),
            ] {
                v.push((base.join(app).join("Contents/MacOS").join(bin), kind));
            }
        }
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for (name, kind) in [
            ("chromium", "chromium"),
            ("chromium-browser", "chromium"),
            ("google-chrome", "chrome"),
            ("google-chrome-stable", "chrome"),
            ("brave-browser", "brave"),
            ("microsoft-edge", "edge"),
        ] {
            for d in std::env::split_paths(&path) {
                v.push((d.join(name), kind));
            }
        }
    }
    v
}

/// Find a browser: `configured` (config `preview.browser`), `$VIBEKE_BROWSER`, Playwright
/// Chromium, then installed Chromium/Chrome/Brave/Edge.
pub fn find_browser(configured: Option<&str>) -> Option<BrowserBin> {
    if let Some(c) = configured.filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        return p.is_file().then(|| BrowserBin {
            path: p,
            kind: "config".into(),
        });
    }
    if let Some(e) = std::env::var_os("VIBEKE_BROWSER").filter(|e| !e.is_empty()) {
        let p = PathBuf::from(e);
        if p.is_file() {
            return Some(BrowserBin {
                path: p,
                kind: "env".into(),
            });
        }
    }
    if let Some(p) = playwright_chromium().into_iter().next() {
        return Some(BrowserBin {
            path: p,
            kind: "playwright".into(),
        });
    }
    installed()
        .into_iter()
        .find(|(p, _)| p.is_file())
        .map(|(p, k)| BrowserBin {
            path: p,
            kind: k.into(),
        })
}

/// Firefox for the window (06 B3.4; the browser pane always uses Chromium): `configured` if it
/// is a Firefox binary, else `$VIBEKE_FIREFOX`, else the installed app (`/Applications`,
/// `~/Applications`) / `firefox` on `PATH`.
pub fn find_firefox(configured: Option<&str>) -> Option<BrowserBin> {
    let cand = configured
        .filter(|c| !c.is_empty() && *c != "firefox")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("VIBEKE_FIREFOX")
                .filter(|e| !e.is_empty())
                .map(PathBuf::from)
        });
    if let Some(p) = cand {
        return p.is_file().then(|| BrowserBin {
            path: p,
            kind: "firefox".into(),
        });
    }
    let mut v: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        for base in [PathBuf::from("/Applications"), home().join("Applications")] {
            for app in [
                "Firefox.app",
                "Firefox Developer Edition.app",
                "Firefox Nightly.app",
            ] {
                v.push(base.join(app).join("Contents/MacOS/firefox"));
            }
        }
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for d in std::env::split_paths(&path) {
            v.push(d.join("firefox"));
        }
    }
    v.into_iter().find(|p| p.is_file()).map(|p| BrowserBin {
        path: p,
        kind: "firefox".into(),
    })
}

/// A binary that is Firefox (by file name), for `[preview] browser = "/path/to/firefox"`.
pub fn is_firefox_path(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().starts_with("firefox"))
}

/// `user.js` for a Vibeke Firefox profile (06 B3.4): the SOCKS route for remote machines
/// (`network.proxy.type=1`, `socks_version=5`, `socks_remote_dns=true` so names resolve on the
/// route, `allow_hijacking_localhost=true` so `localhost` is proxied too, no bypass list), or
/// no proxy for local previews; plus first-run/telemetry suppression. Rewritten on every
/// launch, so the route always matches the current SOCKS port.
pub fn firefox_prefs(socks_port: Option<u16>) -> String {
    let mut p: Vec<(String, String)> = vec![];
    let mut set = |k: &str, v: String| p.push((k.to_string(), v));
    match socks_port {
        Some(port) => {
            set("network.proxy.type", "1".into());
            set("network.proxy.socks", "\"127.0.0.1\"".into());
            set("network.proxy.socks_port", port.to_string());
            set("network.proxy.socks_version", "5".into());
            set("network.proxy.socks_remote_dns", "true".into());
            set("network.proxy.allow_hijacking_localhost", "true".into());
            set("network.proxy.no_proxies_on", "\"\"".into());
            set("network.proxy.share_proxy_settings", "false".into());
            set("network.proxy.http", "\"\"".into());
            set("network.proxy.ssl", "\"\"".into());
            // WebRTC must not send UDP around the route (same policy as Chromium).
            set("media.peerconnection.ice.proxy_only", "true".into());
            set("network.trr.mode", "5".into());
        }
        None => set("network.proxy.type", "0".into()),
    }
    for (k, v) in [
        ("browser.shell.checkDefaultBrowser", "false"),
        ("browser.aboutwelcome.enabled", "false"),
        ("browser.startup.homepage_override.mstone", "\"ignore\""),
        ("browser.tabs.warnOnClose", "false"),
        ("datareporting.policy.dataSubmissionEnabled", "false"),
        ("datareporting.healthreport.uploadEnabled", "false"),
        ("toolkit.telemetry.reportingpolicy.firstRun", "false"),
        ("app.normandy.enabled", "false"),
        ("signon.rememberSignons", "false"),
    ] {
        set(k, v.into());
    }
    let mut out = String::from(
        "// Written by Vibeke for this preview profile (06 B3.4). Edits are overwritten.\n",
    );
    for (k, v) in p {
        out.push_str(&format!("user_pref(\"{k}\", {v});\n"));
    }
    out
}

/// Firefox command line: always an explicit `-profile` (never the user's default profile);
/// `-new-instance` on the first launch so it can't hand off to a running Firefox of another
/// profile; later opens on the same profile go to our running instance.
pub fn firefox_args(spec: &LaunchSpec, first: bool) -> Vec<String> {
    let mut a = vec![
        "-profile".to_string(),
        spec.profile_dir.display().to_string(),
    ];
    if first {
        a.push("-new-instance".into());
    }
    if spec.headless {
        a.push("-headless".into());
    }
    a.extend(spec.extra_args.iter().cloned());
    a.push(spec.url.clone());
    a
}

/// Profile names are path components: `[A-Za-z0-9_.-]{1,64}`, not `.`/`..`.
pub fn valid_profile_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// Map an arbitrary label to a profile name.
pub fn profile_name(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        s = "default".into();
    }
    s
}

/// `<state_root>/browser-profiles`.
pub fn profiles_root(state_root: &Path) -> PathBuf {
    state_root.join("browser-profiles")
}

/// What to launch.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub bin: PathBuf,
    pub profile_dir: PathBuf,
    /// SOCKS5 port on 127.0.0.1 (remote profiles); `None` = no proxy (local machine).
    pub socks_port: Option<u16>,
    pub url: String,
    pub headless: bool,
    pub extra_args: Vec<String>,
    /// Firefox instead of Chromium (`user.js` prefs instead of switches).
    pub firefox: bool,
    /// A browser already runs on this profile (the launch hands the URL to it).
    pub reuse: bool,
}

/// WebRTC may not send UDP around the SOCKS route. The headless shell reads the force switch's
/// value, full Chromium/Chrome the plain switch; both are passed (same as
/// `vk_browser::headless::WEBRTC_UDP_POLICY`).
pub const WEBRTC_UDP_POLICY: [&str; 2] = [
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    "--webrtc-ip-handling-policy=disable_non_proxied_udp",
];

/// Command-line arguments (06 B3.4). `<-loopback>` removes Chromium's implicit loopback bypass
/// so `localhost` goes through the proxy; with `socks5://` hostnames resolve proxy-side.
pub fn args(spec: &LaunchSpec) -> Vec<String> {
    let mut a = vec![
        format!("--user-data-dir={}", spec.profile_dir.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-sync".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
    ];
    if let Some(p) = spec.socks_port {
        a.push(format!("--proxy-server=socks5://127.0.0.1:{p}"));
        a.push("--proxy-bypass-list=<-loopback>".into());
        a.extend(WEBRTC_UDP_POLICY.iter().map(|s| s.to_string()));
    }
    if spec.headless {
        a.push("--headless=new".into());
    }
    a.extend(spec.extra_args.iter().cloned());
    a.push(spec.url.clone());
    a
}

/// Refuse to run against anything that looks like the user's own browser profile.
pub fn check_profile_dir(dir: &Path, root: &Path) -> Result<()> {
    if !dir.starts_with(root) {
        bail!(
            "profile {} is outside {}; refusing",
            dir.display(),
            root.display()
        );
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(path)?;
    f.write_all(data)
}

/// Launch detached from the server's terminal (own process group, null stdio, stderr to
/// `log`). The caller keeps the child to reap it and to know the root pid.
pub fn launch(spec: &LaunchSpec, log: Option<&Path>) -> Result<tokio::process::Child> {
    std::fs::create_dir_all(&spec.profile_dir)
        .with_context(|| format!("create {}", spec.profile_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&spec.profile_dir, std::fs::Permissions::from_mode(0o700));
    }
    let mut c = tokio::process::Command::new(&spec.bin);
    if spec.firefox {
        let prefs = spec.profile_dir.join("user.js");
        write_private(&prefs, firefox_prefs(spec.socks_port).as_bytes())
            .with_context(|| format!("write {}", prefs.display()))?;
        c.args(firefox_args(spec, !spec.reuse))
            .env("MOZ_CRASHREPORTER_DISABLE", "1");
    } else {
        c.args(args(spec));
    }
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(false)
        .process_group(0);
    match log.and_then(|l| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(l)
            .ok()
    }) {
        Some(f) => c.stderr(f),
        None => c.stderr(std::process::Stdio::null()),
    };
    c.spawn()
        .with_context(|| format!("launch {}", spec.bin.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_never_use_default_profile() {
        let spec = LaunchSpec {
            bin: "/x/chrome".into(),
            profile_dir: "/state/browser-profiles/devbox".into(),
            socks_port: Some(41234),
            url: "http://localhost:5173/".into(),
            headless: false,
            extra_args: vec![],
            firefox: false,
            reuse: false,
        };
        let a = args(&spec);
        assert_eq!(a[0], "--user-data-dir=/state/browser-profiles/devbox");
        assert!(a.contains(&"--proxy-server=socks5://127.0.0.1:41234".to_string()));
        assert!(a.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
        assert!(
            a.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string())
        );
        assert!(a.contains(&"--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()));
        assert!(a.contains(&"--no-first-run".to_string()));
        assert!(a.contains(&"--no-default-browser-check".to_string()));
        assert_eq!(a.last().unwrap(), "http://localhost:5173/");
        let local = LaunchSpec {
            socks_port: None,
            ..spec
        };
        assert!(!args(&local).iter().any(|x| x.starts_with("--proxy")));
        assert!(!args(&local).iter().any(|x| x.contains("webrtc")));
    }

    #[test]
    fn profile_names() {
        assert!(valid_profile_name("devbox"));
        assert!(valid_profile_name("task-k7"));
        for bad in ["", ".", "..", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_profile_name(bad), "{bad:?}");
        }
        assert_eq!(profile_name("alice@devbox.ts.net"), "alice-devbox.ts.net");
        assert_eq!(profile_name("../x"), "..-x");
        assert!(valid_profile_name(&profile_name("../x")));
        assert_eq!(profile_name(""), "default");
        let root = Path::new("/s/browser-profiles");
        assert!(check_profile_dir(&root.join("devbox"), root).is_ok());
        assert!(
            check_profile_dir(
                Path::new("/Users/me/Library/Application Support/Google/Chrome"),
                root
            )
            .is_err()
        );
    }

    #[test]
    fn firefox_profile_prefs_and_args() {
        let p = firefox_prefs(Some(41234));
        for line in [
            "user_pref(\"network.proxy.type\", 1);",
            "user_pref(\"network.proxy.socks\", \"127.0.0.1\");",
            "user_pref(\"network.proxy.socks_port\", 41234);",
            "user_pref(\"network.proxy.socks_version\", 5);",
            "user_pref(\"network.proxy.socks_remote_dns\", true);",
            "user_pref(\"network.proxy.allow_hijacking_localhost\", true);",
            "user_pref(\"network.proxy.no_proxies_on\", \"\");",
            "user_pref(\"media.peerconnection.ice.proxy_only\", true);",
        ] {
            assert!(p.contains(line), "{line} missing:\n{p}");
        }
        let local = firefox_prefs(None);
        assert!(local.contains("user_pref(\"network.proxy.type\", 0);"));
        assert!(!local.contains("socks"));
        let spec = LaunchSpec {
            bin: "/Applications/Firefox.app/Contents/MacOS/firefox".into(),
            profile_dir: "/state/browser-profiles/devbox-firefox".into(),
            socks_port: Some(41234),
            url: "http://localhost:5173/".into(),
            headless: true,
            extra_args: vec![],
            firefox: true,
            reuse: false,
        };
        let a = firefox_args(&spec, true);
        assert_eq!(
            a,
            vec![
                "-profile",
                "/state/browser-profiles/devbox-firefox",
                "-new-instance",
                "-headless",
                "http://localhost:5173/"
            ]
        );
        assert!(!firefox_args(&spec, false).contains(&"-new-instance".to_string()));
        assert!(is_firefox_path(Path::new("/usr/bin/firefox")));
        assert!(is_firefox_path(Path::new("/x/firefox-esr")));
        assert!(!is_firefox_path(Path::new("/x/chrome")));
        assert_eq!(find_firefox(Some("/nonexistent/firefox")), None);
    }

    /// The prefs file lands in the Vibeke profile (0600), never anywhere else; the launch
    /// itself is exercised with a stand-in binary (`/bin/echo`), so no Firefox is needed.
    #[tokio::test]
    async fn firefox_launch_writes_prefs_into_the_vibeke_profile() {
        let root = tempfile::tempdir().unwrap();
        let dir = profiles_root(root.path()).join("devbox-firefox");
        let spec = LaunchSpec {
            bin: "/bin/echo".into(),
            profile_dir: dir.clone(),
            socks_port: Some(5555),
            url: "http://localhost:1/".into(),
            headless: true,
            extra_args: vec![],
            firefox: true,
            reuse: false,
        };
        check_profile_dir(&dir, &profiles_root(root.path())).unwrap();
        let mut c = launch(&spec, None).unwrap();
        let _ = c.wait().await;
        let prefs = std::fs::read_to_string(dir.join("user.js")).unwrap();
        assert!(prefs.contains("network.proxy.socks_port\", 5555"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("user.js"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dmode & 0o777, 0o700);
    }

    #[test]
    fn configured_browser_must_exist() {
        assert_eq!(find_browser(Some("/nonexistent/chrome")), None);
        let me = std::env::current_exe().unwrap();
        let b = find_browser(Some(me.to_str().unwrap())).unwrap();
        assert_eq!(b.kind, "config");
    }
}
