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
            let _ = writeln!(std::io::stdout().lock(), "{data}");
        } else {
            let _ = writeln!(std::io::stdout().lock(), "{message}");
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
            if progress.is_some() { "3600" } else { "30" },
            "--speed-limit",
            "1024",
            "--speed-time",
            "60",
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

// Private transport seam for hermetic signed-release tests. Production always uses the
// fixed public endpoints and embedded keys; there is no environment or CLI trust override.
trait Source {
    async fn fetch(
        &self,
        url: &str,
        dest: &Path,
        limit: u64,
        progress: Option<&Reporter>,
    ) -> Result<()>;
    fn keys(&self) -> Vec<String> {
        bootstrap::trusted_keys()
    }
}
struct PublicRelease;
impl Source for PublicRelease {
    async fn fetch(
        &self,
        url: &str,
        dest: &Path,
        limit: u64,
        progress: Option<&Reporter>,
    ) -> Result<()> {
        fetch(url, dest, limit, progress).await
    }
}

fn verify_manifest(
    keys: &[String],
    release: &Release,
    data: &[u8],
    sig: &str,
) -> Result<bootstrap::ManifestArtifact> {
    let m = bootstrap::verify_manifest_with(keys, data, sig).map_err(anyhow::Error::msg)?;
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
    online_with(g, layout, o, r, &PublicRelease).await
}

async fn online_with(
    g: &Global,
    layout: &crate::doctor::Layout,
    o: &Options,
    r: &Reporter,
    source: &impl Source,
) -> Result<()> {
    r.emit("checking", "Checking for updates…", json!({}));
    let cache = layout.data.join("update-downloads");
    std::fs::create_dir_all(&cache)?;
    let temp = tempfile::tempdir_in(cache)?;
    let api = match &o.version {
        Some(v) => format!("{API}/tags/v{v}"),
        None => format!("{API}/latest"),
    };
    let meta = temp.path().join("release.json");
    source.fetch(&api, &meta, META_LIMIT, None).await?;
    let release = Release::parse(
        &serde_json::from_slice(&std::fs::read(meta)?)?,
        o.version.as_deref(),
    )?;
    let manifest = temp.path().join("manifest.json");
    let sig = temp.path().join("manifest.json.minisig");
    let manifest_url = format!("{}/manifest.json", release.base);
    let signature_url = format!("{}/manifest.json.minisig", release.base);
    tokio::try_join!(
        source.fetch(&manifest_url, &manifest, META_LIMIT, None),
        source.fetch(&signature_url, &sig, 4096, None)
    )?;
    let artifact = verify_manifest(
        &source.keys(),
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
    refuse_pane_scope(g).await?;
    r.emit(
        "downloading",
        &format!("Downloading v{}…", release.version),
        detail.clone(),
    );
    let binary = temp.path().join("vibeke");
    source
        .fetch(&artifact.url, &binary, BINARY_LIMIT, Some(r))
        .await?;
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
    // Refuse a live-but-unreachable session before changing the installation. A dead
    // socket left by a crashed process is equivalent to no running session.
    connect_running(g).await?;
    if &candidate < newest_local {
        ensure_schema_compatible(g, &binary).await?;
    }
    let dest = crate::doctor::install_version(layout, &release.version, &binary)?;
    let previous = crate::doctor::switch_current(layout, &release.version)?;
    match restart(g, &dest).await {
        Ok((before, after)) => {
            crate::doctor::prune_unused_versions(layout).await;
            if let Some(warning) = pane_warning(before, after) {
                r.emit("warning", &warning, json!({}));
            }
            r.emit("installed", &format!("Installed v{}. {}", release.version, if before.is_some() { "Local session restarted; other sessions update on their next restart." } else { "The next session will use it." }),
                json!({"version": release.version, "binary": dest, "server_restarted": before.is_some(), "session": g.session}));
            Ok(())
        }
        Err(e) => {
            if let Some(prev) =
                recover_exec_failure(layout, previous.as_deref(), &release.version, &e)?
            {
                bail!(
                    "restart exec failed: {e}; restored installation v{prev}; the existing server is still running"
                );
            }
            if e.downcast_ref::<ExecFailure>().is_some() {
                bail!(
                    "installed v{} but its server exec failed: {e}. The existing server is still running; no previous installation was available to restore",
                    release.version
                );
            }
            // No old-image exec here: a timeout can mean the replacement has already
            // migrated state.db and is still starting. Downgrading could corrupt it.
            bail!(
                "installed v{}; could not confirm the session restart: {e:#}. The new server may still be starting. The installation remains v{}; check `vibeke server status` before taking further action",
                release.version,
                release.version
            );
        }
    }
}

fn recover_exec_failure(
    layout: &crate::doctor::Layout,
    previous: Option<&str>,
    version: &str,
    error: &anyhow::Error,
) -> Result<Option<String>> {
    if error.downcast_ref::<ExecFailure>().is_some()
        && let Some(prev) = previous.filter(|v| *v != version)
    {
        crate::doctor::switch_current(layout, prev)
            .context("restore previous installation after exec failure")?;
        return Ok(Some(prev.into()));
    }
    Ok(None)
}

/// A downgrade must not strand this or another session on an unreadable database.
/// Older binaries without the read-only probe are conservatively refused when state exists.
pub(crate) async fn ensure_schema_compatible(g: &Global, candidate: &Path) -> Result<()> {
    let mut paths = vec![vk_server::paths::Paths::new(&g.session).db()];
    if let Ok(entries) = std::fs::read_dir(vk_server::paths::state_root()) {
        paths.extend(
            entries
                .flatten()
                .map(|e| e.path().join("state.db"))
                .filter(|p| p.is_file()),
        );
    }
    let mut schema = None;
    for path in paths {
        if let Some(v) = vk_store::database_schema_version(&path)? {
            schema = Some(schema.map_or(v, |old: u64| old.max(v)));
        }
    }
    check_candidate_schema(candidate, schema).await
}

async fn check_candidate_schema(candidate: &Path, schema: Option<u64>) -> Result<()> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(candidate)
            .arg("--internal-schema-version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let supported = output
        .status
        .success()
        .then(|| {
            std::str::from_utf8(&output.stdout)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .flatten();
    let supported = supported.context("refusing downgrade: the target cannot report database compatibility; keep the current installation or restore a compatible backup offline first")?;
    anyhow::ensure!(
        supported >= schema,
        "refusing downgrade: database schema {schema} is newer than the target supports ({supported}); the installation and running session are unchanged. Restore a compatible backup offline before downgrading"
    );
    Ok(())
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

#[derive(Debug)]
struct ExecFailure(String);
impl std::fmt::Display for ExecFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ExecFailure {}

fn pane_warning(before: Counts, after: Counts) -> Option<String> {
    match (before, after) {
        (Some(b), Some(a)) if a.0 < b.0 => Some(format!(
            "Server restarted; pane count changed from {} to {}. A pane may have exited during the restart.",
            b.0, a.0
        )),
        _ => None,
    }
}

/// A pane-scoped caller may not restart the server (`server.restart` is pane-forbidden), so
/// it must not change the installation either: refuse before anything is downloaded.
async fn refuse_pane_scope(g: &Global) -> Result<()> {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let Ok(stream) = client::connect(&socket).await else {
        return Ok(());
    };
    let mut c = Client::new(stream);
    if let Ok(h) = c.hello("cli").await
        && is_pane_scope(&h)
    {
        bail!(
            "this shell runs inside a Vibeke pane, which may not restart the session; run `vibeke update` from a terminal outside Vibeke, use the update action in the TUI, or approve `vibeke auth elevate` first. Nothing was installed"
        );
    }
    Ok(())
}

fn is_pane_scope(hello: &Value) -> bool {
    hello["capabilities"] == json!(["pane"])
}

async fn connect_running(g: &Global) -> Result<Option<Client<tokio::net::UnixStream>>> {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client::connect(&socket).await {
                Ok(stream) => {
                    let mut c = Client::new(stream);
                    c.hello("cli").await?;
                    return Ok(Some(c));
                }
                Err(e) => {
                    let absent = e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        )
                    });
                    if !absent {
                        return Err(e).context("connect to the local session");
                    }
                    if !client::server_alive(&g.session) {
                        return Ok(None);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("local server is alive but not accepting connections; try again when it is ready")?
}

/// Explicit binary, same socket/session, fresh boot id AND the requested version. No stop/spawn
/// fallback: that loses custom server options and can interrupt a client launched in the TUI.
pub(crate) async fn restart(g: &Global, bin: &Path) -> Result<(Counts, Counts)> {
    async fn go(g: &Global, bin: &Path) -> Result<(Counts, Counts)> {
        let socket = client::socket_path(&g.session, g.socket.as_deref());
        let Some(mut c) = connect_running(g).await? else {
            return Ok((None, None));
        };
        let want = binary_version(bin).await?;
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
                    if st["boot_id"] == before["boot_id"]
                        && let Some(e) = st["restart_error"].as_str()
                    {
                        return Err(ExecFailure(e.into()).into());
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
        Ok(r) => r,
        Err(_) => bail!("server restart timed out"),
    }
}

/// Whether a server reporting `server` should be restarted onto a CLI of version `cli`: only
/// when it is strictly older. Unparsable versions never trigger a restart.
pub(crate) fn server_outdated(server: &str, cli: &str) -> bool {
    let parse = |v: &str| Version::parse(v.trim().trim_start_matches('v')).ok();
    matches!((parse(server), parse(cli)), (Some(s), Some(c)) if s < c)
}

/// Before the TUI attaches to the local session: a server left running by an older
/// installation (the installer only swaps links) is restarted onto this CLI's binary, so the
/// session and its gateway run the installed version. Best effort: on failure the old server
/// is attached as before. `VIBEKE_NO_AUTO_RESTART=1` opts out.
pub(crate) async fn restart_if_outdated(g: &Global) {
    if std::env::var("VIBEKE_NO_AUTO_RESTART").as_deref() == Ok("1") {
        return;
    }
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let running = tokio::time::timeout(Duration::from_secs(2), async {
        let mut c = Client::new(client::connect(&socket).await.ok()?);
        // A pane-scoped caller may not restart the server.
        if is_pane_scope(&c.hello("cli").await.ok()?) {
            return None;
        }
        let status = c.call("server.status", json!({})).await.ok()?;
        status["version"].as_str().map(str::to_owned)
    })
    .await
    .ok()
    .flatten();
    let Some(running) = running.filter(|v| server_outdated(v, vk_proto::VERSION)) else {
        return;
    };
    let Some(bin) = crate::commands::current_bin() else {
        return;
    };
    eprintln!(
        "Restarting the Vibeke server (v{running} → v{})…",
        vk_proto::VERSION
    );
    let r = async {
        ensure_schema_compatible(g, &bin).await?;
        restart(g, &bin).await
    }
    .await;
    match r {
        Ok((before, after)) => {
            if let Some(warning) = pane_warning(before, after) {
                eprintln!("{warning}");
            }
        }
        Err(e) => eprintln!(
            "could not restart the server: {e:#}; attaching to v{running}. Run `vibeke server restart` to retry"
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_an_older_server_is_restarted() {
        use super::server_outdated;
        assert!(server_outdated("0.2.0", "0.3.0"));
        assert!(server_outdated("v0.2.9", "0.3.0"));
        assert!(server_outdated("0.3.0-rc.1", "0.3.0"));
        assert!(!server_outdated("0.3.0", "0.3.0"));
        assert!(!server_outdated("0.4.0", "0.3.0"));
        assert!(!server_outdated("dev", "0.3.0"));
        assert!(!server_outdated("0.2.0", "dev"));
        assert!(!server_outdated("", "0.3.0"));
    }

    #[test]
    fn pane_scoped_hello_is_detected() {
        assert!(super::is_pane_scope(
            &serde_json::json!({"capabilities": ["pane"]})
        ));
        assert!(!super::is_pane_scope(
            &serde_json::json!({"capabilities": ["*"]})
        ));
        assert!(!super::is_pane_scope(&serde_json::json!({})));
    }

    use super::*;
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|a| a.to_string()).collect()
    }
    fn release() -> Value {
        let base = format!("{REPO}/releases/download/v99.0.0");
        let assets = [
            "manifest.json".to_string(),
            "manifest.json.minisig".into(),
            format!("vibeke-{}", crate::doctor::platform_target()),
        ]
        .iter()
        .map(|n| json!({"name":n,"browser_download_url":format!("{base}/{n}")}))
        .collect::<Vec<_>>();
        json!({"tag_name":"v99.0.0", "draft":false, "prerelease":false, "assets": assets})
    }
    struct Fixture {
        files: std::collections::HashMap<String, Vec<u8>>,
    }
    impl Source for Fixture {
        async fn fetch(
            &self,
            url: &str,
            dest: &Path,
            limit: u64,
            _: Option<&Reporter>,
        ) -> Result<()> {
            let bytes = self.files.get(url).context("unexpected fixture URL")?;
            anyhow::ensure!(bytes.len() as u64 <= limit, "size limit");
            std::fs::write(dest, bytes)?;
            Ok(())
        }
        fn keys(&self) -> Vec<String> {
            vec![vk_remote::minisign::testing::public_key_b64()]
        }
    }
    fn fixture(dir: &Path) -> Fixture {
        let base = format!("{REPO}/releases/download/v99.0.0");
        let binary = b"#!/bin/sh\necho 'vibeke 99.0.0'\n".to_vec();
        let sample = dir.join("sample");
        std::fs::write(&sample, &binary).unwrap();
        let url = format!("{base}/vibeke-{}", crate::doctor::platform_target());
        let manifest = serde_json::to_vec(&json!({"version":"99.0.0", "artifacts":[{
            "target": crate::doctor::platform_target(), "url":url, "sha256":bootstrap::sha256_file(&sample).unwrap()
        }]})).unwrap();
        let sig = vk_remote::minisign::testing::sign(&manifest, "version:99.0.0");
        Fixture {
            files: [
                (
                    format!("{API}/latest"),
                    serde_json::to_vec(&release()).unwrap(),
                ),
                (format!("{base}/manifest.json"), manifest),
                (format!("{base}/manifest.json.minisig"), sig.into_bytes()),
                (url, binary),
            ]
            .into(),
        }
    }
    #[tokio::test]
    async fn signed_online_install_handles_a_stale_socket_and_rejects_tampering() {
        let d = tempfile::tempdir().unwrap();
        let layout = crate::doctor::Layout {
            data: d.path().join("data"),
            bin: d.path().join("bin"),
        };
        let socket = d.path().join("dead.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let g = Global {
            session: format!("update-test-{}", std::process::id()),
            socket: Some(socket),
            ..Default::default()
        };
        let mut f = fixture(d.path());
        let binary_url = format!(
            "{REPO}/releases/download/v99.0.0/vibeke-{}",
            crate::doctor::platform_target()
        );
        let binary = f.files[&binary_url].clone();
        f.files.insert(binary_url.clone(), b"corrupted".to_vec());
        let error = online_with(&g, &layout, &Options::default(), &Reporter(false), &f)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checksum"));
        assert!(layout.current_version().is_none());
        f.files.insert(binary_url, binary);
        online_with(&g, &layout, &Options::default(), &Reporter(false), &f)
            .await
            .unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("99.0.0"));
        assert_eq!(
            binary_version(&layout.bin.join("vibeke")).await.unwrap(),
            "99.0.0"
        );
    }
    #[test]
    fn timeout_keeps_new_image_selected_but_explicit_exec_failure_restores_previous() {
        let d = tempfile::tempdir().unwrap();
        let layout = crate::doctor::Layout {
            data: d.path().join("data"),
            bin: d.path().join("bin"),
        };
        let bin = d.path().join("candidate");
        std::fs::write(&bin, b"test").unwrap();
        for v in ["0.2.0", "0.3.0"] {
            crate::doctor::install_version(&layout, v, &bin).unwrap();
            crate::doctor::switch_current(&layout, v).unwrap();
        }
        assert!(
            recover_exec_failure(
                &layout,
                Some("0.2.0"),
                "0.3.0",
                &anyhow::anyhow!("restart timed out")
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(layout.current_version().as_deref(), Some("0.3.0"));
        assert_eq!(
            recover_exec_failure(
                &layout,
                Some("0.2.0"),
                "0.3.0",
                &ExecFailure("exec failed".into()).into()
            )
            .unwrap()
            .as_deref(),
            Some("0.2.0")
        );
        assert_eq!(layout.current_version().as_deref(), Some("0.2.0"));
        assert!(pane_warning(Some((2, 2)), Some((1, 1))).is_some());
    }
    #[tokio::test]
    async fn downgrade_preflight_refuses_newer_databases_and_legacy_targets() {
        let d = tempfile::tempdir().unwrap();
        let bin = d.path().join("candidate");
        std::fs::write(&bin, "#!/bin/sh\necho 2\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_candidate_schema(&bin, Some(2)).await.is_ok());
        assert!(
            check_candidate_schema(&bin, Some(3))
                .await
                .unwrap_err()
                .to_string()
                .contains("installation and running session are unchanged")
        );
        std::fs::write(&bin, "#!/bin/sh\necho 'unknown option'\nexit 2\n").unwrap();
        assert!(check_candidate_schema(&bin, Some(1)).await.is_err());
        assert!(check_candidate_schema(&bin, None).await.is_ok());
    }
    #[test]
    fn rejects_partial_prerelease_and_mismatched_releases() {
        assert_eq!(Release::parse(&release(), None).unwrap().version, "99.0.0");
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
        assert!(
            verify_manifest(
                &bootstrap::trusted_keys(),
                &r,
                br#"{"version":"0.3.0","artifacts":[]}"#,
                ""
            )
            .is_err()
        );
    }
}
