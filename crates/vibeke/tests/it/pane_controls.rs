//! Pane layout changes keep the existing terminals and survive server recovery.
use super::support::{Rpc, Session, alive};
use serde_json::{Value, json};
use vk_proto::model::LayoutNode;

fn call(r: &mut Rpc, method: &str, params: Value) -> Value {
    assert!(vk_server::api_schema::validate_params(method, &params).is_empty());
    let result = r.call(method, params).unwrap();
    assert!(
        vk_server::api_schema::validate_result(method, &result).is_empty(),
        "{method}: {result}"
    );
    result
}
fn width(layout: Value, pane: &str) -> u16 {
    let t: LayoutNode = serde_json::from_value(layout).unwrap();
    vk_proto::layout::rects(
        &t,
        vk_proto::layout::Rect {
            x: 0,
            y: 0,
            w: 101,
            h: 41,
        },
    )
    .into_iter()
    .find(|(p, _)| p == pane)
    .unwrap()
    .1
    .w
}

#[test]
fn resize_shrink_toggle_swap_presets_and_restart_keep_the_terminals() {
    let s = Session::new();
    let a = s.workspace("sh -c \"printf 'pane-control-marker\\n'; sleep 1000\"");
    s.wait_output(&a, "pane-control-marker", 5000);
    let b = s.split(&a, "sleep 1000");
    let pid_a = s.pane(&a)["child_pid"].as_i64().unwrap();
    let pid_b = s.pane(&b)["child_pid"].as_i64().unwrap();
    let mut r = Rpc::connect(&s.socket());
    for pane in [&a, &b] {
        let shrunk = call(
            &mut r,
            "pane.resize",
            json!({"pane": pane, "direction": "left", "mode": "size", "percent": 10}),
        );
        assert_eq!(width(shrunk["layout"].clone(), pane), 40);
        let grown = call(
            &mut r,
            "pane.resize",
            json!({"pane": pane, "direction": "right", "mode": "size", "percent": 10}),
        );
        assert_eq!(width(grown["layout"].clone(), pane), 50);
    }
    // Legacy callers still select the neighbour and can send a negative amount.
    let edge = call(
        &mut r,
        "pane.resize",
        json!({"pane": a, "direction": "right", "percent": -10}),
    );
    assert_eq!(width(edge["layout"].clone(), &a), 40);
    assert!(
        r.call(
            "pane.resize",
            json!({"pane": a, "direction": "right", "mode": "size", "percent": -10})
        )
        .is_err()
    );
    let toggled = call(&mut r, "pane.toggle_split", json!({"pane": b}));
    assert_eq!(toggled["tab"]["layout"]["Split"]["dir"], "Vertical");
    let swapped = call(
        &mut r,
        "pane.swap",
        json!({"pane": b, "direction": "previous"}),
    );
    let tree: LayoutNode = serde_json::from_value(swapped["tab"]["layout"].clone()).unwrap();
    assert_eq!(tree.panes(), vec![b.clone(), a.clone()]);
    let mut final_tab = Value::Null;
    for layout in ["columns", "rows", "main-left", "main-top", "tiled", "next"] {
        final_tab =
            call(&mut r, "pane.arrange", json!({"pane": a, "layout": layout}))["tab"].clone();
        let tree: LayoutNode = serde_json::from_value(final_tab["layout"].clone()).unwrap();
        assert_eq!(tree.panes(), vec![b.clone(), a.clone()]);
    }
    assert!(
        r.call("pane.arrange", json!({"pane": a, "layout": "invalid"}))
            .is_err()
    );
    drop(r);
    s.kill_server();
    s.json(&["server", "status"]);
    let recovered = s.json(&["tab", "list"]);
    let recovered = recovered["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == final_tab["id"])
        .unwrap();
    assert_eq!(recovered["layout"], final_tab["layout"]);
    assert!(alive(pid_a) && alive(pid_b));
    assert_eq!(s.pane(&a)["child_pid"], pid_a);
    assert_eq!(s.pane(&b)["child_pid"], pid_b);
    s.wait_output(&a, "pane-control-marker", 5000);
}

#[test]
fn break_then_join_closes_the_empty_tab_and_preserves_focus_and_process() {
    let s = Session::new();
    let a = s.workspace("sleep 1000");
    let b = s.split(&a, "sleep 1000");
    let before = s.pane(&b);
    let mut r = Rpc::connect(&s.socket());
    let moved = call(
        &mut r,
        "pane.move",
        json!({"pane": b, "to": {"new_tab_in": before["workspace"]}, "focus": true}),
    );
    let new_tab = moved["pane"]["tab"].clone();
    assert_ne!(new_tab, before["tab"]);
    let joined = call(
        &mut r,
        "pane.move",
        json!({"pane": b, "to": {"tab": before["tab"]}, "anchor": a, "direction": "down", "focus": true}),
    );
    assert_eq!(joined["pane"]["tab"], before["tab"]);
    let tabs = call(&mut r, "tab.list", json!({}));
    let tabs = tabs["tabs"].as_array().unwrap();
    assert!(!tabs.iter().any(|t| t["id"] == new_tab));
    let tab = tabs.iter().find(|t| t["id"] == before["tab"]).unwrap();
    assert_eq!(tab["layout"]["Split"]["dir"], "Vertical");
    let focus = call(&mut r, "pane.get", json!({"pane": "@focused"}));
    assert_eq!(focus["pane"]["id"], b);
    assert_eq!(s.pane(&b)["child_pid"], before["child_pid"]);
}

#[test]
fn moving_the_last_tiled_pane_does_not_orphan_floating_panes() {
    let s = Session::new();
    let a = s.workspace("sleep 1000");
    let before = s.pane(&a);
    let mut r = Rpc::connect(&s.socket());
    let float = call(
        &mut r,
        "pane.float",
        json!({"tab": before["tab"], "command": ["sleep", "1000"]}),
    );
    assert!(
        r.call(
            "pane.move",
            json!({"pane": a, "to": {"new_tab_in": before["workspace"]}})
        )
        .is_err()
    );
    assert_eq!(s.pane(&a)["tab"], before["tab"]);
    assert_eq!(
        s.pane(float["pane"]["id"].as_str().unwrap())["tab"],
        before["tab"]
    );
}

#[test]
fn single_pane_noops_keep_zoom_and_a_stale_destination_keeps_the_source() {
    let s = Session::new();
    let a = s.workspace("sleep 1000");
    let before = s.pane(&a);
    let mut r = Rpc::connect(&s.socket());
    let zoomed = call(&mut r, "pane.zoom", json!({"pane": a, "zoomed": true}));
    for method in ["pane.toggle_split", "pane.swap", "pane.arrange"] {
        let result = call(&mut r, method, json!({"pane": a}));
        assert_eq!(result["tab"], zoomed["tab"]);
    }
    let target = call(
        &mut r,
        "tab.create",
        json!({"workspace": before["workspace"], "command": ["sleep", "1000"]}),
    );
    call(&mut r, "tab.close", json!({"tab": target["tab"]["id"]}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let tabs = call(&mut r, "tab.list", json!({}));
        if !tabs["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == target["tab"]["id"])
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "destination did not close"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(r.call("pane.move", json!({"pane": a, "to": {"tab": target["tab"]["id"]}, "anchor": target["root_pane"]["id"]})).is_err());
    assert_eq!(s.pane(&a)["tab"], before["tab"]);
}
