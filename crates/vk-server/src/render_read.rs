//! Bounded read work for a render connection. Mutations and input stay in wire order.
use super::*;
use vk_store::archive::{PaneReadFence, ReadGeneration};

const MAX_READS: usize = 8;
const MAX_QUEUED_READS: usize = 128;
// Keep slots available for scrollback while slow repository calls are in flight.
const MAX_COMMAND_READS: usize = 6;

pub(super) struct ReadOutput {
    pub frame: ServerFrame,
    history_fence: Option<(PaneReadFence, ReadGeneration)>,
}

pub(super) enum ReadJob {
    Command { req: u64, json: String, ctx: Ctx },
    History(HistoryRead),
}
impl ReadJob {
    async fn run(self, server: Arc<Server>) -> Result<ReadOutput> {
        Ok(match self {
            Self::Command { req, json, ctx } => ReadOutput {
                frame: ServerFrame::CommandResult {
                    req,
                    json: api::handle_line(&server, &ctx, &json).await,
                },
                history_fence: None,
            },
            Self::History(history) => {
                tokio::task::spawn_blocking(move || history.finish(&server)).await?
            }
        })
    }
}

impl Session {
    pub(super) fn start_reads(&mut self) {
        while self.reads.len() < MAX_READS {
            let Some(index) = self.pending_reads.iter().position(|job| {
                self.read_commands < MAX_COMMAND_READS || matches!(job, ReadJob::History(_))
            }) else {
                break;
            };
            let job = self.pending_reads.remove(index).unwrap();
            if matches!(job, ReadJob::Command { .. }) {
                self.read_commands += 1;
            }
            self.reads.spawn(job.run(self.server.clone()));
        }
    }

    pub(super) async fn queue_read<W: AsyncWrite + Unpin>(
        &mut self,
        job: ReadJob,
        wr: &mut W,
    ) -> Result<bool> {
        if self.pending_reads.len() < MAX_QUEUED_READS {
            self.pending_reads.push_back(job);
            self.start_reads();
            return Ok(true);
        }
        // Bound queued work without inventing errors or disconnecting a busy client.
        // Extreme floods retain the original serialized behavior until capacity returns.
        let frame = job.run(self.server.clone()).await?;
        self.write_read(frame, wr).await
    }

    pub(super) async fn write_read<W: AsyncWrite + Unpin>(
        &self,
        result: ReadOutput,
        wr: &mut W,
    ) -> Result<bool> {
        if let Err(reason) = self.auth_ok() {
            asyncio::write_frame(wr, &ServerFrame::Goodbye { reason }).await?;
            wr.flush().await?;
            return Ok(false);
        }
        let mut frame = result.frame;
        // A forget/retention/close after capture invalidates even rows already decoded.
        // This lock-free check is the read's final visibility point before queueing output.
        if let ServerFrame::History {
            pane,
            start,
            total,
            lines,
            ..
        } = &mut frame
            && (result
                .history_fence
                .as_ref()
                .is_none_or(|(fence, generation)| fence.generation() != *generation)
                || self.server.pane_rt(pane).is_none())
        {
            *start = 0;
            *total = 0;
            lines.clear();
        }
        let allowed = match (&self.auth.share, &frame) {
            (Some(share), ServerFrame::History { pane, .. }) => {
                self.server.with_core(|c| share.allows(&c.model, pane))
            }
            _ => true,
        };
        if allowed {
            asyncio::write_frame(wr, &frame).await?;
        }
        Ok(true)
    }

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
        let fence = self.server.archive_read_fence.for_pane(pane);
        let mut history = HistoryRead {
            req,
            pane: pane.into(),
            start,
            total: 0,
            archive_end: start,
            memory: vec![],
            generation: fence.generation(),
            fence,
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
    generation: ReadGeneration,
    fence: PaneReadFence,
}
impl HistoryRead {
    pub(super) fn finish(self, server: &Server) -> ReadOutput {
        let mut lines = Vec::new();
        if self.start < self.archive_end && self.generation == self.fence.generation() {
            let prepared = server.archive.lock().unwrap().prepare_read(
                &self.pane,
                self.start as u64,
                self.archive_end as u64,
            );
            // File descriptors and buffered prefixes were captured under the mutex.
            // File I/O and decompression run after releasing it.
            let rows = prepared.and_then(|read| read.finish()).unwrap_or_default();
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
        ReadOutput {
            frame: ServerFrame::History {
                pane: self.pane,
                req: self.req,
                start: self.start,
                total: self.total,
                lines,
            },
            history_fence: Some((self.fence, self.generation)),
        }
    }
}
