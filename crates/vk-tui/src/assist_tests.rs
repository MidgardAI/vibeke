//! Assist tests (fake control-stream replies only; no model calls): disabled and consent states,
//! preview → explicit confirm with the preview digest → editable draft, cancel at the preview,
//! Suggest task details filling the Track form without saving, Suggest title applied only on
//! enter, Summarize review from task details, and the briefing saved as a draft.

use super::*;
use crate::drafts::tests::{ch, commands, ctl, fleet, named, only, reply, reply_err, screen, typ};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::{ClientFrame, PushedEvent};

fn preview_reply() -> Value {
    json!({
        "request": {"id": "as_1", "state": "awaiting_confirmation"},
        "preview": {"digest": "dg-1", "system": "SYSTEM PROMPT", "user": "Summarize:\nUSER CONTEXT LINE",
                    "model": "claude-haiku-4-5-20251001", "adapter": "anthropic", "endpoint_host": "api.anthropic.com",
                    "execution_machine": "laptop", "max_output_tokens": 1024, "bytes": 2048,
                    "estimated_input_tokens": 683, "estimated_max_cost_usd": 0.0058,
                    "sources": [], "omitted": [], "redactions": 1, "notice": "secrets were redacted"},
        "requires_confirmation": true,
        "confirm_with": {"method": "assistant.confirm", "params": {"request": "as_1", "preview_digest": "dg-1"}}
    })
}

/// generate → preview → y → confirm → done (via a pushed event + get).
fn run_to_done(app: &mut crate::app::App, rx: &mut UnboundedReceiver<ClientFrame>, output: Value) {
    let (req, _) = only(&commands(rx), "assistant.generate");
    reply(app, 0, req, preview_reply());
    app.on_key(ch('y'));
    let (req, p) = only(&commands(rx), "assistant.confirm");
    assert_eq!(p, json!({"request": "as_1", "preview_digest": "dg-1"}));
    reply(
        app,
        0,
        req,
        json!({"request": {"id": "as_1", "state": "queued"}}),
    );
    crate::push::on_events(
        app,
        0,
        vec![PushedEvent {
            seq: 1,
            kind: "assistant.request_finished".into(),
            json: json!({"type": "assistant.request_finished", "subject": {"request": "as_1"}, "data": {}}).to_string(),
        }],
        false,
    );
    let (req, p) = only(&commands(rx), "assistant.get");
    assert_eq!(p["request"], "as_1");
    reply(
        app,
        0,
        req,
        json!({"request": {"id": "as_1", "state": "done", "output": output}}),
    );
}

#[test]
fn disabled_state_is_explicit() {
    let (mut app, mut rxs) = fleet();
    app.action("assist_briefing", None);
    let (req, p) = only(&commands(&mut rxs[0]), "assistant.generate");
    assert_eq!(p["operation"], "briefing");
    assert_eq!(p["workspace"], "W1");
    reply_err(
        &mut app,
        0,
        req,
        "unsupported",
        json!({"category": "disabled", "reason": null}),
    );
    let s = screen(&app);
    assert!(s.contains("Assistance is off — enable in config"), "{s}");
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn consent_is_an_explicit_action() {
    let (mut app, mut rxs) = fleet();
    app.action("assist_briefing", None);
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.generate");
    reply_err(
        &mut app,
        0,
        req,
        "permission_denied",
        json!({"category": "permission_denied", "reason": "consent_required"}),
    );
    let s = screen(&app);
    assert!(s.contains("Grant consent for this workspace?"), "{s}");
    assert!(s.contains("api (/src/api)"), "{s}");
    // Other keys do nothing; g grants for this workspace and operation, then regenerates.
    app.on_key(ch('y'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('g'));
    let (req, p) = only(&commands(&mut rxs[0]), "assistant.consent");
    assert_eq!(p, json!({"workspace": "W1", "operations": ["briefing"]}));
    reply(&mut app, 0, req, json!({"consent": {}, "notice": "ok"}));
    let (_, p) = only(&commands(&mut rxs[0]), "assistant.generate");
    assert_eq!(p["operation"], "briefing");
}

#[test]
fn preview_confirm_then_editable_briefing() {
    let (mut app, mut rxs) = fleet();
    app.action("assist_briefing", None);
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.generate");
    reply(&mut app, 0, req, preview_reply());
    // The exact preview is shown before anything is sent.
    let s = screen(&app);
    assert!(s.contains("exactly this will be sent"), "{s}");
    assert!(s.contains("SYSTEM PROMPT"), "{s}");
    assert!(s.contains("USER CONTEXT LINE"), "{s}");
    assert!(
        s.contains("claude-haiku-4-5-20251001 via anthropic → api.anthropic.com"),
        "{s}"
    );
    assert!(s.contains("1 redaction(s)"), "{s}");
    assert!(commands(&mut rxs[0]).is_empty());
    // Re-render through the shared helper from here on.
    if let Some(f) = &mut app.assist {
        f.phase = Phase::Generating;
    }
    let f = app.assist.clone().unwrap();
    generate(&mut app, &f);
    run_to_done(
        &mut app,
        &mut rxs[0],
        json!({"generated": true, "items": [{"text": "r1 waits for approval", "urgency": "high", "targets": ["r1"]}], "coverage": "1 workspace"}),
    );
    let s = screen(&app);
    assert!(s.contains("[high] r1 waits for approval"), "{s}");
    assert!(s.contains("edit freely"), "{s}");
    // Editable; ctrl+d saves the edited text as a workspace draft (never sends).
    typ(&mut app, "!");
    app.on_key(ctl('d'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.create");
    assert_eq!(p["scope"], "workspace");
    assert_eq!(p["id"], "W1");
    assert!(p["text"].as_str().unwrap().ends_with('!'));
    assert!(
        cmds.iter()
            .all(|c| c.1 != "agent.prompt" && c.1 != "draft.send")
    );
    reply(&mut app, 0, req, json!({"draft": {"id": "d1"}}));
    assert!(screen(&app).contains("Saved as a draft"));
}

#[test]
fn cancel_at_preview_sends_nothing() {
    let (mut app, mut rxs) = fleet();
    app.action("assist_briefing", None);
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.generate");
    reply(&mut app, 0, req, preview_reply());
    app.on_key(ch('n'));
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().all(|c| c.1 != "assistant.confirm"));
    let (_, p) = only(&cmds, "assistant.cancel");
    assert_eq!(p["request"], "as_1");
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.toasts.iter().any(|t| t.text == "Nothing was sent"));
}

#[test]
fn suggest_task_details_fills_the_form_unsaved() {
    let (mut app, mut rxs) = fleet();
    crate::tasks::open_track(&mut app, 0, "p1");
    let (req, _) = only(&commands(&mut rxs[0]), "task.sources");
    reply(
        &mut app,
        0,
        req,
        json!({"run": "r1", "identity_verified": true, "turns": [
            {"n": 1, "prompt": "old"}, {"n": 2, "prompt": "Fix the login redirect. Preserve SSO."}]}),
    );
    assert!(screen(&app).contains("ctrl+g Suggest task details"));
    app.on_key(ctl('g'));
    assert!(matches!(app.mode, Mode::Popup(Popup::Assist)));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "assistant.generate");
    assert_eq!(p["operation"], "suggest_task_details");
    assert_eq!(p["run"], "r1");
    assert_eq!(p["turns"], json!([2]));
    // Feed the same request again through the helper.
    let f = app.assist.clone().unwrap();
    generate(&mut app, &f);
    run_to_done(
        &mut app,
        &mut rxs[0],
        json!({"generated": true, "title": "Fix login redirect", "objective": "Return users to their page",
               "constraints": ["Preserve SSO behavior"], "criteria": [{"text": "Redirect test passes", "required": false}],
               "suggested_checks": [{"text": "SSO smoke test", "selected": false}], "stop_at": "draft_pr", "questions": []}),
    );
    let s = screen(&app);
    assert!(s.contains("Title: Fix login redirect"), "{s}");
    assert!(s.contains("Suggested checks (not selected):"), "{s}");
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Popup(Popup::Track)));
    let f = app.track.as_ref().unwrap();
    assert_eq!(f.title, "Fix login redirect");
    assert_eq!(f.objective, "Return users to their page");
    assert_eq!(
        f.criteria,
        vec![
            "Preserve SSO behavior".to_string(),
            "Redirect test passes".to_string()
        ]
    );
    assert_eq!(crate::tasks::STOP_AT[f.stop].0, "draft_pr");
    assert!(f.assisted);
    assert!(screen(&app).contains("Suggested by the assistant"));
    // Nothing tracked until the user presses Track task.
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "task.track"));
}

#[test]
fn suggested_title_applies_only_on_enter() {
    let (mut app, mut rxs) = fleet();
    app.mode = Mode::Popup(Popup::Peek { pane: "p1".into() });
    app.on_key(ch('s'));
    let (_, p) = only(&commands(&mut rxs[0]), "assistant.generate");
    assert_eq!(p["operation"], "pane_title");
    assert_eq!(p["pane"], "p1");
    let f = app.assist.clone().unwrap();
    generate(&mut app, &f);
    run_to_done(
        &mut app,
        &mut rxs[0],
        json!({"generated": true, "title": "login fix"}),
    );
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "pane.rename"));
    typ(&mut app, " v2");
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "pane.rename");
    assert_eq!(p, json!({"pane": "p1", "title": "login fix v2"}));
    reply(&mut app, 0, req, json!({}));
    assert!(screen(&app).contains("Pane renamed"));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(&app.mode, Mode::Popup(Popup::Peek { pane }) if pane == "p1"));
}

#[test]
fn summarize_review_and_failures() {
    let (mut app, mut rxs) = fleet();
    summarize_review(&mut app, 0, "k1");
    let (_, p) = only(&commands(&mut rxs[0]), "assistant.generate");
    assert_eq!(p["operation"], "review_summary");
    assert_eq!(p["task"], "k1");
    let f = app.assist.clone().unwrap();
    generate(&mut app, &f);
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.generate");
    reply(&mut app, 0, req, preview_reply());
    app.on_key(ch('y'));
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.confirm");
    reply(
        &mut app,
        0,
        req,
        json!({"request": {"id": "as_1", "state": "queued"}}),
    );
    // Polling while waiting (no push on this machine).
    if let Some(f) = &mut app.assist {
        f.last_poll = None;
    }
    tick(&mut app);
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.get");
    reply(
        &mut app,
        0,
        req,
        json!({"request": {"id": "as_1", "state": "failed", "error": {"category": "invalid_output", "message": "the reply was not valid JSON"}}}),
    );
    assert!(screen(&app).contains("✗ the reply was not valid JSON"));
    // Rendering of a review summary output.
    let text = render_output(
        Op::ReviewSummary,
        &json!({"summary": "Adds a redirect", "changes": ["auth.rs"], "validation": [{"text": "tests pass", "basis": "recorded_check"}], "outstanding": [], "risks": ["SSO"]}),
    );
    assert!(text.contains("Adds a redirect"));
    assert!(text.contains("- tests pass [recorded_check]"));
    assert!(text.contains("Risks:\n- SSO"));
}
