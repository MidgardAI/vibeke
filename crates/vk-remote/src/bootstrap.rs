//! No-sudo bootstrap (06 A3): probe the remote, push the matching verified artifact into
//! `~/.local/share/vibeke/versions/<v>/`, re-check its sha256 there, and switch the `current`
//! symlink atomically. Never touches system paths; never uses sudo.

use crate::ssh::{Target, sh_quote};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const REMOTE_BIN: &str = "~/.local/share/vibeke/current/vibeke";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub os: String,
    pub arch: String,
    pub home: String,
    pub version: Option<String>,
    pub libc: String,
}

impl Probe {
    /// Release target name: `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, …
    pub fn target(&self) -> String {
        let os = match self.os.as_str() {
            "Linux" => "linux",
            "Darwin" => "macos",
            o => o,
        };
        let arch = match self.arch.as_str() {
            "arm64" | "aarch64" => "aarch64",
            "x86_64" | "amd64" => "x86_64",
            a => a,
        };
        format!("{os}-{arch}")
    }
}

const PROBE: &str = r#"uname -s; uname -m; echo "$HOME"
B="$HOME/.local/share/vibeke/current/vibeke"
if [ -x "$B" ]; then "$B" --version 2>/dev/null | head -1; else echo none; fi
if ldd --version 2>&1 | grep -qi musl; then echo musl; elif ldd --version >/dev/null 2>&1; then echo glibc; else echo unknown; fi
"#;

pub async fn probe(t: &Target) -> Result<Probe> {
    let out = t.run("sh -s", Some(PROBE.as_bytes())).await?;
    let l: Vec<&str> = out.lines().collect();
    if l.len() < 4 {
        bail!("unexpected probe output from {}: {out:?}", t.address);
    }
    let version = l[3].strip_prefix("vibeke ").map(|v| v.trim().to_string());
    Ok(Probe {
        os: l[0].trim().into(),
        arch: l[1].trim().into(),
        home: l[2].trim().into(),
        version,
        libc: l.get(4).unwrap_or(&"unknown").trim().into(),
    })
}

/// How an artifact earned trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Checksum from `SHA256SUMS`, whose minisign signature verified against embedded keys.
    Signed,
    /// Checksum from a sidecar / `SHA256SUMS` next to the artifact, no verified signature;
    /// accepted only because the user set `VIBEKE_ALLOW_UNSIGNED=1`.
    UnsignedOptIn,
    /// The running binary pushed as-is: no external checksum exists, so the hash only guards
    /// the transfer. Accepted only with `VIBEKE_ALLOW_UNSIGNED=1`.
    SelfHashedOptIn,
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub version: String,
    pub trust: Trust,
}

pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&data)))
}

/// Verify an artifact against its recorded checksum.
pub fn verify(a: &Artifact) -> Result<()> {
    let actual = sha256_file(&a.path)?;
    if actual != a.sha256 {
        bail!(
            "checksum mismatch for {}: expected {}, got {actual}",
            a.path.display(),
            a.sha256
        );
    }
    Ok(())
}

/// Minisign public keys (base64 key lines) trusted to sign `SHA256SUMS`. No release key
/// exists yet, so this is empty and no signature can verify.
pub const TRUSTED_KEYS: &[&str] = &[];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// This build embeds no release public keys, so nothing can be verified.
    NoTrustedKeys,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::NoTrustedKeys => {
                write!(
                    f,
                    "this build embeds no release signing keys (none exist yet)"
                )
            }
        }
    }
}

impl std::error::Error for SignatureError {}

/// Verify the minisign signature `sig` over the checksum file `sums` against `TRUSTED_KEYS`.
/// No Ed25519 implementation is available in the workspace and no release key exists yet,
/// so this always refuses; the opt-in path is the only way to accept artifacts today.
pub fn verify_signature(
    _sums: &std::path::Path,
    _sig: &std::path::Path,
) -> Result<(), SignatureError> {
    Err(SignatureError::NoTrustedKeys)
}

/// `VIBEKE_ALLOW_UNSIGNED=1`: the user explicitly accepts unsigned development builds.
pub fn allow_unsigned_env() -> bool {
    std::env::var("VIBEKE_ALLOW_UNSIGNED").is_ok_and(|v| v == "1")
}

pub fn unsigned_warning(path: &std::path::Path, sha256: &str) -> String {
    format!(
        "WARNING: VIBEKE_ALLOW_UNSIGNED=1: using UNSIGNED development artifact {} (sha256 {sha256}); \
         its integrity is not backed by a verified signature",
        path.display()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SumSource {
    Sums,
    Sidecar,
}

fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Expected sha256 for `artifact`, from `SHA256SUMS` in the same directory, else the
/// `<artifact>.sha256` sidecar. Never derived from the artifact itself.
fn expected_sha256(artifact: &std::path::Path) -> Result<(String, SumSource)> {
    let name = artifact
        .file_name()
        .and_then(|n| n.to_str())
        .context("artifact name")?;
    let dir = artifact.parent().unwrap_or(std::path::Path::new("."));
    if let Ok(text) = std::fs::read_to_string(dir.join("SHA256SUMS")) {
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(h), Some(n)) = (it.next(), it.next())
                && n.trim_start_matches('*') == name
                && valid_sha(h)
            {
                return Ok((h.to_ascii_lowercase(), SumSource::Sums));
            }
        }
    }
    let mut sc = artifact.as_os_str().to_owned();
    sc.push(".sha256");
    if let Ok(text) = std::fs::read_to_string(&sc)
        && let Some(h) = text.split_whitespace().next()
        && valid_sha(h)
    {
        return Ok((h.to_ascii_lowercase(), SumSource::Sidecar));
    }
    bail!(
        "no checksum for {name}: expected an entry in SHA256SUMS or a {name}.sha256 sidecar next to it"
    )
}

/// Decide whether `artifact` may be installed or executed. Returns the verified sha256.
/// (a) the expected checksum must come from a file next to it, and match; and
/// (b) `SHA256SUMS.minisig` must verify, or `allow_unsigned` must be set.
pub fn trust_artifact(artifact: &std::path::Path, allow_unsigned: bool) -> Result<(String, Trust)> {
    let (expected, source) = expected_sha256(artifact)?;
    let actual = sha256_file(artifact)?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            artifact.display()
        );
    }
    let dir = artifact.parent().unwrap_or(std::path::Path::new("."));
    let sig = dir.join("SHA256SUMS.minisig");
    let sig_err = if source == SumSource::Sums && sig.is_file() {
        match verify_signature(&dir.join("SHA256SUMS"), &sig) {
            Ok(()) => return Ok((actual, Trust::Signed)),
            Err(e) => e.to_string(),
        }
    } else if source == SumSource::Sums {
        "no SHA256SUMS.minisig next to it".to_string()
    } else {
        "checksum came from a sidecar, which no signature covers".to_string()
    };
    if allow_unsigned {
        return Ok((actual, Trust::UnsignedOptIn));
    }
    bail!(
        "{} is not signed ({sig_err}); refusing. Set VIBEKE_ALLOW_UNSIGNED=1 only for development builds you made yourself",
        artifact.display()
    )
}

/// Build an [`Artifact`] from a path after [`trust_artifact`].
pub fn load_artifact(path: PathBuf, version: String, allow_unsigned: bool) -> Result<Artifact> {
    let (sha256, trust) = trust_artifact(&path, allow_unsigned)?;
    Ok(Artifact {
        path,
        sha256,
        version,
        trust,
    })
}

/// The running binary as an artifact. Requires the opt-in because there is no independent
/// checksum for it; the hash computed here only protects the transfer.
pub fn load_self_artifact(exe: PathBuf, version: String, allow_unsigned: bool) -> Result<Artifact> {
    if let Ok(a) = load_artifact(exe.clone(), version.clone(), allow_unsigned) {
        return Ok(a);
    }
    if !allow_unsigned {
        bail!("pushing the running binary requires VIBEKE_ALLOW_UNSIGNED=1");
    }
    Ok(Artifact {
        sha256: sha256_file(&exe)?,
        path: exe,
        version,
        trust: Trust::SelfHashedOptIn,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    AlreadyCurrent,
    Installed,
    Upgraded,
}

/// Make sure the remote runs `artifact.version`. `upgrade_ok` gates replacing another version.
pub async fn ensure(
    t: &Target,
    p: &Probe,
    artifact: Option<&Artifact>,
    upgrade_ok: bool,
) -> Result<Outcome> {
    let want = artifact.map(|a| a.version.clone());
    match (&p.version, &want) {
        (Some(have), Some(w)) if have == w => return Ok(Outcome::AlreadyCurrent),
        (Some(_), None) => return Ok(Outcome::AlreadyCurrent),
        (None, None) => bail!(
            "vibeke is not installed on {} and no {} artifact is available locally (build one with `mise run dist`)",
            t.label,
            p.target()
        ),
        (Some(have), Some(w)) if !upgrade_ok => bail!(
            "{} runs vibeke {have}, local is {w}; rerun with --upgrade (panes survive the upgrade)",
            t.label
        ),
        _ => {}
    }
    let a = artifact.context("artifact")?;
    if !valid_sha(&a.sha256) {
        bail!("invalid sha256 for artifact");
    }
    if a.version.is_empty()
        || !a
            .version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        bail!("invalid artifact version {:?}", a.version);
    }
    verify(a)?;
    let data = std::fs::read(&a.path)?;
    let dir = format!("~/.local/share/vibeke/versions/{}", a.version);
    let upload = format!(
        "mkdir -p {d} && chmod 700 ~/.local/share/vibeke && cat > {d}/vibeke.tmp",
        d = sh_quote(&dir)
    );
    t.run(&upload, Some(&data)).await.context("upload")?;
    // Re-check on the remote before the switch; the previous version stays on mismatch.
    // sha and version are validated above (hex / [A-Za-z0-9._+-]) and quoted anyway.
    let activate = format!(
        r#"set -e
V={v}
cd {d}
if command -v sha256sum >/dev/null 2>&1; then echo "{sha}  vibeke.tmp" | sha256sum -c - >/dev/null
else echo "{sha}  vibeke.tmp" | shasum -a 256 -c - >/dev/null; fi
chmod 755 vibeke.tmp && mv vibeke.tmp vibeke
cd ~/.local/share/vibeke
rm -f current.new
ln -s "versions/$V" current.new
# Atomic rename over the symlink where `mv -T` exists (GNU coreutils, modern busybox).
# Fallback (BSD/macOS mv has no -T): `ln -sfn` is NOT atomic, but `current` still only ever
# names a fully written, checksum-verified version directory.
if ! mv -Tf current.new current 2>/dev/null; then
  ln -sfn "versions/$V" current
  rm -f current.new
fi
mkdir -p ~/.local/bin && ln -sfn ~/.local/share/vibeke/current/vibeke ~/.local/bin/vibeke
# keep the last 2 versions
ls -1t versions | tail -n +3 | while read old; do [ "$old" = "$V" ] || rm -rf "versions/$old"; done
"$HOME/.local/share/vibeke/current/vibeke" --version"#,
        d = sh_quote(&dir),
        sha = a.sha256,
        v = sh_quote(&a.version)
    );
    let out = t
        .run("sh -s", Some(activate.as_bytes()))
        .await
        .context("activate")?;
    if !out.contains(&a.version) {
        bail!("activation check failed: {out}");
    }
    Ok(if p.version.is_some() {
        Outcome::Upgraded
    } else {
        Outcome::Installed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names() {
        let p = Probe {
            os: "Linux".into(),
            arch: "aarch64".into(),
            home: "/home/x".into(),
            version: None,
            libc: "glibc".into(),
        };
        assert_eq!(p.target(), "linux-aarch64");
        let p = Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            ..p
        };
        assert_eq!(p.target(), "linux-x86_64");
    }

    #[test]
    fn verify_checksums() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("vibeke");
        std::fs::write(&f, b"binary").unwrap();
        let sha = sha256_file(&f).unwrap();
        assert!(
            verify(&Artifact {
                path: f.clone(),
                sha256: sha,
                version: "0.1.0".into(),
                trust: Trust::UnsignedOptIn
            })
            .is_ok()
        );
        assert!(
            verify(&Artifact {
                path: f,
                sha256: "00".into(),
                version: "0.1.0".into(),
                trust: Trust::UnsignedOptIn
            })
            .is_err()
        );
    }

    fn dist(sidecar: bool, sums: bool, good: bool) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("vibeke-linux-x86_64");
        std::fs::write(&f, b"binary").unwrap();
        let sha = if good {
            sha256_file(&f).unwrap()
        } else {
            "ab".repeat(32)
        };
        if sidecar {
            std::fs::write(
                d.path().join("vibeke-linux-x86_64.sha256"),
                format!("{sha}  x\n"),
            )
            .unwrap();
        }
        if sums {
            std::fs::write(
                d.path().join("SHA256SUMS"),
                format!("{}  other\n{sha}  vibeke-linux-x86_64\n", "cd".repeat(32)),
            )
            .unwrap();
        }
        (d, f)
    }

    #[test]
    fn checksum_never_derived_from_candidate() {
        let (_d, f) = dist(false, false, true);
        assert!(
            trust_artifact(&f, true).is_err(),
            "no sidecar -> no trust even when opted in"
        );
        let (_d, f) = dist(true, false, false);
        assert!(
            trust_artifact(&f, true).is_err(),
            "mismatch refused even when opted in"
        );
    }

    #[test]
    fn unsigned_requires_opt_in() {
        let (_d, f) = dist(true, false, true);
        assert!(trust_artifact(&f, false).is_err());
        assert_eq!(trust_artifact(&f, true).unwrap().1, Trust::UnsignedOptIn);
        let (_d, f) = dist(false, true, true);
        assert!(trust_artifact(&f, false).is_err());
        assert_eq!(trust_artifact(&f, true).unwrap().1, Trust::UnsignedOptIn);
    }

    #[test]
    fn signature_not_verifiable_without_keys() {
        let (d, f) = dist(false, true, true);
        std::fs::write(d.path().join("SHA256SUMS.minisig"), b"junk").unwrap();
        assert!(
            trust_artifact(&f, false).is_err(),
            "a signature file alone is not trust"
        );
        assert_eq!(
            verify_signature(
                &d.path().join("SHA256SUMS"),
                &d.path().join("SHA256SUMS.minisig")
            ),
            Err(SignatureError::NoTrustedKeys)
        );
    }

    #[test]
    fn self_artifact_needs_opt_in() {
        let (_d, f) = dist(false, false, true);
        assert!(load_self_artifact(f.clone(), "0.1.0".into(), false).is_err());
        let a = load_self_artifact(f, "0.1.0".into(), true).unwrap();
        assert_eq!(a.trust, Trust::SelfHashedOptIn);
    }
}
