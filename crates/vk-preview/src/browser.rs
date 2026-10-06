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
}

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
    c.args(args(spec))
        .stdin(std::process::Stdio::null())
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
        };
        let a = args(&spec);
        assert_eq!(a[0], "--user-data-dir=/state/browser-profiles/devbox");
        assert!(a.contains(&"--proxy-server=socks5://127.0.0.1:41234".to_string()));
        assert!(a.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
        assert!(a.contains(&"--no-first-run".to_string()));
        assert!(a.contains(&"--no-default-browser-check".to_string()));
        assert_eq!(a.last().unwrap(), "http://localhost:5173/");
        let local = LaunchSpec {
            socks_port: None,
            ..spec
        };
        assert!(!args(&local).iter().any(|x| x.starts_with("--proxy")));
    }

    #[test]
    fn profile_names() {
        assert!(valid_profile_name("devbox"));
        assert!(valid_profile_name("task-k7"));
        for bad in ["", ".", "..", "a/b", "a b", &"x".repeat(65)] {
            assert!(!valid_profile_name(bad), "{bad:?}");
        }
        assert_eq!(profile_name("demo@devbox.ts.net"), "demo-devbox.ts.net");
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
    fn configured_browser_must_exist() {
        assert_eq!(find_browser(Some("/nonexistent/chrome")), None);
        let me = std::env::current_exe().unwrap();
        let b = find_browser(Some(me.to_str().unwrap())).unwrap();
        assert_eq!(b.kind, "config");
    }
}
