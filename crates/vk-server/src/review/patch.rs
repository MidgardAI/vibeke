//! Lane 2C, spec 15 §5 / §4.2: **selected-patch snapshots** and **dirty end candidates**.
//!
//! - `task.review.snapshot {task, paths: [..]}` or `{task, patch: "<unified diff>"}` captures
//!   only that selection of the uncommitted work on top of HEAD
//!   (`vk_review::selection`), stored like a dirty snapshot (`refs/vibeke/snapshots/`). The
//!   latest selected patch is the task's current, accept-capable candidate while it still
//!   describes the checkout (same HEAD; whole files: the same content of the selected files;
//!   a patch: the same whole-checkout digest) and it is newer than the latest dirty snapshot.
//!   Checks against it run in a disposable checkout of the snapshot commit: they verify the
//!   selection alone. Accepting it never claims the rest of the checkout is reviewed.
//! - When a binding closes with uncommitted work in the checkout, the end candidate is a dirty
//!   snapshot captured right then (instead of only the committed range, or **No bound end
//!   candidate**). If no consistent capture can be made, the committed candidate (if any) is
//!   pinned and the note says the uncommitted part was not captured.

use super::t4::{SnapRec, prune_snapshot_refs, snap_gate, snaps_of};
use super::*;
use vk_review::selection::{self, PatchSelection};
use vk_review::snapshot::{self as snap, SnapshotError, SnapshotOptions};
use vk_review::subject::{Selection, SelectionMode};

/// Whether a `task.review.snapshot` call selects a patch (`paths` or `patch`).
pub(super) fn is_selection(p: &Value) -> bool {
    p.get("paths").is_some_and(|v| !v.is_null()) || p.get("patch").is_some_and(|v| !v.is_null())
}

fn parse_selection(p: &Value) -> Result<PatchSelection, RpcError> {
    match (p.get("paths"), s(p, "patch")) {
        (Some(v), None) if !v.is_null() => {
            let paths: Vec<String> = match v {
                Value::Array(a) => a
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| invalid("paths: strings only"))
                    })
                    .collect::<Result<_, _>>()?,
                Value::String(one) => vec![one.clone()],
                _ => return Err(invalid("paths: a list of repository-relative paths")),
            };
            Ok(PatchSelection::Paths(paths))
        }
        (None, Some(t)) | (Some(Value::Null), Some(t)) => Ok(PatchSelection::Patch(t.to_string())),
        _ => Err(invalid("pass either `paths` or `patch`, not both")),
    }
}

/// The user-facing label of a selected-patch subject.
pub fn label(sel: &Selection) -> String {
    let what = match sel.mode {
        SelectionMode::Paths => format!("{} selected file(s)", sel.paths.len()),
        SelectionMode::Patch => format!("selected hunks in {} file(s)", sel.paths.len()),
    };
    if sel.excludes_other_changes {
        format!("Selected changes ({what}) · the rest of the checkout is not part of this review")
    } else {
        format!("Selected changes ({what})")
    }
}

/// `task.review.snapshot {task, paths | patch, idempotency_key?}`.
pub(super) async fn snapshot_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.review.snapshot";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let sel = parse_selection(p)?;
    let task = tracking::find_task(server, task_id)?;
    let path = checkout_of(&task).ok_or_else(|| {
        conflict(
            "checkout_unavailable",
            "the task's checkout is unavailable; nothing to select",
        )
    })?;
    let _gate = snap_gate().lock().await;
    let srv = server.clone();
    let (t2, p2, sel2) = (task.clone(), path.clone(), sel.clone());
    let captured = blocking(move || {
        let (base, _) = base_for(&srv, &t2, &p2);
        let base = base.ok_or_else(|| {
            (
                "snapshot_failed",
                "no review base could be resolved".to_string(),
                json!({}),
            )
        })?;
        selection::capture_selected_patch(&p2, &base, &sel2, &SnapshotOptions::default()).map_err(
            |e| {
                let extra = match &e {
                    SnapshotError::UnsupportedCapture { what, paths } => json!({
                        "unsupported": what,
                        "paths": paths,
                        "offers": ["leave the submodule out of the selection", "select a committed revision"],
                    }),
                    SnapshotError::WorkspaceChanging { .. } => json!({
                        "label": snap::WORKSPACE_CHANGING,
                        "offers": ["select a committed revision", "snapshot again"],
                    }),
                    _ => json!({}),
                };
                (e.reason(), e.to_string(), extra)
            },
        )
    })
    .await?;
    let subj = match captured {
        Ok(s) => s,
        Err((reason, msg, mut d)) => {
            d["reason"] = json!(reason);
            return Err(err(ErrorKind::Conflict, msg).details(d));
        }
    };
    let sn = subj.snapshot.clone().expect("selected patch has content");
    let sel_rec = subj
        .selection
        .clone()
        .expect("selected patch has a selection");
    let rec = SnapRec {
        task: task.id.clone(),
        subject_id: subj.id.clone(),
        head_sha: subj.head_sha.clone(),
        dirty_digest: subj.dirty_digest.clone().unwrap_or_default(),
        content_sha: sn.commit.clone(),
        attempts: sn.attempts,
        created_by: tracking::user(ctx),
        created_at_ms: now(),
        selection: Some(sel_rec.clone()),
        binding_end: None,
    };
    let result = json!({
        "subject": subj,
        "snapshot": sn,
        "selection": sel_rec,
        "label": label(&sel_rec),
        "note": format!(
            "Stored as an immutable commit under {}; your index, files and branches are unchanged. Checks on this subject verify the selection alone; accepting it reviews only the selection.",
            sn.ref_name
        ),
    });
    {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let mut tx = Tx::new();
        if c.store
            .get::<Value>(K_SUBJECT, &subj.id)
            .ok()
            .flatten()
            .is_none()
        {
            tx.m.close(K_SUBJECT, &subj.id, None, &subj);
        }
        let key = format!("{}:{}", task.id, subj.id);
        if c.store.get::<Value>(K_CAND, &key).ok().flatten().is_none() {
            let cr = CandidateRec {
                task: task.id.clone(),
                subject_id: subj.id.clone(),
                head_sha: subj.head_sha.clone(),
                base_sha: subj.base_sha.clone(),
                source: "selected_patch".into(),
                binding: None,
                created_at_ms: now(),
            };
            tx.m.close(K_CAND, &key, None, &cr);
            tx.event(
                "review.candidate_created",
                json!({"task": task.id}),
                json!({"subject": subj.id, "head": subj.head_sha, "source": "selected_patch"}),
            );
        }
        tx.m.put(super::t4::K_SNAP, &key, None, &rec);
        tx.event_by(
            "review.snapshot_created",
            json!({"task": task.id, "subject": subj.id}),
            json!({"kind": "user", "id": rec.created_by.id}),
            json!({"head": subj.head_sha, "content": sn.commit, "ref": sn.ref_name, "attempts": sn.attempts, "selection": sel_rec.mode, "paths": sel_rec.paths.len()}),
        );
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    let srv = server.clone();
    let _ = blocking(move || prune_snapshot_refs(&srv, &path, None, false, false)).await;
    spawn_refresh(server, &task.id);
    Ok(result)
}

/// Hook from `candidates_blocking` (live, dirty checkout): the task's latest selected patch
/// becomes the current candidate while it still describes the checkout and is newer than the
/// current dirty snapshot (if any). Blocking (Git).
pub(super) fn apply_current(server: &Server, task: &str, path: &Path, out: &mut Candidates) {
    let (latest_sel, latest_full) = server.with_core(|c| {
        let snaps = snaps_of(c, task);
        (
            snaps.iter().rev().find(|s| s.selection.is_some()).cloned(),
            snaps.iter().rev().find(|s| s.selection.is_none()).cloned(),
        )
    });
    let Some(rec) = latest_sel else { return };
    let current_is_newer_snapshot = out.current.as_ref().is_some_and(|c| {
        c.kind == SubjectKind::DirtySnapshot
            && latest_full
                .as_ref()
                .is_some_and(|f| f.subject_id == c.id && f.created_at_ms > rec.created_at_ms)
    });
    if current_is_newer_snapshot {
        return;
    }
    let Some(subj) = server.with_core(|c| {
        c.store
            .get::<ChangeSubject>(K_SUBJECT, &rec.subject_id)
            .ok()
            .flatten()
    }) else {
        return;
    };
    if !subj.is_immutable() || !subj.verify_id() || !selection::selection_is_current(path, &subj) {
        return;
    }
    if let Some(prev) = out.current.take() {
        let src = match prev.kind {
            SubjectKind::DirtySnapshot => "dirty_snapshot",
            _ => "current_head",
        };
        out.extra.push((prev, src));
    }
    out.current = Some(subj);
}

/// The listing source of a known snapshot subject.
pub(super) fn source_of(s: &ChangeSubject) -> &'static str {
    if s.kind == SubjectKind::SelectedPatch {
        "selected_patch"
    } else {
        "dirty_snapshot"
    }
}

/// Warnings for the package when the selected subject is a selected patch.
pub(super) fn notes(selected: Option<&ChangeSubject>) -> Vec<String> {
    let Some(sel) = selected.and_then(|s| s.selection.as_ref()) else {
        return vec![];
    };
    let mut v = vec![format!(
        "{} — checks on this subject verify the selection alone; acceptance reviews only the selection",
        label(sel)
    )];
    if sel.excludes_other_changes {
        v.push("Uncommitted changes outside the selection are not reviewed by accepting it".into());
    }
    v
}

/// Whether the package's live digest check applies (a patch-mode selection is current only
/// while the whole checkout digest is unchanged).
pub(super) fn digest_checked(s: &ChangeSubject) -> bool {
    s.kind == SubjectKind::DirtySnapshot
        || (s.kind == SubjectKind::SelectedPatch
            && s.selection
                .as_ref()
                .is_some_and(|x| x.mode == SelectionMode::Patch))
}

/// Right before an acceptance transaction: a current whole-files selection must still match the
/// checkout (a later edit of a selected file is a known competing update).
pub(super) async fn revalidate(pkg: &Pkg) -> Result<(), RpcError> {
    let Some(cur) = pkg.current.clone().filter(|s| {
        s.kind == SubjectKind::SelectedPatch
            && s.selection
                .as_ref()
                .is_some_and(|x| x.mode == SelectionMode::Paths)
    }) else {
        return Ok(());
    };
    let Some((path, _)) = pkg.live_head.clone() else {
        return Ok(());
    };
    let ok = blocking(move || selection::selection_is_current(&path, &cur)).await?;
    if ok {
        Ok(())
    } else {
        Err(review_changed(
            "a selected file changed since the selection was captured; select again and review",
            json!({"field": "subject", "detail": "selection_outdated"}),
        ))
    }
}

/// At a binding's close with uncommitted work in the checkout: capture it as the end
/// candidate (15 §4.2, T4). Returns the subject and its snapshot record, or a note why the
/// uncommitted part could not be captured. Blocking (Git), called synchronously at the
/// boundary.
pub(super) fn dirty_end(
    server: &Server,
    task: &Task,
    path: &Path,
    b: &TaskRunBinding,
) -> Result<Option<(ChangeSubject, SnapRec)>, String> {
    let dirty = subject::observation_baseline(path)
        .map(|x| x.dirty_state)
        .map_err(|e| e.to_string())?;
    if dirty == DirtyState::Clean {
        return Ok(None);
    }
    let (base, _) = base_for(server, task, path);
    let base = base.ok_or_else(|| "no review base could be resolved".to_string())?;
    let opts = SnapshotOptions {
        max_attempts: 2,
        retry_delay: Duration::from_millis(50),
    };
    let s = snap::capture_dirty_snapshot(path, &base, &opts).map_err(|e| e.to_string())?;
    let sn = s.snapshot.clone().expect("dirty snapshot has content");
    let rec = SnapRec {
        task: task.id.clone(),
        subject_id: s.id.clone(),
        head_sha: s.head_sha.clone(),
        dirty_digest: s.dirty_digest.clone().unwrap_or_default(),
        content_sha: sn.commit.clone(),
        attempts: sn.attempts,
        created_by: Actor::system("binding_end"),
        created_at_ms: now(),
        selection: None,
        binding_end: Some(b.id.clone()),
    };
    Ok(Some((s, rec)))
}

use vk_review::Actor;
