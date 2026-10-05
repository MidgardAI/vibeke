//! Local → remote file transfer for pastes and drops (06 A10, A11). Only the local client
//! initiates transfers, and only for content the user pasted or dropped.

use crate::app::{App, Pending};
use crate::paste::{self, ParsedPaste};
use base64::Engine;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use vk_proto::render::ClientFrame;

const IMAGE_MARK: &str = "\u{0}image";

#[derive(Default)]
pub struct Uploads {
    /// original paste text → (parsed tokens, uploaded remote paths)
    pub pending: HashMap<String, (Option<ParsedPaste>, Vec<Option<String>>)>,
}

fn max_bytes(app: &App) -> u64 {
    app.config.paste.max_auto_bytes.0
}

/// Upload every dropped/pasted local file to the focused remote machine's inbox, then paste
/// the rewritten text (same escaping style, same basenames).
pub fn translate_paste(
    app: &mut App,
    pane: &str,
    original: String,
    parsed: ParsedPaste,
    home: &Path,
) {
    let files: Vec<std::path::PathBuf> = parsed.tokens.iter().map(|t| t.local_path(home)).collect();
    let mut total = 0u64;
    for f in &files {
        match std::fs::metadata(f) {
            Ok(m) if m.is_dir() => {
                app.toast("directory drops are not supported yet — pasted the original path");
                return send_paste(app, pane, original);
            }
            Ok(m) => total += m.len(),
            Err(_) => return send_paste(app, pane, original),
        }
    }
    if total > max_bytes(app) {
        app.toast(format!(
            "{} MB is over paste.max_auto_bytes — pasted the original path",
            total >> 20
        ));
        return send_paste(app, pane, original);
    }
    let n = files.len();
    app.uploads
        .pending
        .insert(original.clone(), (Some(parsed), vec![None; n]));
    app.toast(format!("⇡ uploading {} file(s) to {}", n, app.m().label));
    for (i, f) in files.iter().enumerate() {
        let data = match std::fs::read(f) {
            Ok(d) => d,
            Err(e) => {
                app.uploads.pending.remove(&original);
                app.toast(format!("read {}: {e}", f.display()));
                return send_paste(app, pane, original);
            }
        };
        let name = f
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let params = json!({"name": name, "data_b64": base64::engine::general_purpose::STANDARD.encode(&data)});
        let req = app.next_req;
        app.next_req += 1;
        let jsonl =
            json!({"jsonrpc":"2.0","id":req,"method":"blob.put","params":params}).to_string();
        let m = app.m_mut();
        m.pending.insert(
            req,
            Pending::PasteUpload {
                pane: pane.to_string(),
                original: original.clone(),
                index: i,
                total: n,
            },
        );
        m.send(ClientFrame::Command { req, json: jsonl });
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
            app.uploads
                .pending
                .insert(IMAGE_MARK.into(), (None, vec![None]));
            let params = json!({"name": name, "mime": mime, "data_b64": base64::engine::general_purpose::STANDARD.encode(&data)});
            let req = app.next_req;
            app.next_req += 1;
            let jsonl =
                json!({"jsonrpc":"2.0","id":req,"method":"blob.put","params":params}).to_string();
            let m = app.m_mut();
            m.pending.insert(
                req,
                Pending::PasteUpload {
                    pane,
                    original: IMAGE_MARK.into(),
                    index: 0,
                    total: 1,
                },
            );
            m.send(ClientFrame::Command { req, json: jsonl });
            app.toast("⇡ uploading clipboard image");
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

pub fn uploaded(
    app: &mut App,
    machine: usize,
    pane: &str,
    original: &str,
    index: usize,
    total: usize,
    path: String,
) {
    let Some((_, results)) = app.uploads.pending.get_mut(original) else {
        return;
    };
    if index < results.len() {
        results[index] = Some(path);
    }
    if results.iter().any(Option::is_none) {
        return;
    }
    let (parsed, results) = app.uploads.pending.remove(original).unwrap();
    let paths: Vec<String> = results.into_iter().flatten().collect();
    let text = match parsed {
        Some(p) => paste::rewrite(original, &p, &paths),
        // Image paste: TUI harnesses (Claude, Codex) attach image paths pasted as text.
        None => paths.first().map(|p| shell_escape(p)).unwrap_or_default(),
    };
    let _ = total;
    let saved = app.cur;
    app.cur = machine;
    send_paste(app, pane, text);
    app.cur = saved;
    app.toast("✓ uploaded");
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

fn send_paste(app: &mut App, pane: &str, text: String) {
    let id = app.next_input;
    app.next_input += 1;
    app.m().send(ClientFrame::Paste {
        input_id: id,
        pane: pane.to_string(),
        text,
    });
}
