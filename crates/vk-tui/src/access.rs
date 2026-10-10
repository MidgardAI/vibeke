//! Client UI capabilities. The host still authorizes every request independently.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub host: bool,
    pub approve: bool,
    pub control: bool,
    pub workspace: bool,
}

impl Capabilities {
    pub fn from_features(features: &[String]) -> Self {
        let has = |name: &str| features.iter().any(|f| f == name);
        let host = !has("shared_tui");
        let control = host || has("shared_tui.control");
        Self {
            host,
            approve: control || has("shared_tui.approve"),
            control,
            workspace: host || has("shared_tui.workspace"),
        }
    }

    /// Only offer actions backed by the scoped model or the shared-session API.
    pub fn action(self, action: &str) -> bool {
        if self.host {
            return true;
        }
        match action {
            "help"
            | "detach"
            | "command_palette"
            | "toggle_sidebar"
            | "workspace_picker"
            | "goto"
            | "last_workspace"
            | "previous_workspace"
            | "next_workspace"
            | "switch_workspace"
            | "next_tab"
            | "previous_tab"
            | "switch_tab"
            | "focus_pane_left"
            | "focus_pane_right"
            | "focus_pane_up"
            | "focus_pane_down"
            | "cycle_pane_next"
            | "cycle_pane_previous"
            | "last_pane"
            | "enter_copy_mode"
            | "search_scrollback"
            | "url_hints"
            | "review_clipboard"
            | "inbox"
            | "next_attention"
            | "next_attention_focus"
            | "agent_list"
            | "previous_agent"
            | "next_agent"
            | "focus_agent" => true,
            "batch_approvals" => self.approve,
            "rename_pane" | "close_pane" | "paste_buffer" => self.control,
            "new_tab" | "rename_tab" | "close_tab" => self.control && self.workspace,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::{self, Mode};
    use vk_proto::render::{ClientFrame, ServerFrame};

    #[test]
    fn shares_do_not_start_host_services_or_offer_host_actions() {
        for scope in ["view", "approve", "control"] {
            let (mut app, mut rxs) = app::test_app(1);
            app.machines[0].features = vec!["shared_tui".into(), format!("shared_tui.{scope}")];
            app.on_connected(0);
            app.on_frame(
                0,
                ServerFrame::Model {
                    model: Default::default(),
                    focus: Default::default(),
                    seen: vec![],
                },
            );
            app.on_tick();
            while let Ok(frame) = rxs[0].try_recv() {
                assert!(!matches!(frame, ClientFrame::Command { .. }), "{frame:?}");
            }
            let entries = crate::nav::palette_entries(&app);
            assert!(!entries.iter().any(|e| matches!(
                e.id.as_str(),
                "devices" | "setup" | "new_workspace" | "split_vertical"
            )));
            assert_eq!(
                entries.iter().any(|e| e.id == "batch_approvals"),
                scope != "view"
            );
            assert!(entries.iter().any(|e| e.id == "enter_copy_mode"));
            let menu = crate::menu::build(&app, &[]);
            assert!(
                !menu
                    .iter()
                    .flat_map(|g| &g.items)
                    .any(|i| i.label == "split right")
            );
            app.action("devices", None);
            assert!(matches!(app.mode, Mode::Normal));
            app.action("command_palette", None);
            app.on_input(crate::input::Input::Key(vk_proto::input::KeyEvent::new(
                vk_proto::input::Key::Named(vk_proto::input::NamedKey::Escape),
                vk_proto::input::Mods::empty(),
            )));
            app.on_tick();
            assert!(rxs[0].try_recv().is_err());
        }
    }
}
