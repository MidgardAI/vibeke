//! `vibeke browser install` (spec 06 B5): a pinned Chrome-for-Testing `chrome-headless-shell`,
//! verified by SHA-256 before anything is unpacked.
//!
//! The command **asks first** (the CLI shows the plan and needs a yes; the API needs
//! `confirm: true`). The download itself is a pluggable [`Fetch`] function (`curl` in
//! production, a fake in tests). A build whose pin has no recorded checksum can only be installed
//! with an explicit `--sha256` from the user: nothing is ever unpacked unverified.
//!
//! Layout: `<root>/chrome-headless-shell-<version>-<platform>/chrome-headless-shell-<platform>/
//! chrome-headless-shell`, written via a staging directory and renamed into place.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pinned Chrome-for-Testing build (the version Stage 0 measured with).
pub const PINNED_VERSION: &str = "153.0.8010.12";

/// Known SHA-256 checksums per (version, platform). Chrome for Testing publishes no checksums;
/// entries are recorded by whoever bumps the pin after verifying a download out of band (these
/// were checked against the MD5 Google Cloud Storage reports for each object, 2026-10-10). A
/// version or platform without one needs an explicit `--sha256`.
pub const PINNED_SHA256: &[(&str, &str, &str)] = &[
    (
        "153.0.8010.12",
        "mac-arm64",
        "89d80a6d26ccd0ccfd51e22d9e1297283862af2b0cd91dce07459b35ca0059f2",
    ),
    (
        "153.0.8010.12",
        "mac-x64",
        "5c2eaa1aad62111bb5a70dd0889dd3093f3142277b8f78957a238257ee85f009",
    ),
    (
        "153.0.8010.12",
        "linux64",
        "a9da028861a0cf789ff25c2fed45f5f1aaf969ed9247835b6a7821a4f7af9d1d",
    ),
];

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

/// Plan an install into `root`. `url`/`sha256` override the pin (e.g. a mirror).
pub fn plan(
    root: &Path,
    version: Option<&str>,
    url: Option<&str>,
    sha256: Option<&str>,
) -> Result<InstallPlan> {
    let platform = platform().context("no Chrome for Testing build for this platform")?;
    let version = version.unwrap_or(PINNED_VERSION).to_string();
    if !version.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        bail!("invalid version {version}");
    }
    let url = url.map(str::to_string).unwrap_or_else(|| {
        format!(
            "https://storage.googleapis.com/chrome-for-testing-public/{version}/{platform}/chrome-headless-shell-{platform}.zip"
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
            .find(|(v, p, _)| *v == version && *p == platform)
            .map(|(_, _, h)| h.to_string()),
    };
    let dir = root.join(format!("chrome-headless-shell-{version}-{platform}"));
    let binary = dir
        .join(format!("chrome-headless-shell-{platform}"))
        .join("chrome-headless-shell");
    Ok(InstallPlan {
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
/// components, or symlinks. `names` is `unzip -Z1` output; `long` is the plain `unzip -Z`
/// listing, whose lines for symlinks start with `l`.
pub fn check_zip_entries(names: &str, long: &str) -> Result<()> {
    for n in names.lines().filter(|l| !l.is_empty()) {
        if n.starts_with('/')
            || n.starts_with('\\')
            || n.split(['/', '\\']).any(|c| c == "..")
            || n.contains('\0')
            || n.as_bytes().get(1) == Some(&b':')
        {
            bail!("archive entry {n:?} escapes the install directory; nothing was installed");
        }
    }
    if long.lines().any(|l| l.starts_with('l')) {
        bail!("archive contains a symlink; nothing was installed");
    }
    Ok(())
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
            "no recorded SHA-256 for chrome-headless-shell {} ({}); pass --sha256 <hex> after verifying the download",
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

/// The newest installed build under `root`, if any.
pub fn installed(root: &Path) -> Option<PathBuf> {
    let platform = platform()?;
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("chrome-headless-shell-")
        })
        .map(|e| {
            e.path()
                .join(format!("chrome-headless-shell-{platform}"))
                .join("chrome-headless-shell")
        })
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v.pop()
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
        assert!(c("a/b\n", "lrwxr-xr-x  3.0 unx  10 bx stor a/link\n").is_err());
    }
    use super::*;

    fn have(cmd: &str) -> bool {
        Command::new("which")
            .arg(cmd)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A zip laid out like the real archive, made with the `zip` tool.
    fn fake_archive(dir: &Path, platform: &str) -> PathBuf {
        let src = dir.join("src");
        let inner = src.join(format!("chrome-headless-shell-{platform}"));
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("chrome-headless-shell"), "#!/bin/sh\n").unwrap();
        let zip = dir.join("a.zip");
        let st = Command::new("zip")
            .current_dir(&src)
            .args(["-q", "-r"])
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
        let pl = plan(t.path(), None, None, None).unwrap();
        assert_eq!(pl.version, PINNED_VERSION);
        assert!(
            pl.url
                .starts_with("https://storage.googleapis.com/chrome-for-testing-public/")
        );
        assert!(pl.url.ends_with(&format!("chrome-headless-shell-{p}.zip")));
        assert!(pl.binary.starts_with(t.path()));
        assert!(plan(t.path(), None, Some("http://x/y.zip"), None).is_err());
        assert!(plan(t.path(), None, None, Some("abc")).is_err());
        assert!(plan(t.path(), Some("1;rm"), None, None).is_err());
        // The pin has a recorded checksum for every supported platform.
        assert!(pl.sha256.as_deref().is_some_and(valid_sha), "{pl:?}");
        // Without a known checksum (an unpinned version) nothing is fetched.
        let unpinned = plan(t.path(), Some("1.2.3"), None, None).unwrap();
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
        let zip = fake_archive(t.path(), p);
        let root = t.path().join("browsers");
        let pl = plan(&root, None, None, Some(&"0".repeat(64))).unwrap();
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
        let zip = fake_archive(t.path(), p);
        let sha = sha256_file(&zip).unwrap();
        let root = t.path().join("browsers");
        let pl = plan(
            &root,
            None,
            Some("https://mirror.example/x.zip"),
            Some(&sha),
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
        let found = crate::headless::discover(None, &root);
        // `$VIBEKE_HEADLESS_BROWSER` (if a developer set it) takes precedence.
        if std::env::var_os("VIBEKE_HEADLESS_BROWSER").is_none() {
            assert_eq!(found.unwrap().kind, "installed");
        }
        // Reinstall is a no-op.
        install(&pl, &|_, _| bail!("must not download again")).unwrap();
    }
}
