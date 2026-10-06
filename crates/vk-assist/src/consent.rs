//! Per-workspace assistance consent (14 §5.1, §6, §10; 15 §11).
//!
//! Grants are user-owned and live in one 0600 JSON file in the user's state root (shared by
//! all sessions of that user), never in a repository. A grant binds a canonical workspace path
//! to one connection ID *and* that connection's adapter/endpoint fingerprint: changing the
//! adapter or endpoint invalidates it. Grants list the allowed context classes, optionally the
//! allowed operations (empty = all) and the operations allowed to skip preview confirmation.

use crate::config::Resolved;
use crate::{AssistError, Category, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub workspace: String,
    pub connection: String,
    pub fingerprint: String,
    pub adapter: String,
    pub endpoint_host: String,
    /// Allowed operations; empty means every operation.
    #[serde(default)]
    pub operations: Vec<String>,
    pub classes: Vec<String>,
    #[serde(default)]
    pub auto_send: Vec<String>,
    pub granted_at_ms: i64,
    pub granted_by: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    grants: Vec<Grant>,
}

pub fn load(path: &Path) -> Vec<Grant> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<File>(&s).ok())
        .map(|f| f.grants)
        .unwrap_or_default()
}

fn save(path: &Path, grants: &[Grant]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec_pretty(&File {
        version: 1,
        grants: grants.to_vec(),
    })?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Record (or replace) the grant for `(workspace, connection)`.
pub fn grant(path: &Path, g: Grant) -> std::io::Result<Grant> {
    let mut all = load(path);
    all.retain(|x| !(x.workspace == g.workspace && x.connection == g.connection));
    all.push(g.clone());
    save(path, &all)?;
    Ok(g)
}

/// Remove grants for a workspace (one connection or all). Returns the removed grants.
pub fn revoke(
    path: &Path,
    workspace: &str,
    connection: Option<&str>,
) -> std::io::Result<Vec<Grant>> {
    let all = load(path);
    let (gone, keep): (Vec<Grant>, Vec<Grant>) = all
        .into_iter()
        .partition(|g| g.workspace == workspace && connection.is_none_or(|c| g.connection == c));
    if !gone.is_empty() {
        save(path, &keep)?;
    }
    Ok(gone)
}

fn denied(reason: &str, msg: String) -> AssistError {
    AssistError::new(Category::PermissionDenied, format!("{reason}: {msg}"))
}

/// The grant authorizing `op` with `classes` on `workspace` through `r`, checked before any
/// content is retrieved.
pub fn check<'a>(
    grants: &'a [Grant],
    workspace: &str,
    r: &Resolved,
    op: &str,
    classes: &[&str],
) -> Result<&'a Grant> {
    let g = grants
        .iter()
        .find(|g| g.workspace == workspace && g.connection == r.connection_id)
        .ok_or_else(|| {
            denied(
                "consent_required",
                format!(
                    "no assistance consent for workspace {workspace} and connection `{}`; run `vibeke assist consent`",
                    r.connection_id
                ),
            )
        })?;
    if g.fingerprint != r.fingerprint {
        return Err(denied(
            "consent_invalidated",
            format!(
                "connection `{}` changed adapter or endpoint since consent was given",
                r.connection_id
            ),
        ));
    }
    if !g.operations.is_empty() && !g.operations.iter().any(|o| o == op) {
        return Err(denied(
            "operation_not_granted",
            format!("operation {op} is not in this workspace's consent"),
        ));
    }
    for c in classes {
        if !g.classes.iter().any(|x| x == c) {
            return Err(denied(
                "context_class_not_granted",
                format!("context class `{c}` is not in this workspace's consent"),
            ));
        }
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AssistConfig;
    use serde_json::json;

    fn resolved(endpoint: &str) -> Resolved {
        AssistConfig::from_json(json!({
            "connections": {"c": {"adapter": "ollama", "endpoint": endpoint}},
            "profiles": {"interactive": {"connection": "c", "model": "m"}},
        }))
        .unwrap()
        .resolve(None)
        .unwrap()
    }

    fn g(r: &Resolved) -> Grant {
        Grant {
            workspace: "/w".into(),
            connection: "c".into(),
            fingerprint: r.fingerprint.clone(),
            adapter: "ollama".into(),
            endpoint_host: r.endpoint_host(),
            operations: vec![],
            classes: vec!["selected_text".into()],
            auto_send: vec![],
            granted_at_ms: 1,
            granted_by: "test".into(),
        }
    }

    #[test]
    fn grant_check_revoke() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("consent.json");
        let r = resolved("http://127.0.0.1:1");
        assert!(check(&load(&p), "/w", &r, "pane_title", &["selected_text"]).is_err());
        grant(&p, g(&r)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let all = load(&p);
        assert!(check(&all, "/w", &r, "pane_title", &["selected_text"]).is_ok());
        assert!(check(&all, "/other", &r, "pane_title", &["selected_text"]).is_err());
        let e = check(&all, "/w", &r, "briefing", &["structured_state"]).unwrap_err();
        assert!(e.message.starts_with("context_class_not_granted"));
        // Endpoint change invalidates.
        let r2 = resolved("http://127.0.0.1:2");
        let e = check(&all, "/w", &r2, "pane_title", &["selected_text"]).unwrap_err();
        assert!(e.message.starts_with("consent_invalidated"));
        assert_eq!(revoke(&p, "/w", None).unwrap().len(), 1);
        assert!(load(&p).is_empty());
    }
}
