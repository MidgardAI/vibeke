//! Herdr 0.9.3 socket methods with a Vibeke equivalent that the earlier lanes left
//! `unsupported`: `pane.clear`, `pane.selection.read`, `server.agent_manifests` and
//! `server.reload_agent_manifests`.

use super::*;

/// Methods handled here.
pub const METHODS: &[&str] = &[
    "pane.clear",
    "pane.selection.read",
    "server.agent_manifests",
    "server.reload_agent_manifests",
];

/// A pane-scoped caller acts on its own pane and the panes it created only.
fn may_touch(caller: &Caller, x: &Pane) -> Result<(), WireError> {
    match &caller.ctx.pane_scope {
        Some(s) if &x.id != s && x.created_by != format!("agent:{s}") => Err(WireError::new(
            "permission_denied",
            "a pane can only change its own panes",
        )),
        _ => Ok(()),
    }
}

/// Herdr's `clear_screen` (Ghostty's clear, without a prompt redraw): on the primary screen
/// the scrollback is erased and the rows above the cursor are removed, so the cursor row
/// becomes the top row and keeps its column; the alternate screen is left alone. Only the
/// server's terminal state changes (the program is not told), as in Herdr.
fn clear_screen(server: &Server, rt: &crate::pane::PaneRt) {
    let rev = {
        let mut sc = rt.screen.lock().unwrap();
        if sc.engine.modes().alt_screen {
            return;
        }
        let cur = sc.engine.cursor();
        let mut seq = String::from("\x1b[3J");
        if cur.row > 0 {
            seq.push_str(&format!(
                "\x1b[H\x1b[{}M\x1b[1;{}H",
                cur.row,
                u32::from(cur.col) + 1
            ));
        }
        let mut effects = Vec::new();
        sc.engine.feed(seq.as_bytes(), &mut effects);
        let _ = sc.engine.take_damage();
        sc.rev += 1;
        sc.rev
    };
    rt.rev_tx.send_replace(rev);
    server.screen_dirty.notify_waiters();
}

/// A `PaneTextPoint` (`{row, col}`; `row` counts from the oldest scrollback row kept).
fn point(p: &Value, k: &str) -> Result<(u64, u16), WireError> {
    let v = p
        .get(k)
        .ok_or_else(|| wire::invalid_request(format!("missing field `{k}`")))?;
    let row = v.get("row").and_then(Value::as_u64);
    let col = v
        .get("col")
        .and_then(Value::as_u64)
        .and_then(|c| u16::try_from(c).ok());
    match (row, col) {
        (Some(r), Some(c)) => Ok((r, c)),
        _ => Err(wire::invalid_request(format!(
            "invalid value for `{k}`: expected {{row, col}}"
        ))),
    }
}

/// The text between two absolute cells, inclusive, as Herdr's `read_text_screen` returns it:
/// soft-wrapped rows joined, each line's trailing whitespace trimmed. `None` when a row is not
/// kept any more.
fn selection_text(engine: &vk_term::Engine, a: (u64, u16), b: (u64, u16)) -> Option<String> {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let hist = engine.history_len() as u64;
    let total = hist + u64::from(engine.rows());
    if end.0 >= total {
        return None;
    }
    let mut out = String::new();
    for abs in start.0..=end.0 {
        let row = if abs < hist {
            engine.history_row(abs as usize)?
        } else {
            engine.row((abs - hist) as u16)
        };
        let mut cells = Vec::new();
        super::row_cells(&row, 0, &mut cells);
        let from = if abs == start.0 { start.1 } else { 0 };
        let to = if abs == end.0 { end.1 } else { u16::MAX };
        let mut line = String::new();
        let mut next = from;
        for c in cells.iter().filter(|c| c.end_col >= from && c.col <= to) {
            // Blank cells the row's spans leave out.
            while next < c.col {
                line.push(' ');
                next += 1;
            }
            line.push_str(&c.text);
            next = c.end_col.saturating_add(1);
        }
        let last = abs == end.0;
        if row.wrapped && !last {
            out.push_str(&line);
        } else {
            out.push_str(line.trim_end());
            if !last {
                out.push('\n');
            }
        }
    }
    Some(out)
}

/// Herdr's `AgentManifestInfo` for every active agent manifest.
fn manifest_infos() -> Vec<Value> {
    use vk_agents::manifest::Source;
    crate::agents::manifests::all()
        .iter()
        .filter(|(_, l)| crate::agents::manifests::repo_active(l))
        .map(|(_, l)| {
            let (source, kind) = match &l.source {
                Source::Builtin => ("bundled".to_string(), "bundled"),
                Source::Remote { serial } => (format!("remote:{serial}"), "remote"),
                Source::User(p) => (p.display().to_string(), "local override"),
                Source::Repo { file, .. } => (file.display().to_string(), "local override"),
            };
            let mut v = json!({
                "agent": l.m.id,
                "source": source,
                "source_kind": kind,
                "local_override_shadowing_remote": false,
            });
            if let Some(w) = l.warnings.first() {
                v["warning"] = json!(w);
            }
            v
        })
        .collect()
}

pub async fn call(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    method: &str,
    p: &Value,
) -> Result<Value, WireError> {
    match method {
        // Herdr `pane.clear {pane_id}`: `ok`; `pane_not_found`.
        "pane.clear" => {
            let x = herdr_pane(sn, p)?;
            may_touch(caller, &x)?;
            let rt = server.pane_rt(&x.id).ok_or_else(|| {
                WireError::new("pane_not_found", format!("pane {} not found", x.handle))
            })?;
            clear_screen(server, &rt);
            Ok(ok())
        }
        // Herdr `pane.selection.read {pane_id, anchor, cursor, content_revision?}`:
        // `pane_selection {pane_id, text}`; `stale_content` when the content changed;
        // `selection_unavailable` for rows no longer kept.
        "pane.selection.read" => {
            let id =
                sp(p, "pane_id").ok_or_else(|| wire::invalid_request("missing field `pane_id`"))?;
            let (anchor, cursor) = (point(p, "anchor")?, point(p, "cursor")?);
            let not_found = || WireError::new("pane_not_found", format!("pane not found: {id}"));
            let x = sn.pane(id).cloned().ok_or_else(not_found)?;
            // Reading needs the same right as `pane.read`.
            native(
                server,
                caller,
                "pane.read",
                json!({"pane": x.id, "source": "visible", "lines": 1}),
            )
            .await?;
            let rt = server.pane_rt(&x.id).ok_or_else(not_found)?;
            let sc = rt.screen.lock().unwrap();
            if p.get("content_revision")
                .and_then(Value::as_u64)
                .is_some_and(|r| r != sc.rev)
            {
                return Err(WireError::new("stale_content", "pane content changed"));
            }
            let text = selection_text(&sc.engine, anchor, cursor).ok_or_else(|| {
                WireError::new("selection_unavailable", "selection text is unavailable")
            })?;
            Ok(typed(
                "pane_selection",
                json!({"pane_id": id, "text": text}),
            ))
        }
        // Herdr `server.agent_manifests`: `agent_manifest_status {manifests}`.
        "server.agent_manifests" => Ok(typed(
            "agent_manifest_status",
            json!({"manifests": manifest_infos()}),
        )),
        // Herdr `server.reload_agent_manifests`: reloads them (Vibeke's
        // `agent.manifests_reload`) and answers `agent_manifest_reload {manifests}`.
        "server.reload_agent_manifests" => {
            native(server, caller, "agent.manifests_reload", json!({})).await?;
            Ok(typed(
                "agent_manifest_reload",
                json!({"manifests": manifest_infos()}),
            ))
        }
        other => Err(wire::unknown_method(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_join_soft_wraps_and_trim_lines() {
        let mut e = vk_term::Engine::new(10, 4, 100);
        let mut fx = Vec::new();
        e.feed(b"abcdefghijKLM\r\nxy   \r\nlast", &mut fx);
        let hist = e.history_len() as u64;
        // Row 0 wraps into row 1: one logical line; row 2 has trailing blanks trimmed.
        assert_eq!(
            selection_text(&e, (hist, 2), (hist + 2, 4)).as_deref(),
            Some("cdefghijKLM\nxy")
        );
        // Reversed points select the same text.
        assert_eq!(
            selection_text(&e, (hist + 1, 1), (hist, 8)).as_deref(),
            Some("ijKL")
        );
        assert!(selection_text(&e, (hist, 0), (hist + 40, 0)).is_none());
    }
}
