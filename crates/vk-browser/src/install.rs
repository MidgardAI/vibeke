//! `vibeke browser install` (spec 06 B5): a pinned Chrome-for-Testing build, verified by
//! SHA-256 before anything is unpacked. Two builds ([`Flavor`]): the full browser ("Google Chrome
//! for Testing", also good for preview windows) on a machine with a display, the smaller
//! `chrome-headless-shell` (browser panes and agent sessions only) on a headless one.
//!
//! The command **asks first** (the CLI shows the plan and needs a yes; the API needs
//! `confirm: true`). The download itself is a pluggable [`Fetch`] function (`curl` in
//! production, a fake in tests). A build whose pin has no recorded checksum can only be installed
//! with an explicit `--sha256` from the user: nothing is ever unpacked unverified.
//!
//! Layout: `<root>/<build>-<version>-<platform>/` holding the archive's top directory
//! (`chrome-headless-shell-<platform>/chrome-headless-shell`, `chrome-<platform>/chrome` or the
//! macOS `Google Chrome for Testing.app`), written via a staging directory and renamed into place.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pinned Chrome-for-Testing build (the version Stage 0 measured with).
pub const PINNED_VERSION: &str = "153.0.8010.12";

/// Known SHA-256 checksums per (version, build, platform). Chrome for Testing publishes no
/// checksums; entries are recorded by whoever bumps the pin after verifying a download out of
/// band (these were checked against the MD5 Google Cloud Storage reports for each object,
/// 2026-10-10). A version or platform without one needs an explicit `--sha256`.
pub const PINNED_SHA256: &[(&str, &str, &str, &str)] = &[
    (
        "153.0.8010.12",
        "chrome-headless-shell",
        "mac-arm64",
        "89d80a6d26ccd0ccfd51e22d9e1297283862af2b0cd91dce07459b35ca0059f2",
    ),
    (
        "153.0.8010.12",
        "chrome-headless-shell",
        "mac-x64",
        "5c2eaa1aad62111bb5a70dd0889dd3093f3142277b8f78957a238257ee85f009",
    ),
    (
        "153.0.8010.12",
        "chrome-headless-shell",
        "linux64",
        "a9da028861a0cf789ff25c2fed45f5f1aaf969ed9247835b6a7821a4f7af9d1d",
    ),
    (
        "153.0.8010.12",
        "chrome",
        "mac-arm64",
        "930e2a2c15addbaca1fe9b07bfa520667bced556d7988707186819cb4279ef3b",
    ),
    (
        "153.0.8010.12",
        "chrome",
        "mac-x64",
        "4e12e2b8a297a2eb79c16258e46961c73f8aa2fa6be6b1d3cdbb553fb5d4ef1c",
    ),
    (
        "153.0.8010.12",
        "chrome",
        "linux64",
        "8aac35011c18f6e2d10696154af89a5728ac2ddd6dc6fad24ffdf243c3fcfd5a",
    ),
];

/// Which Chrome for Testing build to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// The full browser: browser panes (as `--headless=new`), agent sessions and preview windows.
    Full,
    /// `chrome-headless-shell`: smaller, for browser panes and agent sessions only (no window).
    HeadlessShell,
}

impl Flavor {
    /// The Chrome for Testing build name (archive `<build>-<platform>.zip`).
    pub fn build(self) -> &'static str {
        match self {
            Flavor::Full => "chrome",
            Flavor::HeadlessShell => "chrome-headless-shell",
        }
    }

    /// The API name (`browser.install {flavor}`).
    pub fn as_str(self) -> &'static str {
        match self {
            Flavor::Full => "full",
            Flavor::HeadlessShell => "headless_shell",
        }
    }

    pub fn parse(s: &str) -> Option<Flavor> {
        match s {
            "full" => Some(Flavor::Full),
            "headless_shell" | "headless-shell" => Some(Flavor::HeadlessShell),
            _ => None,
        }
    }

    /// The binary inside the archive, relative to the install directory.
    fn binary(self, platform: &str) -> PathBuf {
        match self {
            Flavor::HeadlessShell => PathBuf::from(format!("chrome-headless-shell-{platform}"))
                .join("chrome-headless-shell"),
            Flavor::Full if platform.starts_with("mac") => PathBuf::from(format!(
                "chrome-{platform}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
            )),
            Flavor::Full => PathBuf::from(format!("chrome-{platform}")).join("chrome"),
        }
    }
}

/// The build to install when the caller does not choose, with the reason shown in the plan: the
/// full browser where someone can look at a window (macOS, or Linux with `DISPLAY` or
/// `WAYLAND_DISPLAY`, as seen by the server), else the headless shell.
pub fn default_flavor() -> (Flavor, &'static str) {
    let display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    default_flavor_for(std::env::consts::OS, display)
}

fn default_flavor_for(os: &str, display: bool) -> (Flavor, &'static str) {
    if os == "macos" {
        (
            Flavor::Full,
            "this Mac can show windows: the full browser also opens preview windows",
        )
    } else if display {
        (
            Flavor::Full,
            "a display is available: the full browser also opens preview windows",
        )
    } else {
        (
            Flavor::HeadlessShell,
            "no display: the headless shell is enough for browser panes and agent sessions",
        )
    }
}

/// `mac-arm64`, `mac-x64`, `linux64` (Chrome for Testing platform names).
pub fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("mac-arm64"),
        ("macos", "x86_64") => Some("mac-x64"),
        ("linux", "x86_64") => Some("linux64"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub flavor: Flavor,
    /// Why this build ("--full", "no display: …").
    pub reason: String,
    pub version: String,
    pub platform: String,
    pub url: String,
    /// Lower-case hex; `None` = unknown (install refuses).
    pub sha256: Option<String>,
    pub root: PathBuf,
    pub dir: PathBuf,
    pub binary: PathBuf,
}

impl InstallPlan {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "flavor": self.flavor.as_str(),
            "build": self.flavor.build(),
            "reason": self.reason,
            "windows": self.flavor == Flavor::Full,
            "version": self.version,
            "platform": self.platform,
            "url": self.url,
            "sha256": self.sha256,
            "checksum_known": self.sha256.is_some(),
            "dir": self.dir,
            "binary": self.binary,
            "installed": self.binary.is_file(),
        })
    }
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Plan an install into `root`. `url`/`sha256` override the pin (e.g. a mirror); `flavor`
/// `None` picks [`default_flavor`].
pub fn plan(
    root: &Path,
    version: Option<&str>,
    url: Option<&str>,
    sha256: Option<&str>,
    flavor: Option<Flavor>,
) -> Result<InstallPlan> {
    let (flavor, reason) = match flavor {
        Some(f @ Flavor::Full) => (f, "--full".to_string()),
        Some(f @ Flavor::HeadlessShell) => (f, "--headless-shell".to_string()),
        None => {
            let (f, r) = default_flavor();
            (f, r.to_string())
        }
    };
    let build = flavor.build();
    let platform = platform().context("no Chrome for Testing build for this platform")?;
    let version = version.unwrap_or(PINNED_VERSION).to_string();
    if !version.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        bail!("invalid version {version}");
    }
    let url = url.map(str::to_string).unwrap_or_else(|| {
        format!(
            "https://storage.googleapis.com/chrome-for-testing-public/{version}/{platform}/{build}-{platform}.zip"
        )
    });
    if !url.starts_with("https://") {
        bail!("download URL must be https");
    }
    let sha256 = match sha256 {
        Some(s) if valid_sha(s) => Some(s.to_ascii_lowercase()),
        Some(_) => bail!("--sha256 must be 64 hex digits"),
        None => PINNED_SHA256
            .iter()
            .find(|(v, b, p, _)| *v == version && *b == build && *p == platform)
            .map(|(_, _, _, h)| h.to_string()),
    };
    let dir = root.join(format!("{build}-{version}-{platform}"));
    let binary = dir.join(flavor.binary(platform));
    Ok(InstallPlan {
        flavor,
        reason,
        version,
        platform: platform.to_string(),
        url,
        sha256,
        root: root.to_path_buf(),
        dir,
        binary,
    })
}

/// Downloads `url` to the file `dest`.
pub type Fetch<'a> = &'a dyn Fn(&str, &Path) -> Result<()>;

/// Upper bound for the browser archive (512 MiB), passed to curl `--max-filesize`.
const MAX_DOWNLOAD: &str = "536870912";

/// Refuse archive entries that could escape the extraction directory: absolute paths, `..`
/// components, or an entry written through a symlink entry. `names` is `unzip -Z1` output;
/// `long` is the plain `unzip -Z` listing, whose lines for symlinks start with `l`. Symlinks
/// themselves are allowed (the macOS app bundle's frameworks have them); where they point is
/// checked after unpacking by [`check_symlinks`].
pub fn check_zip_entries(names: &str, long: &str) -> Result<()> {
    let names: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
    for n in &names {
        if n.starts_with('/')
            || n.starts_with('\\')
            || n.split(['/', '\\']).any(|c| c == "..")
            || n.contains('\0')
            || n.as_bytes().get(1) == Some(&b':')
        {
            bail!("archive entry {n:?} escapes the install directory; nothing was installed");
        }
    }
    // A symlink line ends with its entry name; the longest matching name is the entry.
    let links: Vec<&str> = long
        .lines()
        .filter(|l| l.starts_with('l'))
        .map(|l| {
            names
                .iter()
                .filter(|n| l.ends_with(&format!(" {n}")))
                .max_by_key(|n| n.len())
                .copied()
                .ok_or_else(|| {
                    anyhow::anyhow!("unreadable symlink entry {l:?}; nothing was installed")
                })
        })
        .collect::<Result<_>>()?;
    for link in &links {
        let inside = format!("{}/", link.trim_end_matches('/'));
        if let Some(n) = names.iter().find(|n| n.starts_with(&inside)) {
            bail!(
                "archive entry {n:?} is written through the symlink {link:?}; nothing was installed"
            );
        }
    }
    Ok(())
}

/// After unpacking into `dir`: every symlink must be relative and resolve (lexically, from its
/// own directory) to a path inside `dir`.
pub fn check_symlinks(dir: &Path) -> Result<()> {
    fn walk(root: &Path, d: &Path) -> Result<()> {
        for e in std::fs::read_dir(d)? {
            let e = e?;
            let p = e.path();
            let ft = e.file_type()?;
            if ft.is_symlink() {
                let target = std::fs::read_link(&p)?;
                let rel = p.parent().unwrap_or(root).strip_prefix(root)?;
                let mut depth: Vec<std::ffi::OsString> =
                    rel.components().map(|c| c.as_os_str().to_owned()).collect();
                let ok = !target.is_absolute()
                    && target.components().all(|c| match c {
                        std::path::Component::Normal(x) => {
                            depth.push(x.to_owned());
                            true
                        }
                        std::path::Component::CurDir => true,
                        std::path::Component::ParentDir => depth.pop().is_some(),
                        _ => false,
                    });
                if !ok {
                    bail!(
                        "symlink {} points outside the install ({}); nothing was installed",
                        p.strip_prefix(root).unwrap_or(&p).display(),
                        target.display()
                    );
                }
            } else if ft.is_dir() {
                walk(root, &p)?;
            }
        }
        Ok(())
    }
    walk(dir, dir)
}

/// Production fetcher: `curl` over https only, failing on HTTP errors.
pub fn curl_fetch(url: &str, dest: &Path) -> Result<()> {
    let st = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--max-filesize",
            MAX_DOWNLOAD,
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .context("run curl")?;
    if !st.success() {
        bail!("download failed ({st})");
    }
    Ok(())
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Download, verify, unpack and move into place. Returns the binary path.
pub fn install(plan: &InstallPlan, fetch: Fetch) -> Result<PathBuf> {
    let Some(want) = &plan.sha256 else {
        bail!(
            "no recorded SHA-256 for {} {} ({}); pass --sha256 <hex> after verifying the download",
            plan.flavor.build(),
            plan.version,
            plan.platform
        );
    };
    if plan.binary.is_file() {
        return Ok(plan.binary.clone());
    }
    std::fs::create_dir_all(&plan.root)?;
    let part = plan
        .root
        .join(format!(".download-{}.zip.part", plan.version));
    let staging = plan.root.join(format!(".staging-{}", plan.version));
    let _ = std::fs::remove_file(&part);
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| -> Result<PathBuf> {
        fetch(&plan.url, &part)?;
        let got = sha256_file(&part)?;
        if &got != want {
            bail!("checksum mismatch: expected {want}, got {got}; nothing was installed");
        }
        let list = |flag: &str| -> Result<String> {
            let o = Command::new("unzip")
                .args(["-Z", flag])
                .arg(&part)
                .output()
                .context("run unzip -Z")?;
            if !o.status.success() {
                bail!("unzip could not list the archive ({})", o.status);
            }
            Ok(String::from_utf8_lossy(&o.stdout).into_owned())
        };
        let names = list("-1")?;
        let long = Command::new("unzip")
            .arg("-Z")
            .arg(&part)
            .output()
            .context("run unzip -Z")?;
        check_zip_entries(&names, &String::from_utf8_lossy(&long.stdout))?;
        std::fs::create_dir_all(&staging)?;
        let st = Command::new("unzip")
            .args(["-q", "-o"])
            .arg(&part)
            .arg("-d")
            .arg(&staging)
            .status()
            .context("run unzip")?;
        if !st.success() {
            bail!("unzip failed ({st})");
        }
        check_symlinks(&staging)?;
        let rel = plan.binary.strip_prefix(&plan.dir).unwrap_or(&plan.binary);
        let staged_bin = staging.join(rel);
        if !staged_bin.is_file() {
            bail!("archive does not contain {}", rel.display());
        }
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&staged_bin)?.permissions();
            perm.set_mode(perm.mode() | 0o755);
            std::fs::set_permissions(&staged_bin, perm)?;
        }
        let _ = std::fs::remove_dir_all(&plan.dir);
        std::fs::rename(&staging, &plan.dir)?;
        Ok(plan.binary.clone())
    })();
    let _ = std::fs::remove_file(&part);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// The newest installed build of `flavor` under `root`, if any.
pub fn installed_flavor(root: &Path, flavor: Flavor) -> Option<PathBuf> {
    let platform = platform()?;
    let build = flavor.build();
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| {
            // `chrome-<version>-…` vs `chrome-headless-shell-<version>-…`: the version starts
            // with a digit.
            e.file_name()
                .to_string_lossy()
                .strip_prefix(&format!("{build}-"))
                .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
        })
        .map(|e| e.path().join(flavor.binary(platform)))
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v.pop()
}

/// A build for headless use (agent sessions, browser panes): the headless shell, else the full
/// browser.
pub fn installed(root: &Path) -> Option<PathBuf> {
    installed_flavor(root, Flavor::HeadlessShell).or_else(|| installed_full(root))
}

/// The installed full browser (the only build that can open a preview window).
pub fn installed_full(root: &Path) -> Option<PathBuf> {
    installed_flavor(root, Flavor::Full)
}

#[cfg(test)]
mod tests {
    #[test]
    fn zip_entries_are_vetted() {
        use super::check_zip_entries as c;
        let long = "Archive:  x.zip\n-rw-r--r--  3.0 unx  10 bx defN a/b\n";
        assert!(c("a/b\na/c/d\n", long).is_ok());
        assert!(c("/etc/passwd\n", long).is_err());
        assert!(c("a/../../x\n", long).is_err());
        assert!(c("a\\..\\x\n", long).is_err());
        // A symlink entry is fine (its target is checked after unpacking) …
        let link = "lrwxr-xr-x  3.0 unx  10 bx stor a/link\n";
        assert!(c("a/b\na/link\n", link).is_ok());
        // … but nothing may be written through it.
        let e = c("a/link\na/link/x\n", link).unwrap_err();
        assert!(format!("{e:#}").contains("through the symlink"), "{e:#}");
        // A name with spaces (the macOS framework links) is matched whole.
        let fw = "lrwxr-xr-x  3.0 unx  26 bx stor a/My Framework.framework/Resources\n";
        assert!(c("a/My Framework.framework/Resources\n", fw).is_ok());
    }

    #[test]
    fn default_flavor_follows_the_display() {
        assert_eq!(default_flavor_for("macos", false).0, Flavor::Full);
        assert_eq!(default_flavor_for("linux", true).0, Flavor::Full);
        let (f, why) = default_flavor_for("linux", false);
        assert_eq!(f, Flavor::HeadlessShell);
        assert!(why.contains("no display"));
        assert_eq!(Flavor::parse("full"), Some(Flavor::Full));
        assert_eq!(Flavor::parse("headless-shell"), Some(Flavor::HeadlessShell));
        assert_eq!(Flavor::parse("x"), None);
    }

    use super::*;

    fn have(cmd: &str) -> bool {
        Command::new("which")
            .arg(cmd)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A zip laid out like the real archive of `flavor`, made with the `zip` tool (`-y` keeps
    /// symlinks), plus a symlink `link` → `target` next to the binary when given.
    fn fake_archive(
        dir: &Path,
        platform: &str,
        flavor: Flavor,
        link: Option<(&str, &str)>,
    ) -> PathBuf {
        let src = dir.join("src");
        let bin = src.join(flavor.binary(platform));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        if let Some((name, target)) = link {
            std::os::unix::fs::symlink(target, bin.parent().unwrap().join(name)).unwrap();
        }
        let zip = dir.join("a.zip");
        let st = Command::new("zip")
            .current_dir(&src)
            .args(["-q", "-r", "-y"])
            .arg(&zip)
            .arg(".")
            .status()
            .unwrap();
        assert!(st.success());
        zip
    }

    #[test]
    fn plan_defaults_and_validation() {
        let Some(p) = platform() else { return };
        let t = tempfile::tempdir().unwrap();
        let pl = plan(t.path(), None, None, None, Some(Flavor::HeadlessShell)).unwrap();
        assert_eq!(pl.version, PINNED_VERSION);
        assert_eq!(pl.reason, "--headless-shell");
        assert!(
            pl.url
                .starts_with("https://storage.googleapis.com/chrome-for-testing-public/")
        );
        assert!(pl.url.ends_with(&format!("/chrome-headless-shell-{p}.zip")));
        assert!(pl.binary.starts_with(t.path()));
        assert!(plan(t.path(), None, Some("http://x/y.zip"), None, None).is_err());
        assert!(plan(t.path(), None, None, Some("abc"), None).is_err());
        assert!(plan(t.path(), Some("1;rm"), None, None, None).is_err());
        // The full browser: its own archive, directory and binary.
        let full = plan(t.path(), None, None, None, Some(Flavor::Full)).unwrap();
        assert!(
            full.url.ends_with(&format!("/chrome-{p}.zip")),
            "{}",
            full.url
        );
        assert!(full.dir.ends_with(format!("chrome-{PINNED_VERSION}-{p}")));
        assert!(full.binary.ends_with(Flavor::Full.binary(p)));
        assert_eq!(full.to_json()["windows"], true);
        assert_eq!(pl.to_json()["windows"], false);
        // No flavor: the machine's default, with a reason.
        let auto = plan(t.path(), None, None, None, None).unwrap();
        assert_eq!(auto.flavor, default_flavor().0);
        assert!(!auto.reason.is_empty());
        // The pin has a recorded checksum for both builds on every supported platform.
        for pl in [&pl, &full] {
            assert!(pl.sha256.as_deref().is_some_and(valid_sha), "{pl:?}");
        }
        // Without a known checksum (an unpinned version) nothing is fetched.
        let unpinned = plan(t.path(), Some("1.2.3"), None, None, None).unwrap();
        assert_eq!(unpinned.sha256, None);
        let fetched = std::cell::Cell::new(false);
        let e = install(&unpinned, &|_, _| {
            fetched.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{e:#}").contains("--sha256"));
        assert!(!fetched.get());
    }

    #[test]
    fn checksum_mismatch_installs_nothing() {
        let Some(p) = platform() else { return };
        if !have("zip") || !have("unzip") {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let zip = fake_archive(t.path(), p, Flavor::HeadlessShell, None);
        let root = t.path().join("browsers");
        let pl = plan(
            &root,
            None,
            None,
            Some(&"0".repeat(64)),
            Some(Flavor::HeadlessShell),
        )
        .unwrap();
        let e = install(&pl, &|_, dest| {
            std::fs::copy(&zip, dest)?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{e:#}").contains("checksum mismatch"));
        assert!(!pl.dir.exists());
        assert_eq!(installed(&root), None);
        // No leftovers.
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
    }

    #[test]
    fn verified_archive_is_installed_and_discovered() {
        let Some(p) = platform() else { return };
        if !have("zip") || !have("unzip") {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let zip = fake_archive(t.path(), p, Flavor::HeadlessShell, None);
        let sha = sha256_file(&zip).unwrap();
        let root = t.path().join("browsers");
        let pl = plan(
            &root,
            None,
            Some("https://mirror.example/x.zip"),
            Some(&sha),
            Some(Flavor::HeadlessShell),
        )
        .unwrap();
        let urls = std::cell::RefCell::new(Vec::new());
        let bin = install(&pl, &|url, dest| {
            urls.borrow_mut().push(url.to_string());
            std::fs::copy(&zip, dest)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(urls.borrow().as_slice(), ["https://mirror.example/x.zip"]);
        assert!(bin.is_file());
        assert_eq!(installed(&root), Some(bin.clone()));
        // The headless shell cannot open a window.
        assert_eq!(installed_full(&root), None);
        let found = crate::headless::discover(None, &root);
        // `$VIBEKE_HEADLESS_BROWSER` (if a developer set it) takes precedence.
        if std::env::var_os("VIBEKE_HEADLESS_BROWSER").is_none() {
            assert_eq!(found.unwrap().kind, "installed");
        }
        // Reinstall is a no-op.
        install(&pl, &|_, _| bail!("must not download again")).unwrap();
    }

    #[test]
    fn full_browser_with_internal_symlinks_installs() {
        let Some(p) = platform() else { return };
        if !have("zip") || !have("unzip") {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        // Like the framework's `Versions/Current` link: relative, inside the bundle.
        let zip = fake_archive(t.path(), p, Flavor::Full, Some(("Current", "../MacOS")));
        let sha = sha256_file(&zip).unwrap();
        let root = t.path().join("browsers");
        let pl = plan(&root, None, None, Some(&sha), Some(Flavor::Full)).unwrap();
        let bin = install(&pl, &|_, dest| {
            std::fs::copy(&zip, dest)?;
            Ok(())
        })
        .unwrap();
        assert!(bin.is_file());
        assert_eq!(installed_full(&root), Some(bin.clone()));
        // Without a headless shell, headless use falls back to the full browser.
        assert_eq!(installed(&root), Some(bin));
    }

    #[test]
    fn symlink_out_of_the_install_is_refused() {
        let Some(p) = platform() else { return };
        if !have("zip") || !have("unzip") {
            return;
        }
        for target in ["/etc", "../../../../../../outside"] {
            let t = tempfile::tempdir().unwrap();
            let zip = fake_archive(t.path(), p, Flavor::Full, Some(("evil", target)));
            let sha = sha256_file(&zip).unwrap();
            let root = t.path().join("browsers");
            let pl = plan(&root, None, None, Some(&sha), Some(Flavor::Full)).unwrap();
            let e = install(&pl, &|_, dest| {
                std::fs::copy(&zip, dest)?;
                Ok(())
            })
            .unwrap_err();
            assert!(
                format!("{e:#}").contains("points outside"),
                "{target}: {e:#}"
            );
            assert!(!pl.dir.exists());
            assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        }
    }
}
