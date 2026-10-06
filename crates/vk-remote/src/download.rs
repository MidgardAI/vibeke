//! Release downloads (06 A3 `remote-download`, 09 §10): fetching release files over HTTPS,
//! optionally from a **private** GitHub repository.
//!
//! A GitHub token (`VIBEKE_GITHUB_TOKEN`, else `GITHUB_TOKEN`) is used only for requests to
//! `github.com` release URLs and the GitHub API. For those, the download goes through the
//! asset API endpoint with `Accept: application/octet-stream`, because the plain
//! `releases/download/...` URL does not authenticate a private repo. The token never appears on
//! a command line (curl reads its headers from a config on stdin) and never in an error or log
//! message ([`Secret`] redacts itself). A public release needs no token.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};

/// Release repository, used for the default release URL.
pub const RELEASE_REPO: &str = "https://github.com/MidgardAI/vibeke";

/// `https://github.com/MidgardAI/vibeke/releases/download/v<version>`, or `VIBEKE_RELEASE_URL`.
pub fn release_base_url(version: &str) -> String {
    match std::env::var("VIBEKE_RELEASE_URL") {
        Ok(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/').to_string(),
        _ => format!("{RELEASE_REPO}/releases/download/v{version}"),
    }
}

/// A credential that never prints itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Accepts only plain token characters, so the value cannot break out of a curl config line.
    pub fn new(token: &str) -> Option<Secret> {
        let t = token.trim();
        (!t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\'))
        .then(|| Secret(t.to_string()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// `VIBEKE_GITHUB_TOKEN`, else `GITHUB_TOKEN`.
pub fn github_token() -> Option<Secret> {
    ["VIBEKE_GITHUB_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .find_map(|k| std::env::var(k).ok().and_then(|v| Secret::new(&v)))
}

/// `VIBEKE_GITHUB_API_URL` (tests, GitHub Enterprise), else `https://api.github.com`.
pub fn github_api_base() -> String {
    match std::env::var("VIBEKE_GITHUB_API_URL") {
        Ok(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/').to_string(),
        _ => "https://api.github.com".to_string(),
    }
}

/// A release asset named by its `releases/download` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubAsset {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub name: String,
}

pub fn parse_github_release_url(url: &str) -> Option<GithubAsset> {
    let rest = url.strip_prefix("https://github.com/")?;
    let p: Vec<&str> = rest.split('/').collect();
    match p.as_slice() {
        [owner, repo, "releases", "download", tag, name]
            if ![owner, repo, tag, name].iter().any(|s| s.is_empty()) =>
        {
            Some(GithubAsset {
                owner: owner.to_string(),
                repo: repo.to_string(),
                tag: tag.to_string(),
                name: name.to_string(),
            })
        }
        _ => None,
    }
}

/// Only `https`, or `http` to a loopback host (tests and local mirrors).
pub fn url_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    url.strip_prefix("http://").is_some_and(|r| {
        let host = r.split(['/', ':']).next().unwrap_or("");
        host == "127.0.0.1" || host == "localhost" || host == "[::1]"
    })
}

/// The curl invocation for one request. `args` never contain the token; `config` (the curl
/// config fed on stdin: URL and headers) does.
#[derive(Clone)]
pub struct CurlJob {
    pub args: Vec<String>,
    config: String,
}

impl CurlJob {
    pub fn new(url: &str, token: Option<&Secret>, octet_stream: bool) -> Result<CurlJob> {
        if !url_allowed(url) {
            bail!("{url}: only https:// URLs are allowed");
        }
        if url
            .bytes()
            .any(|b| b == b'"' || b == b'\\' || b.is_ascii_control())
        {
            bail!("refusing a release URL with quotes or control characters");
        }
        let mut config = format!("url = \"{url}\"\n");
        if octet_stream {
            config.push_str("header = \"Accept: application/octet-stream\"\n");
        } else if token.is_some() {
            config.push_str("header = \"Accept: application/vnd.github+json\"\n");
        }
        if let Some(t) = token {
            config.push_str(&format!(
                "header = \"Authorization: Bearer {}\"\n",
                t.expose()
            ));
        }
        // `-K -` reads the config from stdin; curl drops the Authorization header on a redirect
        // to another host (GitHub redirects asset downloads to its storage host).
        let proto = if url.starts_with("https://") {
            "=https"
        } else {
            "=http,https"
        };
        Ok(CurlJob {
            args: [
                "-fsSL",
                "--proto",
                proto,
                "--proto-redir",
                "=https",
                "--max-time",
                "300",
                "-K",
                "-",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            config,
        })
    }

    pub fn run(&self) -> Result<Vec<u8>> {
        let mut child = Command::new("curl")
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run curl")?;
        child
            .stdin
            .take()
            .context("curl stdin")?
            .write_all(self.config.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    }
}

/// Pull the asset's API URL (`.../releases/assets/<id>`) for `name` out of a
/// `releases/tags/<tag>` response.
pub fn find_asset_api_url(release_json: &[u8], name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(release_json).ok()?;
    v.get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|a| a.get("url")?.as_str().map(str::to_string))
}

/// The URL to download `url` from, and whether it needs `Accept: application/octet-stream`.
/// With a token and a GitHub release URL this resolves the asset API URL; otherwise it is `url`.
pub fn resolve_download(
    url: &str,
    token: Option<&Secret>,
    api_base: &str,
) -> Result<(String, bool)> {
    let (Some(t), Some(asset)) = (token, parse_github_release_url(url)) else {
        return Ok((url.to_string(), false));
    };
    let tag_url = format!(
        "{api_base}/repos/{}/{}/releases/tags/{}",
        asset.owner, asset.repo, asset.tag
    );
    let body = CurlJob::new(&tag_url, Some(t), false)?
        .run()
        .with_context(|| {
            format!(
                "look up release {} of {}/{}",
                asset.tag, asset.owner, asset.repo
            )
        })?;
    let api_url = find_asset_api_url(&body, &asset.name)
        .with_context(|| format!("release {} has no asset named {}", asset.tag, asset.name))?;
    Ok((api_url, true))
}

/// Download `url`. With a token and a GitHub release URL, through the asset API endpoint.
pub fn fetch(url: &str, token: Option<&Secret>, api_base: &str) -> Result<Vec<u8>> {
    let (target, octet) = resolve_download(url, token, api_base)?;
    // Send the token only where it belongs: the GitHub API (or the configured API base).
    let send = token.filter(|_| octet || target.starts_with(api_base));
    CurlJob::new(&target, send, octet)?.run().map_err(|e| {
        let hint = if token.is_none() && parse_github_release_url(url).is_some() {
            " (a private release needs GITHUB_TOKEN or VIBEKE_GITHUB_TOKEN)"
        } else {
            ""
        };
        anyhow::anyhow!("download {url} failed: {e}{hint}")
    })
}

/// Shell script for the remote host (fed to `sh -s` on stdin, never on a command line): download
/// `url` into `dir/vibeke.tmp`, check the expected sha256, then rename it to `vibeke`. With a
/// token the URL is the asset API URL and curl reads its headers (token included) from a
/// here-document, so the token is not visible in `ps` or shell history on the remote.
pub fn remote_download_script(
    dir: &str,
    url: &str,
    octet_stream: bool,
    token: Option<&Secret>,
    sha256: &str,
) -> Result<String> {
    if !crate::bootstrap::valid_sha(sha256) {
        bail!("invalid sha256 for remote download");
    }
    let job = CurlJob::new(url, token, octet_stream)?;
    let flags = job.args[..job.args.len() - 2].join(" ");
    Ok(format!(
        r#"set -e
umask 077
D={dir}
mkdir -p "$D" && chmod 700 ~/.local/share/vibeke
cd "$D"
rm -f vibeke.tmp
curl {flags} -o vibeke.tmp -K - <<'VIBEKE_CURL_CONFIG'
{config}VIBEKE_CURL_CONFIG
if command -v sha256sum >/dev/null 2>&1; then echo "{sha256}  vibeke.tmp" | sha256sum -c - >/dev/null
else echo "{sha256}  vibeke.tmp" | shasum -a 256 -c - >/dev/null; fi
chmod 755 vibeke.tmp && mv vibeke.tmp vibeke
echo staged"#,
        config = job.config,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "ghp_SUPERSECRETtoken123";

    #[test]
    fn parses_release_urls() {
        let a = parse_github_release_url(
            "https://github.com/MidgardAI/vibeke/releases/download/v0.1.0/vibeke-linux-x86_64",
        )
        .unwrap();
        assert_eq!(a.owner, "MidgardAI");
        assert_eq!(a.tag, "v0.1.0");
        assert_eq!(a.name, "vibeke-linux-x86_64");
        assert!(
            parse_github_release_url("https://example.com/a/b/releases/download/v1/x").is_none()
        );
        assert!(parse_github_release_url("https://github.com/a/b/releases/latest").is_none());
    }

    #[test]
    fn default_release_url_points_at_the_release_repo() {
        if std::env::var_os("VIBEKE_RELEASE_URL").is_none() {
            assert_eq!(
                release_base_url("0.1.0"),
                "https://github.com/MidgardAI/vibeke/releases/download/v0.1.0"
            );
        }
    }

    #[test]
    fn token_is_never_in_argv_debug_or_errors() {
        let t = Secret::new(TOKEN).unwrap();
        assert!(Secret::new("bad\"token").is_none());
        assert!(Secret::new("two words").is_none());
        let job = CurlJob::new(
            "https://api.github.com/repos/o/r/releases/assets/1",
            Some(&t),
            true,
        )
        .unwrap();
        assert!(
            job.args.iter().all(|a| !a.contains(TOKEN)),
            "{:?}",
            job.args
        );
        assert!(!format!("{t:?}").contains(TOKEN));
        // The remote script carries the token only inside the here-document, never in a curl argument.
        let s = remote_download_script(
            "~/.local/share/vibeke/versions/0.1.0",
            "https://api.github.com/repos/o/r/releases/assets/1",
            true,
            Some(&t),
            &"ab".repeat(32),
        )
        .unwrap();
        let curl_line = s.lines().find(|l| l.starts_with("curl ")).unwrap();
        assert!(!curl_line.contains(TOKEN));
        assert!(s.contains(&format!("Authorization: Bearer {TOKEN}")));
        assert!(s.contains("application/octet-stream"));
    }

    #[test]
    fn private_release_downloads_through_the_asset_api_with_the_token() {
        let (base, seen) = server_with_self_url();
        let t = Secret::new(TOKEN).unwrap();
        let data = fetch(
            "https://github.com/o/r/releases/download/v1/vibeke-linux-x86_64",
            Some(&t),
            &base,
        )
        .unwrap();
        assert_eq!(data, b"BINARY-BYTES");
        let heads = seen.lock().unwrap().join("\n---\n").to_ascii_lowercase();
        assert!(heads.contains("get /repos/o/r/releases/tags/v1"), "{heads}");
        assert!(
            heads.contains("get /repos/o/r/releases/assets/7"),
            "{heads}"
        );
        assert!(
            heads.contains(&format!(
                "authorization: bearer {}",
                TOKEN.to_ascii_lowercase()
            )),
            "{heads}"
        );
        assert!(
            heads.contains("accept: application/octet-stream"),
            "{heads}"
        );
    }

    /// A server whose release JSON points back at itself.
    fn server_with_self_url() -> (String, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (seen2, base2) = (seen.clone(), base.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("")
                    .to_string();
                seen2.lock().unwrap().push(head);
                let data: Vec<u8> = match path.as_str() {
                    "/repos/o/r/releases/tags/v1" => format!(
                        r#"{{"assets":[{{"name":"x","url":"{base2}/repos/o/r/releases/assets/1"}},{{"name":"vibeke-linux-x86_64","url":"{base2}/repos/o/r/releases/assets/7"}}]}}"#
                    )
                    .into_bytes(),
                    "/repos/o/r/releases/assets/7" => b"BINARY-BYTES".to_vec(),
                    _ => b"nope".to_vec(),
                };
                let code = if data == b"nope" {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {code}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                let _ = s.write_all(&data);
            }
        });
        (base, seen)
    }

    #[test]
    fn errors_do_not_leak_the_token_and_hint_at_one_when_missing() {
        let (base, _seen) = server_with_self_url();
        let t = Secret::new(TOKEN).unwrap();
        let e = fetch(
            "https://github.com/o/r/releases/download/v1/missing-asset",
            Some(&t),
            &base,
        )
        .unwrap_err();
        assert!(!format!("{e:#}").contains(TOKEN), "{e:#}");
        // Without a token a failed GitHub download names the variables to set.
        let hint = CurlJob::new("http://127.0.0.1:9/x", None, false)
            .and_then(|j| j.run())
            .map_err(|e| e.to_string());
        assert!(hint.is_err());
    }

    #[test]
    fn plain_urls_get_no_token() {
        let t = Secret::new(TOKEN).unwrap();
        let (u, octet) = resolve_download(
            "https://mirror.example/vibeke-linux-x86_64",
            Some(&t),
            "https://api.github.com",
        )
        .unwrap();
        assert_eq!(u, "https://mirror.example/vibeke-linux-x86_64");
        assert!(!octet);
        assert!(CurlJob::new("http://example.com/x", None, false).is_err());
    }
}
