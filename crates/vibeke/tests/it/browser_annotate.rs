//! Page annotations end to end (06 B11) with the fake Chromium: a browser pane rendered for a
//! minimal render client, `vibeke browser annotate --pane <bp>` turns annotate mode on, a click
//! in the pane becomes a pick (no mouse event reaches the page), the note is edited and sent,
//! and `vibeke browser feedback --pane <source> --wait 15` receives the prompt
//! (`browser.annotation_update`, `browser.annotations_send` through `vibeke api call`).

use crate::browser_pane::{Render, Session, count, http, media_pane, root_pane};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use vk_proto::input::{Mods, MouseButton, MouseKind};
use vk_proto::render::BrowserCmd;

fn mouse(kind: MouseKind, x: f32, y: f32) -> BrowserCmd {
    BrowserCmd::Mouse {
        kind,
        button: if kind == MouseKind::Move {
            MouseButton::None
        } else {
            MouseButton::Left
        },
        x,
        y,
        mods: Mods::empty(),
        clicks: 1,
    }
}

fn wait_json(s: &Session, args: &[&str], what: &str, f: impl Fn(&Value) -> bool) -> Value {
    let t0 = Instant::now();
    loop {
        let v = s.json(args);
        if f(&v) {
            return v;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}: {v}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn annotate_pick_edit_send_and_feedback() {
    let s = Session::new(true);
    let src = root_pane(&s);
    let port = http(vec![("/", "<button id=save>Save</button>".into())]);
    let url = format!("http://localhost:{port}/");
    let o = s.json(&["preview", "open", &url, "--split", "right", "--pane", &src]);
    let bp = o["pane"].as_str().unwrap().to_string();
    let spec = s.browser_spec(&bp);
    let mut r = Render::attach(&s.socket(), "annotate-e2e");
    r.view(vec![media_pane(&bp, spec, 40, 20)]);
    r.media(|m| m.reset, Duration::from_secs(20));

    // Annotate mode on (the media host is this server).
    let on = s.json(&["browser", "annotate", &bp, "--on"]);
    assert_eq!(on["on"], true, "{on}");
    s.wait_cdp("DOM and Overlay enabled", |l| {
        count(l, "DOM.enable") >= 1 && count(l, "Overlay.enable") >= 1
    });
    // Hover, then a click: a pick, not a click on the page.
    r.cmd(&bp, mouse(MouseKind::Move, 50.0, 40.0));
    r.cmd(&bp, mouse(MouseKind::Press, 60.0, 40.0));
    r.cmd(&bp, mouse(MouseKind::Release, 60.0, 40.0));
    let log = s.wait_cdp("the pick", |l| count(l, "Runtime.callFunctionOn") >= 1);
    assert!(
        !log.iter()
            .any(|v| v["method"] == "Input.dispatchMouseEvent"),
        "no pointer input reaches the page while annotating"
    );
    assert!(log.iter().any(|v| v["method"] == "Page.createIsolatedWorld"
        && v["params"]["worldName"] == "vibeke-annotate"));
    let d = wait_json(
        &s,
        &["browser", "annotations", &bp],
        "the note in the draft",
        |v| v["draft"]["notes"].as_array().is_some_and(|a| a.len() == 1),
    );
    let draft = d["draft"]["id"].as_str().unwrap().to_string();
    let note = &d["draft"]["notes"][0];
    assert_eq!(note["selector"], "#save", "{d}");
    assert_eq!(note["kind"], "element");
    let img = note["image"].as_str().expect("a screenshot path");
    assert!(std::path::Path::new(img).is_file(), "{img}");
    assert_eq!(d["draft"]["state"], "open");

    // Edit the note through the generic CLI, then send it.
    let upd = json!({"draft": draft, "n": 1, "note": "make it bigger"}).to_string();
    s.json(&["api", "call", "browser.annotation_update", &upd]);
    let waiter = {
        let mut c = s.cmd(&["browser", "feedback", "--pane", &src, "--wait", "15"]);
        std::thread::spawn(move || c.output().unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    let send = json!({"draft": draft}).to_string();
    let sent = s.json(&["api", "call", "browser.annotations_send", &send]);
    assert!(
        sent["state"] == "delivered" || sent["state"] == "queued",
        "{sent}"
    );
    assert!(
        sent["text"]
            .as_str()
            .unwrap()
            .contains("Note: make it bigger")
    );
    let out = waiter.join().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fb: Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = fb["text"].as_str().unwrap_or_default();
    assert!(
        text.starts_with(&format!("Browser feedback on \"{url}\"")),
        "{fb}"
    );
    assert!(text.contains("#save"), "{text}");
    assert!(text.contains("Screenshot: "), "{text}");
    assert!(text.contains("Note: make it bigger"), "{text}");
    assert_eq!(fb["draft"]["state"], "delivered");
    assert_eq!(fb["draft"]["via"], "feedback");

    // Annotate off: clicks reach the page again.
    s.json(&["browser", "annotate", &bp, "--no-on"]);
    r.cmd(&bp, mouse(MouseKind::Press, 60.0, 40.0));
    r.cmd(&bp, mouse(MouseKind::Release, 60.0, 40.0));
    s.wait_cdp("a click on the page", |l| {
        l.iter().any(|v| {
            v["method"] == "Input.dispatchMouseEvent" && v["params"]["type"] == "mousePressed"
        })
    });
    // The next pick starts a new draft; the sent one keeps its state.
    let after = s.json(&["browser", "annotations", "--draft", &draft]);
    assert_eq!(after["draft"]["state"], "delivered", "{after}");
}
