//! The terminal render lane for an existing gateway share. No host-wide frames or commands.
use serde::Deserialize;
use vk_proto::model::{ClientFocus, LayoutNode, SessionModel};
use vk_proto::render::{AckStatus, ClientFrame, ServerFrame};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    pub pane: Option<String>,
    pub workspace: Option<String>,
    pub scope: String,
    pub expires_at: u64,
}
impl Share {
    pub fn valid(&self) -> bool {
        (self.pane.as_ref().is_some_and(|s| !s.is_empty())
            || self.workspace.as_ref().is_some_and(|s| !s.is_empty()))
            && matches!(self.scope.as_str(), "view" | "approve" | "full")
    }
    pub fn allows(&self, model: &SessionModel, pane: &str) -> bool {
        model.panes.iter().any(|p| {
            p.id == pane
                && match &self.pane {
                    Some(id) => id == &p.id,
                    None => self.workspace.as_ref().is_some_and(|id| id == &p.workspace),
                }
        })
    }
    pub fn model(&self, source: &SessionModel, focus: &ClientFocus) -> (SessionModel, ClientFocus) {
        // Select the shared entities. Their fields are copied in full, except for the
        // host-only references and pane-share layout removed below.
        let panes: Vec<_> = source
            .panes
            .iter()
            .filter(|p| self.allows(source, &p.id))
            .cloned()
            .collect();
        let has = |id: &str| panes.iter().any(|p| p.id == id);
        let mut tabs: Vec<_> = source
            .tabs
            .iter()
            .filter(|t| panes.iter().any(|p| p.tab == t.id))
            .cloned()
            .collect();
        for tab in &mut tabs {
            if let Some(p) = &self.pane {
                tab.layout = LayoutNode::Leaf { pane: p.clone() };
                tab.floating.clear();
                tab.focused_pane = Some(p.clone());
                tab.zoomed_pane = None;
            }
        }
        let mut workspaces: Vec<_> = source
            .workspaces
            .iter()
            .filter(|w| panes.iter().any(|p| p.workspace == w.id))
            .cloned()
            .collect();
        for w in &mut workspaces {
            w.task = None;
        }
        let mut runs: Vec<_> = source
            .runs
            .iter()
            .filter(|r| has(&r.pane))
            .cloned()
            .collect();
        for r in &mut runs {
            r.task = None;
        }
        let mut interactions: Vec<_> = source
            .interactions
            .iter()
            .filter(|i| has(&i.pane))
            .cloned()
            .collect();
        for i in &mut interactions {
            i.answerable &= self.scope != "view";
        }
        let pane = focus
            .pane
            .as_ref()
            .filter(|p| has(p))
            .and_then(|id| panes.iter().find(|p| &p.id == id))
            .or(panes.first());
        let focus = pane
            .map(|p| ClientFocus {
                workspace: Some(p.workspace.clone()),
                tab: Some(p.tab.clone()),
                pane: Some(p.id.clone()),
            })
            .unwrap_or_default();
        (
            SessionModel {
                machine: source.machine.clone(),
                server_version: source.server_version.clone(),
                workspaces,
                tabs,
                runs,
                interactions,
                pane_live: source
                    .pane_live
                    .iter()
                    .filter(|p| has(&p.pane))
                    .cloned()
                    .collect(),
                panes,
                ..Default::default()
            },
            focus,
        )
    }
    pub fn filter(
        &self,
        model: &SessionModel,
        f: ClientFrame,
    ) -> Result<ClientFrame, Option<ServerFrame>> {
        let allowed = |p: &str| self.allows(model, p);
        match f {
            ClientFrame::Key { ref pane, input_id, .. } | ClientFrame::RawInput { ref pane, input_id, .. }
            | ClientFrame::Mouse { ref pane, input_id, .. } | ClientFrame::Paste { ref pane, input_id, .. } => {
                if self.scope == "full" && allowed(pane) { Ok(f) }
                else { Err(Some(ServerFrame::InputAck { input_id, status: AckStatus::Rejected })) }
            }
            ClientFrame::SyncInput { input_id, .. } | ClientFrame::Browser { input_id, .. } =>
                Err(Some(ServerFrame::InputAck { input_id, status: AckStatus::Rejected })),
            ClientFrame::ViewHint { mut panes, .. } => {
                panes.retain(|p| allowed(&p.pane));
                Ok(ClientFrame::ViewHint { panes, active: false })
            }
            ClientFrame::Ack { ref pane, .. } | ClientFrame::Resync { ref pane }
            | ClientFrame::FetchHistory { ref pane, .. } | ClientFrame::Focus { ref pane } =>
                if allowed(pane) { Ok(f) } else { Err(None) },
            ClientFrame::Ping { .. } | ClientFrame::Detach => Ok(f),
            // Commands are dispatched through the gateway API, except local focus (handled by
            // Session). No event replay, browser routing, shared memory or clipboard requests.
            ClientFrame::Command { req, .. } => Err(Some(ServerFrame::CommandResult { req,
                json: serde_json::json!({"jsonrpc":"2.0","id":req,"error":{"code":-32000,"message":"Unavailable in a shared terminal","data":{"kind":"forbidden"}}}).to_string() })),
            _ => Err(None),
        }
    }
}
