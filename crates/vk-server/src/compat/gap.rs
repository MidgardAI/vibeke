//! Lane 3C gap closure for the Herdr compatibility layer (07 §8.2, §8.3):
//!
//! * `compat.herdr_socket_path`: where the default session's public listener binds, for setups
//!   where Herdr is gone. A live socket at the path is never replaced, so a running Herdr (or
//!   an existing client's expectations of one) is untouched.
//! * `HERDR_*` in ordinary panes (`compat.herdr_env`), exported only while the compat listener
//!   is enabled, so Herdr-style integrations inside a pane reach Vibeke's endpoint.
//! * `server.stop` on the compat endpoint, behind `[compat.herdr] allow_server_stop`: stops
//!   *this* Vibeke session, never from a plugin or a pane.
//! * `agent.explain`: why a run has its current Herdr status.
//!
//! Shapes the baseline has not been captured for stay *unverified* (inventory: partial).

use super::*;

/// Methods handled here.
pub const METHODS: &[&str] = &["server.stop", "agent.explain"];

// ---- socket path ------------------------------------------------------------------------------

/// Expand a leading `~/` against `home`; an empty string means "not configured".
pub fn expand_socket_path(configured: &str, home: &Path) -> Option<PathBuf> {
    let t = configured.trim();
    if t.is_empty() {
        return None;
    }
    let p = if t == "~" {
        home.to_path_buf()
    } else if let Some(rest) = t.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(t)
    };
    // A relative path would depend on the server's working directory.
    p.is_absolute().then_some(p)
}

/// The listener path of `session`. With a configured default-session path the named sessions
/// live next to it (`<dir>/sessions/<name>/herdr.sock`), the layout Herdr itself uses.
pub fn session_listener(configured: Option<&Path>, runtime_root: &Path, session: &str) -> PathBuf {
    match configured {
        None => herdr::session_socket(runtime_root, session),
        Some(p) if session == herdr::DEFAULT_SESSION => p.to_path_buf(),
        Some(p) => {
            let dir = p.parent().unwrap_or_else(|| Path::new("/"));
            herdr::session_socket(dir, session)
        }
    }
}

/// `compat.herdr_socket_path` from the user's config, expanded.
pub fn configured_socket() -> Option<PathBuf> {
    let (c, _) = vk_config::Config::load(vk_config::config_path()).ok()?;
    expand_socket_path(&c.compat.herdr_socket_path, &crate::paths::home())
}

/// The listener path of `session` under the current configuration (also used by the CLI).
pub fn listener_for(session: &str) -> PathBuf {
    session_listener(
        configured_socket().as_deref(),
        &crate::paths::runtime_root().join("herdr-compat"),
        session,
    )
}

// ---- HERDR_* in ordinary panes ----------------------------------------------------------------

/// The aliases an ordinary pane gets (07 §8.2). `HERDR_PANE_ID` and friends are Vibeke's own
/// handles, which share Herdr's grammar.
pub fn pane_aliases(
    socket: &Path,
    bin: &Path,
    pane: &str,
    tab: &str,
    workspace: &str,
) -> Vec<(String, String)> {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    vec![
        ("HERDR_ENV".into(), "1".into()),
        ("HERDR_SOCKET_PATH".into(), s(socket)),
        ("HERDR_PANE_ID".into(), pane.into()),
        ("HERDR_WORKSPACE_ID".into(), workspace.into()),
        ("HERDR_TAB_ID".into(), tab.into()),
        ("HERDR_BIN_PATH".into(), s(bin)),
    ]
}

/// Add the aliases to a pane's environment when `compat.herdr_env` is on and the compat
/// listener is enabled. Called from `Server::pane_env` (must not lock `core`).
pub fn extend_pane_env(
    server: &Server,
    env: &mut Vec<(String, String)>,
    handle: &str,
    tab_handle: &str,
    ws_handle: &str,
) {
    let Ok((c, _)) = vk_config::Config::load(vk_config::config_path()) else {
        return;
    };
    if !c.compat.herdr_env || !(c.compat.herdr.enabled || c.compat.herdr_socket) {
        return;
    }
    let socket = session_listener(
        expand_socket_path(&c.compat.herdr_socket_path, &crate::paths::home()).as_deref(),
        &herdr_root(server),
        &server.opts.session,
    );
    for (k, v) in pane_aliases(&socket, &launcher(server), handle, tab_handle, ws_handle) {
        env.retain(|(x, _)| x != &k);
        env.push((k, v));
    }
}

// ---- server.stop and agent.explain -----------------------------------------------------------

/// Whether `caller` may stop the server through the compat endpoint.
pub fn stop_allowed(enabled: bool, caller: &Caller) -> Result<(), WireError> {
    if !enabled {
        return Err(WireError::new(
            "unsupported",
            "server.stop is disabled on the Herdr compatibility endpoint ([compat.herdr] allow_server_stop = false); use `vibeke server stop`",
        ));
    }
    if caller.plugin.is_some() || caller.sandboxed || caller.broker.is_some() {
        return Err(WireError::new(
            "permission_denied",
            "a plugin cannot stop the server",
        ));
    }
    if caller.ctx.pane_scope.is_some() {
        return Err(WireError::new(
            "permission_denied",
            "server.stop is not allowed from a pane",
        ));
    }
    Ok(())
}

/// The reason line for a run's Herdr status.
pub fn explain_reason(status: &str, execution: &str, blocked: bool, unseen: bool) -> String {
    match status {
        "blocked" => "waiting for an answer to an open approval or question".to_string(),
        "working" => format!("the agent is {execution}"),
        "done" if unseen => "a turn finished and has not been looked at yet".to_string(),
        "done" => "a turn finished".to_string(),
        "idle" => "the agent is idle and nothing is waiting".to_string(),
        _ if blocked => "an interaction is open".to_string(),
        _ => format!("no status could be derived (execution {execution})"),
    }
}

pub async fn call(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    method: &str,
    p: &Value,
) -> Result<Value, WireError> {
    match method {
        "server.stop" => {
            let enabled = vk_config::Config::load(vk_config::config_path())
                .map(|(c, _)| c.compat.herdr.allow_server_stop)
                .unwrap_or(false);
            stop_allowed(enabled, caller)?;
            native(
                server,
                caller,
                "server.stop",
                json!({"kill_panes": p.get("kill_panes").and_then(Value::as_bool).unwrap_or(false)}),
            )
            .await?;
            Ok(ok())
        }
        "agent.explain" => {
            let r = run_target(sn, p)?;
            let status = sn.run_status(&r);
            let blocked = sn.blocked.contains(&r.id);
            let unseen = sn
                .pane(&r.pane)
                .is_some_and(|x| (x.unread || x.marked_unread) && r.turns_completed > 0);
            Ok(typed(
                "agent_explain",
                json!({
                    "agent": sn.agent_json(server, &r),
                    "agent_status": status,
                    "reason": explain_reason(status, r.execution.value.as_str(), blocked, unseen),
                    "execution": r.execution.value.as_str(),
                    "blocked": blocked,
                    "unseen_turn": unseen,
                    "turns_completed": r.turns_completed,
                    "last_tool": r.last_tool,
                    "last_message": r.last_message,
                    "source": r.integration,
                }),
            ))
        }
        other => Err(WireError::new(
            "method_not_found",
            format!("unknown method {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller() -> Caller {
        Caller {
            ctx: Ctx {
                client_id: "c".into(),
                kind: "herdr".into(),
                pane_scope: None,
                remote: false,
            },
            plugin: None,
            default_pane: None,
            invocation: None,
            broker: None,
            grant_id: None,
            cross_session: false,
            sandboxed: false,
            peer_bound: false,
            token: None,
        }
    }

    #[test]
    fn socket_path_expansion_and_layout() {
        let home = Path::new("/home/u");
        assert_eq!(expand_socket_path("", home), None);
        assert_eq!(expand_socket_path("  ", home), None);
        assert_eq!(expand_socket_path("relative/h.sock", home), None);
        let p = expand_socket_path("~/.config/herdr/herdr.sock", home).unwrap();
        assert_eq!(p, Path::new("/home/u/.config/herdr/herdr.sock"));
        let root = Path::new("/run/vibeke/herdr-compat");
        assert_eq!(
            session_listener(None, root, "default"),
            root.join("herdr.sock")
        );
        assert_eq!(session_listener(Some(&p), root, "default"), p);
        assert_eq!(
            session_listener(Some(&p), root, "work"),
            Path::new("/home/u/.config/herdr/sessions/work/herdr.sock")
        );
    }

    #[test]
    fn pane_aliases_use_herdr_names() {
        let a = pane_aliases(
            Path::new("/s/herdr.sock"),
            Path::new("/b/herdr"),
            "w1:p2",
            "w1:t1",
            "w1",
        );
        let get = |k: &str| a.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
        assert_eq!(get("HERDR_ENV"), Some("1"));
        assert_eq!(get("HERDR_SOCKET_PATH"), Some("/s/herdr.sock"));
        assert_eq!(get("HERDR_PANE_ID"), Some("w1:p2"));
        assert_eq!(get("HERDR_TAB_ID"), Some("w1:t1"));
        assert_eq!(get("HERDR_WORKSPACE_ID"), Some("w1"));
        assert_eq!(get("HERDR_BIN_PATH"), Some("/b/herdr"));
    }

    #[test]
    fn stop_is_off_by_default_and_never_for_plugins_or_panes() {
        let c = caller();
        assert_eq!(stop_allowed(false, &c).unwrap_err().code, "unsupported");
        assert!(stop_allowed(true, &c).is_ok());
        let mut p = caller();
        p.plugin = Some(("x".into(), "d".into()));
        assert_eq!(
            stop_allowed(true, &p).unwrap_err().code,
            "permission_denied"
        );
        let mut s = caller();
        s.sandboxed = true;
        assert_eq!(
            stop_allowed(true, &s).unwrap_err().code,
            "permission_denied"
        );
        let mut pane = caller();
        pane.ctx.pane_scope = Some("w1:p1".into());
        assert_eq!(
            stop_allowed(true, &pane).unwrap_err().code,
            "permission_denied"
        );
    }

    #[test]
    fn reasons_cover_every_status() {
        assert!(explain_reason("blocked", "idle", true, false).contains("waiting"));
        assert!(explain_reason("working", "working", false, false).contains("working"));
        assert!(explain_reason("done", "idle", false, true).contains("not been looked"));
        assert!(explain_reason("idle", "idle", false, false).contains("idle"));
        assert!(explain_reason("unknown", "error", false, false).contains("error"));
    }
}
