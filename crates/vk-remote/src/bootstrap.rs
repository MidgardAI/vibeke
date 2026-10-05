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

#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub version: String,
}

pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&data)))
}

/// Verify an artifact against its recorded checksum (`<artifact>.sha256`, `sha256sum` format).
/// Signature verification of the checksum file is the release pipeline's job (09 §10); dev
/// builds made on this machine are trusted by their local checksum.
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
    verify(a)?;
    let data = std::fs::read(&a.path)?;
    let dir = format!("~/.local/share/vibeke/versions/{}", a.version);
    let upload = format!(
        "mkdir -p {d} && chmod 700 ~/.local/share/vibeke && cat > {d}/vibeke.tmp",
        d = sh_quote(&dir)
    );
    t.run(&upload, Some(&data)).await.context("upload")?;
    // Re-check on the remote before the atomic switch; previous version stays on mismatch.
    let activate = format!(
        r#"set -e
cd {d}
if command -v sha256sum >/dev/null 2>&1; then echo "{sha}  vibeke.tmp" | sha256sum -c - >/dev/null
else echo "{sha}  vibeke.tmp" | shasum -a 256 -c - >/dev/null; fi
chmod 755 vibeke.tmp && mv vibeke.tmp vibeke
cd ~/.local/share/vibeke && ln -sfn versions/{v} current.tmp && mv -f current.tmp current 2>/dev/null || {{ rm -f current; mv current.tmp current; }}
mkdir -p ~/.local/bin && ln -sfn ~/.local/share/vibeke/current/vibeke ~/.local/bin/vibeke
# keep the last 2 versions
ls -1t versions | tail -n +3 | while read old; do [ "$old" = "{v}" ] || rm -rf "versions/$old"; done
"$HOME/.local/share/vibeke/current/vibeke" --version"#,
        d = sh_quote(&dir),
        sha = a.sha256,
        v = a.version
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
                version: "0.1.0".into()
            })
            .is_ok()
        );
        assert!(
            verify(&Artifact {
                path: f,
                sha256: "00".into(),
                version: "0.1.0".into()
            })
            .is_err()
        );
    }
}
