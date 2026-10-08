use super::*;
use serde_json::json;

fn gi(state: &str, autostart: bool, devices: Option<u32>) -> GatewayIndicator {
    GatewayIndicator {
        state: state.into(),
        autostart,
        devices,
        last_error: None,
    }
}

#[test]
fn parses_status_object_and_event_envelope() {
    let st = json!({"state": "online", "autostart": true, "devices": 2, "log": "/x"});
    let want = gi("online", true, Some(2));
    assert_eq!(GatewayIndicator::parse(&st), Some(want.clone()));
    assert_eq!(
        GatewayIndicator::parse(&json!({"type": "gateway.status", "data": st})),
        Some(want)
    );
    assert_eq!(GatewayIndicator::parse(&Value::Null), None);
    assert_eq!(GatewayIndicator::parse(&json!({"devices": 1})), None);
    assert_eq!(
        GatewayIndicator::parse(&json!({"state": "off"})),
        Some(gi("off", false, None))
    );
}

#[test]
fn hidden_when_off_and_not_autostart() {
    assert_eq!(gi("off", false, None).render(), None);
    assert_eq!(gi("off", true, None).render().as_deref(), Some("○ gw"));
}

#[test]
fn render_and_tone_per_state() {
    let cases = [
        ("online", Some(2), "● gw 2", Tone::Ok),
        ("online", None, "● gw", Tone::Ok),
        ("local_only", None, "● gw", Tone::Ok),
        ("starting", None, "◌ gw", Tone::Warn),
        ("connecting", None, "◌ gw", Tone::Warn),
        ("offline", None, "○ gw", Tone::Error),
        ("crashed", None, "✗ gw", Tone::Error),
        ("external", Some(1), "◇ gw", Tone::Neutral),
    ];
    for (state, devices, text, tone) in cases {
        let g = gi(state, true, devices);
        assert_eq!(g.render().as_deref(), Some(text), "{state}");
        assert_eq!(g.tone(), tone, "{state}");
    }
}

#[test]
fn login_required_parses_and_asks_for_a_sign_in() {
    let st = json!({"state": "login_required", "autostart": true, "devices": null,
                    "last_error": "the relay needs a signed-in account"});
    let g = GatewayIndicator::parse(&st).unwrap();
    assert_eq!(g.state, "login_required");
    assert_eq!(
        g.last_error.as_deref(),
        Some("the relay needs a signed-in account")
    );
    assert_eq!(g.tone(), Tone::Warn);
    assert_eq!(g.render().as_deref(), Some("! gw sign in"));
    // Visible even without autostart: the gateway is set up, it just can't connect.
    let g = GatewayIndicator::parse(&json!({"state": "login_required"})).unwrap();
    assert_eq!(g.last_error, None);
    assert!(g.render().is_some());
}
