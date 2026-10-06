//! Render stream sessions (07 §3): state-sync damage frames per client, logical input in.

use crate::api::{self, Ctx};
use crate::{Server, UiEvent};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use vk_proto::frame::asyncio;
use vk_proto::holder::InputStatus;
use vk_proto::render::*;
use vk_term::encode::{self, InputModes};

const MAX_UNACKED: u32 = 2;

struct PaneView {
    epoch: u32,
    rev: u64,
    rows: Vec<Row>,
    cursor: Cursor,
    modes: PaneModes,
    title: String,
    unacked: u32,
    last_sent: Instant,
    screen_rev: u64,
    scrolled: u64,
    dirty_since_ack: bool,
    /// Kitty placements this client has for the pane (03 §9).
    images: Vec<ImagePlace>,
}

pub struct Session {
    server: Arc<Server>,
    client_id: String,
    views: HashMap<String, PaneView>,
    visible: Vec<PaneRect>,
    focused: Option<String>,
    model_rev: u64,
    max_fps: u32,
    bg_fps: u32,
    remote: bool,
    /// Holder acks for this client's inputs, forwarded as `InputAck` by the session loop.
    acks: mpsc::UnboundedSender<(u64, AckStatus)>,
    /// Media channel (browser panes rendered on this server, 06 B3.2).
    media: crate::browser_pane::MediaSession,
    /// A `ClientFrame::Subscribe` waiting to be applied by the session loop (event push).
    pending_sub: Option<(Vec<String>, Option<i64>)>,
    /// Kitty image hashes whose pixels this client already has (03 §9: once per hash). A
    /// client that evicted pixels sends `Resync` for the pane, which forgets its hashes here.
    images_sent: std::collections::HashSet<String>,
}

/// Hex form of an image content hash (the render protocol's image key).
fn image_key(h: &[u8; 16]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `render.attach` features this server supports (listed in the attach result).
pub const FEATURES: &[&str] = &["event_push", "scroll_report"];

/// Most events replayed for a `Subscribe { after }` (older ones: `events.read`).
const PUSH_BACKLOG: usize = 1000;

/// Event push state of one render session (07 §3, `ClientFrame::Subscribe`).
#[derive(Default)]
pub struct EventPush {
    rx: Option<tokio::sync::broadcast::Receiver<Arc<vk_store::Event>>>,
    types: Vec<String>,
    last: i64,
}

impl EventPush {
    pub fn matches(&self, kind: &str) -> bool {
        self.types.iter().any(|g| vk_store::glob_match(g, kind))
    }

    /// Apply a subscription: live events from now on, plus a replay after `after`.
    pub fn subscribe(
        &mut self,
        server: &Server,
        types: Vec<String>,
        after: Option<i64>,
    ) -> Vec<PushedEvent> {
        if types.is_empty() {
            *self = EventPush::default();
            return vec![];
        }
        // Subscribe before reading the backlog so nothing falls in between (dupes are
        // dropped by sequence number).
        self.rx = Some(server.events.subscribe());
        self.types = types;
        self.last = 0;
        let Some(after) = after else {
            return vec![];
        };
        let backlog = server
            .with_core(|c| c.store.events_after(after, PUSH_BACKLOG, &self.types))
            .unwrap_or_default();
        let out: Vec<PushedEvent> = backlog.iter().map(pushed).collect();
        if let Some(e) = backlog.last() {
            self.last = e.seq;
        }
        out
    }

    /// Filter a batch of live events.
    pub fn filter(&mut self, evs: &[Arc<vk_store::Event>]) -> Vec<PushedEvent> {
        let mut out = Vec::new();
        for e in evs {
            if e.seq > self.last && self.matches(&e.kind) {
                self.last = e.seq;
                out.push(pushed(e));
            }
        }
        out
    }
}

fn pushed(e: &vk_store::Event) -> PushedEvent {
    PushedEvent {
        seq: e.seq,
        kind: e.kind.clone(),
        json: serde_json::to_string(e).unwrap_or_default(),
    }
}

/// Wait for the next live event (forever when not subscribed).
async fn next_event(
    rx: &mut Option<tokio::sync::broadcast::Receiver<Arc<vk_store::Event>>>,
) -> Result<Arc<vk_store::Event>, tokio::sync::broadcast::error::RecvError> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// `client.attached` / `client.detached` (event push lets other clients refresh their device
/// and client lists without polling).
fn client_event(server: &Server, kind: &str, client_id: &str, remote: bool) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event(
        kind,
        serde_json::json!({"client": client_id}),
        serde_json::json!({"kind": "tui", "remote": remote}),
    );
    let _ = server.commit(&mut c, tx);
}

/// Client-facing status for a holder ack (07 §3.2). `Duplicate` means the bytes were written
/// once already (e.g. a resend after reconnect), which the client treats as written.
/// `Failed` (written only partly, child gone) and a vanished pane task are `Unconfirmed`:
/// the input may or may not have reached the program.
fn ack_status(st: Option<InputStatus>) -> AckStatus {
    match st {
        Some(InputStatus::Written | InputStatus::Duplicate) => AckStatus::Written,
        Some(InputStatus::ChildExited) => AckStatus::Rejected,
        Some(InputStatus::Failed | InputStatus::Unconfirmed) | None => AckStatus::Unconfirmed,
    }
}

/// Hash a client input id into the holder's dedupe space, so a client that resends unacked
/// input after a server restart is deduped by the holder (01 §1.2).
pub fn holder_input_id(client: &str, id: u64) -> u64 {
    let h = blake3::hash(format!("{client}:{id}").as_bytes());
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) & !(1u64 << 63)
}

pub fn input_modes(server: &Server, pane: &str) -> InputModes {
    match server.pane_rt(pane) {
        Some(rt) => rt.screen.lock().unwrap().engine.input_modes(),
        None => InputModes::default(),
    }
}

pub async fn serve<R, W>(
    server: Arc<Server>,
    mut rd: R,
    wr: W,
    client_id: String,
    remote: bool,
    max_fps: u32,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut wr = BufWriter::with_capacity(256 * 1024, wr);
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<ClientFrame>();
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<(u64, AckStatus)>();
    let reader = tokio::spawn(async move {
        while let Ok(f) = asyncio::read_frame::<_, ClientFrame>(&mut rd).await {
            if in_tx.send(f).is_err() {
                break;
            }
        }
    });
    // Read from `core` before locking `clients` (lock order: core → clients).
    let last_focus: Option<vk_proto::model::ClientFocus> = server.with_core(|c| {
        c.store
            .kv_get("server", "last_focus")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
    });
    {
        let mut clients = server.clients.lock().unwrap();
        let st = clients.entry(client_id.clone()).or_default();
        st.kind = "tui".into();
        st.attached_at_ms = vk_store::now_ms();
        st.last_active = Some(Instant::now());
        if st.focus.pane.is_none() {
            // Restore the last focus of any client, else the first pane.
            st.focus = last_focus.unwrap_or_default();
        }
    }
    server.fix_client_focus();
    client_event(&server, "client.attached", &client_id, remote);
    *server.geometry_leader.lock().unwrap() = Some(client_id.clone());
    let mut s = Session {
        server: server.clone(),
        client_id: client_id.clone(),
        views: HashMap::new(),
        visible: Vec::new(),
        focused: None,
        model_rev: 0,
        max_fps: max_fps.max(1),
        bg_fps: if remote { 1 } else { 4 },
        remote,
        acks: ack_tx,
        media: crate::browser_pane::MediaSession::new(&server, remote),
        pending_sub: None,
        images_sent: Default::default(),
    };
    let mut push = EventPush::default();
    let hello = ServerFrame::Hello {
        protocol: PROTOCOL,
        server_version: vk_proto::VERSION.into(),
        session: server.opts.session.clone(),
        machine: server.opts.machine.clone(),
        client_id: client_id.clone(),
    };
    asyncio::write_frame(&mut wr, &hello).await?;
    let mut model_rx = server.model_rev.subscribe();
    let mut ui_rx = server.ui.subscribe();
    let media_wake = s.media.notify.clone();
    let result: Result<()> = async {
        s.send_model(&mut wr).await?;
        wr.flush().await?;
        loop {
            let deadline = s.next_deadline();
            let mut got_event = None;
            tokio::select! {
                f = in_rx.recv() => {
                    let Some(f) = f else { break };
                    if !s.on_client(f, &mut wr).await? { break }
                    while let Ok(f) = in_rx.try_recv() {
                        if !s.on_client(f, &mut wr).await? { return Ok(()) }
                    }
                }
                Some((input_id, status)) = ack_rx.recv() => {
                    asyncio::write_frame(&mut wr, &ServerFrame::InputAck { input_id, status }).await?;
                    while let Ok((input_id, status)) = ack_rx.try_recv() {
                        asyncio::write_frame(&mut wr, &ServerFrame::InputAck { input_id, status }).await?;
                    }
                }
                _ = model_rx.changed() => {}
                _ = server.screen_dirty.notified() => {}
                _ = media_wake.notified() => {}
                ev = ui_rx.recv() => {
                    if let Ok(ev) = ev && !s.on_ui(ev, &mut wr).await? { break }
                }
                ev = next_event(&mut push.rx) => got_event = Some(ev),
                _ = tokio::time::sleep_until(deadline) => {}
                _ = server.shutdown.notified() => {
                    asyncio::write_frame(&mut wr, &ServerFrame::Goodbye { reason: "server stopping".into() }).await?;
                    wr.flush().await?;
                    break;
                }
            }
            if let Some((types, after)) = s.pending_sub.take() {
                let backlog = push.subscribe(&s.server, types, after);
                if !backlog.is_empty() {
                    asyncio::write_frame(&mut wr, &ServerFrame::Events { events: backlog, lagged: false }).await?;
                }
            }
            if let Some(ev) = got_event {
                use tokio::sync::broadcast::error::{RecvError, TryRecvError};
                let mut batch = Vec::new();
                let mut lagged = false;
                match ev {
                    Ok(e) => batch.push(e),
                    Err(RecvError::Lagged(_)) => lagged = true,
                    Err(RecvError::Closed) => push = EventPush::default(),
                }
                if let Some(rx) = push.rx.as_mut() {
                    loop {
                        match rx.try_recv() {
                            Ok(e) => batch.push(e),
                            Err(TryRecvError::Lagged(_)) => lagged = true,
                            Err(_) => break,
                        }
                    }
                }
                let events = push.filter(&batch);
                if !events.is_empty() || lagged {
                    asyncio::write_frame(&mut wr, &ServerFrame::Events { events, lagged }).await?;
                }
            }
            if *model_rx.borrow() != s.model_rev {
                s.send_model(&mut wr).await?;
            }
            s.send_panes(&mut wr).await?;
            // Media after cells: a busy page never delays text panes (03 §5).
            s.media.flush(&s.server, &mut wr).await?;
            wr.flush().await?;
        }
        Ok(())
    }
    .await;
    reader.abort();
    s.media.close(&server);
    // Remember focus for the next attach.
    let focus = server.client_focus(&client_id);
    server.with_core(|c| {
        let mut tx = crate::core::Tx::new();
        tx.m.kv(
            "server",
            "last_focus",
            Some(serde_json::to_string(&focus).unwrap_or_default()),
        );
        let _ = c.commit(tx);
    });
    server.clients.lock().unwrap().remove(&client_id);
    client_event(&server, "client.detached", &client_id, remote);
    result
}

impl Session {
    fn next_deadline(&self) -> tokio::time::Instant {
        // Wake again when a throttled pane becomes eligible; otherwise idle (no timer wakeups).
        let mut soonest: Option<Instant> = None;
        for v in &self.visible {
            if let (Some(view), Some(rt)) = (self.views.get(&v.pane), self.server.pane_rt(&v.pane))
                && rt.rev() != view.screen_rev
            {
                let at = view.last_sent + self.interval(&v.pane);
                soonest = Some(soonest.map_or(at, |s| s.min(at)));
            }
        }
        let at = soonest.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::time::Instant::from_std(at.max(Instant::now() + Duration::from_millis(1)))
    }

    fn interval(&self, pane: &str) -> Duration {
        let focused = self.focused.as_deref() == Some(pane);
        let fps = if focused {
            self.max_fps
        } else {
            self.max_fps.min(30)
        };
        Duration::from_millis(1000 / fps as u64)
    }

    async fn on_ui<W: AsyncWrite + Unpin>(&mut self, ev: UiEvent, wr: &mut W) -> Result<bool> {
        let f = match ev {
            UiEvent::Bell { pane } => ServerFrame::Bell { pane },
            UiEvent::Clipboard {
                pane,
                primary,
                data,
            } => ServerFrame::Clipboard {
                selection: if primary {
                    ClipSel::Primary
                } else {
                    ClipSel::Clipboard
                },
                data,
                pane,
            },
            UiEvent::Notify(n) => ServerFrame::Notify {
                title: n.title,
                body: n.body,
                pane: n.pane,
                urgency: n.urgency,
                delivered: n
                    .channels
                    .iter()
                    .filter(|c| *c == "native")
                    .cloned()
                    .collect(),
            },
            UiEvent::Goodbye(reason) => {
                asyncio::write_frame(wr, &ServerFrame::Goodbye { reason }).await?;
                return Ok(false);
            }
            UiEvent::ClipboardQuery {
                client,
                req,
                pane,
                primary,
            } => {
                if client != self.client_id {
                    return Ok(true);
                }
                ServerFrame::ClipboardQuery {
                    req,
                    pane,
                    selection: if primary {
                        ClipSel::Primary
                    } else {
                        ClipSel::Clipboard
                    },
                }
            }
        };
        asyncio::write_frame(wr, &f).await?;
        Ok(true)
    }

    async fn send_model<W: AsyncWrite + Unpin>(&mut self, wr: &mut W) -> Result<()> {
        self.model_rev = *self.server.model_rev.borrow();
        let model = self.server.with_core(|c| c.model.clone());
        let seen = self
            .server
            .with_core(|c| c.store.reads("local").unwrap_or_default());
        let focus = self.server.client_focus(&self.client_id);
        self.focused = focus.pane.clone();
        asyncio::write_frame(
            wr,
            &ServerFrame::Model {
                model: Box::new(model),
                focus,
                seen,
            },
        )
        .await?;
        Ok(())
    }

    fn touch(&self) {
        *self.server.geometry_leader.lock().unwrap() = Some(self.client_id.clone());
        if let Some(st) = self.server.clients.lock().unwrap().get_mut(&self.client_id) {
            st.last_active = Some(Instant::now());
        }
    }

    async fn on_client<W: AsyncWrite + Unpin>(
        &mut self,
        f: ClientFrame,
        wr: &mut W,
    ) -> Result<bool> {
        let f = match crate::session_api::readonly_filter(&self.client_id, f) {
            Ok(f) => f,
            Err(reply) => {
                if let Some(r) = reply {
                    asyncio::write_frame(wr, &r).await?;
                }
                return Ok(true);
            }
        };
        match f {
            ClientFrame::Ack { pane, epoch, rev } => {
                if let Some(v) = self.views.get_mut(&pane)
                    && v.epoch == epoch
                    && rev <= v.rev
                {
                    v.unacked = v.unacked.saturating_sub(1);
                }
            }
            ClientFrame::Key {
                input_id,
                pane,
                key,
            } => {
                self.touch();
                let bytes = encode::encode_key(&key, &input_modes(&self.server, &pane));
                self.write_input(input_id, &pane, bytes, wr).await?;
            }
            ClientFrame::RawInput {
                input_id,
                pane,
                bytes,
            } => {
                self.touch();
                self.write_input(input_id, &pane, bytes, wr).await?;
            }
            ClientFrame::Mouse {
                input_id,
                pane,
                event,
            } => {
                self.touch();
                let bytes = encode::encode_mouse(&event, &input_modes(&self.server, &pane));
                self.write_input(input_id, &pane, bytes, wr).await?;
            }
            ClientFrame::Paste {
                input_id,
                pane,
                text,
            } => {
                self.touch();
                // Drops translated into the host inbox are `/vibeke/inbox/…` in a box (06 A11.4).
                let text = crate::sandbox::paste_text(&self.server, &pane, text);
                let bytes = encode::encode_paste(&text, &input_modes(&self.server, &pane));
                self.write_input(input_id, &pane, bytes, wr).await?;
            }
            ClientFrame::Focus { pane } => {
                self.touch();
                let prev = self.focused.clone();
                self.server.focus_pane(&self.client_id, &pane);
                // Focus events to apps that asked for them (03 §7.2).
                if prev.as_deref() != Some(&pane) {
                    if let Some(p) = prev {
                        let b = encode::encode_focus(false, &input_modes(&self.server, &p));
                        if !b.is_empty()
                            && let Some(rt) = self.server.pane_rt(&p)
                        {
                            rt.send(crate::pane::PaneCmd::Input {
                                id: self.server.next_internal_input_id(),
                                bytes: b,
                                ack: None,
                            });
                        }
                    }
                    let b = encode::encode_focus(true, &input_modes(&self.server, &pane));
                    if !b.is_empty()
                        && let Some(rt) = self.server.pane_rt(&pane)
                    {
                        rt.send(crate::pane::PaneCmd::Input {
                            id: self.server.next_internal_input_id(),
                            bytes: b,
                            ack: None,
                        });
                    }
                }
                self.focused = Some(pane);
            }
            ClientFrame::ViewHint { panes, active } => {
                if active {
                    self.touch();
                }
                let leader = self.server.geometry_leader.lock().unwrap().clone();
                if leader.as_deref() == Some(&self.client_id) {
                    for p in &panes {
                        if let Some(rt) = self.server.pane_rt(&p.pane) {
                            rt.resize(p.cols, p.rows);
                        }
                    }
                }
                if let Some(st) = self.server.clients.lock().unwrap().get_mut(&self.client_id) {
                    st.visible = panes.iter().map(|p| p.pane.clone()).collect();
                    st.host_focused = active;
                }
                self.visible = panes;
                self.views
                    .retain(|k, _| self.visible.iter().any(|v| &v.pane == k));
            }
            ClientFrame::Resync { pane } => {
                // A resync is also how a client asks for the pixels of images it evicted
                // (03 §9): forget that the pane's images were sent, so the keyframe brings
                // them again.
                if let Some(v) = self.views.remove(&pane) {
                    for p in &v.images {
                        self.images_sent.remove(&p.hash);
                    }
                }
            }
            ClientFrame::FetchHistory {
                req,
                pane,
                start,
                count,
            } => {
                let f = self.history(req, &pane, start, count);
                asyncio::write_frame(wr, &f).await?;
            }
            ClientFrame::Command { req, json } => {
                let ctx = Ctx {
                    client_id: self.client_id.clone(),
                    kind: "tui".into(),
                    pane_scope: None,
                    remote: self.remote,
                };
                let resp = api::handle_line(&self.server, &ctx, &json).await;
                asyncio::write_frame(wr, &ServerFrame::CommandResult { req, json: resp }).await?;
                self.focused = self.server.client_focus(&self.client_id).pane;
            }
            ClientFrame::Ping { nonce } => {
                asyncio::write_frame(
                    wr,
                    &ServerFrame::Pong {
                        nonce,
                        server_ts_ms: vk_store::now_ms(),
                    },
                )
                .await?;
            }
            ClientFrame::Detach => return Ok(false),
            ClientFrame::Subscribe { types, after } => {
                self.pending_sub = Some((types.into_iter().take(64).collect(), after));
            }
            ClientFrame::ScrollView {
                pane,
                offset,
                total,
            } => crate::compat::scroll_changed(&self.server, &self.client_id, &pane, offset, total),
            ClientFrame::MediaView {
                panes,
                shm,
                key_releases,
            } => {
                self.media.on_view(&self.server, panes, shm, key_releases);
            }
            ClientFrame::MediaAck { pane, seq } => self.media.on_ack(&pane, seq),
            ClientFrame::ClipboardReply { req, pane, data } => {
                let _ = crate::term_effects::clipboard_reply(
                    &self.server,
                    &self.client_id,
                    req,
                    &pane,
                    data,
                );
            }
            ClientFrame::Browser {
                input_id,
                pane,
                cmd,
            } => {
                self.touch();
                self.media.on_cmd(&self.server, &pane, cmd);
                asyncio::write_frame(
                    wr,
                    &ServerFrame::InputAck {
                        input_id,
                        status: AckStatus::Written,
                    },
                )
                .await?;
            }
        }
        Ok(true)
    }

    async fn write_input<W: AsyncWrite + Unpin>(
        &mut self,
        input_id: u64,
        pane: &str,
        bytes: Vec<u8>,
        wr: &mut W,
    ) -> Result<()> {
        let status = if bytes.is_empty() {
            AckStatus::Written
        } else if let Some(reason) = self.server.agents.input_blocked(pane) {
            tracing::debug!(pane, reason, "input rejected");
            AckStatus::Rejected
        } else if let Some(rt) = self.server.pane_rt(pane) {
            // Don't wait for the holder ack on the hot path: a small task forwards it to the
            // session loop once the holder confirms the PTY write (07 §3.2), so the client
            // can drop the input from its resend ledger.
            let id = holder_input_id(&self.client_id, input_id);
            *rt.last_input.lock().unwrap() = Some(Instant::now());
            // Typing into an idle-suspended (paused) box wakes it (13 §11).
            crate::sandbox::extras::touch_pane(&self.server, pane);
            let (tx, rx) = tokio::sync::oneshot::channel();
            rt.send(crate::pane::PaneCmd::Input {
                id,
                bytes,
                ack: Some(tx),
            });
            let acks = self.acks.clone();
            tokio::spawn(async move {
                let st = rx.await.ok();
                let _ = acks.send((input_id, ack_status(st)));
            });
            return Ok(());
        } else {
            AckStatus::Rejected
        };
        asyncio::write_frame(wr, &ServerFrame::InputAck { input_id, status }).await?;
        Ok(())
    }

    fn history(&self, req: u64, pane: &str, start: u32, count: u32) -> ServerFrame {
        let Some(rt) = self.server.pane_rt(pane) else {
            return ServerFrame::History {
                pane: pane.into(),
                req,
                start,
                total: 0,
                lines: vec![],
            };
        };
        let sc = rt.screen.lock().unwrap();
        let mem = sc.engine.history_len() as u32;
        let total_scrolled = sc.engine.scrolled_total();
        let first_mem_abs = total_scrolled.saturating_sub(mem as u64);
        drop(sc);
        // Archived rows (older than memory) are addressed after in-memory ones by the client:
        // index space = [archive rows .. memory rows], oldest first.
        let archived: u32 = first_mem_abs.min(u32::MAX as u64) as u32;
        let total = archived + mem;
        let end = (start + count).min(total);
        let mut lines = Vec::new();
        if start < archived {
            let a_end = end.min(archived);
            if let Ok(rows) =
                self.server
                    .archive
                    .lock()
                    .unwrap()
                    .read(pane, start as u64, a_end as u64)
            {
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
                for n in start..a_end {
                    lines.push(map.remove(&(n as u64)).unwrap_or_default());
                }
            }
        }
        if end > archived {
            let sc = rt.screen.lock().unwrap();
            for i in start.max(archived)..end {
                lines.push(
                    sc.engine
                        .history_row((i - archived) as usize)
                        .unwrap_or_default(),
                );
            }
        }
        ServerFrame::History {
            pane: pane.into(),
            req,
            start,
            total,
            lines,
        }
    }

    async fn send_panes<W: AsyncWrite + Unpin>(&mut self, wr: &mut W) -> Result<()> {
        let now = Instant::now();
        for v in self.visible.clone() {
            let Some(rt) = self.server.pane_rt(&v.pane) else {
                continue;
            };
            let screen_rev = rt.rev();
            let focused = self.focused.as_deref() == Some(&v.pane);
            let echo = rt
                .last_input
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < Duration::from_millis(50));
            if let Some(view) = self.views.get(&v.pane) {
                if view.screen_rev == screen_rev {
                    continue;
                }
                if view.unacked >= MAX_UNACKED {
                    continue;
                }
                if !echo && now < view.last_sent + self.interval(&v.pane) {
                    continue;
                }
            }
            let (epoch, rows, cursor, modes, title, cols, nrows, scrolled, places, pixels) = {
                let sc = rt.screen.lock().unwrap();
                if sc.recovering {
                    continue;
                }
                // Kitty placements, and the pixels of images this client lacks (03 §9).
                let placements = sc.engine.image_placements();
                let mut pixels: Vec<(String, u32, u32, Vec<u8>)> = Vec::new();
                for p in &placements {
                    let k = image_key(&p.hash);
                    if !self.images_sent.contains(&k)
                        && !pixels.iter().any(|(x, ..)| *x == k)
                        && let Some(px) = sc.engine.image_rgba(p)
                    {
                        pixels.push((k, p.width, p.height, px));
                    }
                }
                let places: Vec<ImagePlace> = placements
                    .iter()
                    .map(|p| ImagePlace {
                        hash: image_key(&p.hash),
                        width: p.width,
                        height: p.height,
                        col: p.col,
                        row: p.row,
                        cols: p.cols.min(u16::MAX as u32) as u16,
                        rows: p.rows.min(u16::MAX as u32) as u16,
                        z: p.z,
                    })
                    .collect();
                (
                    sc.epoch,
                    sc.engine.visible_rows(),
                    sc.engine.cursor(),
                    sc.engine.modes(),
                    sc.engine.title(),
                    sc.engine.cols(),
                    sc.engine.rows(),
                    sc.engine.scrolled_total(),
                    places,
                    pixels,
                )
            };
            match self.views.get_mut(&v.pane) {
                Some(view) if view.epoch == epoch && view.rows.len() == rows.len() => {
                    // Spinner throttling (03 §12.3): unfocused panes with tiny changes go at bg fps.
                    let changed: Vec<u16> = (0..rows.len())
                        .filter(|&i| rows[i] != view.rows[i])
                        .map(|i| i as u16)
                        .collect();
                    let tiny = changed.len() <= 1 && scrolled == view.scrolled;
                    if !focused
                        && tiny
                        && !echo
                        && now
                            < view.last_sent
                                + Duration::from_millis(1000 / self.bg_fps.max(1) as u64)
                    {
                        continue;
                    }
                    let mut ops = Vec::new();
                    let n = scrolled.saturating_sub(view.scrolled);
                    let mut base = view.rows.clone();
                    if n > 0 && (n as usize) < rows.len() && !modes.alt_screen {
                        let n = n as usize;
                        if rows[..rows.len() - n] == base[n..]
                            || rows[..rows.len() - n]
                                .iter()
                                .zip(&base[n..])
                                .filter(|(a, b)| a == b)
                                .count()
                                * 2
                                > rows.len() - n
                        {
                            ops.push(DiffOp::ScrollUp { n: n as u16 });
                            base.drain(..n);
                            base.extend(std::iter::repeat_n(Row::default(), n));
                        }
                    }
                    let upd: Vec<(u16, Row)> = (0..rows.len())
                        .filter(|&i| rows[i] != base[i])
                        .map(|i| (i as u16, rows[i].clone()))
                        .collect();
                    if !upd.is_empty() {
                        ops.push(DiffOp::Rows(upd));
                    }
                    let base_rev = view.rev;
                    view.rev += 1;
                    let f = ServerFrame::PaneDiff {
                        pane: v.pane.clone(),
                        epoch,
                        base_rev,
                        rev: view.rev,
                        ops,
                        cursor,
                        modes,
                        title: title.clone(),
                    };
                    view.rows = rows;
                    view.cursor = cursor;
                    view.modes = modes;
                    view.title = title;
                    view.unacked += 1;
                    view.last_sent = now;
                    view.screen_rev = screen_rev;
                    view.scrolled = scrolled;
                    view.dirty_since_ack = true;
                    asyncio::write_frame(wr, &f).await?;
                }
                _ => {
                    let f = ServerFrame::PaneFull {
                        pane: v.pane.clone(),
                        epoch,
                        rev: 1,
                        cols,
                        rows: nrows,
                        lines: rows.clone(),
                        cursor,
                        modes,
                        title: title.clone(),
                    };
                    self.views.insert(
                        v.pane.clone(),
                        PaneView {
                            epoch,
                            rev: 1,
                            rows,
                            cursor,
                            modes,
                            title,
                            unacked: 1,
                            last_sent: now,
                            screen_rev,
                            scrolled,
                            dirty_since_ack: true,
                            images: Vec::new(),
                        },
                    );
                    asyncio::write_frame(wr, &f).await?;
                }
            }
            self.send_images(wr, &v.pane, epoch, places, pixels).await?;
        }
        Ok(())
    }

    /// After a pane's cell frame: pixels of images new to this client, then the placement
    /// set when it changed (03 §9).
    async fn send_images<W: AsyncWrite + Unpin>(
        &mut self,
        wr: &mut W,
        pane: &str,
        epoch: u32,
        places: Vec<ImagePlace>,
        pixels: Vec<(String, u32, u32, Vec<u8>)>,
    ) -> Result<()> {
        for (hash, width, height, px) in pixels {
            let rgba_z = vk_browser::kitty::zlib(&px, 1);
            self.images_sent.insert(hash.clone());
            asyncio::write_frame(
                wr,
                &ServerFrame::Image {
                    hash,
                    width,
                    height,
                    rgba_z,
                },
            )
            .await?;
        }
        if let Some(view) = self.views.get_mut(pane)
            && view.images != places
        {
            view.images = places.clone();
            asyncio::write_frame(
                wr,
                &ServerFrame::PaneImages {
                    pane: pane.to_string(),
                    epoch,
                    places,
                },
            )
            .await?;
        }
        Ok(())
    }
}

fn unicode_cols(s: &str) -> u16 {
    s.chars().count().min(u16::MAX as usize) as u16
}

/// Await a holder ack for an input (CLI / API paths that must know the input was written).
pub async fn write_and_ack(server: &Server, pane: &str, id: u64, bytes: Vec<u8>) -> InputStatus {
    match server.pane_rt(pane) {
        Some(rt) => rt.input(id, bytes).await,
        None => InputStatus::ChildExited,
    }
}

#[cfg(test)]
#[path = "render_push_tests.rs"]
mod push_tests;
