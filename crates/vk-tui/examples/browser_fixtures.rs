// Wire fixtures for the real browser module regression test.
use vk_proto::{
    model::*,
    render::{ClientFrame, ServerFrame},
};
fn main() {
    let mut m = SessionModel {
        machine: "Review host".into(),
        ..Default::default()
    };
    m.previews.push(Preview {
        id: "v1".into(),
        handle: "v1".into(),
        machine: "Review host".into(),
        pane: None,
        task: None,
        port: 5173,
        path: "/".into(),
        label: None,
        url: "http://localhost:5173/".into(),
        scheme: "http".into(),
        status: PreviewStatus::Up,
        source: PreviewSource::Declared,
        pid: None,
        first_seen_ms: 0,
        last_seen_ms: 0,
    });

    for (id, rev) in [("i1", 3), ("i2", 7)] {
        m.interactions.push(Interaction {
            id: id.into(),
            handle: id.into(),
            run: "r1".into(),
            pane: "p1".into(),
            kind: InteractionKind::Approval,
            status: InteractionStatus::Open,
            title: "Run tests".into(),
            body_md: None,
            action: Some(ActionInfo {
                tool: "Bash".into(),
                summary: "Run tests".into(),
                command: Some("cargo test".into()),
                paths: vec![],
                diff: None,
                risk: Risk::Low,
                risk_reasons: vec![],
            }),
            questions: vec![],
            plan_md: None,
            answer_channel: AnswerChannel::Native,
            native_ref: None,
            source: StateSource::Structured,
            confidence: 1.0,
            answerable: true,
            gate: false,
            decision_rev: rev,
            delivery: DeliveryState::None,
            delivery_error: None,
            answer: None,
            answered_by: None,
            answer_key: None,
            opened_at_ms: 0,
            answered_at_ms: None,
            picker: None,
        });
    }
    let f = ServerFrame::Model {
        model: Box::new(m),
        focus: ClientFocus::default(),
        seen: vec![],
    };
    let command = vk_proto::frame::encode(&ClientFrame::Command {
        req: 0,
        json: String::new(),
    })
    .unwrap();
    let result = vk_proto::frame::encode(&ServerFrame::CommandResult {
        req: 0,
        json: String::new(),
    })
    .unwrap();
    println!(
        "{}",
        serde_json::json!({"model": vk_proto::frame::encode(&f).unwrap(), "commandTag": command[4], "resultTag": result[4]})
    );
}
