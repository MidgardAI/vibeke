//! Online updates for the public stable channel. The CLI is also the TUI's worker: `--json`
//! emits one bounded JSON object per progress event. No candidate runs before verification.
use anyhow::{Context, Result, bail};
use semver::Version;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use vk_cli::{
    EXIT_API, EXIT_OK, EXIT_USAGE, Global,
    client::{self, Client},
};
use vk_remote::bootstrap;

const REPO: &str = "https://github.com/MidgardAI/vibeke";
const API: &str = "https://api.github.com/repos/MidgardAI/vibeke/releases";
const META_LIMIT: u64 = 2 * 1024 * 1024;
const BINARY_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Default, Debug)]
struct Options {
    check: bool,
    version: Option<String>,
    force: bool,
    allow_downgrade: bool,
    offline: bool,
}

fn stable_version(s: &str) -> Result<Version> {
    let v = Version::parse(s).context("invalid release version")?;
    if !v.pre.is_empty() || !v.build.is_empty() || v.to_string() != s {
        bail!("updates use stable versions such as 0.2.0");
    }
    Ok(v)
}

fn options(args: &[String]) -> Result<Options> {
    let mut o = Options::default();
    let mut i = args.iter();
    while let Some(a) = i.next() {
        match a.as_str() {
            "--check" => o.check = true,
            "--force" => o.force = true,
            "--allow-downgrade" => o.allow_downgrade = true,
            "--version" => {
                let v = i.next().context("--version needs a stable version")?;
                stable_version(v)?;
                o.version = Some(v.clone());
            }
            "--from" => {
                i.next().context("--from needs a path")?;
                o.offline = true;
            }
            "--rollback" | "--cached" => o.offline = true,
            _ => bail!("unknown update option {a}"),
        }
    }
    if o.offline && o.version.is_some() {
        bail!("--version cannot be combined with --from, --cached or --rollback");
    }
    if args.iter().any(|a| a == "--rollback")
        && (o.check || args.iter().any(|a| a == "--from" || a == "--cached"))
    {
        bail!("--rollback cannot be combined with --check, --from or --cached");
    }
    Ok(o)
}

struct Reporter(bool);
impl Reporter {
    fn emit(&self, state: &str, message: &str, mut data: Value) {
        data["state"] = json!(state);
        data["message"] = json!(message);
        if self.0 {
            println!("{data}");
        } else {
            println!("{message}");
        }
        let _ = std::io::stdout().flush();
    }
}

/// One writer across sessions and TUI instances. Never unlink a flock file: another waiter
/// may already have its inode open. File descriptors are close-on-exec.
fn lock(dir: &Path) -> Result<File> {
    std::fs::create_dir_all(dir)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("update.lock"))?;
    // SAFETY: the file owns this descriptor throughout the lock's lifetime.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another update is running; wait for it to finish");
    }
    Ok(f)
}

pub async fn run(g: &Global, args: &[String]) -> i32 {
    let r = Reporter(g.json == Some(true));
    let o = match options(args) {
        Ok(o) => o,
        Err(e) => {
            r.emit("error", &format!("{e:#}"), json!({}));
            return EXIT_USAGE;
        }
    };
    if g.machine.is_some() {
        r.emit(
            "error",
            "This updates the local host. Use `vibeke ssh <host> --upgrade` for a remote host.",
            json!({}),
        );
        return EXIT_USAGE;
    }
    let layout = crate::doctor::Layout::from_env();
    let _lock = if o.check {
        None
    } else {
        match lock(&layout.data) {
            Ok(f) => Some(f),
            Err(e) => {
                r.emit("error", &e.to_string(), json!({}));
                return EXIT_API;
            }
        }
    };
    if o.offline {
        if r.0 {
            r.emit("error", "--json is supported for online updates; use --pretty with --from, --cached or --rollback", json!({}));
            return EXIT_USAGE;
        }
        let args: Vec<_> = args
            .iter()
            .filter(|a| a.as_str() != "--cached")
            .cloned()
            .collect();
        return crate::doctor::update(g, &args).await;
    }
    match online(g, &layout, &o, &r).await {
        Ok(()) => EXIT_OK,
        Err(e) => {
            r.emit("error", &format!("{e:#}"), json!({}));
            EXIT_API
        }
    }
}

#[derive(Debug)]
struct Release {
    version: String,
    base: String,
}
impl Release {
    fn parse(v: &Value, requested: Option<&str>) -> Result<Self> {
        if v["draft"] != false || v["prerelease"] != false {
            bail!("release is not a published stable release");
        }
        let version = v["tag_name"]
            .as_str()
            .and_then(|s| s.strip_prefix('v'))
            .context("release has no version tag")?;
        stable_version(version)?;
        if requested.is_some_and(|want| want != version) {
            bail!("release tag does not match requested version");
        }
        let base = format!("{REPO}/releases/download/v{version}");
        let assets = v["assets"].as_array().context("release has no assets")?;
        for name in [
            "manifest.json".to_string(),
            "manifest.json.minisig".into(),
            format!("vibeke-{}", crate::doctor::platform_target()),
        ] {
            let url = format!("{base}/{name}");
            if !assets
                .iter()
                .any(|a| a["name"] == name && a["browser_download_url"] == url)
            {
                bail!("release is incomplete or unavailable for this platform: missing {name}");
            }
        }
        Ok(Self {
            version: version.into(),
            base,
        })
    }
}

/// curl is already an installer prerequisite. Both initial and redirected requests require
/// HTTPS, metadata and binaries have size/time bounds, and a killed worker kills its download.
async fn fetch(url: &str, dest: &Path, limit: u64, progress: Option<&Reporter>) -> Result<()> {
    let mut child = tokio::process::Command::new("curl")
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            if progress.is_some() { "300" } else { "30" },
            "--max-filesize",
            &limit.to_string(),
            "--user-agent",
            concat!("vibeke/", env!("CARGO_PKG_VERSION")),
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start curl (required to download updates)")?;
    let stderr = child.stderr.take().context("download stderr")?;
    let errors = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut b = Vec::new();
        let _ = stderr.take(8192).read_to_end(&mut b).await;
        b
    });
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let status = loop {
        tokio::select! {
            s = child.wait() => break s?,
            _ = tick.tick() => {
                if let Some(r) = progress {
                    let n = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                    r.emit("downloading", &format!("Downloading update… {} MiB", n / 1024 / 1024), json!({"downloaded_bytes": n}));
                }
            }
        }
    };
    let errors = errors.await?;
    if !status.success() {
        bail!(
            "download failed: {}",
            String::from_utf8_lossy(&errors).trim()
        );
    }
    if std::fs::metadata(dest)?.len() > limit {
        bail!("download exceeds size limit");
    }
    Ok(())
}

fn verify_manifest(
    release: &Release,
    data: &[u8],
    sig: &str,
) -> Result<bootstrap::ManifestArtifact> {
    let m = bootstrap::verify_manifest(data, sig).map_err(anyhow::Error::msg)?;
    if m.version != release.version {
        bail!("signed manifest version does not match the release tag");
    }
    let target = crate::doctor::platform_target();
    let entry = m
        .artifact(&target)
        .context("signed manifest has no artifact for this platform")?;
    if entry.url != format!("{}/vibeke-{target}", release.base) {
        bail!("signed artifact URL does not belong to this release");
    }
    Ok(entry.clone())
}

async fn online(
    g: &Global,
    layout: &crate::doctor::Layout,
    o: &Options,
    r: &Reporter,
) -> Result<()> {
    r.emit("checking", "Checking for updates…", json!({}));
    let cache = vk_server::paths::data_root().join("update-downloads");
    std::fs::create_dir_all(&cache)?;
    let temp = tempfile::tempdir_in(cache)?;
    let api = match &o.version {
        Some(v) => format!("{API}/tags/v{v}"),
        None => format!("{API}/latest"),
    };
    let meta = temp.path().join("release.json");
    fetch(&api, &meta, META_LIMIT, None).await?;
    let release = Release::parse(
        &serde_json::from_slice(&std::fs::read(meta)?)?,
        o.version.as_deref(),
    )?;
    let manifest = temp.path().join("manifest.json");
    let sig = temp.path().join("manifest.json.minisig");
    let manifest_url = format!("{}/manifest.json", release.base);
    let signature_url = format!("{}/manifest.json.minisig", release.base);
    tokio::try_join!(
        fetch(&manifest_url, &manifest, META_LIMIT, None),
        fetch(&signature_url, &sig, 4096, None)
    )?;
    let artifact = verify_manifest(
        &release,
        &std::fs::read(manifest)?,
        &std::fs::read_to_string(sig)?,
    )?;
    let current = Version::parse(vk_proto::VERSION)?;
    let installed = layout.current_version();
    let installed_v = installed.as_deref().and_then(|v| Version::parse(v).ok());
    // Another session may have installed a newer binary while this client kept running.
    let newest_local = installed_v.as_ref().map_or(&current, |v| v.max(&current));
    let candidate = stable_version(&release.version)?;
    let available = &candidate > newest_local;
    let server = server_version(g).await;
    let detail = json!({"current_version": vk_proto::VERSION, "installed_version": installed, "server_version": server,
        "version": release.version, "release_url": format!("{REPO}/releases/tag/v{}", release.version)});
    if o.check || (!available && o.version.is_none() && !o.force) {
        r.emit(
            if available { "available" } else { "up_to_date" },
            &if available {
                format!("Update available: v{}", release.version)
            } else {
                format!(
                    "Up to date (running {}, installed {}; latest stable {})",
                    vk_proto::VERSION,
                    layout
                        .current_version()
                        .unwrap_or_else(|| vk_proto::VERSION.into()),
                    release.version
                )
            },
            detail,
        );
        return Ok(());
    }
    if &candidate < newest_local && !o.allow_downgrade {
        bail!("refusing to downgrade from {newest_local}; use --allow-downgrade explicitly");
    }
    r.emit(
        "downloading",
        &format!("Downloading v{}…", release.version),
        detail.clone(),
    );
    let binary = temp.path().join("vibeke");
    fetch(&artifact.url, &binary, BINARY_LIMIT, Some(r)).await?;
    r.emit("verifying", "Verifying update…", detail.clone());
    if bootstrap::sha256_file(&binary)? != artifact.sha256 {
        bail!("download checksum does not match the signed manifest; nothing installed");
    }
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
    if binary_version(&binary).await? != release.version {
        bail!("download reports a different version; nothing installed");
    }
    r.emit(
        "installing",
        "Installing update and restarting the local session…",
        detail,
    );
    let dest = crate::doctor::install_version(layout, &release.version, &binary)?;
    let previous = crate::doctor::switch_current(layout, &release.version)?;
    match restart(g, &dest).await {
        Ok((before, after)) => {
            if let (Some(b), Some(a)) = (before, after)
                && a.0 < b.0
            {
                bail!(
                    "new server started but pane count fell from {} to {}; inspect the session before continuing",
                    b.0,
                    a.0
                );
            }
            r.emit("installed", &format!("Installed v{}. {}", release.version, if before.is_some() { "Local session restarted; other sessions update on their next restart." } else { "The next session will use it." }),
                json!({"version": release.version, "binary": dest, "server_restarted": before.is_some(), "session": g.session}));
            Ok(())
        }
        Err(e) => {
            if let Some(prev) = previous {
                crate::doctor::switch_current(layout, &prev)
                    .context("restore previous installation after restart failure")?;
                let recovery = restart(g, &layout.version_bin(&prev)).await;
                bail!(
                    "restart failed: {e}; restored v{prev}; recovery: {}",
                    match recovery {
                        Ok(_) => "previous server available".into(),
                        Err(e) => e,
                    }
                );
            }
            bail!(
                "installed v{} but restart failed: {e}; retry `vibeke server restart --binary {}`",
                release.version,
                dest.display()
            );
        }
    }
}

async fn binary_version(bin: &Path) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        bail!("candidate --version failed");
    }
    let s = std::str::from_utf8(&output.stdout)?
        .trim()
        .strip_prefix("vibeke ")
        .context("candidate has no Vibeke version")?;
    Ok(s.into())
}

async fn server_version(g: &Global) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let socket = client::socket_path(&g.session, g.socket.as_deref());
        let mut c = Client::new(client::connect(&socket).await.ok()?);
        c.hello("cli").await.ok()?;
        let status = c.call("server.status", json!({})).await.ok()?;
        status["version"].as_str().map(str::to_owned)
    })
    .await
    .ok()
    .flatten()
}

type Counts = Option<(u64, u64)>;
/// Explicit binary, same socket/session, fresh boot id AND the requested version. No stop/spawn
/// fallback: that loses custom server options and can interrupt a client launched in the TUI.
pub(crate) async fn restart(
    g: &Global,
    bin: &Path,
) -> std::result::Result<(Counts, Counts), String> {
    async fn go(g: &Global, bin: &Path) -> Result<(Counts, Counts)> {
        let socket = client::socket_path(&g.session, g.socket.as_deref());
        let stream = match client::connect(&socket).await {
            Ok(s) => s,
            Err(_) if !socket.exists() => return Ok((None, None)),
            Err(e) => return Err(e).context("connect to the local session"),
        };
        let want = binary_version(bin).await?;
        let mut c = Client::new(stream);
        c.hello("cli").await?;
        let before = c.call("server.status", json!({})).await?;
        c.call("server.restart", json!({"binary": bin})).await?;
        drop(c);
        let counts = |v: &Value| {
            Some((
                v["panes"].as_u64().unwrap_or(0),
                v["holders"]["live"].as_u64().unwrap_or(0),
            ))
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(s) = client::connect(&socket).await {
                let mut c = Client::new(s);
                if c.hello("cli").await.is_ok()
                    && let Ok(st) = c.call("server.status", json!({})).await
                {
                    if let Some(e) = st["restart_error"].as_str() {
                        bail!("{e}");
                    }
                    if st["boot_id"] != before["boot_id"] && st["version"] == want {
                        return Ok((counts(&before), counts(&st)));
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("server did not report version {want} after restart");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    match tokio::time::timeout(Duration::from_secs(30), go(g, bin)).await {
        Ok(r) => r.map_err(|e| format!("{e:#}")),
        Err(_) => Err("server restart timed out".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|a| a.to_string()).collect()
    }
    fn release() -> Value {
        let base = format!("{REPO}/releases/download/v0.3.0");
        let assets = [
            "manifest.json".to_string(),
            "manifest.json.minisig".into(),
            format!("vibeke-{}", crate::doctor::platform_target()),
        ]
        .iter()
        .map(|n| json!({"name":n,"browser_download_url":format!("{base}/{n}")}))
        .collect::<Vec<_>>();
        json!({"tag_name":"v0.3.0", "draft":false, "prerelease":false, "assets": assets})
    }
    #[test]
    fn rejects_partial_prerelease_and_mismatched_releases() {
        assert_eq!(Release::parse(&release(), None).unwrap().version, "0.3.0");
        for key in ["draft", "prerelease"] {
            let mut v = release();
            v[key] = json!(true);
            assert!(Release::parse(&v, None).is_err());
        }
        let mut v = release();
        v["assets"][0]["browser_download_url"] = json!("https://example.com/manifest.json");
        assert!(Release::parse(&v, None).is_err());
        assert!(Release::parse(&release(), Some("0.4.0")).is_err());
        let mut v = release();
        v["assets"] = json!([]);
        assert!(Release::parse(&v, None).is_err());
    }
    #[test]
    fn strict_version_and_option_validation() {
        for v in ["../x", "0.3.0-rc.1", "0.3.0+dev", "v0.3.0", "01.2.3"] {
            assert!(stable_version(v).is_err());
        }
        assert!(options(&args(&["--check", "--rollback"])).is_err());
        assert!(options(&args(&["--version", "0.3.0", "--from", "x"])).is_err());
        assert!(options(&args(&["--version"])).is_err());
        assert!(
            options(&args(&["--check", "--version", "0.3.0"]))
                .unwrap()
                .check
        );
    }
    #[test]
    fn writers_are_serialized_and_crashed_writers_release_the_lock() {
        let d = tempfile::tempdir().unwrap();
        let a = lock(d.path()).unwrap();
        assert!(lock(d.path()).is_err());
        drop(a);
        assert!(lock(d.path()).is_ok());
    }
    #[test]
    fn unsigned_metadata_is_never_accepted() {
        let r = Release::parse(&release(), None).unwrap();
        assert!(verify_manifest(&r, br#"{"version":"0.3.0","artifacts":[]}"#, "").is_err());
    }
}
