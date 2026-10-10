//! Gallery tests: navigation and the full view, environment / binding labels, open locally
//! (explicit only), diff selection (marked base or next older; environment mismatch needs `!`),
//! delete with confirmation and the acceptance-reference force, 📷 counters from
//! `screenshot.captured` (sidebar, peek, cleared by opening), the screenshot pane following new
//! screenshots, and kitty images: local files read directly, remote images only after `v`.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, fleet_n, named, only, reply, reply_err, screen};
use vk_proto::render::PushedEvent;

fn shot(
    id: &str,
    handle: &str,
    kind: &str,
    label: &str,
    binding: &str,
    path: &str,
    ts: i64,
) -> Value {
    json!({
        "id": id, "handle": handle, "kind": "screenshot", "blob": format!("blob{id}"), "width": 100, "height": 50,
        "bytes": 1000, "created_at_ms": ts, "label": label, "url": "http://localhost:5173/login",
        "title": "Login", "environment": {"kind": kind}, "binding": binding,
        "binding_reason": if binding == "bound" { "" } else { "the running build reports another commit" },
        "code": {"head_sha": "abcdef1234567", "dirty_state": "dirty"}, "task": "k1", "pane": "p1",
        "preview": "v4", "path_on_machine": path, "exists": true
    })
}

fn list() -> Value {
    json!({"screenshots": [
        shot("S3", "s3", "remote_headless", "devbox · headless · fresh context", "illustrative", "/nonexistent/s3.png", 3),
        shot("S2", "s2", "local_pane", "your browser pane · profile devbox", "bound", "/nonexistent/s2.png", 2),
        shot("S1", "s1", "local_pane", "your browser pane · profile devbox", "bound", "/nonexistent/s1.png", 1),
    ], "count": 3, "total": 3})
}

fn tiny_png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&100u32.to_be_bytes());
    v.extend_from_slice(&50u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

fn open_list(
    app: &mut crate::app::App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>,
) {
    app.action("screenshots", None);
    let (req, p) = only(&commands(rx), "screenshot.list");
    assert_eq!(p, json!({"limit": 100}));
    reply(app, 0, req, list());
}

#[test]
fn navigate_and_labels() {
    let (mut app, mut rxs) = fleet();
    open_list(&mut app, &mut rxs[0]);
    let s = screen(&app);
    assert!(s.contains("s3"), "{s}");
    assert!(s.contains("devbox · headless · fresh context"), "{s}");
    assert!(
        s.contains("illustrative: the running build reports another commit"),
        "{s}"
    );
    assert!(
        s.contains("not proof of what your logged-in profile shows"),
        "{s}"
    );
    assert!(s.contains("abcdef12 + uncommitted changes"), "{s}");
    app.on_key(ch('j'));
    assert_eq!(app.gallery.view.as_ref().unwrap().sel, 1);
    let s = screen(&app);
    assert!(s.contains("bound to this code"), "{s}");
    app.on_key(named(NamedKey::Enter));
    assert_eq!(app.gallery.view.as_ref().unwrap().view, View::Full);
    // No graphics here: metadata + open locally.
    let s = screen(&app);
    assert!(s.contains("[o] open image locally"), "{s}");
    assert!(s.contains("2/3"), "{s}");
    app.on_key(named(NamedKey::Right));
    assert_eq!(app.gallery.view.as_ref().unwrap().sel, 2);
    app.on_key(named(NamedKey::Left));
    // esc: full → list → closed.
    app.on_key(named(NamedKey::Escape));
    assert_eq!(app.gallery.view.as_ref().unwrap().view, View::List);
    app.on_key(named(NamedKey::Escape));
    assert!(app.gallery.view.is_none());
    assert!(matches!(app.mode, Mode::Normal));
}

/// Review finding 9: metadata the page supplied (title, final URL, the build id inside
/// `binding_reason`) reaches the gallery escaped, never as control sequences.
#[test]
fn page_controlled_metadata_is_escaped() {
    let evil = "\x1b]52;c;cHduZWQ=\x07\u{9b}2J";
    let mut v = shot(
        "S9",
        "s9",
        "remote_headless",
        "devbox",
        "illustrative",
        "/x.png",
        1,
    );
    v["binding_reason"] = json!(format!("Build not verified: build id {evil} is not tied"));
    v["title"] = json!(format!("Login{evil}"));
    v["final_url"] = json!(format!("http://localhost:5173/{evil}"));
    let s = Shot::parse(&v).unwrap();
    let clean = |t: &str| !t.chars().any(char::is_control);
    for t in [&s.binding_text(), &s.title, &s.url, &s.code_text()] {
        assert!(clean(t), "{t:?}");
    }
    assert!(
        s.binding_text().contains("build id \\x1b]52;c;"),
        "{}",
        s.binding_text()
    );
    let (mut app, mut rxs) = fleet();
    app.action("screenshots", None);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(&mut app, 0, req, json!({"screenshots": [v], "count": 1}));
    let text = screen(&app);
    assert!(text.contains("build id \\x1b]52;c;"), "{text}");
}

#[test]
fn open_locally_is_explicit() {
    let (mut app, mut rxs) = fleet();
    open_list(&mut app, &mut rxs[0]);
    assert!(app.gallery.opened.is_empty());
    app.on_key(ch('o'));
    assert_eq!(app.gallery.opened, vec!["/nonexistent/s3.png".to_string()]);
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn diff_selection_and_environment_mismatch() {
    let (mut app, mut rxs) = fleet();
    open_list(&mut app, &mut rxs[0]);
    // No mark: the selected screenshot against the next older one.
    app.on_key(ch('d'));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.diff");
    assert_eq!(p, json!({"a": "S2", "b": "S3"}));
    reply_err(
        &mut app,
        0,
        req,
        "invalid_params",
        json!({"reason": "environment_mismatch"}),
    );
    let s = screen(&app);
    assert!(s.contains("Different environments"), "{s}");
    assert!(s.contains("[!] compare anyway"), "{s}");
    app.on_key(ch('!'));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.diff");
    assert_eq!(p["force"], true);
    reply(
        &mut app,
        0,
        req,
        json!({"changed_ratio": 0.05, "regions": [{"x": 0, "y": 16, "width": 32, "height": 16, "pixels": 100}], "regions_total": 1, "blob": "dblob", "path_on_machine": "/nonexistent/d.png"}),
    );
    let s = screen(&app);
    assert!(s.contains("5.00% of pixels changed · 1 region(s)"), "{s}");
    app.on_key(named(NamedKey::Escape));
    assert!(app.gallery.view.as_ref().unwrap().diff.is_none());
    // A marked base is used instead.
    app.on_key(ch(' ')); // mark s3
    app.on_key(ch('j'));
    app.on_key(ch('j')); // s1
    app.on_key(ch('d'));
    let (_, p) = only(&commands(&mut rxs[0]), "browser.diff");
    assert_eq!(p, json!({"a": "S3", "b": "S1"}));
}

#[test]
fn delete_confirms_and_force_for_acceptance() {
    let (mut app, mut rxs) = fleet();
    open_list(&mut app, &mut rxs[0]);
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Delete this screenshot?"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S3"}));
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "referenced_by_acceptance"}),
    );
    commands(&mut rxs[0]);
    assert!(screen(&app).contains("[!] delete anyway"));
    app.on_key(ch('!'));
    let (_, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S3", "force": true}));
}

fn captured(seq: i64, pane: &str) -> PushedEvent {
    PushedEvent {
        seq,
        kind: "screenshot.captured".into(),
        json: json!({"type": "screenshot.captured", "subject": {"screenshot": "S9", "pane": pane}, "data": {"handle": "s9"}}).to_string(),
    }
}

#[test]
fn counters_from_captured_events() {
    let (mut app, mut rxs) = fleet();
    crate::push::on_events(
        &mut app,
        0,
        vec![captured(1, "p1"), captured(2, "p1")],
        false,
    );
    assert_eq!(badge(&app, 0, "p1"), Some(2));
    assert!(crate::draw::agent_row_text(&app, 0, "r1").contains("📷2"));
    // Unfocused pane corner shows it; the focused agent pane is never drawn over.
    app.machines[0].focus.pane = Some("p2".into());
    assert!(screen(&app).contains("📷2"));
    app.machines[0].focus.pane = Some("p1".into());
    // Peek shows it and p opens that agent's screenshots, clearing the counter.
    app.mode = Mode::Popup(Popup::Peek { pane: "p1".into() });
    assert!(screen(&app).contains("📷 2 new screenshot(s)"));
    app.on_key(ch('p'));
    assert_eq!(badge(&app, 0, "p1"), None);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    let mut l = list();
    l["screenshots"][0]["pane"] = json!("p2");
    reply(&mut app, 0, req, l);
    assert_eq!(app.gallery.view.as_ref().unwrap().shots.len(), 2);
    // While open for that pane, new captures refresh instead of counting.
    crate::push::on_events(&mut app, 0, vec![captured(3, "p1")], false);
    assert_eq!(badge(&app, 0, "p1"), None);
    assert!(
        commands(&mut rxs[0])
            .iter()
            .any(|c| c.1 == "screenshot.list")
    );
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(&app.mode, Mode::Popup(Popup::Peek { pane }) if pane == "p1"));
}

#[test]
fn screenshot_pane_follows_new_screenshots() {
    let (mut app, mut rxs) = fleet();
    app.action("screenshot_pane", None);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(&mut app, 0, req, list());
    let g = app.gallery.view.as_ref().unwrap();
    assert!(g.follow && g.pane_mode && g.view == View::Full);
    assert!(screen(&app).contains("following new"));
    crate::push::on_events(&mut app, 0, vec![captured(1, "p1")], false);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    let mut l = list();
    l["screenshots"].as_array_mut().unwrap().insert(
        0,
        shot(
            "S4",
            "s4",
            "local_pane",
            "your browser pane · profile devbox",
            "bound",
            "/x/s4.png",
            4,
        ),
    );
    reply(&mut app, 0, req, l);
    let g = app.gallery.view.as_ref().unwrap();
    assert_eq!(g.cur().unwrap().id, "S4");
    // Moving back in history stops following; esc closes the pane view.
    app.on_key(ch('j'));
    assert!(!app.gallery.view.as_ref().unwrap().follow);
    app.on_key(named(NamedKey::Escape));
    assert!(app.gallery.view.is_none());
}

#[test]
fn kitty_local_reads_file_remote_waits_for_v() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s3.png");
    std::fs::write(&path, tiny_png()).unwrap();
    let (mut app, mut rxs) = fleet_n(2);
    app.caps.kitty_graphics = true;
    app.caps.cell_w = 10;
    app.caps.cell_h = 20;
    app.gallery.gfx_dir = Some(dir.path().join("gfx"));
    // Local machine: the file is read directly; nothing is fetched.
    open_list(&mut app, &mut rxs[0]);
    if let Some(g) = &mut app.gallery.view {
        g.shots[0].path = path.to_string_lossy().into_owned();
    }
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(app.gallery.image("0:S3").is_some());
    before_draw(&mut app);
    let out = String::from_utf8_lossy(&app.browser.out).to_string();
    assert!(out.contains(&format!("a=T,i={IMAGE_ID},f=100")), "{out}");
    assert!(out.contains("U=1"), "{out}");
    // The host terminal is local: the PNG goes through a temp file (`t=t`), not base64.
    assert!(out.contains("t=t"), "{out}");
    // Placeholders for the image are drawn in the grid.
    let s = screen(&app);
    assert!(s.contains('\u{10EEEE}'));
    // Unchanged placement: nothing re-sent.
    app.browser.out.clear();
    before_draw(&mut app);
    assert!(app.browser.out.is_empty());
    // Closing deletes the image.
    app.on_key(named(NamedKey::Escape));
    app.on_key(named(NamedKey::Escape));
    before_draw(&mut app);
    let out = String::from_utf8_lossy(&app.browser.out).to_string();
    assert!(out.contains(&format!("a=d,d=I,i={IMAGE_ID}")), "{out}");
    // Remote machine: no image request until v.
    open(&mut app, 1, Scope::All, View::List, false);
    let (req, _) = only(&commands(&mut rxs[1]), "screenshot.list");
    reply(&mut app, 1, req, list());
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[1]).is_empty());
    assert!(screen(&app).contains("[v] view image"));
    app.on_key(ch('v'));
    let (req, p) = only(&commands(&mut rxs[1]), "screenshot.get");
    assert_eq!(p, json!({"id": "S3", "inline": true}));
    let b64 = base64::engine::general_purpose::STANDARD.encode(tiny_png());
    reply(&mut app, 1, req, json!({"id": "S3", "data_b64": b64}));
    assert!(app.gallery.image("1:S3").is_some());
    app.browser.out.clear();
    before_draw(&mut app);
    assert!(String::from_utf8_lossy(&app.browser.out).contains("a=T"));
}

#[test]
fn fit_keeps_aspect() {
    let (mut app, _rxs) = fleet();
    app.caps.cell_w = 10;
    app.caps.cell_h = 20;
    // 100×50 px into 40×40 cells: width-bound → 40 cols × 10 rows.
    assert_eq!(fit_cells(&app, 100, 50, 40, 40), (40, 10));
    assert_eq!(png_size(&tiny_png()), Some((100, 50)));
    assert_eq!(png_size(b"nope"), None);
}

#[test]
fn png_goes_through_a_private_temp_file_unless_ssh() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let gfx = dir.path().join("gfx");
    let png = tiny_png();
    let (mut app, _rxs) = fleet();
    app.caps.kitty_graphics = true;
    app.gallery.gfx_dir = Some(gfx.clone());
    let h = vk_browser::kitty::Header::new(IMAGE_ID, vk_browser::kitty::PixelFormat::Png, 0, 0);
    // Local: `t=t` with the path of a file kitty will accept (and delete) — 0700 dir, the
    // `tty-graphics-protocol` marker in the name, our bytes inside.
    let mut out = Vec::new();
    transmit_png(&mut app, &mut out, &h, &png);
    let o = String::from_utf8_lossy(&out).to_string();
    assert!(o.starts_with("\x1b_Ga=T,i="), "{o}");
    assert!(o.contains("t=t") && !o.contains("t=d"), "{o}");
    assert_eq!(
        std::fs::metadata(&gfx).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let files: Vec<_> = std::fs::read_dir(&gfx).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    let name = files[0].file_name().to_string_lossy().into_owned();
    assert!(name.contains("tty-graphics-protocol"), "{name}");
    assert_eq!(std::fs::read(files[0].path()).unwrap(), png);
    // The payload is that path (base64), not the image.
    let payload = o.split(';').nth(1).unwrap().trim_end_matches("\x1b\\");
    let path = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(path, files[0].path().to_string_lossy());
    // Over ssh the terminal can't read this machine's files: chunked direct transmission.
    app.caps.host_remote = true;
    let mut out = Vec::new();
    transmit_png(&mut app, &mut out, &h, &png);
    let o = String::from_utf8_lossy(&out).to_string();
    assert!(o.contains("t=d") && !o.contains("t=t"), "{o}");
    assert_eq!(std::fs::read_dir(&gfx).unwrap().count(), 1, "no new file");
    // A directory that isn't private (or isn't ours) is refused: direct again.
    app.caps.host_remote = false;
    std::fs::set_permissions(&gfx, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut out = Vec::new();
    transmit_png(&mut app, &mut out, &h, &png);
    assert!(String::from_utf8_lossy(&out).contains("t=d"));
    // A symlink in place of the directory is refused too.
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    app.gallery.gfx_dir = Some(link);
    let mut out = Vec::new();
    transmit_png(&mut app, &mut out, &h, &png);
    assert!(String::from_utf8_lossy(&out).contains("t=d"));
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
}

#[test]
fn agent_images_show_caption_not_url() {
    let (mut app, mut rxs) = fleet();
    app.action("screenshots", None);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    let mut a = shot(
        "S7",
        "s7",
        "agent",
        "attached by agent",
        "illustrative",
        "/nonexistent/s7.png",
        7,
    );
    a["url"] = json!("");
    a["caption"] = json!("Login page after the fix");
    a["source_name"] = json!("login.png");
    a["code"] = json!({});
    let mut b = a.clone();
    b["id"] = json!("S8");
    b["handle"] = json!("s8");
    b["caption"] = Value::Null;
    reply(
        &mut app,
        0,
        req,
        json!({"screenshots": [a, b], "count": 2, "total": 2}),
    );
    let s = screen(&app);
    assert!(s.contains("Login page after the fix"), "{s}");
    assert!(s.contains("login.png"), "{s}");
    assert!(s.contains("attached by agent"), "{s}");
    assert!(!s.contains("no checkout"), "{s}");
    assert!(!s.contains("illustrative"), "{s}");
    app.on_key(named(NamedKey::Enter));
    let s = screen(&app);
    assert!(s.contains("login.png"), "{s}");
    app.on_key(ch('j'));
    let s = screen(&app);
    assert!(s.contains("login.png"), "{s}");
}
