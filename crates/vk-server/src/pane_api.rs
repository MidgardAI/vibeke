//! `pane.move`, `pane.scroll` and `pane.screenshot` (07 §2.6).
//!
//! - `pane.move` moves a tiled pane into another tab, another workspace's tab, or a new tab of a
//!   workspace. Its process keeps running; across workspaces the pane gets a handle in the
//!   destination workspace (`previous_pane_handle` is the old one). An emptied source tab closes;
//!   moving the only pane of a workspace's only tab is refused.
//! - `pane.scroll` asks attached clients to move their viewport of the pane in its scrollback
//!   (`pane.scroll_requested`, delivered like every event); scrolling is client-side, so the
//!   result is the requested offset, not a confirmation. `pane.read` never scrolls.
//! - `pane.screenshot` captures the pane grid as text, ANSI (SGR colors and attributes) or a
//!   standalone HTML page into the blob store (`blob.get`), or rendered as SVG or PNG
//!   ([`crate::pane_render`]).

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, resolve_pane, s, u};
use crate::core::{Tx, subject_pane};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};
use vk_proto::layout::{self, Direction};
use vk_proto::model::*;
use vk_proto::render::{Color, Row, Style, attr};
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[
    ("pane.move", true),
    ("pane.scroll", true),
    ("pane.screenshot", false),
];

fn conflict(reason: &str, msg: impl Into<String>) -> vk_proto::rpc::RpcError {
    err(ErrorKind::Conflict, format!("{reason}: {}", msg.into())).details(json!({"reason": reason}))
}

// ---- pane.move ------------------------------------------------------------------------------

enum Dest {
    Tab(Tab),
    NewTabIn(Workspace),
}

fn pane_move(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    let to = p
        .get("to")
        .filter(|v| v.is_object())
        .ok_or_else(|| invalid("to: {tab} | {workspace} | {new_tab_in: workspace} is required"))?;
    let dir = Direction::parse(
        s(p, "direction")
            .or_else(|| p.get("position").and_then(|x| s(x, "direction")))
            .unwrap_or("right"),
    )
    .ok_or_else(|| invalid("direction must be right|down|left|up"))?;
    let anchor = s(p, "anchor").or_else(|| p.get("position").and_then(|x| s(x, "pane")));
    let mut c = server.core.lock().unwrap();
    let dest = if let Some(t) = s(to, "tab") {
        Dest::Tab(c.tab(t).cloned().ok_or_else(|| not_found("tab", t))?)
    } else if let Some(w) = s(to, "new_tab_in") {
        Dest::NewTabIn(c.ws(w).cloned().ok_or_else(|| not_found("workspace", w))?)
    } else if let Some(w) = s(to, "workspace") {
        let ws = c.ws(w).cloned().ok_or_else(|| not_found("workspace", w))?;
        // The workspace's active tab: the one with the most recently focused pane, else first.
        let focused_tab = server
            .client_focus(&ctx.client_id)
            .tab
            .filter(|t| c.tab(t).is_some_and(|t| t.workspace == ws.id));
        let tab = focused_tab
            .and_then(|t| c.tab(&t).cloned())
            .or_else(|| c.tabs_of(&ws.id).first().map(|t| (*t).clone()))
            .ok_or_else(|| conflict("no_tab", "the workspace has no tab"))?;
        Dest::Tab(tab)
    } else {
        return Err(invalid(
            "to: {tab} | {workspace} | {new_tab_in: workspace} is required",
        ));
    };
    let dest_ws_id = match &dest {
        Dest::Tab(t) => t.workspace.clone(),
        Dest::NewTabIn(w) => w.id.clone(),
    };
    if ctx.pane_scope.is_some() && dest_ws_id != pane.workspace {
        return Err(err(
            ErrorKind::PermissionDenied,
            "pane.move to another workspace is not allowed from a pane",
        )
        .details(json!({"scope": "pane"})));
    }
    let mut src = c
        .tab(&pane.tab)
        .cloned()
        .ok_or_else(|| not_found("tab", &pane.tab))?;
    if src.floating.iter().any(|f| f.pane == pane.id) {
        return Err(conflict(
            "floating",
            "embed the floating pane first (pane.embed), then move it",
        ));
    }
    if let Dest::Tab(t) = &dest
        && t.id == src.id
    {
        return Err(conflict("same_tab", "the pane is already in that tab"));
    }
    let mut tx = Tx::new();
    // Detach from the source tab.
    let src_closed = match layout::remove(&src.layout, &pane.id) {
        Some(l) => {
            src.layout = l;
            if src.focused_pane.as_deref() == Some(pane.id.as_str()) {
                src.focused_pane = src.layout.panes().first().cloned();
            }
            if src.zoomed_pane.as_deref() == Some(pane.id.as_str()) {
                src.zoomed_pane = None;
            }
            false
        }
        None => {
            if c.tabs_of(&src.workspace).len() <= 1 && dest_ws_id != src.workspace {
                return Err(conflict(
                    "last_pane",
                    "the pane is the only one in its workspace; move it with new_tab_in in the same workspace or close the workspace instead",
                ));
            }
            true
        }
    };
    let dest_ws = c
        .ws(&dest_ws_id)
        .cloned()
        .ok_or_else(|| not_found("workspace", &dest_ws_id))?;
    let mut moved = pane.clone();
    let previous = pane.handle.clone();
    if dest_ws.id != pane.workspace {
        let n = pane.handle.rsplit(':').next().unwrap_or(&pane.handle);
        moved.handle = format!("{}:{n}", dest_ws.handle);
        moved.workspace = dest_ws.id.clone();
    }
    let dst = match dest {
        Dest::Tab(mut t) => {
            let anchor = match anchor {
                Some(a) => {
                    let ap = c.pane(a).cloned().ok_or_else(|| not_found("pane", a))?;
                    if ap.tab != t.id || ap.id == pane.id {
                        return Err(invalid(
                            "anchor must be another pane of the destination tab",
                        ));
                    }
                    ap.id
                }
                None => t
                    .focused_pane
                    .clone()
                    .filter(|f| t.layout.panes().contains(f))
                    .or_else(|| t.layout.panes().first().cloned())
                    .ok_or_else(|| conflict("no_pane", "the destination tab has no tiled pane"))?,
            };
            if !layout::split(&mut t.layout, &anchor, &pane.id, dir, 0.5) {
                return Err(internal("split failed"));
            }
            t.zoomed_pane = None;
            t
        }
        Dest::NewTabIn(ws) => {
            let number = c.next_tab_number(&ws.id);
            tx.counters = true;
            let order = c
                .tabs_of(&ws.id)
                .iter()
                .map(|t| t.order)
                .fold(0.0, f64::max)
                + 1.0;
            let t = Tab {
                id: crate::core::ulid(),
                handle: format!("{}:t{number}", ws.handle),
                workspace: ws.id.clone(),
                title: None,
                number,
                layout: LayoutNode::Leaf {
                    pane: pane.id.clone(),
                },
                focused_pane: Some(pane.id.clone()),
                zoomed_pane: None,
                order,
                floating: vec![],
                floats_hidden: false,
            };
            tx.event(
                "tab.created",
                json!({"tab": t.id, "workspace": ws.id}),
                json!({"number": number}),
            );
            t
        }
    };
    moved.tab = dst.id.clone();
    if src_closed {
        tx.event(
            "tab.closed",
            json!({"tab": src.id, "workspace": src.workspace}),
            json!({}),
        );
        tx.close_tab(&src);
    } else {
        tx.event("tab.layout_changed", json!({"tab": src.id}), json!({}));
        tx.tab(src.clone());
    }
    tx.event("tab.layout_changed", json!({"tab": dst.id}), json!({}));
    tx.event(
        "pane.moved",
        subject_pane(&moved),
        json!({"from_tab_id": src.handle, "to_tab_id": dst.handle}),
    );
    tx.tab(dst.clone());
    tx.pane(moved.clone());
    let events = server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    if b(p, "focus") == Some(true) && ctx.pane_scope.is_none() {
        server.focus_pane(&ctx.client_id, &moved.id);
    }
    let seq = events.last().map(|e| e.seq);
    Ok(json!({
        "pane": moved,
        "tab": dst,
        "previous_pane_handle": previous,
        "source_tab_closed": src_closed,
        "cursor": crate::api::cursor(server, seq),
    }))
}

// ---- pane.scroll ----------------------------------------------------------------------------

/// Last offset requested per pane (rows above the live screen).
fn offsets() -> &'static Mutex<HashMap<String, u64>> {
    static O: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    O.get_or_init(Mutex::default)
}

fn pane_scroll(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    let rt = server
        .pane_rt(&pane.id)
        .ok_or_else(|| conflict("not_running", "the pane has no live terminal"))?;
    let total = rt.screen.lock().unwrap().engine.history_len() as u64;
    let cur = offsets()
        .lock()
        .unwrap()
        .get(&pane.id)
        .copied()
        .unwrap_or(0)
        .min(total);
    let to = s(p, "to");
    let delta = p.get("delta").and_then(Value::as_i64);
    let offset = match (to, delta) {
        (Some("bottom"), _) => 0,
        (Some("top"), _) => total,
        (Some("line"), _) => {
            let line = u(p, "line").ok_or_else(|| invalid("to: line needs `line`"))?;
            total.saturating_sub(line.min(total))
        }
        (Some(other), _) => {
            return Err(invalid(format!("to must be bottom|top|line, not {other}")));
        }
        (None, Some(d)) => (cur as i64 + d).clamp(0, total as i64) as u64,
        (None, None) => return Err(invalid("to or delta is required")),
    };
    offsets().lock().unwrap().insert(pane.id.clone(), offset);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "pane.scroll_requested",
        subject_pane(&pane),
        json!({"offset": offset, "total": total, "client": s(p, "client")}),
    );
    let events = server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({
        "scroll": {"offset": offset, "total": total, "at_bottom": offset == 0},
        "cursor": crate::api::cursor(server, events.last().map(|e| e.seq)),
    }))
}

// ---- pane.screenshot ------------------------------------------------------------------------

fn sgr_color(c: Color, base: u8, out: &mut Vec<String>) {
    match c {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => out.push(format!("{}", base as u16 + n as u16)),
        Color::Indexed(n) if n < 16 => out.push(format!("{}", base as u16 + 60 + (n - 8) as u16)),
        Color::Indexed(n) => out.push(format!("{};5;{n}", base + 8)),
        Color::Rgb(r, g, b) => out.push(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

fn sgr(st: &Style) -> String {
    let mut v: Vec<String> = vec![];
    let a = st.attrs;
    for (bit, code) in [
        (attr::BOLD, "1"),
        (attr::DIM, "2"),
        (attr::ITALIC, "3"),
        (attr::BLINK, "5"),
        (attr::INVERSE, "7"),
        (attr::HIDDEN, "8"),
        (attr::STRIKE, "9"),
    ] {
        if a & bit != 0 {
            v.push(code.into());
        }
    }
    if a & attr::DOUBLE_UNDERLINE != 0 {
        v.push("21".into());
    } else if a & attr::ANY_UNDERLINE != 0 {
        v.push("4".into());
    }
    sgr_color(st.fg, 30, &mut v);
    sgr_color(st.bg, 40, &mut v);
    if v.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", v.join(";"))
    }
}

/// Rows as text with SGR escapes (each row starts from default attributes and resets).
pub fn rows_ansi(rows: &[Row]) -> String {
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let mut spans = r.spans.clone();
        // Trailing default-styled blanks carry nothing.
        while let Some(last) = spans.last_mut() {
            if last.style == Style::default() {
                let t = last.text.trim_end().to_string();
                if t.is_empty() {
                    spans.pop();
                    continue;
                }
                last.text = t;
            }
            break;
        }
        for sp in &spans {
            let code = sgr(&sp.style);
            out.push_str(&code);
            out.push_str(&sp.text);
            if !code.is_empty() {
                out.push_str("\x1b[0m");
            }
        }
        if i + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

pub fn rows_text(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| r.text().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// xterm-256 palette entry.
fn palette(n: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match n {
        0..=15 => BASE[n as usize],
        16..=231 => {
            let i = n - 16;
            let step = |x: u8| if x == 0 { 0 } else { 55 + 40 * x };
            (step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        _ => {
            let g = 8 + 10 * (n - 232);
            (g, g, g)
        }
    }
}

fn css(c: Color) -> Option<String> {
    let (r, g, b) = match c {
        Color::Default => return None,
        Color::Indexed(n) => palette(n),
        Color::Rgb(r, g, b) => (r, g, b),
    };
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A standalone HTML page (dark background, monospace) of the rows.
pub fn rows_html(rows: &[Row], title: &str) -> String {
    let mut body = String::new();
    for (i, r) in rows.iter().enumerate() {
        for sp in &r.spans {
            let st = &sp.style;
            let (mut fg, mut bg) = (css(st.fg), css(st.bg));
            if st.attrs & attr::INVERSE != 0 {
                std::mem::swap(&mut fg, &mut bg);
                fg = fg.or(Some("#1e1e1e".into()));
                bg = bg.or(Some("#d4d4d4".into()));
            }
            let mut style = String::new();
            if let Some(f) = fg {
                let _ = write!(style, "color:{f};");
            }
            if let Some(b) = bg {
                let _ = write!(style, "background:{b};");
            }
            if st.attrs & attr::BOLD != 0 {
                style.push_str("font-weight:bold;");
            }
            if st.attrs & attr::DIM != 0 {
                style.push_str("opacity:.6;");
            }
            if st.attrs & attr::ITALIC != 0 {
                style.push_str("font-style:italic;");
            }
            let mut deco = vec![];
            if st.attrs & attr::ANY_UNDERLINE != 0 {
                deco.push("underline");
            }
            if st.attrs & attr::STRIKE != 0 {
                deco.push("line-through");
            }
            if !deco.is_empty() {
                let _ = write!(style, "text-decoration:{};", deco.join(" "));
            }
            let text = if st.attrs & attr::HIDDEN != 0 {
                " ".repeat(sp.cols as usize)
            } else {
                esc_html(&sp.text)
            };
            if style.is_empty() {
                body.push_str(&text);
            } else {
                let _ = write!(body, "<span style=\"{style}\">{text}</span>");
            }
        }
        if i + 1 < rows.len() {
            body.push('\n');
        }
    }
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{}</title>\n<style>body{{margin:0;background:#1e1e1e}}pre{{margin:0;padding:12px;color:#d4d4d4;font:13px/1.25 ui-monospace,Menlo,Consolas,monospace;white-space:pre}}</style></head>\n<body><pre>{body}</pre></body></html>\n",
        esc_html(title)
    )
}

fn pane_screenshot(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    let format = s(p, "format").unwrap_or("ansi");
    let (ext, mime) = match format {
        "text" => ("txt", "text/plain"),
        "ansi" => ("ans", "text/x-ansi"),
        "html" => ("html", "text/html"),
        "svg" => ("svg", "image/svg+xml"),
        "png" => ("png", "image/png"),
        other => {
            return Err(invalid(format!(
                "format must be text|ansi|html|svg|png, not {other}"
            )));
        }
    };
    let source = s(p, "source").unwrap_or("visible");
    let rt = server
        .pane_rt(&pane.id)
        .ok_or_else(|| conflict("not_running", "the pane has no live terminal"))?;
    let (rows, cols, nrows, cursor) = {
        let sc = rt.screen.lock().unwrap();
        let e = &sc.engine;
        let mut rows = vec![];
        match source {
            "visible" => {}
            "recent" => {
                let want = u(p, "lines").unwrap_or(200) as usize;
                let h = e.history_len();
                let n = want.saturating_sub(e.rows() as usize).min(h);
                for i in h - n..h {
                    if let Some(r) = e.history_row(i) {
                        rows.push(r);
                    }
                }
            }
            other => {
                return Err(invalid(format!(
                    "source must be visible|recent, not {other}"
                )));
            }
        }
        rows.extend(e.visible_rows());
        (rows, e.cols(), e.rows(), e.cursor())
    };
    let title = format!("{} — {}", pane.handle, pane.display_title());
    let data: Vec<u8> = match format {
        "text" => rows_text(&rows).into_bytes(),
        "ansi" => rows_ansi(&rows).into_bytes(),
        "html" => rows_html(&rows, &title).into_bytes(),
        _ => {
            let dark = {
                let a = server.theme.current();
                !a.known || a.dark
            };
            let colors =
                crate::pane_render::Colors::from_palette(&crate::theme::query_palette(dark));
            // The cursor cell, when asked for and on screen (visible rows come last).
            let first_visible = rows.len().saturating_sub(nrows as usize);
            let grid = crate::pane_render::Grid {
                rows: &rows,
                cols,
                cursor: (b(p, "include_cursor") == Some(true) && cursor.visible)
                    .then_some((first_visible + cursor.row as usize, cursor.col)),
            };
            if format == "svg" {
                crate::pane_render::svg(&grid, &colors, &title).into_bytes()
            } else {
                crate::pane_render::png(&grid, &colors)
                    .map_err(|e| invalid(e).details(json!({"reason": "too_large"})))?
            }
        }
    };
    let meta = json!({
        "mime": mime,
        "kind": "pane_screenshot",
        "pane": pane.id,
        "pane_handle": pane.handle,
        "format": format,
        "cols": cols,
        "rows": nrows,
        "created_at_ms": vk_store::now_ms(),
    });
    let (hash, path) =
        crate::agent_browser::store_blob(server, &data, ext, &meta).map_err(internal)?;
    let rev = rt.rev();
    let mut out = json!({
        "blob": {"hash": hash, "size": data.len(), "mime": mime, "path": crate::privacy::readable_path(server, &path)},
        "format": format,
        "source": source,
        "cols": cols,
        "rows": nrows,
        "lines": rows.len(),
        "revision": rev,
    });
    if b(p, "include_cursor") == Some(true) {
        out["cursor"] = json!({"row": cursor.row, "col": cursor.col, "visible": cursor.visible});
    }
    if b(p, "inline") == Some(true) {
        if format == "png" {
            use base64::Engine as _;
            out["data_b64"] = json!(base64::engine::general_purpose::STANDARD.encode(&data));
        } else {
            out["data"] = json!(String::from_utf8_lossy(&data));
        }
    }
    if matches!(format, "png" | "svg") {
        let (w, h) = crate::pane_render::size(&crate::pane_render::Grid {
            rows: &rows,
            cols,
            cursor: None,
        });
        out["width"] = json!(w);
        out["height"] = json!(h);
    }
    Ok(out)
}

/// Dispatch hook for `pane.move|scroll|screenshot`.
pub fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "pane.move" => pane_move(server, ctx, p),
        "pane.scroll" => pane_scroll(server, ctx, p),
        "pane.screenshot" => pane_screenshot(server, ctx, p),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_proto::render::Span;

    fn span(text: &str, style: Style) -> Span {
        Span {
            style,
            text: text.into(),
            cols: text.chars().count() as u16,
        }
    }

    #[test]
    fn ansi_and_html_rendering() {
        let red_bold = Style {
            fg: Color::Indexed(1),
            attrs: attr::BOLD,
            ..Default::default()
        };
        let rgb_bg = Style {
            bg: Color::Rgb(1, 2, 3),
            ..Default::default()
        };
        let rows = vec![
            Row::new(
                vec![
                    span("ok ", Style::default()),
                    span("ERR", red_bold),
                    span("   ", Style::default()),
                ],
                false,
            ),
            Row::new(vec![span("<x>", rgb_bg)], false),
        ];
        assert_eq!(rows_text(&rows), "ok ERR\n<x>");
        assert_eq!(
            rows_ansi(&rows),
            "ok \x1b[1;31mERR\x1b[0m\n\x1b[48;2;1;2;3m<x>\x1b[0m"
        );
        let html = rows_html(&rows, "w1:p1 <t>");
        assert!(html.contains("<span style=\"color:#cd0000;font-weight:bold;\">ERR</span>"));
        assert!(html.contains("background:#010203;\">&lt;x&gt;</span>"));
        assert!(html.contains("<title>w1:p1 &lt;t&gt;</title>"));
        assert_eq!(palette(196), (255, 0, 0));
        assert_eq!(palette(244), (128, 128, 128));
    }
}
