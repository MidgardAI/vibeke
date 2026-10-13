//! Bounded read work for a render connection. Mutations and input stay in wire order.
use super::*;

pub(super) const MAX_READS: usize = 8;

impl Session {
    /// Only opt in handlers whose work is read-only and whose target can be frozen here.
    /// The normal API handler remains responsible for authorization and validation.
    pub(super) fn prepare_read(&self, json: &str) -> Option<String> {
        let mut request: vk_proto::rpc::Request = serde_json::from_str(json).ok()?;
        if request.jsonrpc != "2.0" {
            return None;
        }
        match request.method.as_str() {
            "git.status" | "git.diff" => {
                if api::s(&request.params, "path").is_none() {
                    let pane = api::resolve_pane(
                        &self.server,
                        &self.ctx(),
                        api::s(&request.params, "pane"),
                    )
                    .ok()?;
                    request
                        .params
                        .as_object_mut()?
                        .insert("pane".into(), pane.id.into());
                }
            }
            "fs.browse" | "repo.candidates" => {}
            _ => return None,
        }
        serde_json::to_string(&request).ok()
    }

    pub(super) fn capture_history(
        &self,
        req: u64,
        pane: &str,
        start: u32,
        count: u32,
    ) -> HistoryRead {
        let mut history = HistoryRead {
            req,
            pane: pane.into(),
            start,
            total: 0,
            archive_end: start,
            memory: vec![],
        };
        let Some(rt) = self.server.pane_rt(pane) else {
            return history;
        };
        let sc = rt.screen.lock().unwrap();
        let mem = sc.engine.history_len() as u32;
        let archived = sc
            .engine
            .scrolled_total()
            .saturating_sub(mem as u64)
            .min(u32::MAX as u64) as u32;
        history.total = archived.saturating_add(mem);
        let end = start.saturating_add(count).min(history.total);
        // Capture the rows and their absolute origin under the same lock. The ring may
        // roll over while the archive worker runs; those old indices must not be reused.
        if self.auth.share.is_some() {
            history.start = start.max(archived);
        }
        history.archive_end = end.min(archived).max(history.start);
        for i in history.start.max(archived)..end {
            history.memory.push(
                sc.engine
                    .history_row((i - archived) as usize)
                    .unwrap_or_default(),
            );
        }
        history
    }
}

pub(super) struct HistoryRead {
    req: u64,
    pane: String,
    start: u32,
    total: u32,
    archive_end: u32,
    memory: Vec<Row>,
}
impl HistoryRead {
    pub(super) fn finish(self, server: &Server) -> ServerFrame {
        let mut lines = Vec::new();
        if self.start < self.archive_end {
            let rows = server
                .archive
                .lock()
                .unwrap()
                .read(&self.pane, self.start as u64, self.archive_end as u64)
                .unwrap_or_default();
            let mut map: HashMap<u64, Row> = rows
                .into_iter()
                .map(|r| {
                    (
                        r.n,
                        Row::new(
                            vec![Span {
                                style: Style::default(),
                                cols: unicode_cols(&r.t),
                                text: r.t,
                            }],
                            r.w,
                        ),
                    )
                })
                .collect();
            for n in self.start..self.archive_end {
                lines.push(map.remove(&(n as u64)).unwrap_or_default());
            }
        }
        lines.extend(self.memory);
        ServerFrame::History {
            pane: self.pane,
            req: self.req,
            start: self.start,
            total: self.total,
            lines,
        }
    }
}
