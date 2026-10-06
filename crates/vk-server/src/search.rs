//! Scrollback search and archive paging (07 §2.14 `search.query`, 02 §3 `scrollback_fts`,
//! 03 §11.2 archive).
//!
//! * `search.query {q|text, pane?, workspace?, machine?, since?, limit?, regex?, context?,
//!   sources?}` searches live panes (screen + in-memory scrollback) and the archive (FTS5; with
//!   `regex`, a scan of the archive segments of the panes in scope). Each hit carries the pane,
//!   absolute line number, text, context lines and the archive position to page from.
//! * `pane.read {pane, source: "archive", from?, to?, lines?}` pages a pane's whole history by
//!   absolute line: archive segments, then the in-memory scrollback and the screen. Works for
//!   closed panes whose archive is still on disk (edit-scrollback / copy mode paging).
//!
//! Read scope (09 §5.1 rule 4): pane-scoped callers see panes in their own workspace by default
//! (`security.pane_scope.read = "workspace" | "session" | "self"`).

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, resolve_pane, resolve_ws, s, u};
use serde_json::{Value, json};
use std::collections::HashSet;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::archive::ArchivedRow;
use vk_store::{FtsQuery, now_ms};

/// What a caller may read.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadScope {
    All,
    Workspace(String),
    Pane(String),
}

pub fn read_scope_setting() -> String {
    vk_config::Config::load(vk_config::config_path())
        .ok()
        .and_then(|(c, _)| {
            c.extra
                .get("security")
                .and_then(|s| s.get("pane_scope"))
                .and_then(|p| p.get("read"))
                .and_then(|r| r.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "workspace".into())
}

pub fn read_scope(server: &Server, ctx: &Ctx) -> ReadScope {
    let Some(pane) = &ctx.pane_scope else {
        return ReadScope::All;
    };
    match read_scope_setting().as_str() {
        "session" => ReadScope::All,
        "self" => ReadScope::Pane(pane.clone()),
        _ => match server.with_core(|c| c.pane(pane).map(|p| p.workspace.clone())) {
            Some(ws) => ReadScope::Workspace(ws),
            None => ReadScope::Pane(pane.clone()),
        },
    }
}

fn allowed(scope: &ReadScope, pane: &str, ws: Option<&str>) -> bool {
    match scope {
        ReadScope::All => true,
        ReadScope::Workspace(w) => ws == Some(w.as_str()),
        ReadScope::Pane(p) => p == pane,
    }
}

/// Read-scope check for content reads of other panes (called from `api::authorize`).
pub fn authorize_read(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Result<(), RpcError> {
    if ctx.pane_scope.is_none() || !matches!(method, "pane.read" | "pane.wait_output") {
        return Ok(());
    }
    let scope = read_scope(server, ctx);
    let target = s(p, "pane").unwrap_or("@current");
    let (id, ws) = match resolve_pane(server, ctx, Some(target)) {
        Ok(x) => (x.id, Some(x.workspace)),
        Err(e) => match server.with_core(|c| c.store.archive_pane(target).ok().flatten()) {
            Some((id, ws, ..)) => (id, Some(ws)),
            None => return Err(e),
        },
    };
    if allowed(&scope, &id, ws.as_deref()) {
        Ok(())
    } else {
        Err(err(
            ErrorKind::PermissionDenied,
            format!("{method}: pane is outside your read scope (security.pane_scope.read)"),
        )
        .details(json!({"scope": "pane"})))
    }
}

/// `since`: epoch ms, or a duration back from now (`90s`, `30m`, `2h`, `7d`).
pub fn parse_since(v: &Value, now: i64) -> Result<Option<i64>, RpcError> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_i64()),
        Value::String(t) => {
            if let Ok(n) = t.parse::<i64>() {
                return Ok(Some(n));
            }
            let (num, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
            let n: i64 = num
                .parse()
                .map_err(|_| invalid(format!("bad since `{t}` (epoch ms or 30m/2h/7d)")))?;
            let ms = match unit {
                "ms" => 1,
                "s" => 1000,
                "m" => 60_000,
                "h" => 3_600_000,
                "d" => 86_400_000,
                _ => return Err(invalid(format!("bad since unit in `{t}`"))),
            };
            Ok(Some(now - n * ms))
        }
        _ => Err(invalid("since must be a number or string")),
    }
}

/// A pane's live rows: `(abs line, text, wrapped, in_history)`, plus the first in-memory line.
/// `(abs line, text, wrapped, in_history)`.
pub type LiveRow = (u64, String, bool, bool);

pub fn live_rows(server: &Server, pane: &str) -> Option<(u64, Vec<LiveRow>)> {
    let rt = server.pane_rt(pane)?;
    let sc = rt.screen.lock().unwrap();
    let e = &sc.engine;
    let total = e.scrolled_total();
    let hist = e.history_len() as u64;
    let first = total.saturating_sub(hist);
    let mut out = Vec::with_capacity(hist as usize + e.rows() as usize);
    for i in 0..hist {
        if let Some(r) = e.history_row(i as usize) {
            out.push((first + i, r.text().trim_end().to_string(), r.wrapped, true));
        }
    }
    for (i, r) in e.visible_rows().into_iter().enumerate() {
        out.push((
            total + i as u64,
            r.text().trim_end().to_string(),
            r.wrapped,
            false,
        ));
    }
    while out.last().is_some_and(|r| !r.3 && r.1.is_empty()) {
        out.pop();
    }
    Some((first, out))
}

enum Matcher {
    Sub(String),
    Re(regex::Regex),
}

impl Matcher {
    fn is_match(&self, t: &str) -> bool {
        match self {
            Matcher::Sub(n) => t.to_lowercase().contains(n),
            Matcher::Re(r) => r.is_match(t),
        }
    }
}

fn ctx_lines(rows: &[(u64, String)], i: usize, n: usize) -> Value {
    let before: Vec<&str> = rows[i.saturating_sub(n)..i]
        .iter()
        .map(|r| r.1.as_str())
        .collect();
    let after: Vec<&str> = rows[(i + 1).min(rows.len())..(i + 1 + n).min(rows.len())]
        .iter()
        .map(|r| r.1.as_str())
        .collect();
    json!({"before": before, "after": after})
}

pub fn query(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let q = s(p, "q")
        .or_else(|| s(p, "text"))
        .or_else(|| s(p, "query"))
        .ok_or_else(|| invalid("missing param `q`"))?
        .to_string();
    if q.trim().is_empty() {
        return Err(invalid("empty query"));
    }
    if let Some(m) = s(p, "machine")
        && m != server.opts.machine
        && m != "local"
    {
        return Err(invalid(format!(
            "this is machine {}; search another machine with the global --machine {m}",
            server.opts.machine
        )));
    }
    let limit = u(p, "limit").unwrap_or(50).clamp(1, 1000) as usize;
    let nctx = u(p, "context").unwrap_or(2).min(20) as usize;
    let since = parse_since(p.get("since").unwrap_or(&Value::Null), now_ms())?;
    let regex = b(p, "regex").unwrap_or(false);
    let sources: Vec<String> = match p.get("sources") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(x)) => x.split(',').map(str::to_string).collect(),
        _ => vec!["live".into(), "archive".into()],
    };
    let want_live = sources
        .iter()
        .any(|x| matches!(x.as_str(), "live" | "scrollback" | "screen"));
    let want_archive = sources.iter().any(|x| x == "archive" || x == "scrollback");
    let matcher = if regex {
        Matcher::Re(regex::Regex::new(&q).map_err(|e| invalid(e.to_string()))?)
    } else {
        Matcher::Sub(q.to_lowercase())
    };
    let scope = read_scope(server, ctx);
    // Narrow by explicit filters.
    let pane_filter: Option<(String, Option<String>)> = match s(p, "pane") {
        Some(t) => Some(match resolve_pane(server, ctx, Some(t)) {
            Ok(x) => (x.id, Some(x.workspace)),
            Err(e) => match server.with_core(|c| c.store.archive_pane(t).ok().flatten()) {
                Some((id, ws, ..)) => (id, Some(ws)),
                None => return Err(e),
            },
        }),
        None => None,
    };
    if let Some((id, ws)) = &pane_filter
        && !allowed(&scope, id, ws.as_deref())
    {
        return Err(err(
            ErrorKind::PermissionDenied,
            "search.query: pane is outside your read scope",
        ));
    }
    let ws_filter: Option<String> = match s(p, "workspace") {
        Some(w) => Some(resolve_ws(server, ctx, Some(w))?.id),
        None => None,
    };
    if let (Some(w), ReadScope::Workspace(mine)) = (&ws_filter, &scope)
        && w != mine
    {
        return Err(err(
            ErrorKind::PermissionDenied,
            "search.query: workspace is outside your read scope",
        ));
    }
    let in_scope = |pane: &str, ws: Option<&str>| {
        allowed(&scope, pane, ws)
            && pane_filter.as_ref().is_none_or(|(id, _)| id == pane)
            && ws_filter.as_deref().is_none_or(|w| ws == Some(w))
    };
    // Live panes in scope: (id, handle, workspace, title).
    let live: Vec<(String, String, String, String)> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .map(|x| {
                (
                    x.id.clone(),
                    x.handle.clone(),
                    x.workspace.clone(),
                    x.display_title().to_string(),
                )
            })
            .collect()
    });
    let live: Vec<_> = live
        .into_iter()
        .filter(|(id, _, ws, _)| {
            in_scope(id, Some(ws)) && crate::browser_pane::page_io::may_read_output(server, ctx, id)
        })
        .collect();
    let mut hits: Vec<Value> = Vec::new();
    let mut first_in_mem: std::collections::HashMap<String, u64> = Default::default();
    let mut truncated = false;
    for (id, handle, ws, title) in &live {
        let Some((first, rows)) = live_rows(server, id) else {
            continue;
        };
        first_in_mem.insert(id.clone(), first);
        if !want_live {
            continue;
        }
        let plain: Vec<(u64, String)> = rows.iter().map(|r| (r.0, r.1.clone())).collect();
        for i in (0..rows.len()).rev() {
            if !matcher.is_match(&rows[i].1) {
                continue;
            }
            if hits.len() >= limit {
                truncated = true;
                break;
            }
            let n = rows[i].0;
            hits.push(json!({
                "pane": id, "pane_handle": handle, "workspace": ws, "title": title,
                "source": if rows[i].3 { "scrollback" } else { "screen" },
                "live": true, "line": n, "text": rows[i].1,
                "context": ctx_lines(&plain, i, nctx),
                "position": {"line": n, "from": n.saturating_sub(nctx as u64), "to": n + nctx as u64 + 1},
            }));
        }
    }
    if want_archive && hits.len() < limit {
        let left = limit - hits.len();
        let dup = |pane: &str, n: u64| first_in_mem.get(pane).is_some_and(|f| n >= *f);
        // `ts == 0`: found by a segment scan (no FTS timestamp).
        let mut archive_hits: Vec<vk_store::FtsHit> = Vec::new();
        if regex {
            // FTS can't evaluate a regex: scan the segments of the panes in scope.
            let panes: Vec<(String, String, Option<String>, Option<String>)> = match &pane_filter {
                Some((id, ws)) => vec![(id.clone(), ws.clone().unwrap_or_default(), None, None)],
                None => live
                    .iter()
                    .map(|(id, h, ws, t)| {
                        (id.clone(), ws.clone(), Some(h.clone()), Some(t.clone()))
                    })
                    .collect(),
            };
            'outer: for (id, ws, handle, title) in panes {
                let rows = server
                    .archive
                    .lock()
                    .unwrap()
                    .read(&id, 0, u64::MAX)
                    .map_err(internal)?;
                for r in rows.iter().rev() {
                    if dup(&id, r.n) || !matcher.is_match(&r.t) {
                        continue;
                    }
                    if archive_hits.len() >= left {
                        truncated = true;
                        break 'outer;
                    }
                    archive_hits.push(vk_store::FtsHit {
                        pane: id.clone(),
                        line: r.n,
                        ts: 0,
                        text: r.t.clone(),
                        workspace: Some(ws.clone()),
                        handle: handle.clone(),
                        title: title.clone(),
                    });
                }
            }
        } else {
            let f = FtsQuery {
                q: q.clone(),
                panes: pane_filter.as_ref().map(|(id, _)| vec![id.clone()]),
                workspaces: match (&scope, &ws_filter) {
                    (_, Some(w)) => Some(vec![w.clone()]),
                    (ReadScope::Workspace(w), None) => Some(vec![w.clone()]),
                    _ => None,
                },
                since_ms: since,
                // Over-fetch: rows still in memory are reported as live hits instead.
                limit: (left * 4).max(50),
            };
            let fts = server
                .with_core(|c| c.store.fts_query(&f))
                .map_err(internal)?;
            for h in fts {
                if dup(&h.pane, h.line) || matches!(&scope, ReadScope::Pane(pn) if *pn != h.pane) {
                    continue;
                }
                if archive_hits.len() >= left {
                    truncated = true;
                    break;
                }
                archive_hits.push(h);
            }
        }
        let live_ids: HashSet<&String> = live.iter().map(|l| &l.0).collect();
        for h in archive_hits {
            let (pane, n, text) = (h.pane, h.line, h.text);
            let (ws, handle, title) = (h.workspace, h.handle, h.title);
            let ts = (h.ts != 0).then_some(h.ts);
            let from = n.saturating_sub(nctx as u64);
            let around: Vec<ArchivedRow> = server
                .archive
                .lock()
                .unwrap()
                .read(&pane, from, n + nctx as u64 + 1)
                .unwrap_or_default();
            let rows: Vec<(u64, String)> = around.into_iter().map(|r| (r.n, r.t)).collect();
            let context = match rows.iter().position(|r| r.0 == n) {
                Some(i) => ctx_lines(&rows, i, nctx),
                None => json!({"before": [], "after": []}),
            };
            hits.push(json!({
                "pane": pane, "pane_handle": handle, "workspace": ws, "title": title,
                "source": "archive", "live": live_ids.contains(&pane), "line": n, "text": text, "ts": ts,
                "context": context,
                "position": {"line": n, "from": from, "to": n + nctx as u64 + 1},
            }));
        }
    }
    Ok(json!({"hits": hits, "truncated": truncated, "query": q, "regex": regex}))
}

/// `pane.read {source: "archive"}`.
pub fn read_archive(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let target = s(p, "pane").unwrap_or("@current");
    let (id, ws, live) = match resolve_pane(server, ctx, Some(target)) {
        Ok(x) => (x.id, x.workspace, true),
        Err(e) => match server.with_core(|c| c.store.archive_pane(target).ok().flatten()) {
            Some((id, ws, ..)) => (id, ws, false),
            None => return Err(e),
        },
    };
    if !allowed(&read_scope(server, ctx), &id, Some(&ws)) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "pane.read: pane is outside your read scope",
        ));
    }
    let mem = if live { live_rows(server, &id) } else { None };
    let (afirst, alast) = {
        let mut a = server.archive.lock().unwrap();
        (
            a.first_line(&id).map_err(internal)?,
            a.last_line(&id).map_err(internal)?,
        )
    };
    let mem_first = mem.as_ref().map(|(f, _)| *f);
    let first = afirst.or(mem_first).unwrap_or(0);
    let end = match &mem {
        Some((_, rows)) => rows
            .last()
            .map(|r| r.0 + 1)
            .unwrap_or(mem_first.unwrap_or(0)),
        None => alast.map(|l| l + 1).unwrap_or(0),
    };
    if !live && afirst.is_none() {
        return Err(not_found("archive", target));
    }
    const MAX: u64 = 5000;
    let lines = u(p, "lines").unwrap_or(200).min(MAX);
    let (from, to) = match (u(p, "from"), u(p, "to")) {
        (Some(f), Some(t)) => (f, t.min(f + MAX)),
        (Some(f), None) => (f, f + lines),
        (None, Some(t)) => (t.saturating_sub(lines), t),
        (None, None) => (end.saturating_sub(lines), end),
    };
    let (from, to) = (from.max(first), to.min(end));
    let mut rows: Vec<Value> = Vec::new();
    let split = mem_first.unwrap_or(u64::MAX);
    if from < to && from < split {
        let arch = server
            .archive
            .lock()
            .unwrap()
            .read(&id, from, to.min(split))
            .map_err(internal)?;
        rows.extend(
            arch.into_iter()
                .map(|r| json!({"n": r.n, "text": r.t, "wrapped": r.w})),
        );
    }
    if let Some((_, mem_rows)) = &mem {
        rows.extend(
            mem_rows
                .iter()
                .filter(|r| r.0 >= from.max(split) && r.0 < to)
                .map(|r| json!({"n": r.0, "text": r.1, "wrapped": r.2})),
        );
    }
    let text = rows
        .iter()
        .map(|r| r["text"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({
        "pane": id, "source": "archive", "live": live,
        "first": first, "end": end, "from": from, "to": to.max(from),
        // First in-memory line (live panes): clients page memory with `FetchHistory` and older
        // rows with this method.
        "mem_first": mem_first,
        "more_before": from > first, "more_after": to < end,
        "rows": rows, "text": text,
    }))
}

/// `scrollback.forget {pane | workspace | before | all: true, dry_run?}` (09 §9.3, 02 "Archive
/// search as implemented"): delete archived scrollback — segment files, `scrollback_fts` rows
/// and `archive_panes` metadata — for the scope. Full scope only. `dry_run` reports what would
/// go. Idempotent. It does not touch the live screen, in-memory scrollback, VT snapshots, the
/// event log, blobs, the desk index, drafts or notes. `before` is segment-granular: a segment
/// whose last write is older than the time goes; one that straddles it stays whole.
/// The result carries the canonical `scope` (pane/workspace id, absolute `before` ms),
/// `pane_ids` and a `plan` digest of both; with `plan` given the call is refused (`conflict`)
/// unless the scope still resolves to exactly that plan (how the CLI confirms a dry run).
pub fn forget(server: &Server, ctx: &Ctx, p: &Value) -> R {
    use vk_store::archive::Select;
    let all = b(p, "all").unwrap_or(false);
    let pane = s(p, "pane");
    let ws = s(p, "workspace");
    let before = crate::desk::time_param(p, "before")?;
    let given = [all, pane.is_some(), ws.is_some(), before.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    if given != 1 {
        return Err(invalid(
            "scrollback.forget needs exactly one of pane, workspace, before or all=true",
        ));
    }
    let dry_run = b(p, "dry_run").unwrap_or(false);
    let safe_id = |id: &str| !id.is_empty() && !id.contains(['/', '\\']) && id != "." && id != "..";
    // Resolve the scope to pane ids (None = every pane).
    let (scope, panes, sel): (Value, Option<Vec<String>>, Select) = if all {
        (json!({"all": true}), None, Select::All)
    } else if let Some(t) = pane {
        let id = match resolve_pane(server, ctx, Some(t)) {
            Ok(x) => x.id,
            Err(e) => match server.with_core(|c| c.store.archive_pane(t).ok().flatten()) {
                Some((id, ..)) => id,
                // Not a known pane: an archive directory of that exact name may still exist.
                None if safe_id(t)
                    && server
                        .archive
                        .lock()
                        .unwrap()
                        .pane_ids()
                        .iter()
                        .any(|i| i == t) =>
                {
                    t.to_string()
                }
                None => return Err(e),
            },
        };
        (json!({"pane": id}), Some(vec![id]), Select::All)
    } else if let Some(w) = ws {
        let w = resolve_ws(server, ctx, Some(w))
            .map(|w| w.id)
            .unwrap_or_else(|_| w.to_string());
        let ids = server.with_core(|c| {
            let mut v: Vec<String> = c
                .model
                .panes
                .iter()
                .filter(|p| p.workspace == w)
                .map(|p| p.id.clone())
                .collect();
            v.extend(c.store.archive_panes_in_workspace(&w).unwrap_or_default());
            v
        });
        (json!({"workspace": w}), Some(ids), Select::All)
    } else {
        let t = before.unwrap_or_default();
        (json!({"before": t}), None, Select::OlderThan(t))
    };
    if let Some(ids) = &panes
        && let Some(bad) = ids.iter().find(|i| !safe_id(i))
    {
        return Err(invalid(format!("unsafe pane id `{bad}`")));
    }
    // The canonical plan: resolved pane ids and an absolute cutoff. A confirmed call sends the
    // dry run's `scope` and `plan` back; if the scope no longer resolves to the same panes the
    // call is refused rather than deleting something the user never saw.
    let pane_ids: Option<Vec<String>> = panes.as_ref().map(|v| {
        let mut v = v.clone();
        v.sort();
        v.dedup();
        v
    });
    let plan = {
        let canon = json!({"scope": scope, "panes": pane_ids});
        let h = blake3::hash(canon.to_string().as_bytes()).to_hex();
        format!("fp1-{}", &h[..32])
    };
    if let Some(want) = s(p, "plan")
        && want != plan
    {
        return Err(err(
            ErrorKind::Conflict,
            "scrollback.forget: the scope no longer resolves to the confirmed plan; run the dry run again",
        )
        .details(json!({"scope": scope, "pane_ids": pane_ids, "plan": plan, "expected": want})));
    }
    let report = {
        let mut a = server.archive.lock().unwrap();
        let _ = a.flush();
        let mut c = server.core.lock().unwrap();
        let r = c
            .store
            .purge_archive(&mut a, panes.as_deref(), sel, dry_run)
            .map_err(internal)?;
        if !dry_run {
            // Rows still waiting for the next FTS flush must not resurrect forgotten text.
            let mut f = server.fts_buf.lock().unwrap();
            match (&panes, sel) {
                (None, Select::All) => f.clear(),
                (Some(ids), _) => f.retain(|row| !ids.contains(&row.0)),
                _ => {}
            }
            // Metadata only: scope and counts, never text.
            let mut tx = crate::core::Tx::new();
            tx.event(
                "scrollback.forgotten",
                json!({"scope": scope}),
                json!({"panes": r.panes, "segments": r.segments, "bytes": r.bytes, "fts_rows": r.fts_rows, "panes_dropped": r.panes_dropped}),
            );
            server.commit(&mut c, tx).map_err(internal)?;
        }
        r
    };
    // Derived assistant results and cached excerpts for this scope go too (14 §8).
    let assistant_purged = if dry_run {
        0
    } else {
        crate::assist::forget_scope(server, &scope)
    };
    Ok(json!({
        "scope": scope,
        "pane_ids": pane_ids,
        "plan": plan,
        "dry_run": dry_run,
        "panes": report.panes,
        "segments_deleted": report.segments,
        "bytes_deleted": report.bytes,
        "fts_rows_deleted": report.fts_rows,
        "archive_panes_dropped": report.panes_dropped,
        "assistant_purged": assistant_purged,
        // Spec 15 derived objects in the same scope (lane 2C).
        "review": crate::review::purge::on_scrollback_forget(server, &scope, pane_ids.as_deref(), dry_run),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_parsing() {
        assert_eq!(parse_since(&json!(null), 1000).unwrap(), None);
        assert_eq!(parse_since(&json!(5), 1000).unwrap(), Some(5));
        assert_eq!(parse_since(&json!("2s"), 10_000).unwrap(), Some(8000));
        assert_eq!(parse_since(&json!("1h"), 3_600_000).unwrap(), Some(0));
        assert_eq!(parse_since(&json!("123"), 0).unwrap(), Some(123));
        assert!(parse_since(&json!("3y"), 0).is_err());
    }

    #[test]
    fn scope_rules() {
        let w = ReadScope::Workspace("w1".into());
        assert!(allowed(&w, "p", Some("w1")));
        assert!(!allowed(&w, "p", Some("w2")));
        assert!(!allowed(&w, "p", None));
        assert!(allowed(&ReadScope::Pane("p".into()), "p", None));
        assert!(allowed(&ReadScope::All, "x", None));
    }
}
