//! Tell the user when access to this host is handed out or used: a pairing link, a share, a
//! peer invitation, a device that paired, an invitation someone redeemed. The server stores a
//! `gateway.access` event and shows a notification (a toast in the TUI, the inbox) through
//! `gateway.notice`. Best effort: an older server without the method, or no server at all,
//! changes nothing. The texts never carry a link.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::server::Server;
use crate::state::Scope;

/// What a level of access lets a device do (the same words as the TUI and the security docs).
pub fn access_text(scope: Scope) -> &'static str {
    match scope {
        Scope::Full => "can run any command as you on this host",
        Scope::Approve => {
            "can approve agent requests, including \"always allow\", and send text to agents"
        }
        Scope::View => "can read all pane output, transcripts, files and diffs",
    }
}

/// One notice. `event` is one of the server's `gateway.notice` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub event: &'static str,
    pub title: String,
    pub body: String,
    /// `normal` for the owner's own actions, `high` for anything a device did.
    pub urgency: &'static str,
}

/// Send `n` to the server, waiting at most a few seconds.
pub async fn send(server: &Server, n: &Notice) {
    let params = json!({"event": n.event, "title": n.title, "body": n.body,
                        "urgency": n.urgency, "actor": "gateway"});
    match tokio::time::timeout(
        Duration::from_secs(3),
        server.call("gateway.notice", params),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::debug!("gateway.notice: {}", e.message),
        Err(_) => tracing::debug!("gateway.notice: no answer"),
    }
}

/// [`send`] in the background (nothing outside a Tokio runtime).
pub fn spawn(server: &Arc<Server>, n: Notice) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let server = server.clone();
    rt.spawn(async move { send(&server, &n).await });
}

/// Who did it, for the texts: a device's name, or "You" for the owner.
pub fn who(by: Option<&str>) -> String {
    match by {
        Some(name) => format!("\"{}\"", crate::pair::sanitize_label(name, 64)),
        None => "You".into(),
    }
}

/// A pairing link for a new phone or browser.
pub fn pairing_created(scope: Scope, minutes: u64) -> Notice {
    Notice {
        event: "pairing.created",
        title: format!("A pairing link with {} access was created", scope.as_str()),
        body: format!(
            "The device that opens it {}. The link works for {minutes} min. You confirm the device before it is added.",
            access_text(scope)
        ),
        urgency: "normal",
    }
}

/// A device was added: a paired phone, a share someone opened, or a linked host.
pub fn device_added(kind: &str, name: &str, scope: Scope, host: &str) -> Notice {
    let name = crate::pair::sanitize_label(name, 64);
    let host = crate::pair::sanitize_label(host, 64);
    match kind {
        "share" => Notice {
            event: "share.redeemed",
            title: format!("\"{name}\" opened a share of {host}"),
            body: format!(
                "It has {} access to the shared panes. Revoke it in the Devices view.",
                scope.as_str()
            ),
            urgency: "high",
        },
        "peer" => Notice {
            event: "peer.added",
            title: format!("A host linked with {host}: \"{name}\""),
            body: "It can hand work to this host. Remove it in the Devices view.".into(),
            urgency: "high",
        },
        _ => Notice {
            event: "device.paired",
            title: format!("\"{name}\" paired with {host} ({} access)", scope.as_str()),
            body: format!("It {}. Revoke it in the Devices view.", access_text(scope)),
            urgency: "high",
        },
    }
}

/// A share or handoff invitation was created.
pub fn share_created(kind: &str, scope: Scope, by: Option<&str>) -> Notice {
    let (title, body) = if kind == "handoff" {
        (
            format!("{} created a handoff invitation", who(by)),
            "A teammate's host that opens it can hand work to this host.".to_string(),
        )
    } else {
        (
            format!("{} created a share with {} access", who(by), scope.as_str()),
            "Anyone who opens the link in the next 15 minutes can join. Cancel it in the Devices view.".to_string(),
        )
    };
    Notice {
        event: "share.created",
        title,
        body,
        urgency: if by.is_some() { "high" } else { "normal" },
    }
}

/// A peer invitation was created.
pub fn peer_invited(by: Option<&str>) -> Notice {
    Notice {
        event: "peer.invited",
        title: format!("{} created a peer invitation", who(by)),
        body: "Whoever opens it links a host with this one. That host can hand work to this host."
            .into(),
        urgency: if by.is_some() { "high" } else { "normal" },
    }
}

/// This host redeemed a peer or handoff invitation.
pub fn peer_redeemed(peer_name: &str, by: Option<&str>) -> Notice {
    Notice {
        event: "peer.added",
        title: format!(
            "{} linked this host with \"{}\"",
            who(by),
            crate::pair::sanitize_label(peer_name, 64)
        ),
        body: "This host can now hand work to it.".into(),
        urgency: if by.is_some() { "high" } else { "normal" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_name_the_actor_and_the_access() {
        let n = device_added("device", "Pixel\u{1b}[31m", Scope::Full, "devbox");
        assert_eq!(n.event, "device.paired");
        assert!(!n.title.contains('\u{1b}'), "{}", n.title);
        assert!(n.body.contains("can run any command as you"), "{}", n.body);
        assert_eq!(
            device_added("share", "x", Scope::View, "h").event,
            "share.redeemed"
        );
        assert_eq!(
            device_added("peer", "x", Scope::Full, "h").event,
            "peer.added"
        );
        let n = share_created("share", Scope::Approve, Some("Pixel"));
        assert!(
            n.title.starts_with("\"Pixel\" created a share"),
            "{}",
            n.title
        );
        assert_eq!(n.urgency, "high");
        assert_eq!(share_created("share", Scope::View, None).urgency, "normal");
        assert!(peer_invited(None).title.starts_with("You created"));
        assert!(pairing_created(Scope::View, 10).body.contains("10 min"));
    }
}
