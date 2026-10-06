//! The environment and argv of a Herdr plugin invocation (07 §7.7 "Commands, context and
//! callbacks").
//!
//! Runtime invocations get the inherited user environment with every stale `HERDR_*` and
//! Vibeke pane/socket credential removed, then the invocation's own values: `HERDR_ENV=1`,
//! `HERDR_SOCKET_PATH` (the invocation's private broker), `HERDR_BIN_PATH` (the private
//! `herdr` launcher, whose directory is prepended to `PATH`), plugin identity/dirs, the context
//! JSON and the per-entrypoint variables. Build steps get no socket, launcher, context or pane
//! identity at all.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Variables removed from the inherited environment before an invocation is populated.
fn stale(k: &str) -> bool {
    k.starts_with("HERDR_")
        || matches!(
            k,
            "VIBEKE_SOCKET"
                | "VIBEKE_PANE_ID"
                | "VIBEKE_PANE_TOKEN"
                | "VIBEKE_PANE_HANDLE"
                | "VIBEKE_PLUGIN_TOKEN"
                | "VIBEKE_HERDR_BROKER"
                | "VIBEKE_HERDR_TOKEN"
        )
}

/// One runtime invocation of a plugin entrypoint.
#[derive(Debug, Clone, Default)]
pub struct Invocation {
    pub plugin_id: String,
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Private broker socket bound to this invocation's grant.
    pub socket_path: PathBuf,
    /// Absolute path of the private `herdr` launcher.
    pub bin_path: PathBuf,
    /// `PluginInvocationContext` JSON (`HERDR_PLUGIN_CONTEXT_JSON`).
    pub context: Value,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub action_id: Option<String>,
    /// `(event name, event JSON)` for `[[events]]` hooks.
    pub event: Option<(String, Value)>,
    pub entrypoint_id: Option<String>,
    pub clicked_url: Option<String>,
    pub link_handler_id: Option<String>,
    /// The broker's per-invocation secret (`VIBEKE_HERDR_TOKEN`; empty: not exported).
    pub broker_token: String,
}

/// The environment for a runtime invocation.
pub fn runtime_env(
    inv: &Invocation,
    inherited: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited.into_iter().filter(|(k, _)| !stale(k)).collect();
    let launcher_dir = inv
        .bin_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    match env.iter_mut().find(|(k, _)| k == "PATH") {
        Some((_, v)) => *v = format!("{launcher_dir}:{v}"),
        None => env.push(("PATH".into(), format!("{launcher_dir}:/usr/bin:/bin"))),
    }
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let mut set = vec![
        ("HERDR_ENV", "1".to_string()),
        ("HERDR_SOCKET_PATH", s(&inv.socket_path)),
        ("HERDR_BIN_PATH", s(&inv.bin_path)),
        ("HERDR_PLUGIN_ID", inv.plugin_id.clone()),
        ("HERDR_PLUGIN_ROOT", s(&inv.root)),
        ("HERDR_PLUGIN_CONFIG_DIR", s(&inv.config_dir)),
        ("HERDR_PLUGIN_STATE_DIR", s(&inv.state_dir)),
        ("HERDR_PLUGIN_CONTEXT_JSON", inv.context.to_string()),
        // Lets the launcher recognize a Vibeke broker without trusting any Herdr socket.
        ("VIBEKE_HERDR_BROKER", s(&inv.socket_path)),
    ];
    let opt = |k: &'static str, v: &Option<String>, set: &mut Vec<(&'static str, String)>| {
        if let Some(v) = v {
            set.push((k, v.clone()));
        }
    };
    opt("HERDR_WORKSPACE_ID", &inv.workspace_id, &mut set);
    opt("HERDR_TAB_ID", &inv.tab_id, &mut set);
    opt("HERDR_PANE_ID", &inv.pane_id, &mut set);
    opt("HERDR_PLUGIN_ACTION_ID", &inv.action_id, &mut set);
    opt("HERDR_PLUGIN_ENTRYPOINT_ID", &inv.entrypoint_id, &mut set);
    opt("HERDR_PLUGIN_CLICKED_URL", &inv.clicked_url, &mut set);
    opt(
        "HERDR_PLUGIN_LINK_HANDLER_ID",
        &inv.link_handler_id,
        &mut set,
    );
    if let Some((name, data)) = &inv.event {
        set.push(("HERDR_PLUGIN_EVENT", name.clone()));
        set.push(("HERDR_PLUGIN_EVENT_JSON", data.to_string()));
    }
    if !inv.broker_token.is_empty() {
        set.push(("VIBEKE_HERDR_TOKEN", inv.broker_token.clone()));
    }
    env.extend(set.into_iter().map(|(k, v)| (k.to_string(), v)));
    env
}

/// The environment for `[[build]]` steps: no broker, launcher, context or pane identity.
pub fn build_env(
    plugin_id: &str,
    root: &Path,
    inherited: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited.into_iter().filter(|(k, _)| !stale(k)).collect();
    env.push(("HERDR_PLUGIN_ID".into(), plugin_id.into()));
    env.push((
        "HERDR_PLUGIN_ROOT".into(),
        root.to_string_lossy().into_owned(),
    ));
    env
}

/// Resolve `command[0]` against the plugin root when it is a relative path (Herdr ≥ 0.9.0
/// resolves relative pane commands against the root on every OS; reviewr relies on it). Bare
/// program names are left to `PATH`.
pub fn resolve_argv(root: &Path, command: &[String]) -> Vec<String> {
    let mut argv = command.to_vec();
    if let Some(first) = argv.first_mut() {
        let p = Path::new(first.as_str());
        if p.is_relative() && first.contains('/') {
            *first = root.join(p).to_string_lossy().into_owned();
        }
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn runtime_env_replaces_stale_context() {
        let inv = Invocation {
            plugin_id: "acme.demo".into(),
            root: "/p/acme".into(),
            config_dir: "/c/acme".into(),
            state_dir: "/s/acme".into(),
            socket_path: "/run/b/1.sock".into(),
            bin_path: "/run/bin/herdr".into(),
            context: json!({"source": "cli"}),
            workspace_id: Some("w1".into()),
            action_id: Some("open".into()),
            event: Some(("worktree.created".into(), json!({"path": "/x"}))),
            ..Default::default()
        };
        let inherited = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/u".to_string()),
            ("HERDR_PANE_ID".to_string(), "w9:p9".to_string()),
            (
                "HERDR_SOCKET_PATH".to_string(),
                "/home/u/.config/herdr/herdr.sock".to_string(),
            ),
            ("VIBEKE_PANE_TOKEN".to_string(), "secret".to_string()),
            (
                "VIBEKE_HERDR_TOKEN".to_string(),
                "someone-elses".to_string(),
            ),
        ];
        let env = runtime_env(&inv, inherited.clone());
        assert_eq!(get(&env, "VIBEKE_HERDR_TOKEN"), None, "stale token dropped");
        let env = runtime_env(
            &Invocation {
                broker_token: "t0k".into(),
                ..inv.clone()
            },
            inherited.clone(),
        );
        assert_eq!(get(&env, "VIBEKE_HERDR_TOKEN"), Some("t0k"));
        let env = runtime_env(&inv, inherited);
        assert_eq!(get(&env, "PATH"), Some("/run/bin:/usr/bin"));
        assert_eq!(get(&env, "HOME"), Some("/home/u"));
        assert_eq!(get(&env, "HERDR_SOCKET_PATH"), Some("/run/b/1.sock"));
        assert_eq!(get(&env, "HERDR_ENV"), Some("1"));
        assert_eq!(get(&env, "HERDR_WORKSPACE_ID"), Some("w1"));
        assert_eq!(
            get(&env, "HERDR_PANE_ID"),
            None,
            "stale pane id cleared, none supplied"
        );
        assert_eq!(get(&env, "VIBEKE_PANE_TOKEN"), None);
        assert_eq!(get(&env, "HERDR_PLUGIN_EVENT"), Some("worktree.created"));
        assert_eq!(get(&env, "HERDR_PLUGIN_ACTION_ID"), Some("open"));
        assert_eq!(
            env.iter().filter(|(k, _)| k == "HERDR_SOCKET_PATH").count(),
            1,
            "no duplicate keys"
        );
    }

    #[test]
    fn build_env_has_no_authority() {
        let env = build_env(
            "acme.demo",
            Path::new("/p"),
            vec![
                ("HERDR_SOCKET_PATH".to_string(), "/x".to_string()),
                ("VIBEKE_SOCKET".to_string(), "/y".to_string()),
            ],
        );
        assert_eq!(get(&env, "HERDR_SOCKET_PATH"), None);
        assert_eq!(get(&env, "VIBEKE_SOCKET"), None);
        assert_eq!(get(&env, "HERDR_BIN_PATH"), None);
        assert_eq!(get(&env, "HERDR_PLUGIN_ROOT"), Some("/p"));
    }

    #[test]
    fn argv_resolution() {
        let root = Path::new("/p");
        assert_eq!(
            resolve_argv(root, &["bin/x".into(), "a".into()]),
            vec!["/p/bin/x", "a"]
        );
        assert_eq!(resolve_argv(root, &["bash".into()]), vec!["bash"]);
        assert_eq!(resolve_argv(root, &["/abs/x".into()]), vec!["/abs/x"]);
        assert_eq!(
            resolve_argv(root, &["./run.sh".into()]),
            vec!["/p/./run.sh"]
        );
    }
}
