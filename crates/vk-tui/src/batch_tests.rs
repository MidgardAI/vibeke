use super::*;
use crate::app::{test_interaction, test_run};
use crate::drafts::tests::{commands, fleet, pane, reply_err, screen};
use vk_proto::input::Mods;

fn key(app: &mut App, k: Key) {
    app.on_key(KeyEvent::new(k, Mods::empty()));
}

fn approval(id: &str, run: &str, pane: &str, cmd: &str, risk: Risk, at: i64) -> Interaction {
    let mut i = test_interaction(id, pane, &format!("Run {cmd}"), at);
    i.run = run.into();
    i.handle = id.replace('i', "h");
    i.action = Some(ActionInfo {
        tool: "Bash".into(),
        summary: format!("Bash {cmd}"),
        command: Some(cmd.into()),
        paths: vec![],
        diff: None,
        risk,
        risk_reasons: vec![],
    });
    i
}

/// p1 focused (claude r1); four more claude agents with approvals: three equivalent
/// `pnpm test`, one high-risk, plus a question.
fn setup() -> (
    App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
) {
    let (mut app, rxs) = fleet();
    let m = &mut app.machines[0];
    for (p, r) in [
        ("p3", "r3"),
        ("p4", "r4"),
        ("p5", "r5"),
        ("p6", "r6"),
        ("p7", "r7"),
    ] {
        m.model.panes.push(pane(p, "T1", "W1"));
        m.model.runs.push(test_run(r, p, "claude"));
    }
    m.model.interactions = vec![
        approval("i3", "r3", "p3", "pnpm test", Risk::Low, 10),
        approval("i4", "r4", "p4", "pnpm test", Risk::Medium, 20),
        approval("i5", "r5", "p5", "pnpm test", Risk::Low, 30),
        approval("i6", "r6", "p6", "rm -rf /", Risk::High, 40),
    ];
    let mut q = test_interaction("i7", "p7", "Which db?", 50);
    q.run = "r7".into();
    q.kind = InteractionKind::Question;
    m.model.interactions.push(q);
    (app, rxs)
}

#[test]
fn equivalence_groups_only_safe_native_approvals() {
    let (mut app, _rx) = setup();
    let (groups, alone) = super::groups(&app);
    assert_eq!(groups.len(), 1);
    let ids: Vec<&str> = groups[0].1.iter().map(|x| x.1.as_str()).collect();
    assert_eq!(ids, ["i3", "i4", "i5"], "identical commands, oldest first");
    let why: Vec<&str> = alone.iter().map(|x| x.1).collect();
    assert!(why.contains(&"high risk"), "{why:?}");
    assert!(why.iter().any(|w| w.contains("questions")), "{why:?}");
    // Keystroke channel, another harness, another workspace root: not equivalent.
    let m = &mut app.machines[0];
    m.model.interactions[2].answer_channel = AnswerChannel::Keystrokes;
    m.model
        .runs
        .iter_mut()
        .find(|r| r.id == "r4")
        .unwrap()
        .harness = "codex".into();
    let (groups, _) = super::groups(&app);
    assert!(groups.is_empty(), "{groups:?}");
}

/// Review batch 2, finding 7: approvals that differ by a comment-terminating newline or by
/// whitespace (quoted or not) never share a batch; compound commands are never batched; live
/// revalidation compares the same raw bytes.
#[test]
fn commands_differing_by_comments_newlines_or_whitespace_never_share_a_batch() {
    let (mut app, mut rxs) = setup();
    let m = &mut app.machines[0];
    m.model.interactions = vec![
        approval("i3", "r3", "p3", "echo harmless # rm victim", Risk::Low, 10),
        approval(
            "i4",
            "r4",
            "p4",
            "echo harmless #\nrm victim",
            Risk::Low,
            20,
        ),
        approval("i5", "r5", "p5", "echo 'a  b'", Risk::Low, 30),
        approval("i6", "r6", "p6", "echo 'a b'", Risk::Low, 40),
        approval("i7", "r7", "p7", "echo  'a b'", Risk::Low, 50),
    ];
    let (groups, alone) = super::groups(&app);
    assert!(groups.is_empty(), "{groups:?}");
    let why = |id: &str| alone.iter().find(|x| x.0.1 == id).map(|x| x.1).unwrap();
    assert!(why("i3").contains("compound"), "{alone:?}");
    assert!(why("i4").contains("compound"), "{alone:?}");
    assert_eq!(why("i5"), "nothing equivalent is waiting");
    for c in [
        "a; b", "a && b", "a || b", "a | b", "a `b`", "a $(b)", "a\nb", "a\rb", "a # b",
    ] {
        assert!(!batch_safe_command(c), "{c:?}");
    }
    assert!(batch_safe_command("pnpm test --filter=web"));
    // Byte-identical plain commands group; one that changes before "allow all" (same length,
    // different whitespace) is skipped by revalidation.
    let m = &mut app.machines[0];
    m.model.interactions[0].action.as_mut().unwrap().command = Some("echo 'a b'".into());
    let (groups, _) = super::groups(&app);
    assert_eq!(groups.len(), 1);
    let ids: Vec<&str> = groups[0].1.iter().map(|x| x.1.as_str()).collect();
    assert_eq!(ids, ["i3", "i6"]);
    open(&mut app, None);
    app.machines[0].model.interactions[0]
        .action
        .as_mut()
        .unwrap()
        .command = Some("echo 'a\tb'".into());
    while rxs[0].try_recv().is_ok() {}
    key(&mut app, Key::Char('a'));
    let answered: Vec<String> = commands(&mut rxs[0])
        .iter()
        .filter(|c| c.1 == "interaction.answer")
        .map(|c| c.2["interaction"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(answered, ["i6"], "the changed member is not answered");
}

#[test]
fn allow_all_sends_one_answer_each_and_shows_each_delivery() {
    let (mut app, mut rxs) = setup();
    // From a card: `A` opens the batch view on the card's group.
    app.mode = Mode::Popup(Popup::Card {
        interaction: "i4".into(),
        sel: 0,
    });
    key(&mut app, Key::Char('A'));
    assert!(matches!(app.mode, Mode::Popup(Popup::Batch)));
    let s = screen(&app);
    assert!(
        s.contains("batch approvals · 3 equivalent in 1 group"),
        "{s}"
    );
    assert!(s.contains("allow all 3"), "{s}");
    assert!(
        s.contains("answer one by one") && s.contains("high risk"),
        "{s}"
    );
    // Untick i5 (rows: header, i3, i4, i5).
    let v = app.ux.batch.as_mut().unwrap();
    v.sel = 3;
    key(&mut app, Key::Char(' '));
    assert!(screen(&app).contains("allow all 2"));
    // i4 was answered elsewhere meanwhile: revalidation skips it.
    app.machines[0].model.interactions[1].status = InteractionStatus::Answered;
    key(&mut app, Key::Char('a'));
    let cmds = commands(&mut rxs[0]);
    let answers: Vec<&Value> = cmds
        .iter()
        .filter(|c| c.1 == "interaction.answer")
        .map(|c| &c.2)
        .collect();
    assert_eq!(answers.len(), 1, "{cmds:?}");
    assert_eq!(answers[0]["interaction"], "i3");
    assert_eq!(answers[0]["decision"], "allow");
    assert!(
        answers[0]["idempotency_key"]
            .as_str()
            .unwrap()
            .contains("batch-i3")
    );
    let v = app.ux.batch.as_ref().unwrap();
    assert!(
        v.notice.as_deref().unwrap().contains("skipped 1"),
        "{:?}",
        v.notice
    );
    // Per-row delivery from the model: i3 delivered, i4 skipped.
    app.machines[0].model.interactions[0].status = InteractionStatus::Answered;
    app.machines[0].model.interactions[0].delivery = DeliveryState::Delivered;
    let s = screen(&app);
    assert!(s.contains("✓ delivered"), "{s}");
    assert!(s.contains("no longer pending"), "{s}");
}

#[test]
fn deny_all_and_partial_failure_and_config_off() {
    let (mut app, mut rxs) = setup();
    open(&mut app, None);
    key(&mut app, Key::Char('n'));
    let cmds = commands(&mut rxs[0]);
    let n: Vec<_> = cmds
        .iter()
        .filter(|c| c.1 == "interaction.answer")
        .collect();
    assert_eq!(n.len(), 3);
    assert!(n.iter().all(|c| c.2["decision"] == "deny"));
    // One answer fails: only that row shows it.
    reply_err(&mut app, 0, n[1].0, "conflict", json!({}));
    let s = screen(&app);
    assert!(s.contains("✗ conflict!"), "{s}");
    // Off by config.
    app.mode = Mode::Normal;
    app.config.ui.interactions.batch = false;
    open(&mut app, None);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("batch view is off")
    );
}
