//! Hostile media frames (Goal 03 Codex review, finding 3): shm names only from the local
//! server, only for requested panes and only with that pane's tag; bounded geometry and
//! payloads; no SIGBUS on short objects; bounded inflation.

use super::*;
use crate::app::test_app;
use vk_proto::model::*;
use vk_proto::render::MediaTile;

fn pane(id: &str, browser: bool) -> Pane {
    let mut p: Pane = serde_json::from_value(json!({
        "id": id, "handle": "w1:p2", "tab": "T", "workspace": "W", "title": null,
        "auto_title": "x", "cwd": null, "cols": 80, "rows": 24,
        "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
        "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
        "recovered": null,
        "browser": {"url": "http://localhost:5173/", "machine": "", "task": null, "preview": null,
                    "source_pane": "p1", "history": [], "history_index": 0, "title": ""}
    }))
    .unwrap();
    if !browser {
        p.browser = None;
    }
    p
}

/// `n` machines (0 local); the tab with shell p1 and browser pane bp belongs to machine
/// `n - 1`. Views have been sent (to the local media host).
fn setup(n: usize) -> (App, Vec<tokio::sync::mpsc::UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = test_app(n);
    let mi = n - 1;
    app.cur = mi;
    let m = &mut app.machines[mi];
    m.model.workspaces = vec![Workspace {
        id: "W".into(),
        handle: "w1".into(),
        name: None,
        auto_name: "w".into(),
        root_path: "/".into(),
        task: None,
        order: 1.0,
        branch: None,
    }];
    m.model.tabs = vec![Tab {
        id: "T".into(),
        handle: "w1:t1".into(),
        workspace: "W".into(),
        title: None,
        number: 1,
        layout: LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: vec![
                (LayoutNode::Leaf { pane: "p1".into() }, 0.5),
                (LayoutNode::Leaf { pane: "bp".into() }, 0.5),
            ],
        },
        focused_pane: Some("bp".into()),
        zoomed_pane: None,
        order: 1.0,
        floating: Default::default(),
        floats_hidden: false,
    }];
    m.model.panes = vec![pane("p1", false), pane("bp", true)];
    m.focus = ClientFocus {
        workspace: Some("W".into()),
        tab: Some("T".into()),
        pane: Some("bp".into()),
    };
    app.caps.kitty_graphics = true;
    app.caps.truecolor = true;
    app.caps.cell_w = 16;
    app.caps.cell_h = 32;
    app.caps.dpr_x100 = 200;
    app.sidebar = false;
    app.size = (81, 25);
    update_views(&mut app);
    for rx in rxs.iter_mut() {
        while rx.try_recv().is_ok() {}
    }
    (app, rxs)
}

/// The pane is 40×23 content cells at 16×32 px: 640×736.
fn frame(tiles: Vec<MediaTile>) -> MediaFrame {
    MediaFrame {
        pane: "bp".into(),
        seq: 1,
        width: 640,
        height: 736,
        cell_w: 16,
        cell_h: 32,
        tile_cols: 4,
        tile_rows: 2,
        grid_cols: 10,
        grid_rows: 12,
        reset: true,
        tiles,
    }
}

fn tile(index: u32, data: TileData) -> MediaTile {
    MediaTile {
        index,
        col: 0,
        row: 0,
        cols: 4,
        rows: 2,
        w: 64,
        h: 64,
        data,
    }
}

const PX: usize = 64 * 64 * 4;

fn shm_tile(name: &str, len: usize) -> MediaTile {
    tile(
        0,
        TileData::Shm {
            name: name.into(),
            len: len as u32,
        },
    )
}

fn exists(name: &str) -> bool {
    kitty::shm::object_size(name).is_ok()
}

fn acked(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>) -> bool {
    let mut any = false;
    while let Ok(f) = rx.try_recv() {
        any |= matches!(f, ClientFrame::MediaAck { .. });
    }
    any
}

/// A remote machine (or the local one, for a pane it was never asked to render) can't make
/// this client open, forward or unlink a shm object: the frame is dropped and acked, the
/// object stays.
#[test]
fn shm_from_a_remote_or_for_an_unrequested_pane_is_never_touched() {
    let (mut app, mut rxs) = setup(2);
    app.caps.kitty_shm = true;
    let name = kitty::shm::new_name_for("bp");
    kitty::shm::write(&name, &vec![1u8; PX]).unwrap();
    // From the remote owner (machine 1), with a well-formed name for the visible pane.
    on_media(&mut app, 1, frame(vec![shm_tile(&name, PX)]));
    assert!(exists(&name), "a remote frame unlinked a local shm object");
    assert!(
        take_output(&mut app).is_empty(),
        "nothing forwarded to the host"
    );
    assert!(acked(&mut rxs[1]));
    // From the local server, for a pane this client never asked it to render.
    let other = kitty::shm::new_name_for("zz");
    kitty::shm::write(&other, &vec![1u8; PX]).unwrap();
    let mut f = frame(vec![shm_tile(&other, PX)]);
    f.pane = "zz".into();
    on_media(&mut app, 0, f);
    assert!(exists(&other));
    assert!(take_output(&mut app).is_empty());
    kitty::shm::unlink(&name);
    kitty::shm::unlink(&other);
}

/// From the local server, for the requested pane: names without Vibeke's prefix or with
/// another pane's tag are refused (and never unlinked); a valid name is used.
#[test]
fn shm_names_must_carry_the_panes_tag() {
    let (mut app, mut rxs) = setup(1);
    app.caps.kitty_shm = true;
    let victim = format!("/victim-{:x}", std::process::id());
    kitty::shm::write(&victim, &vec![1u8; PX]).unwrap();
    let foreign = kitty::shm::new_name_for("another-pane");
    kitty::shm::write(&foreign, &vec![1u8; PX]).unwrap();
    for n in [&victim, &foreign] {
        on_media(&mut app, 0, frame(vec![shm_tile(n, PX)]));
        assert!(exists(n), "{n} was unlinked");
        assert!(take_output(&mut app).is_empty(), "{n} was forwarded");
        assert!(acked(&mut rxs[0]));
    }
    let good = kitty::shm::new_name_for("bp");
    kitty::shm::write(&good, &vec![1u8; PX]).unwrap();
    on_media(&mut app, 0, frame(vec![shm_tile(&good, PX)]));
    let out = String::from_utf8_lossy(&take_output(&mut app)).into_owned();
    assert!(out.contains("t=s"), "{out}");
    // The host terminal would unlink it after reading; this test does.
    kitty::shm::unlink(&good);
    kitty::shm::unlink(&victim);
    kitty::shm::unlink(&foreign);
}

/// An object smaller than its declared length is neither forwarded (the host would map past
/// its end) nor mapped by the client (SIGBUS).
#[test]
fn short_shm_objects_are_not_mapped_or_forwarded() {
    for host_reads_shm in [true, false] {
        let (mut app, _rxs) = setup(1);
        app.caps.kitty_shm = host_reads_shm;
        let name = kitty::shm::new_name_for("bp");
        kitty::shm::write(&name, &[9u8; 16]).unwrap();
        // The object may be page-rounded; declare far more than a page.
        let mut big = frame(vec![MediaTile {
            w: 512,
            h: 256,
            cols: 32,
            rows: 8,
            ..shm_tile(&name, 512 * 256 * 4)
        }]);
        big.tile_cols = 32;
        big.tile_rows = 8;
        on_media(&mut app, 0, big);
        let out = String::from_utf8_lossy(&take_output(&mut app)).into_owned();
        assert!(!out.contains("t=s"), "short object forwarded: {out}");
        assert!(!out.contains("a=T"), "garbage drawn: {out}");
        kitty::shm::unlink(&name);
    }
}

/// Oversized frames, tiles outside the pane or the frame, ids outside the pane's range and
/// payloads that don't match their declared size are dropped (and acked), never drawn.
#[test]
fn malformed_geometry_and_payloads_are_dropped() {
    let px = vec![1u8; PX];
    let bad: Vec<(&str, MediaFrame)> = vec![
        ("huge frame", {
            let mut f = frame(vec![tile(0, TileData::Rgba(px.clone()))]);
            f.width = 100_000;
            f.height = 100_000;
            f
        }),
        ("frame larger than the pane", {
            let mut f = frame(vec![tile(0, TileData::Rgba(px.clone()))]);
            f.width = 4000;
            f
        }),
        ("tile outside the pane", {
            frame(vec![MediaTile {
                col: 60,
                ..tile(5, TileData::Rgba(px.clone()))
            }])
        }),
        (
            "tile index outside the grid",
            frame(vec![tile(50_000, TileData::Rgba(px.clone()))]),
        ),
        ("tile larger than its cells", {
            frame(vec![MediaTile {
                w: 4096,
                h: 4096,
                ..tile(0, TileData::Rgba(vec![0; 4096 * 4096 * 4]))
            }])
        }),
        (
            "truncated rgba",
            frame(vec![tile(0, TileData::Rgba(vec![1u8; 100]))]),
        ),
        ("zero cell size", {
            let mut f = frame(vec![tile(0, TileData::Rgba(px.clone()))]);
            f.cell_w = 0;
            f
        }),
        ("absurd grid", {
            let mut f = frame(vec![tile(0, TileData::Rgba(px.clone()))]);
            f.grid_cols = u16::MAX;
            f.grid_rows = u16::MAX;
            f
        }),
        ("shm length mismatch", {
            frame(vec![shm_tile(&kitty::shm::new_name_for("bp"), 7)])
        }),
    ];
    for (what, f) in bad {
        let (mut app, mut rxs) = setup(1);
        assert!(validate_frame(&f, 40, 23).is_err(), "{what}");
        on_media(&mut app, 0, f);
        assert!(take_output(&mut app).is_empty(), "{what}: drawn");
        assert!(acked(&mut rxs[0]), "{what}: not acked");
        assert_eq!(app.browser.rejected_frames, 1, "{what}");
    }
    // The well-formed frame passes.
    assert!(validate_frame(&frame(vec![tile(0, TileData::Rgba(px))]), 40, 23).is_ok());
}

/// A zlib tile that inflates beyond its declared pixels is not inflated past the bound.
#[test]
fn decompression_bombs_are_bounded() {
    // SAFETY: test-only env, read by zlib_ok(); every test in this binary sets "0" only.
    unsafe { std::env::set_var("VIBEKE_KITTY_ZLIB", "0") };
    let (mut app, _rxs) = setup(1);
    let bomb = kitty::zlib(&vec![0u8; 8 << 20], 9);
    assert!(bomb.len() < PX, "bomb passes the compressed-size bound");
    on_media(&mut app, 0, frame(vec![tile(0, TileData::ZlibRgba(bomb))]));
    let out = String::from_utf8_lossy(&take_output(&mut app)).into_owned();
    assert!(!out.contains("a=T"), "bomb drawn");
    // Same through the iTerm2 path (which always inflates).
    let (mut app, _rxs) = setup(1);
    app.caps.kitty_graphics = false;
    app.caps.iterm2_images = true;
    let bomb = kitty::zlib(&vec![0u8; 8 << 20], 9);
    on_media(&mut app, 0, frame(vec![tile(0, TileData::ZlibRgba(bomb))]));
}

/// Hidden pane: a frame in flight from the local server with valid names is dropped and its
/// objects freed (they are verifiably this pane's), so nothing leaks.
#[test]
fn hidden_pane_frees_its_own_tiles_only() {
    let (mut app, mut rxs) = setup(1);
    app.caps.kitty_shm = true;
    let good = kitty::shm::new_name_for("bp");
    kitty::shm::write(&good, &vec![1u8; PX]).unwrap();
    app.machines[0].model.tabs[0].zoomed_pane = Some("p1".into());
    update_views(&mut app);
    while rxs[0].try_recv().is_ok() {}
    on_media(&mut app, 0, frame(vec![shm_tile(&good, PX)]));
    assert!(!exists(&good), "valid tile of a hidden pane leaked");
    assert!(take_output(&mut app).is_empty());
}
