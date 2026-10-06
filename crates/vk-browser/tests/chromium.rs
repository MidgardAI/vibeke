//! Live Chromium tests. Skipped unless `VIBEKE_BROWSER_TESTS=1` (CI has no Chromium).
//! Binary: `$VIBEKE_CHROMIUM` or the newest Playwright build on disk; fresh temp profile.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use vk_browser::cdp::{Browser, LaunchOptions, ScreencastFrame, ScreencastParams, TempProfile};
use vk_browser::frame;
use vk_browser::input::{MapOptions, map_key_with_release};
use vk_browser::local_http::LocalServer;
use vk_proto::input::{Key, KeyEvent, Mods, NamedKey};

fn enabled() -> bool {
    std::env::var("VIBEKE_BROWSER_TESTS").as_deref() == Ok("1")
}

fn launch() -> (Browser, TempProfile) {
    let prefer_shell = std::env::var("VIBEKE_BROWSER_FULL").as_deref() != Ok("1");
    let bin = vk_browser::cdp::discover_chromium(prefer_shell).expect("no Chromium found");
    let prof = TempProfile::new(&std::env::temp_dir(), "test").unwrap();
    let mut o = LaunchOptions::new(&bin, &prof.path);
    o.headless_new = !bin.to_string_lossy().contains("headless-shell");
    o.device_scale_factor = Some(2.0);
    (Browser::launch(&o).unwrap(), prof)
}

const INPUT_PAGE: &str = r#"<!doctype html><html><body style="margin:0;background:#fff">
<input id="t" style="font-size:20px;width:300px" autofocus>
<div id="log"></div>
<script>
window.keys = [];
document.addEventListener('keydown', e => keys.push([e.key, e.code, e.altKey, e.ctrlKey, e.metaKey]));
</script></body></html>"#;

fn ev(c: char, mods: Mods, text: Option<&str>, base: Option<char>) -> KeyEvent {
    let mut e = KeyEvent::new(Key::Char(c), mods);
    e.text = text.map(str::to_owned);
    e.base_layout_key = base;
    e
}

#[test]
fn typing_us_and_norwegian_into_a_page() {
    if !enabled() {
        eprintln!("skipped (VIBEKE_BROWSER_TESTS!=1)");
        return;
    }
    let srv = LocalServer::start(HashMap::from([("/".to_owned(), INPUT_PAGE.to_owned())])).unwrap();
    let (browser, _prof) = launch();
    let page = browser.new_page("about:blank").unwrap();
    page.enable().unwrap();
    page.set_viewport_for_cells(80, 20, 16, 32, 2.0).unwrap();
    page.navigate(&srv.url("/")).unwrap();
    page.click(50.0, 15.0).unwrap();
    let opts = MapOptions {
        host_reports_text: true,
        mac_commands: true,
    };
    let mut events = vec![
        ev('h', Mods::empty(), Some("h"), None),
        ev('i', Mods::empty(), Some("i"), None),
        ev('ø', Mods::empty(), Some("ø"), Some(';')),
        ev('æ', Mods::empty(), Some("æ"), Some('\'')),
        ev('å', Mods::empty(), Some("å"), Some('[')),
        {
            let mut e = ev('å', Mods::SHIFT, Some("Å"), Some('['));
            e.shifted = Some('Å');
            e
        },
        // macOS Norwegian Option+2 → '@', Option+8 → '['.
        ev('2', Mods::ALT, Some("@"), Some('2')),
        ev('8', Mods::ALT, Some("["), Some('8')),
        // Dead acute, then 'e' composes to 'é'.
        ev('´', Mods::empty(), None, Some('=')),
        ev('e', Mods::empty(), Some("é"), Some('e')),
        KeyEvent::named(NamedKey::Space),
        ev('+', Mods::empty(), Some("+"), Some('-')),
    ];
    for e in events.drain(..) {
        page.dispatch_wait(&map_key_with_release(&e, opts)).unwrap();
    }
    // Round trip to make sure all fire-and-forget input was processed.
    let v = page.eval("document.getElementById('t').value").unwrap();
    assert_eq!(v.as_str(), Some("hiøæåÅ@[é +"));
    let keys = page.eval("JSON.stringify(window.keys)").unwrap();
    eprintln!("keydown log: {keys}");
    let keys: serde_json::Value = serde_json::from_str(keys.as_str().unwrap()).unwrap();
    // 'ø' reports its physical key.
    assert!(
        keys.as_array()
            .unwrap()
            .iter()
            .any(|k| k[0] == "ø" && k[1] == "Semicolon")
    );
    // Option text carries no altKey (text, not a chord).
    assert!(
        keys.as_array()
            .unwrap()
            .iter()
            .any(|k| k[0] == "@" && k[2] == false)
    );
    assert!(keys.as_array().unwrap().iter().any(|k| k[0] == "Dead"));

    // Cmd+A selects all (mac editing command), then typing replaces.
    page.dispatch_wait(&map_key_with_release(
        &ev('a', Mods::SUPER, None, None),
        opts,
    ))
    .unwrap();
    page.dispatch_wait(&map_key_with_release(
        &ev('x', Mods::empty(), Some("x"), None),
        opts,
    ))
    .unwrap();
    let v = page.eval("document.getElementById('t').value").unwrap();
    if cfg!(target_os = "macos") {
        assert_eq!(v.as_str(), Some("x"));
    }
}

#[test]
fn screencast_and_screenshot_are_pane_sized() {
    if !enabled() {
        eprintln!("skipped (VIBEKE_BROWSER_TESTS!=1)");
        return;
    }
    let srv = LocalServer::start(HashMap::from([(
        "/".to_owned(),
        "<body style='background:#08f'>hello</body>".to_owned(),
    )]))
    .unwrap();
    let (mut browser, _prof) = launch();
    let events = browser.take_events();
    let page = browser.new_page("about:blank").unwrap();
    page.enable().unwrap();
    let (css_w, css_h) = page.set_viewport_for_cells(50, 10, 16, 32, 2.0).unwrap();
    assert_eq!((css_w, css_h), (400, 160));
    page.navigate(&srv.url("/")).unwrap();
    let shot = frame::decode(&page.capture_screenshot(false, 80).unwrap()).unwrap();
    assert_eq!(
        (shot.width, shot.height),
        (800, 320),
        "device px = cells × cell px"
    );
    page.start_screencast(ScreencastParams::default()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = None;
    while Instant::now() < deadline {
        let Ok(ev) = events.recv_timeout(Duration::from_millis(200)) else {
            // Nudge a repaint.
            let _ = page.eval("document.body.style.background='#0f8'");
            continue;
        };
        if ev.method == "Page.screencastFrame" {
            let f = ScreencastFrame::from_event(&ev).unwrap();
            page.ack_frame(f.session_id).unwrap();
            got = Some(frame::decode(&f.data).unwrap());
            break;
        }
    }
    let f = got.expect("no screencast frame within 10 s");
    assert_eq!(
        (f.width, f.height),
        (800, 320),
        "screencast at device px needs --force-device-scale-factor"
    );
    page.stop_screencast().unwrap();
    let v = browser.version().unwrap();
    assert!(v["product"].as_str().unwrap_or("").contains("Chrome"));
}
