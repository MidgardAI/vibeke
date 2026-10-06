//! Local → remote file transfer for pastes and drops (06 A10, A11). Only the local client
//! initiates transfers, and only for content the user pasted or dropped.
//!
//! Bulk bytes never travel on the render stream: every transfer runs in its own task on a
//! dedicated control connection (`blob.begin` / `blob.append` / `blob.commit`, chunked), so key
//! input keeps flowing while files upload. Transfers are keyed by a unique [`TransferId`], and
//! the translated paste goes to the pane the transfer was started for.

use crate::app::{App, Connector, Incoming, Popup};
use crate::paste::{self, ParsedPaste};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::render::ClientFrame;

/// Raw bytes per `blob.append` (the server accepts up to 1 MiB).
const CHUNK: usize = 512 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// Unique per transfer: the machine and pane it was started for plus a client-wide counter.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TransferId {
    pub machine: usize,
    pub pane: String,
    pub seq: u64,
}

#[derive(Clone, Debug)]
pub enum Source {
    Path(PathBuf),
    Bytes(Arc<Vec<u8>>),
    /// An already opened, checked file (a browser drop): read positionally, exactly `size`
    /// bytes, never reopened by path.
    File(Arc<std::fs::File>),
}

/// One file to upload.
#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub size: u64,
    pub src: Source,
}

pub struct Transfer {
    pub original: String,
    /// `None` for an image paste: the resulting path is pasted as shell-escaped text.
    pub parsed: Option<ParsedPaste>,
    pub items: Vec<Item>,
    pub results: Vec<Option<String>>,
    pub sent: u64,
    pub total: u64,
    pub cancel: Arc<AtomicBool>,
    /// Files for a browser pane's page (06 B3.2): when done, the uploaded paths go to the
    /// page as `BrowserCmd::DropFiles` instead of being pasted.
    pub browser: bool,
}

/// Progress reported by a transfer task back to the UI loop.
#[derive(Debug)]
pub enum UploadEvent {
    Progress {
        id: TransferId,
        sent: u64,
    },
    FileDone {
        id: TransferId,
        index: usize,
        path: String,
    },
    Failed {
        id: TransferId,
        message: String,
    },
}

/// What a transfer task needs to open its own control connection.
pub struct Worker {
    pub connectors: Vec<Arc<Connector>>,
    pub inc: mpsc::UnboundedSender<Incoming>,
}

#[derive(Default)]
pub struct Uploads {
    pub transfers: HashMap<TransferId, Transfer>,
    next_seq: u64,
    /// `None` in tests: transfers are tracked but no task is spawned.
    pub worker: Option<Worker>,
}

fn max_bytes(app: &App) -> u64 {
    app.config.paste.max_auto_bytes.0
}

/// Validate the dropped files. On any problem the original text is pasted instead.
fn prepare(
    app: &mut App,
    machine: usize,
    pane: &str,
    original: &str,
    parsed: &ParsedPaste,
    home: &Path,
) -> Option<Vec<Item>> {
    let mut items = Vec::new();
    let mut total = 0u64;
    for t in &parsed.tokens {
        let f = t.local_path(home);
        match std::fs::metadata(&f) {
            Ok(m) if m.is_dir() => {
                app.toast("directory drops are not supported yet — pasted the original path");
                send_paste(app, machine, pane, original.to_string());
                return None;
            }
            Ok(m) => {
                total += m.len();
                let name = f
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                items.push(Item {
                    name,
                    size: m.len(),
                    src: Source::Path(f),
                });
            }
            Err(_) => {
                send_paste(app, machine, pane, original.to_string());
                return None;
            }
        }
    }
    if total > max_bytes(app) {
        app.toast(format!(
            "{} MB is over paste.max_auto_bytes — pasted the original path",
            total >> 20
        ));
        send_paste(app, machine, pane, original.to_string());
        return None;
    }
    Some(items)
}

/// A paste/drop of local paths into a remote pane. With `paste.translate = "ask"` nothing is
/// uploaded until the user explicitly confirms; the other enabled modes upload directly.
pub fn translate_paste(
    app: &mut App,
    pane: &str,
    original: String,
    parsed: ParsedPaste,
    home: &Path,
) {
    let machine = app.cur;
    let Some(items) = prepare(app, machine, pane, &original, &parsed, home) else {
        return;
    };
    if matches!(app.config.paste.translate, vk_config::PasteTranslate::Ask) {
        app.mode = crate::app::Mode::Popup(Popup::PasteAsk {
            machine,
            pane: pane.to_string(),
            original,
            parsed,
            items,
            sel: 2,
        });
        return;
    }
    begin(app, machine, pane, original, Some(parsed), items);
}

/// Register a transfer and start its task. Returns its id.
pub fn begin(
    app: &mut App,
    machine: usize,
    pane: &str,
    original: String,
    parsed: Option<ParsedPaste>,
    items: Vec<Item>,
) -> TransferId {
    start(app, machine, pane, original, parsed, items, false)
}

fn start(
    app: &mut App,
    machine: usize,
    pane: &str,
    original: String,
    parsed: Option<ParsedPaste>,
    items: Vec<Item>,
    browser: bool,
) -> TransferId {
    let id = TransferId {
        machine,
        pane: pane.to_string(),
        seq: app.uploads.next_seq,
    };
    app.uploads.next_seq += 1;
    let cancel = Arc::new(AtomicBool::new(false));
    let total = items.iter().map(|i| i.size).sum();
    app.uploads.transfers.insert(
        id.clone(),
        Transfer {
            original,
            parsed,
            results: vec![None; items.len()],
            items: items.clone(),
            sent: 0,
            total,
            cancel: cancel.clone(),
            browser,
        },
    );
    app.toast(format!(
        "⇡ uploading {} file(s) to {} (prefix+shift+c cancels)",
        items.len(),
        app.machines[machine].label
    ));
    if let Some(w) = &app.uploads.worker
        && let Some(conn) = w.connectors.get(machine).cloned()
    {
        let stage = browser.then_some("browser");
        tokio::spawn(run_transfer(
            conn,
            w.inc.clone(),
            id.clone(),
            items,
            cancel,
            stage,
        ));
    }
    id
}

/// Upload files to media host `host`'s drop directory for browser pane `pane`'s page
/// (`blob.commit {stage: "browser"}`; also when the media host is this machine, so the page
/// only ever gets a private copy): when done they are dropped into the page
/// (`BrowserCmd::DropFiles`).
pub fn begin_browser(app: &mut App, host: usize, pane: &str, items: Vec<Item>) -> TransferId {
    start(app, host, pane, String::new(), None, items, true)
}

/// Keys inside the paste confirmation popup. Only explicit keys act; typing is ignored.
pub fn ask_key(app: &mut App, ev: KeyEvent, popup: Popup) {
    let Popup::PasteAsk {
        machine,
        pane,
        original,
        parsed,
        items,
        sel,
    } = popup
    else {
        return;
    };
    // 0 = upload & paste translated paths, 1 = paste original text, 2 = cancel (the default).
    let choice = match ev.key {
        Key::Char('u' | 'U') => Some(0),
        Key::Char('o' | 'O') => Some(1),
        Key::Char('c' | 'C') | Key::Named(NamedKey::Escape) => Some(2),
        Key::Named(NamedKey::Enter) => Some(sel),
        _ => None,
    };
    match choice {
        Some(0) => {
            if pane_exists(app, machine, &pane) {
                begin(app, machine, &pane, original, Some(parsed), items);
            }
        }
        Some(1) => {
            send_paste(app, machine, &pane, original);
        }
        Some(_) => app.toast("paste cancelled"),
        None => {
            let sel = match ev.key {
                Key::Named(NamedKey::Left) | Key::Named(NamedKey::Up) => (sel + 2) % 3,
                Key::Named(NamedKey::Right)
                | Key::Named(NamedKey::Tab)
                | Key::Named(NamedKey::Down) => (sel + 1) % 3,
                _ => sel,
            };
            app.mode = crate::app::Mode::Popup(Popup::PasteAsk {
                machine,
                pane,
                original,
                parsed,
                items,
                sel,
            });
        }
    }
}

/// ctrl+v with an image on the local clipboard and a remote pane focused (06 A10).
pub fn image_paste(app: &mut App) {
    let Some(pane) = app.focused_pane() else {
        return;
    };
    match crate::clipboard::os_clipboard_image() {
        Ok(Some((mime, data))) => {
            if data.len() as u64 > max_bytes(app) {
                app.toast("clipboard image too large");
                return;
            }
            let ext = if mime.contains("jpeg") { "jpg" } else { "png" };
            let name = format!(
                "clipboard-{}.{ext}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            let item = Item {
                name,
                size: data.len() as u64,
                src: Source::Bytes(Arc::new(data)),
            };
            let machine = app.cur;
            begin(app, machine, &pane, String::new(), None, vec![item]);
        }
        // No image: ctrl+v goes to the pane as usual.
        _ => {
            let id = app.next_input;
            app.next_input += 1;
            let key = vk_proto::input::KeyEvent::new(
                vk_proto::input::Key::Char('v'),
                vk_proto::input::Mods::CTRL,
            );
            app.m().send(ClientFrame::Key {
                input_id: id,
                pane,
                key,
            });
        }
    }
}

fn pane_exists(app: &App, machine: usize, pane: &str) -> bool {
    app.machines
        .get(machine)
        .is_some_and(|m| m.panes.contains_key(pane) || m.model.panes.iter().any(|p| p.id == pane))
}

/// Cancel every active transfer (aborts the server-side uploads).
pub fn cancel_all(app: &mut App) {
    let n = app.uploads.transfers.len();
    if n == 0 {
        app.toast("no transfer in progress");
        return;
    }
    for (_, t) in app.uploads.transfers.drain() {
        t.cancel.store(true, Ordering::SeqCst);
    }
    app.toast(format!("cancelled {n} transfer(s)"));
}

/// Apply a task event. When the last file lands the translated text is pasted into the pane
/// the transfer was started for, if that pane still exists.
pub fn on_event(app: &mut App, ev: UploadEvent) {
    match ev {
        UploadEvent::Progress { id, sent } => {
            if let Some(t) = app.uploads.transfers.get_mut(&id) {
                t.sent = sent.min(t.total);
            }
        }
        UploadEvent::Failed { id, message } => {
            if let Some(t) = app.uploads.transfers.remove(&id) {
                app.toast(format!("✗ upload failed: {message}"));
                // Fall back to what the user pasted; the pane gets the original text.
                if t.parsed.is_some() && pane_exists(app, id.machine, &id.pane) {
                    send_paste(app, id.machine, &id.pane, t.original);
                }
            }
        }
        UploadEvent::FileDone { id, index, path } => {
            let Some(t) = app.uploads.transfers.get_mut(&id) else {
                return;
            };
            if index < t.results.len() {
                t.results[index] = Some(path);
            }
            if t.results.iter().any(Option::is_none) {
                return;
            }
            let t = app.uploads.transfers.remove(&id).unwrap();
            let paths: Vec<String> = t.results.into_iter().flatten().collect();
            if t.browser {
                let input_id = app.next_input;
                app.next_input += 1;
                let sent = app.machines[id.machine].send(ClientFrame::Browser {
                    input_id,
                    pane: id.pane.clone(),
                    cmd: vk_proto::render::BrowserCmd::DropFiles(paths),
                });
                if !sent {
                    app.toast(format!(
                        "✓ uploaded, but {} is offline — not dropped",
                        app.machines[id.machine].label
                    ));
                }
                return;
            }
            let text = match &t.parsed {
                Some(p) => paste::rewrite(&t.original, p, &paths),
                // Image paste: TUI harnesses (Claude, Codex) attach image paths pasted as text.
                None => paths.first().map(|p| shell_escape(p)).unwrap_or_default(),
            };
            if !pane_exists(app, id.machine, &id.pane) {
                app.toast("upload finished but its pane is gone — not pasted");
                return;
            }
            if send_paste(app, id.machine, &id.pane, text) {
                app.toast("✓ uploaded");
            } else {
                app.toast(format!(
                    "✓ uploaded, but {} is offline — not pasted",
                    app.machines[id.machine].label
                ));
            }
        }
    }
}

/// Status-bar text for active transfers.
pub fn status(app: &App) -> Option<String> {
    let n = app.uploads.transfers.len();
    if n == 0 {
        return None;
    }
    let (sent, total) = app
        .uploads
        .transfers
        .values()
        .fold((0u64, 0u64), |a, t| (a.0 + t.sent, a.1 + t.total));
    let pct = (sent * 100).checked_div(total).unwrap_or(100);
    Some(format!(
        "⇡ {n} upload{} {pct}% ({} / {})",
        if n == 1 { "" } else { "s" },
        human(sent),
        human(total)
    ))
}

pub fn human(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn shell_escape(p: &str) -> String {
    let mut out = String::new();
    for c in p.chars() {
        if c.is_whitespace() || "\\'\"()[]{}&;|<>*?$`!#".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Send a paste to a specific pane of a specific machine (not necessarily the focused one).
fn send_paste(app: &mut App, machine: usize, pane: &str, text: String) -> bool {
    let id = app.next_input;
    app.next_input += 1;
    app.machines.get(machine).is_some_and(|m| {
        m.send(ClientFrame::Paste {
            input_id: id,
            pane: pane.to_string(),
            text,
        })
    })
}

// ---- transfer task -------------------------------------------------------------------------

async fn rpc<R, W>(
    rd: &mut R,
    wr: &mut W,
    req: u64,
    method: &str,
    params: Value,
) -> Result<Value, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let line = json!({"jsonrpc":"2.0","id":req,"method":method,"params":params}).to_string();
    let io = async {
        wr.write_all(line.as_bytes()).await?;
        wr.write_all(b"\n").await?;
        wr.flush().await?;
        let mut buf = String::new();
        if rd.read_line(&mut buf).await? == 0 {
            return Err(std::io::Error::other("connection closed"));
        }
        Ok(buf)
    };
    let buf = tokio::time::timeout(RPC_TIMEOUT, io)
        .await
        .map_err(|_| format!("{method}: timed out"))?
        .map_err(|e| format!("{method}: {e}"))?;
    let v: Value = serde_json::from_str(&buf).map_err(|e| format!("{method}: {e}"))?;
    if let Some(e) = v.get("error") {
        return Err(e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string());
    }
    Ok(v["result"].clone())
}

async fn run_transfer(
    conn: Arc<Connector>,
    inc: mpsc::UnboundedSender<Incoming>,
    id: TransferId,
    items: Vec<Item>,
    cancel: Arc<AtomicBool>,
    stage: Option<&'static str>,
) {
    let emit = |e: UploadEvent| {
        let _ = inc.send(Incoming::Upload(e));
    };
    let r = transfer_items(&conn, &emit, &id, &items, &cancel, stage).await;
    if let Err(message) = r
        && !cancel.load(Ordering::SeqCst)
    {
        emit(UploadEvent::Failed { id, message });
    }
}

async fn transfer_items(
    conn: &Arc<Connector>,
    emit: &impl Fn(UploadEvent),
    id: &TransferId,
    items: &[Item],
    cancel: &AtomicBool,
    stage: Option<&'static str>,
) -> Result<(), String> {
    let stream = (conn)().await.map_err(|e| format!("connect: {e}"))?;
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let mut req = 1u64;
    let mut sent = 0u64;
    for (index, item) in items.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let begun = rpc(
            &mut rd,
            &mut wr,
            req,
            "blob.begin",
            json!({"name": item.name, "size": item.size}),
        )
        .await?;
        req += 1;
        let upload_id = begun["upload_id"]
            .as_str()
            .ok_or("blob.begin: no upload_id")?
            .to_string();
        let res = send_item(
            &mut rd, &mut wr, &mut req, emit, id, item, &upload_id, &mut sent, cancel,
        )
        .await;
        match res {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                // Cancelled or failed: tell the server to drop the staged bytes.
                let _ = rpc(
                    &mut rd,
                    &mut wr,
                    req,
                    "blob.abort",
                    json!({"upload_id": upload_id}),
                )
                .await;
                return res.map(|_| ());
            }
        }
        let done = rpc(
            &mut rd,
            &mut wr,
            req,
            "blob.commit",
            match stage {
                Some(st) => json!({"upload_id": upload_id, "stage": st}),
                None => json!({"upload_id": upload_id}),
            },
        )
        .await?;
        req += 1;
        if done["size"].as_u64() != Some(item.size) {
            return Err(format!("{}: size mismatch after upload", item.name));
        }
        let path = done["path_on_machine"]
            .as_str()
            .or(done["path"].as_str())
            .ok_or("blob.commit: no path")?
            .to_string();
        emit(UploadEvent::FileDone {
            id: id.clone(),
            index,
            path,
        });
    }
    Ok(())
}

/// The next chunk of an opened file at `offset` (positional, never past `size`); after the
/// last byte, a file that grew since it was checked is refused (the size read must match).
pub(crate) fn read_snapshot(
    f: &std::fs::File,
    offset: u64,
    size: u64,
    buf: &mut [u8],
) -> Result<usize, String> {
    use std::os::unix::fs::FileExt;
    let want = (size.saturating_sub(offset) as usize).min(buf.len());
    let n = f
        .read_at(&mut buf[..want], offset)
        .map_err(|e| e.to_string())?;
    if n == 0 && want > 0 {
        return Err("file shrank while uploading".into());
    }
    if offset + n as u64 >= size {
        let mut one = [0u8; 1];
        if f.read_at(&mut one, size).map_err(|e| e.to_string())? > 0 {
            return Err("file grew while uploading".into());
        }
    }
    Ok(n)
}

/// Stream one item in chunks. `Ok(false)` means cancelled.
#[allow(clippy::too_many_arguments)]
async fn send_item<R, W>(
    rd: &mut R,
    wr: &mut W,
    req: &mut u64,
    emit: &impl Fn(UploadEvent),
    id: &TransferId,
    item: &Item,
    upload_id: &str,
    sent: &mut u64,
    cancel: &AtomicBool,
) -> Result<bool, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut file = match &item.src {
        Source::Path(p) => Some(
            tokio::fs::File::open(p)
                .await
                .map_err(|e| format!("read {}: {e}", p.display()))?,
        ),
        Source::Bytes(_) | Source::File(_) => None,
    };
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK];
    while offset < item.size {
        if cancel.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let n = match (&mut file, &item.src) {
            (Some(f), _) => f
                .read(&mut buf)
                .await
                .map_err(|e| format!("read {}: {e}", item.name))?,
            (None, Source::Bytes(b)) => {
                let start = offset as usize;
                let end = (start + CHUNK).min(b.len());
                buf[..end - start].copy_from_slice(&b[start..end]);
                end - start
            }
            (None, Source::File(f)) => read_snapshot(f, offset, item.size, &mut buf)
                .map_err(|e| format!("{}: {e}", item.name))?,
            _ => 0,
        };
        if n == 0 {
            return Err(format!("{}: file changed while uploading", item.name));
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
        rpc(
            rd,
            wr,
            *req,
            "blob.append",
            json!({"upload_id": upload_id, "offset": offset, "data_b64": data}),
        )
        .await?;
        *req += 1;
        offset += n as u64;
        *sent += n as u64;
        emit(UploadEvent::Progress {
            id: id.clone(),
            sent: *sent,
        });
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Mode;

    use crate::app::test_app;
    fn drops(rx: &mut mpsc::UnboundedReceiver<ClientFrame>) -> Vec<(String, String)> {
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Paste { pane, text, .. } = f {
                v.push((pane, text));
            }
        }
        v
    }

    fn file(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn key(c: Key) -> KeyEvent {
        KeyEvent::new(c, vk_proto::input::Mods::empty())
    }

    #[test]
    fn identical_drops_in_two_panes_stay_separate() {
        let (mut app, mut rxs) = test_app(2);
        let dir = tempfile::tempdir().unwrap();
        let text = file(dir.path(), "a.txt", "hello");
        let parsed = paste::parse_paste(&text).unwrap();
        let items = prepare(&mut app, 1, "p1", &text, &parsed, Path::new("/")).unwrap();
        let a = begin(
            &mut app,
            1,
            "p1",
            text.clone(),
            Some(parsed.clone()),
            items.clone(),
        );
        let b = begin(
            &mut app,
            1,
            "p2",
            text.clone(),
            Some(parsed.clone()),
            items.clone(),
        );
        let c = begin(&mut app, 0, "p1", text.clone(), Some(parsed), items);
        assert_eq!(app.uploads.transfers.len(), 3);
        assert!(a != b && b != c && a != c);
        // Finish b first, then a; each paste lands in its own pane on its own machine.
        on_event(
            &mut app,
            UploadEvent::FileDone {
                id: b,
                index: 0,
                path: "/in/B/a.txt".into(),
            },
        );
        assert_eq!(app.uploads.transfers.len(), 2);
        on_event(
            &mut app,
            UploadEvent::FileDone {
                id: a,
                index: 0,
                path: "/in/A/a.txt".into(),
            },
        );
        on_event(
            &mut app,
            UploadEvent::FileDone {
                id: c,
                index: 0,
                path: "/in/C/a.txt".into(),
            },
        );
        assert_eq!(
            drops(&mut rxs[1]),
            vec![
                ("p2".into(), "/in/B/a.txt".into()),
                ("p1".into(), "/in/A/a.txt".into())
            ]
        );
        assert_eq!(
            drops(&mut rxs[0]),
            vec![("p1".into(), "/in/C/a.txt".into())]
        );
        assert!(app.uploads.transfers.is_empty());
    }

    #[test]
    fn finished_transfer_pastes_only_if_pane_still_exists() {
        let (mut app, mut rxs) = test_app(2);
        let dir = tempfile::tempdir().unwrap();
        let text = file(dir.path(), "a.txt", "hello");
        let parsed = paste::parse_paste(&text).unwrap();
        let items = prepare(&mut app, 1, "p1", &text, &parsed, Path::new("/")).unwrap();
        let id = begin(&mut app, 1, "p1", text, Some(parsed), items);
        app.machines[1].panes.remove("p1");
        on_event(
            &mut app,
            UploadEvent::FileDone {
                id,
                index: 0,
                path: "/in/x/a.txt".into(),
            },
        );
        assert!(drops(&mut rxs[1]).is_empty());
    }

    #[test]
    fn ask_mode_does_not_upload_without_explicit_consent() {
        let (mut app, mut rxs) = test_app(2);
        app.cur = 1;
        let dir = tempfile::tempdir().unwrap();
        let text = file(dir.path(), "a.txt", "hello");
        let parsed = paste::parse_paste(&text).unwrap();
        translate_paste(&mut app, "p1", text.clone(), parsed, Path::new("/"));
        assert!(app.uploads.transfers.is_empty());
        assert!(matches!(app.mode, Mode::Popup(Popup::PasteAsk { .. })));
        // Typing, including the text of the paste itself, never confirms.
        // (every char except the explicit u/o/c keys, which are covered separately)
        for c in "hell yes plase\n/\\ xyzAB!".chars() {
            app.on_key(key(Key::Char(c)));
            assert!(app.uploads.transfers.is_empty(), "char {c:?} confirmed");
            assert!(matches!(app.mode, Mode::Popup(Popup::PasteAsk { .. })));
        }
        // Left then Right returns the highlight to the default (cancel) button.
        for k in [NamedKey::Backspace, NamedKey::Left, NamedKey::Right] {
            app.on_key(key(Key::Named(k)));
            assert!(app.uploads.transfers.is_empty());
            assert!(matches!(app.mode, Mode::Popup(Popup::PasteAsk { .. })));
        }
        assert!(drops(&mut rxs[1]).is_empty());
        // Enter on the default (cancel) button cancels.
        app.on_key(key(Key::Named(NamedKey::Enter)));
        assert!(app.uploads.transfers.is_empty());
        assert!(matches!(app.mode, Mode::Normal));
        assert!(drops(&mut rxs[1]).is_empty());
    }

    #[test]
    fn ask_mode_explicit_choices() {
        let dir = tempfile::tempdir().unwrap();
        for (k, uploads, pasted) in [('u', 1usize, 0usize), ('o', 0, 1), ('c', 0, 0)] {
            let (mut app, mut rxs) = test_app(2);
            app.cur = 1;
            let text = file(dir.path(), "a.txt", "hello");
            let parsed = paste::parse_paste(&text).unwrap();
            translate_paste(&mut app, "p1", text, parsed, Path::new("/"));
            app.on_key(key(Key::Char(k)));
            assert_eq!(app.uploads.transfers.len(), uploads, "key {k}");
            assert_eq!(drops(&mut rxs[1]).len(), pasted, "key {k}");
            assert!(matches!(app.mode, Mode::Normal));
        }
    }

    #[test]
    fn auto_mode_uploads_without_asking() {
        let (mut app, _rxs) = test_app(2);
        app.cur = 1;
        app.config.paste.translate = vk_config::PasteTranslate::PathsOnly;
        let dir = tempfile::tempdir().unwrap();
        let text = file(dir.path(), "a.txt", "hello");
        let parsed = paste::parse_paste(&text).unwrap();
        translate_paste(&mut app, "p1", text, parsed, Path::new("/"));
        assert_eq!(app.uploads.transfers.len(), 1);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn cancel_drops_transfers_and_late_events_are_ignored() {
        let (mut app, mut rxs) = test_app(1);
        let dir = tempfile::tempdir().unwrap();
        let text = file(dir.path(), "a.txt", "hello");
        let parsed = paste::parse_paste(&text).unwrap();
        let items = prepare(&mut app, 0, "p1", &text, &parsed, Path::new("/")).unwrap();
        let id = begin(&mut app, 0, "p1", text, Some(parsed), items);
        let flag = app.uploads.transfers[&id].cancel.clone();
        cancel_all(&mut app);
        assert!(flag.load(Ordering::SeqCst));
        on_event(
            &mut app,
            UploadEvent::FileDone {
                id,
                index: 0,
                path: "/x".into(),
            },
        );
        assert!(drops(&mut rxs[0]).is_empty());
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    /// A fake control server that checks the chunked protocol and reassembles the file.
    async fn fake_server(stream: tokio::io::DuplexStream) -> Vec<u8> {
        let (rd, mut wr) = tokio::io::split(stream);
        let mut rd = BufReader::new(rd);
        let mut got = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if rd.read_line(&mut line).await.unwrap() == 0 {
                return got;
            }
            let v: Value = serde_json::from_str(&line).unwrap();
            let result = match v["method"].as_str().unwrap() {
                "blob.begin" => json!({"upload_id": "u1"}),
                "blob.append" => {
                    let p = &v["params"];
                    assert_eq!(p["offset"].as_u64().unwrap(), got.len() as u64);
                    let d = base64::engine::general_purpose::STANDARD
                        .decode(p["data_b64"].as_str().unwrap())
                        .unwrap();
                    assert!(d.len() <= 1 << 20);
                    got.extend(d);
                    json!({"offset": got.len()})
                }
                "blob.commit" => json!({"size": got.len(), "path_on_machine": "/in/abc/f.bin"}),
                m => panic!("unexpected {m}"),
            };
            let r = json!({"jsonrpc":"2.0","id":v["id"],"result":result});
            wr.write_all(format!("{r}\n").as_bytes()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn large_file_goes_out_in_bounded_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let data: Vec<u8> = (0..(CHUNK * 3 + 17)).map(|i| (i % 253) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let (client, server) = tokio::io::duplex(1 << 16);
        let srv = tokio::spawn(fake_server(server));
        let slot = std::sync::Mutex::new(Some(client));
        let conn: Arc<Connector> = Arc::new(Box::new(move || {
            let s = slot.lock().unwrap().take().unwrap();
            Box::pin(async move { Ok(Box::new(s) as crate::app::Stream) })
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = TransferId {
            machine: 0,
            pane: "p1".into(),
            seq: 0,
        };
        let item = Item {
            name: "f.bin".into(),
            size: data.len() as u64,
            src: Source::Path(path),
        };
        run_transfer(
            conn,
            tx,
            id,
            vec![item],
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await;
        assert_eq!(srv.await.unwrap(), data);
        let mut done = None;
        let mut last = 0;
        while let Ok(Incoming::Upload(e)) = rx.try_recv() {
            match e {
                UploadEvent::Progress { sent, .. } => last = sent,
                UploadEvent::FileDone { path, .. } => done = Some(path),
                UploadEvent::Failed { message, .. } => panic!("{message}"),
            }
        }
        assert_eq!(last, data.len() as u64);
        assert_eq!(done.as_deref(), Some("/in/abc/f.bin"));
    }
}
