//! User popups (08 §5): `pane.float {popup: {width, height}, command, …}` from a full-scope
//! client (the TUI's `[[keys.command]] type = "popup"` and edit-scrollback) creates the floating
//! pane tagged like a plugin popup (`created_by = plugin-surface:popup:<w>:<h>:vibeke/popup`), so
//! clients draw it session-modal, close it when its command exits and dismiss it with
//! `plugin.surface.close`. Pane-scoped callers (agents) can't open modal popups: the parameter
//! is ignored for them and they get an ordinary float.

use crate::api::Ctx;
use serde_json::Value;
use vk_proto::model::{PluginSurface, SurfaceKind};

/// The popup tag for `p`, if it asks for one.
pub fn tag(p: &Value) -> Option<String> {
    let pop = p.get("popup")?.as_object()?;
    let dim = |k: &str| match pop.get(k) {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "80%".into(),
    };
    Some(
        PluginSurface {
            kind: SurfaceKind::Popup,
            width: dim("width"),
            height: dim("height"),
            plugin: "vibeke".into(),
            entrypoint: "popup".into(),
        }
        .tag(),
    )
}

/// `created_by` for a new float: the popup tag for full-scope callers.
pub fn created_by(ctx: &Ctx, p: &Value) -> Option<String> {
    if ctx.pane_scope.is_some() {
        return None;
    }
    tag(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(pane: Option<&str>) -> Ctx {
        Ctx {
            client_id: "c".into(),
            kind: "tui".into(),
            pane_scope: pane.map(str::to_string),
            remote: false,
        }
    }

    #[test]
    fn popup_tag_only_for_full_scope() {
        let p = json!({"popup": {"width": "60%", "height": 20}});
        let t = created_by(&ctx(None), &p).unwrap();
        assert_eq!(t, "plugin-surface:popup:60%:20:vibeke/popup");
        let s = PluginSurface::parse(&t).unwrap();
        assert!(s.is_popup());
        assert_eq!(created_by(&ctx(Some("p1")), &p), None);
        assert_eq!(created_by(&ctx(None), &json!({})), None);
        assert_eq!(
            tag(&json!({"popup": {}})).as_deref(),
            Some("plugin-surface:popup:80%:80%:vibeke/popup")
        );
    }
}
