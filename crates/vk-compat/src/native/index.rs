//! The plugin marketplace index (07 §7.6, post-1.0 by spec; design kept): a static JSON file
//! built daily from repositories tagged `vibeke-plugin` (and, read-only, `herdr-plugin` repos
//! that pass the compat checker), published at `plugins.vibeke.dev/index.json`. `vibeke plugin
//! search <q>` reads it; nothing here installs or executes anything (09 §6: the index is
//! metadata only).
//!
//! The hosted index does not exist yet, so the URL is configurable (`[plugins] index_url`) and
//! `file://` / plain paths work for local mirrors and tests.

use serde::{Deserialize, Serialize};

use super::caps::Capabilities;

/// Where the index is published once hosting exists.
pub const DEFAULT_URL: &str = "https://plugins.vibeke.dev/index.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    /// `owner/repo[/subdir]`.
    pub repo: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// `vibeke` (native) or `herdr` (read-only listing of compatible Herdr plugins).
    #[serde(default = "native_kind")]
    pub kind: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub stars: u64,
}

fn native_kind() -> String {
    "vibeke".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Index {
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub plugins: Vec<IndexEntry>,
}

impl Index {
    pub fn parse(text: &str) -> Result<Index, String> {
        serde_json::from_str(text).map_err(|e| format!("plugin index: {e}"))
    }

    /// Entries matching every word of `q` (id, name, description, repo; case-insensitive),
    /// best first: more fields hit, then stars, then id.
    pub fn search(&self, q: &str) -> Vec<&IndexEntry> {
        let words: Vec<String> = q
            .split_whitespace()
            .map(|w| w.to_ascii_lowercase())
            .collect();
        let mut hits: Vec<(usize, &IndexEntry)> = self
            .plugins
            .iter()
            .filter_map(|e| {
                let fields = [
                    e.id.to_ascii_lowercase(),
                    e.name.clone().unwrap_or_default().to_ascii_lowercase(),
                    e.description
                        .clone()
                        .unwrap_or_default()
                        .to_ascii_lowercase(),
                    e.repo.to_ascii_lowercase(),
                ];
                let mut score = 0;
                for w in &words {
                    let n = fields.iter().filter(|f| f.contains(w.as_str())).count();
                    if n == 0 {
                        return None;
                    }
                    score += n;
                }
                Some((score, e))
            })
            .collect();
        hits.sort_by(|(sa, a), (sb, b)| {
            sb.cmp(sa).then(b.stars.cmp(&a.stars)).then(a.id.cmp(&b.id))
        });
        hits.into_iter().map(|(_, e)| e).collect()
    }
}

/// Read the index from `url`: `file://…` or a plain path is read directly; `https://…` is
/// fetched with `curl` (no shell, 30 s, https only) when it is installed.
pub fn fetch(url: &str) -> Result<Index, String> {
    let text = if let Some(p) = url.strip_prefix("file://") {
        std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?
    } else if url.starts_with('/') || url.starts_with('.') {
        std::fs::read_to_string(url).map_err(|e| format!("{url}: {e}"))?
    } else if url.starts_with("https://") {
        let out = std::process::Command::new("curl")
            .args(["-fsSL", "--proto", "=https", "--max-time", "30", url])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("curl: {e} (needed to read {url})"))?;
        if !out.status.success() {
            return Err(format!(
                "{url}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        return Err(format!(
            "{url}: only https://, file:// and paths are supported"
        ));
    };
    Index::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_ranks_matches() {
        let idx = Index::parse(
            r#"{"plugins": [
              {"id": "acme.ci", "repo": "acme/vibeke-ci", "description": "CI status in the sidebar", "stars": 3},
              {"id": "zed.ci-lite", "repo": "zed/ci", "name": "CI lite", "stars": 10},
              {"id": "x.notes", "repo": "x/notes", "kind": "herdr"}
            ]}"#,
        )
        .unwrap();
        let ids = |q: &str| {
            idx.search(q)
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids("ci"),
            vec!["zed.ci-lite", "acme.ci"],
            "same score: stars decide"
        );
        assert_eq!(ids("ci sidebar"), vec!["acme.ci"]);
        assert_eq!(ids("notes"), vec!["x.notes"]);
        assert!(ids("nothing").is_empty());
        assert_eq!(idx.plugins[0].kind, "vibeke");
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("index.json");
        std::fs::write(&f, r#"{"plugins": []}"#).unwrap();
        assert!(
            fetch(&format!("file://{}", f.display()))
                .unwrap()
                .plugins
                .is_empty()
        );
        assert!(fetch("ftp://x").is_err());
    }
}
