//! Per-host pending operations in this browser tab. No file paths or lock state.
use super::PendingStore;

#[derive(Debug, Default)]
pub(super) struct Storage {
    key: Option<String>,
}

fn session_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.session_storage().ok().flatten())
}

impl PendingStore {
    pub fn open_browser(host: &str, label: &str) -> Result<Self, String> {
        let key = format!("vibeke-tui-pending:{host}");
        let mut store = Self::default();
        if let Some(storage) = session_storage()
            && let Some(saved) = storage.get_item(&key).map_err(
                |_| "Pending TUI operations could not be read. Keep browser storage for recovery.",
            )?
        {
            store.ops = serde_json::from_str(&saved).map_err(
                |_| "Pending TUI operations could not be read. Keep browser storage for recovery.",
            )?;
        }
        // The host identity is stable even when its display name changes.
        for op in &mut store.ops {
            op.machine = label.into();
        }
        store.storage.key = Some(key);
        Ok(store)
    }

    pub fn adopt_orphans(&mut self) -> usize {
        // sessionStorage already belongs to this tab and survives its reloads.
        0
    }

    pub(super) fn save(&self) -> std::io::Result<()> {
        let Some(key) = &self.storage.key else {
            return Ok(());
        };
        let storage = session_storage().ok_or_else(|| {
            std::io::Error::other("Browser storage is unavailable; operation was not sent")
        })?;
        storage
            .set_item(key, &serde_json::to_string(&self.ops)?)
            .map_err(|_| {
                std::io::Error::other("Could not save the pending operation; it was not sent")
            })
    }
}
