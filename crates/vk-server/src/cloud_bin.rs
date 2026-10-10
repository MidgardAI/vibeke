//! The Linux `vibeke` binary for a cloud box (spec 17): the release asset of the running
//! version, downloaded once and verified exactly like the SSH bootstrap's `remote-download`
//! mode (`vk_remote::bootstrap`): the signed `manifest.json` must verify against the embedded
//! release keys and name this version, and the binary must match the manifest's sha256.
//!
//! The verified file is cached at `~/.cache/vibeke/releases/<version>/vibeke-linux-<arch>`,
//! where `sandbox::container::linux_vibeke` also looks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_remote::{bootstrap, download};

use crate::Server;

/// Architectures with a Linux release asset.
const ARCHES: &[&str] = &["x86_64", "aarch64"];

/// Release target name for `arch` (`linux-x86_64`), or `Unsupported`.
fn target(arch: &str) -> Result<String, RpcError> {
    if ARCHES.contains(&arch) {
        Ok(format!("linux-{arch}"))
    } else {
        Err(RpcError::new(
            ErrorKind::Unsupported,
            format!(
                "no vibeke release for Linux {arch:?}; cloud boxes must run {}",
                ARCHES.join(" or ")
            ),
        ))
    }
}

/// Release asset name for a target (`vibeke-linux-x86_64`).
fn asset_name(target: &str) -> String {
    format!("vibeke-{target}")
}

/// Where the verified binary is cached.
fn cache_path(home: &Path, version: &str, asset: &str) -> PathBuf {
    home.join(".cache/vibeke/releases")
        .join(version)
        .join(asset)
}

/// URL of a file in the release whose download base is `base`.
fn asset_url(base: &str, name: &str) -> String {
    format!("{}/{name}", base.trim_end_matches('/'))
}

/// The verified Linux `vibeke` of this version for `arch` (`x86_64` or `aarch64`): the cached
/// copy, else the release asset, downloaded, verified and cached (mode 0755, written
/// atomically).
pub async fn fetch_linux_vibeke(server: &Arc<Server>, arch: &str) -> Result<PathBuf, RpcError> {
    let target = target(arch)?;
    let home = server.sandbox.home();
    let version = vk_proto::VERSION.to_string();
    tokio::task::spawn_blocking(move || fetch_blocking(&home, &version, &target))
        .await
        .map_err(|e| {
            RpcError::new(
                ErrorKind::Internal,
                format!("vibeke download task failed: {e}"),
            )
        })?
}

fn fetch_blocking(home: &Path, version: &str, target: &str) -> Result<PathBuf, RpcError> {
    let name = asset_name(target);
    let dest = cache_path(home, version, &name);
    if dest.is_file() {
        return Ok(dest);
    }
    let base = download::release_base_url(version);
    let token = download::github_token();
    let api = download::github_api_base();
    let unavailable = |what: &str, e: anyhow::Error| {
        RpcError::new(
            ErrorKind::RemoteUnavailable,
            format!(
                "can't download {what} of vibeke {version} from {base}: {e:#}. For a development \
                 build, set VIBEKE_ARTIFACT_DIR to a directory that holds {name}"
            ),
        )
    };
    let untrusted = |m: String| {
        RpcError::new(
            ErrorKind::Untrusted,
            format!("the vibeke {version} release from {base} is not trusted: {m}"),
        )
    };

    // The signed manifest: the same check as `bootstrap = "remote-download"`.
    let manifest = download::fetch(&asset_url(&base, "manifest.json"), token.as_ref(), &api)
        .map_err(|e| unavailable("manifest.json", e))?;
    let sig = download::fetch(
        &asset_url(&base, "manifest.json.minisig"),
        token.as_ref(),
        &api,
    )
    .map_err(|e| {
        untrusted(format!(
            "no manifest.json.minisig ({e:#}); expected a signature by {}",
            bootstrap::expected_keys_hint()
        ))
    })?;
    let m =
        bootstrap::verify_manifest(&manifest, &String::from_utf8_lossy(&sig)).map_err(untrusted)?;
    if m.version != version {
        return Err(untrusted(format!(
            "the signed manifest is for vibeke {}",
            m.version
        )));
    }
    let entry = m.artifact(target).ok_or_else(|| {
        RpcError::new(
            ErrorKind::Unsupported,
            format!("the vibeke {version} release has no {target} binary"),
        )
    })?;

    let data = download::fetch(&asset_url(&base, &name), token.as_ref(), &api)
        .map_err(|e| unavailable(&name, e))?;
    let actual = format!("{:x}", Sha256::digest(&data));
    if !actual.eq_ignore_ascii_case(&entry.sha256) {
        return Err(untrusted(format!(
            "checksum mismatch for {name}: the signed manifest says {}, the download is {actual}",
            entry.sha256
        )));
    }
    install(&dest, &data).map_err(|e| {
        RpcError::new(
            ErrorKind::Internal,
            format!("can't cache {name} at {}: {e}", dest.display()),
        )
    })?;
    tracing::info!(path = %dest.display(), "cached the verified vibeke release binary");
    Ok(dest)
}

/// Write `data` to `dest` (mode 0755) through a temporary file in the same directory and a
/// rename, so a concurrent reader never sees a partial binary.
fn install(dest: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = dest
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".vibeke-download-")
        .tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_validation() {
        assert_eq!(target("x86_64").unwrap(), "linux-x86_64");
        assert_eq!(target("aarch64").unwrap(), "linux-aarch64");
        for bad in ["arm64", "amd64", "riscv64", "", "../x86_64"] {
            let e = target(bad).unwrap_err();
            assert!(e.kind_is(ErrorKind::Unsupported), "{bad}");
        }
    }

    #[test]
    fn urls_and_paths() {
        let base = format!("{}/releases/download/v{}", download::RELEASE_REPO, "0.9.0");
        assert_eq!(
            asset_url(&base, &asset_name("linux-aarch64")),
            "https://github.com/MidgardAI/vibeke/releases/download/v0.9.0/vibeke-linux-aarch64"
        );
        assert_eq!(
            asset_url("http://127.0.0.1:9/rel/", "manifest.json"),
            "http://127.0.0.1:9/rel/manifest.json"
        );
        assert_eq!(
            cache_path(Path::new("/h"), "0.9.0", "vibeke-linux-x86_64"),
            PathBuf::from("/h/.cache/vibeke/releases/0.9.0/vibeke-linux-x86_64")
        );
    }

    #[test]
    fn install_is_atomic_and_executable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("a/b/vibeke-linux-x86_64");
        install(&dest, b"one").unwrap();
        install(&dest, b"two").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"two");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let left: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn a_cached_binary_is_returned_without_a_download() {
        let d = tempfile::tempdir().unwrap();
        let dest = cache_path(d.path(), "0.0.0-test", "vibeke-linux-x86_64");
        install(&dest, b"cached").unwrap();
        assert_eq!(
            fetch_blocking(d.path(), "0.0.0-test", "linux-x86_64").unwrap(),
            dest
        );
    }
}
