//! Navigation tests: fuzzy ranking and highlights, the palette (recent first, live watch
//! entries), goto ranking over branches / native session ids / recency / urgency, per-client
//! history persistence and `last_workspace`, URL/ID hints, title sync, and drawing.

use super::*;
use crate::app::{test_app, test_interaction, test_run};
use crate::tasks::grid_text;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::Mods;
use vk_proto::model::*;
use vk_proto::render::{ClientFrame, Span};

fn kev(k: Key) -> KeyEvent {
    KeyEvent::new(k, Mods::empty())
}

fn ch(c: char) -> KeyEvent {
    kev(Key::Char(c))
}

fn pane(id: &str, tab: &str, ws: &str, title: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": tab, "workspace": ws, "title": title,
        "auto_title": "zsh", "cwd": "/src/api", "cols": 80, "rows": 24,
        "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
        "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
        "recovered": null
    }))
    .unwrap()
}

fn ws(id: &str, name: &str, root: &str, branch: Option<&str>) -> Workspace {
    Workspace {
        id: id.into(),
        handle: format!("h{id}"),
        name: Some(name.into()),
        auto_name: name.into(),
        root_path: root.into(),
        task: None,
        order: 1.0,
        branch: branch.map(str::to_string),
    }
}

fn tab(id: &str, ws: &str, n: u32, panes: &[&str]) -> Tab {
    let layout = if panes.len() == 1 {
        LayoutNode::Leaf {
            pane: panes[0].into(),
        }
    } else {
        LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: panes
                .iter()
                .map(|p| {
                    (
                        LayoutNode::Leaf {
                            pane: p.to_string(),
                        },
                        0.5,
                    )
                })
                .collect(),
        }
    };
    Tab {
        id: id.into(),
        handle: format!("t{id}"),
        workspace: ws.into(),
        title: None,
        number: n,
        layout,
        focused_pane: Some(panes[0].into()),
        zoomed_pane: None,
        order: n as f64,
        floating: Default::default(),
        floats_hidden: false,
    }
}

/// Two workspaces: `api` (branch `feature/login`, tab 1 with a claude agent `reviewer` whose
/// native session is `sess-7f3a`, and a shell), `web` (tab 1 with a server pane).
fn fleet() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, rxs) = test_app(1);
    let m = &mut app.machines[0];
    m.model.workspaces = vec![
        ws("W1", "api", "/src/api", Some("feature/login")),
        ws("W2", "web", "/src/web", Some("main")),
    ];
    m.model.tabs = vec![
        tab("T1", "W1", 1, &["p1", "p2"]),
        tab("T2", "W2", 1, &["p3"]),
    ];
    m.model.panes = vec![
        pane("p1", "T1", "W1", "claude"),
        pane("p2", "T1", "W1", "shell"),
        pane("p3", "T2", "W2", "vite server"),
    ];
    let mut r = test_run("r1", "p1", "claude");
    r.name = Some("reviewer".into());
    r.harness_session_id = Some("sess-7f3a".into());
    m.model.runs = vec![r];
    m.focus = ClientFocus {
        workspace: Some("W1".into()),
        tab: Some("T1".into()),
        pane: Some("p1".into()),
    };
    (app, rxs)
}

fn drain(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<ClientFrame> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        v.push(f);
    }
    v
}

fn commands(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<(String, Value)> {
    drain(rx)
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Command { json, .. } => {
                let v: Value = serde_json::from_str(&json).unwrap();
                Some((
                    v["method"].as_str().unwrap().to_string(),
                    v["params"].clone(),
                ))
            }
            _ => None,
        })
        .collect()
}

// ---- fuzzy -------------------------------------------------------------------------------

#[test]
fn fuzzy_matches_subsequences_with_positions() {
    assert!(fuzzy("abc", "xaxbxc").is_some());
    assert!(fuzzy("abc", "acb").is_none());
    assert_eq!(fuzzy("nt", "new tab").unwrap().positions, vec![0, 4]);
    assert_eq!(fuzzy("NT", "new tab").unwrap().positions, vec![0, 4]);
    assert_eq!(fuzzy("", "anything").unwrap().score, 0);
    // Every token must match; positions are merged.
    let m = fuzzy("api login", "api ⎇ feature/login").unwrap();
    assert_eq!(&m.positions[..3], &[0, 1, 2]);
    assert!(fuzzy("api nope", "api ⎇ feature/login").is_none());
}

fn best<'a>(q: &str, cands: &[&'a str]) -> &'a str {
    cands
        .iter()
        .filter_map(|c| fuzzy(q, c).map(|m| (m.score, std::cmp::Reverse(c.len()), *c)))
        .max_by_key(|(s, l, _)| (*s, *l))
        .unwrap()
        .2
}

#[test]
fn fuzzy_ranking_prefers_boundaries_runs_and_short() {
    // Word starts beat letters inside words.
    assert_eq!(
        best("nt", &["ant hill", "new tab", "notifications"]),
        "new tab"
    );
    // A run at a word start beats a run inside a word; a run beats a scattered match.
    assert_eq!(best("tab", &["stable", "new tab"]), "new tab");
    assert_eq!(best("abc", &["xaxbxcx", "xxabcxx"]), "xxabcxx");
    // camelCase and path boundaries.
    assert_eq!(
        best("fl", &["shuffle", "src/featureLogin.rs"]),
        "src/featureLogin.rs"
    );
    // Prefix of the whole string wins over a later boundary.
    assert_eq!(
        best("sp", &["close split", "split vertical"]),
        "split vertical"
    );
    // Equal matches: the shorter candidate wins.
    assert_eq!(best("zoom", &["zoom the pane now please", "zoom"]), "zoom");
    // Positions point at the best alignment, not the first letters seen.
    assert_eq!(
        fuzzy("vs", "previous vite server").unwrap().positions,
        vec![9, 14]
    );
}

#[test]
fn highlight_groups_runs() {
    let base = Style::default();
    let hi = Style {
        attrs: attr::BOLD,
        ..Style::default()
    };
    let segs = highlight("new tab", &[0, 4, 5], base, hi);
    let texts: Vec<&str> = segs.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(texts, ["n", "ew ", "ta", "b"]);
    assert_eq!(segs[2].1, hi);
}

// ---- palette -----------------------------------------------------------------------------

#[test]
fn palette_lists_actions_with_bindings_and_recent_first() {
    let (mut app, _rx) = fleet();
    let all = palette_entries(&app);
    let sv = all.iter().find(|e| e.id == "split_vertical").unwrap();
    assert_eq!(sv.desc, "Split side by side");
    assert_eq!(sv.binding.as_deref(), Some("prefix+v"));
    assert!(
        all.iter()
            .any(|e| e.id == "last_workspace" && e.binding.as_deref() == Some("prefix+shift+l"))
    );
    assert!(all.iter().any(|e| e.id == "url_hints"));
    assert!(
        all.iter().any(|e| e.id == "browser_take_over"
            && e.binding.as_deref() == Some("prefix+t (browser pane)"))
    );
    assert!(all.iter().any(|e| e.id == "track_work"));
    assert!(!all.iter().any(|e| e.id == "command_palette"));
    // Fuzzy: description words.
    let r = palette_ranked(&app, "split side");
    assert_eq!(r[0].0.id, "split_vertical");
    let r = palette_ranked(&app, "lastws");
    assert_eq!(r[0].0.id, "last_workspace", "matches the action name too");
    // Recently used first with an empty filter, and boosted in searches.
    run_palette(&mut app, "zoom");
    run_palette(&mut app, "toggle_sidebar");
    let r = palette_ranked(&app, "");
    assert_eq!(r[0].0.id, "toggle_sidebar");
    assert_eq!(r[1].0.id, "zoom");
    assert_eq!(app.nav.hist.actions, ["toggle_sidebar", "zoom"]);
}

#[test]
fn palette_watch_entries_and_keys() {
    let (mut app, mut rxs) = fleet();
    on_reply(
        &mut app,
        0,
        Reply::Sessions,
        Ok(json!({"sessions": [
            {"session": "b3", "url": "http://localhost:5173/login", "owner": {"pane": "p1"}, "human_control": false}
        ]})),
    );
    let r = palette_ranked(&app, "watch b3");
    assert_eq!(r[0].0.id, "watch:0:b3");
    assert!(r[0].0.desc.contains("localhost:5173/login"));
    // Opening the palette refreshes the session list; typing filters; enter runs.
    app.action("command_palette", None);
    assert!(
        commands(&mut rxs[0])
            .iter()
            .any(|(m, _)| m == "browser.list")
    );
    for c in "watch b3".chars() {
        app.on_key(ch(c));
    }
    app.on_key(kev(Key::Named(NamedKey::Enter)));
    let cmds = commands(&mut rxs[0]);
    let (m, p) = cmds.iter().find(|(m, _)| m == "browser.watch").unwrap();
    assert_eq!(m, "browser.watch");
    assert_eq!(p["session"], "b3");
    assert!(matches!(app.mode, Mode::Normal));
    // The peek offers it too.
    assert_eq!(session_of_pane(&app, 0, "p1").unwrap().handle, "b3");
    app.mode = Mode::Popup(Popup::Peek { pane: "p1".into() });
    app.on_key(ch('w'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = cmds.iter().find(|(m, _)| m == "browser.watch").unwrap();
    assert_eq!(p["agent_pane"], "p1");
}

// ---- goto --------------------------------------------------------------------------------

#[test]
fn goto_ranks_by_score_then_recency_then_urgency() {
    let (mut app, mut rxs) = fleet();
    // Branch and native session id are searchable.
    let r = goto_ranked(&app, "login");
    assert_eq!(r[0].0.target, GotoTarget::Workspace("W1".into()));
    let r = goto_ranked(&app, "7f3a");
    assert_eq!(r[0].0.target, GotoTarget::Pane("p1".into()));
    let r = goto_ranked(&app, "reviewer");
    assert_eq!(r[0].0.target, GotoTarget::Pane("p1".into()));
    // Highlight positions are within the shown label.
    assert!(r[0].1.iter().all(|p| *p < r[0].0.label.chars().count()));
    // Kind prefixes.
    assert!(goto_ranked(&app, "@").iter().all(|(e, _)| e.kind == '@'));
    assert!(goto_ranked(&app, "~").iter().all(|(e, _)| e.kind == '~'));
    assert!(goto_ranked(&app, ":web").iter().all(|(e, _)| e.kind == ':'));
    // Recency breaks score ties: two equal shells, the recently visited one first.
    app.machines[0].model.panes[2].title = Some("shell".into());
    app.machines[0].model.tabs[1].title = Some("x".into());
    let first = |app: &App| {
        goto_ranked(app, "shell")
            .into_iter()
            .find(|(e, _)| matches!(e.target, GotoTarget::Pane(_)))
            .unwrap()
            .0
            .target
    };
    let before = first(&app);
    let other = if before == GotoTarget::Pane("p2".into()) {
        "p3"
    } else {
        "p2"
    };
    app.nav.hist.note_target(TargetRef {
        machine: "m0".into(),
        kind: "pane".into(),
        id: other.into(),
        workspace: None,
    });
    assert_eq!(first(&app), GotoTarget::Pane(other.into()));
    // Empty query: urgent agents right after recent targets; `!approve` filters.
    app.nav.hist.targets.clear();
    app.machines[0]
        .model
        .interactions
        .push(test_interaction("i1", "p1", "rm -rf", 0));
    let r = goto_ranked(&app, "");
    assert_eq!(r[0].0.target, GotoTarget::Pane("p1".into()));
    let r = goto_ranked(&app, "!approve");
    assert_eq!(r.len(), 1);
    // `>` switches to the palette; enter jumps.
    app.mode = Mode::Popup(Popup::Goto {
        filter: String::new(),
        sel: 0,
    });
    app.on_key(ch('>'));
    assert!(matches!(app.mode, Mode::Popup(Popup::Palette { .. })));
    app.mode = Mode::Popup(Popup::Goto {
        filter: String::new(),
        sel: 0,
    });
    for c in "~web".chars() {
        app.on_key(ch(c));
    }
    drain(&mut rxs[0]);
    app.on_key(kev(Key::Named(NamedKey::Enter)));
    let focus: Vec<String> = drain(&mut rxs[0])
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Focus { pane } => Some(pane),
            _ => None,
        })
        .collect();
    assert_eq!(focus, ["p3"]);
}

// ---- history -----------------------------------------------------------------------------

#[test]
fn history_persists_per_client_and_last_workspace_toggles() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, mut rxs) = fleet();
    app.nav = Nav::open(dir.path().to_path_buf(), "laptop", "main");
    observe(&mut app);
    // Move to the web workspace.
    app.machines[0].focus = ClientFocus {
        workspace: Some("W2".into()),
        tab: Some("T2".into()),
        pane: Some("p3".into()),
    };
    observe(&mut app);
    assert_eq!(app.nav.hist.cur_ws, Some(("m0".into(), "W2".into())));
    assert_eq!(app.nav.hist.last_ws, Some(("m0".into(), "W1".into())));
    assert_eq!(app.nav.hist.targets[0].id, "p3");
    // Persisted per client key; another client keeps its own history.
    let reloaded = Nav::open(dir.path().to_path_buf(), "laptop", "main");
    assert_eq!(reloaded.hist, app.nav.hist);
    let other = Nav::open(dir.path().to_path_buf(), "desk", "main");
    assert!(other.hist.targets.is_empty());
    assert!(dir.path().join("nav-laptop.json").exists());
    // last_workspace focuses the most recent pane of the previous workspace (p1, not p2).
    drain(&mut rxs[0]);
    app.action("last_workspace", None);
    let f: Vec<String> = drain(&mut rxs[0])
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Focus { pane } => Some(pane),
            _ => None,
        })
        .collect();
    assert_eq!(f, ["p1"]);
    observe(&mut app);
    // …and toggles back.
    app.action("last_workspace", None);
    let f: Vec<String> = drain(&mut rxs[0])
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Focus { pane } => Some(pane),
            _ => None,
        })
        .collect();
    assert_eq!(f, ["p3"]);
    // Caps.
    let mut h = History::default();
    for i in 0..80 {
        h.note_target(TargetRef {
            machine: "m".into(),
            kind: "pane".into(),
            id: format!("p{i}"),
            workspace: None,
        });
        h.note_action(&format!("a{i}"));
    }
    assert_eq!(h.targets.len(), MAX_TARGETS);
    assert_eq!(h.actions.len(), MAX_ACTIONS);
    assert_eq!(h.targets[0].id, "p79");
}

#[test]
fn client_key_is_sanitized() {
    // SAFETY: test-only env var, read in this thread.
    unsafe { std::env::set_var("VIBEKE_CLIENT_NAME", "../my laptop!") };
    assert_eq!(client_key(), "..mylaptop");
    unsafe { std::env::remove_var("VIBEKE_CLIENT_NAME") };
    assert_eq!(client_key(), "default");
}

// ---- hints -------------------------------------------------------------------------------

fn row(s: &str) -> Row {
    Row {
        spans: vec![Span {
            style: Style::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        wrapped: false,
        ..Default::default()
    }
}

#[test]
fn hints_find_urls_paths_shas_and_handles() {
    let lines = vec![
        row("  ➜  Local:   http://localhost:5173/app/"),
        row("error at src/routes/login.tsx:42:7 (see https://example.com/docs)."),
        row("commit 3f9c2ab1 merged; pane w2:p1 and browser b3, task #k12"),
        row("nothing here: 12345 deadbeef 0x1f abc"),
    ];
    let found = scan(&lines);
    let texts: Vec<(&str, HintKind)> = found.iter().map(|f| (f.2.as_str(), f.3)).collect();
    assert_eq!(
        texts,
        [
            ("http://localhost:5173/app/", HintKind::Url),
            ("src/routes/login.tsx:42:7", HintKind::Path),
            ("https://example.com/docs", HintKind::Url),
            ("3f9c2ab1", HintKind::Sha),
            ("w2:p1", HintKind::Id),
            ("b3", HintKind::Id),
            ("#k12", HintKind::Id),
        ]
    );
    // Columns are cell positions.
    assert_eq!((found[0].0, found[0].1), (0, 14));
    assert_eq!(labels(3), ["a", "s", "d"]);
    let many = labels(30);
    assert_eq!(many.len(), 30);
    assert!(many.iter().all(|l| l.len() == 2));
    let set: std::collections::HashSet<_> = many.iter().collect();
    assert_eq!(set.len(), 30);
}

#[test]
fn hint_keys_open_copy_and_cancel() {
    let mut h = Hints {
        machine: 0,
        pane: "p1".into(),
        items: vec![
            Hint {
                label: "a".into(),
                text: "http://localhost:5173/".into(),
                kind: HintKind::Url,
                row: 0,
                col: 0,
            },
            Hint {
                label: "s".into(),
                text: "3f9c2ab1".into(),
                kind: HintKind::Sha,
                row: 1,
                col: 0,
            },
        ],
        typed: String::new(),
    };
    assert_eq!(
        hint_key(&mut h.clone(), &ch('a')),
        HintStep::Done(HintAction::Open("http://localhost:5173/".into()))
    );
    assert_eq!(
        hint_key(&mut h.clone(), &KeyEvent::new(Key::Char('A'), Mods::SHIFT)),
        HintStep::Done(HintAction::Copy("http://localhost:5173/".into()))
    );
    assert_eq!(
        hint_key(&mut h.clone(), &ch('s')),
        HintStep::Done(HintAction::Copy("3f9c2ab1".into()))
    );
    assert_eq!(hint_key(&mut h, &ch('z')), HintStep::Close);
    assert_eq!(
        hint_key(&mut h, &kev(Key::Named(NamedKey::Escape))),
        HintStep::Close
    );
    // Two-letter labels wait for the second key.
    let mut h2 = Hints {
        items: labels(30)
            .into_iter()
            .map(|l| Hint {
                label: l,
                text: "b3".into(),
                kind: HintKind::Id,
                row: 0,
                col: 0,
            })
            .collect(),
        ..h
    };
    h2.typed.clear();
    assert_eq!(hint_key(&mut h2, &ch('a')), HintStep::Wait);
    assert_eq!(
        hint_key(&mut h2, &ch('s')),
        HintStep::Done(HintAction::Copy("b3".into()))
    );
}

#[test]
fn hints_overlay_end_to_end() {
    let (mut app, mut rxs) = fleet();
    app.sidebar = false;
    app.caps.kitty_graphics = true;
    let buf = app.machines[0].panes.get_mut("p1").unwrap();
    buf.lines = vec![
        row("server at http://localhost:5173/ ready"),
        row("docs https://example.com/x and 3f9c2ab1"),
    ];
    app.action("url_hints", None);
    let Mode::Popup(Popup::Hints(h)) = &app.mode else {
        panic!("hints open")
    };
    assert_eq!(h.items.len(), 3);
    // Drawn over the pane at the targets, with the help line.
    let mut g = Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = grid_text(&g);
    assert!(text.contains("hints: type a label"), "{text}");
    let r = app.pane_rects()[0].1;
    let cell = &g.row(r.y)[(r.x + 10) as usize];
    assert_eq!(cell.text.as_str(), "a");
    // `a` opens the localhost URL as a browser pane next to p1.
    app.on_key(ch('a'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = cmds
        .iter()
        .find(|(m, _)| m == "browser.pane.create")
        .expect("browser pane for a localhost URL");
    assert_eq!(p["url"], "http://localhost:5173/");
    assert_eq!(p["pane"], "p1");
    // A public URL goes to the OS opener; a SHA is copied.
    app.action("url_hints", None);
    app.on_key(ch('s'));
    assert_eq!(app.nav.opened, ["https://example.com/x"]);
    app.action("url_hints", None);
    app.on_key(ch('d'));
    let sink = app.clipboard_sink.as_ref().unwrap();
    assert_eq!(sink.last().unwrap().0, b"3f9c2ab1");
}

// ---- title -------------------------------------------------------------------------------

#[test]
fn title_sync_formats_and_writes_osc2_once() {
    let (mut app, _rx) = fleet();
    app.nav.session = "main".into();
    assert_eq!(title(&app).as_deref(), Some("api · reviewer"));
    let first = String::from_utf8(title_update(&mut app).unwrap()).unwrap();
    assert_eq!(first, "\x1b[22;2t\x1b]2;api · reviewer\x07");
    assert!(title_update(&mut app).is_none(), "unchanged → nothing");
    app.machines[0].focus.pane = Some("p2".into());
    let next = String::from_utf8(title_update(&mut app).unwrap()).unwrap();
    assert_eq!(next, "\x1b]2;api · shell\x07");
    app.config.ui.title_format = "{session}:{machine} {workspace}/{tab}\x07evil".into();
    assert_eq!(title(&app).as_deref(), Some("main:m0 api/1evil"));
    app.config.ui.title_sync = false;
    assert!(title_update(&mut app).is_none());
}

// ---- drawing -----------------------------------------------------------------------------

#[test]
fn palette_and_goto_draw_with_highlights() {
    let (mut app, _rx) = fleet();
    app.mode = Mode::Popup(Popup::Palette {
        filter: "split side".into(),
        sel: 0,
    });
    let mut g = Grid::new(120, 40);
    let cursor = crate::draw::compose(&app, &mut g);
    let text = grid_text(&g);
    assert!(text.contains("command palette"), "{text}");
    assert!(text.contains("> split side"));
    assert!(text.contains("Split side by side  split_vertical"));
    assert!(text.contains("prefix+v"));
    assert!(cursor.is_some(), "the filter has a cursor");
    // The matched letters are highlighted (bold+underline).
    let (y, line) = (0..40u16)
        .map(|y| {
            (
                y,
                g.row(y).iter().map(|c| c.text.as_str()).collect::<String>(),
            )
        })
        .find(|(_, l)| l.contains("Split side by side"))
        .unwrap();
    let x = line.find("Split side").unwrap();
    let x = line[..x].chars().count();
    assert!(g.row(y)[x].style.attrs & attr::UNDERLINE != 0);
    app.mode = Mode::Popup(Popup::Goto {
        filter: "login".into(),
        sel: 0,
    });
    let mut g = Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = grid_text(&g);
    assert!(text.contains("goto"), "{text}");
    assert!(text.contains("api ⎇ feature/login"));
    assert!(text.contains("workspace"));
}

// ---- goto secondary action (ctrl+enter / alt+enter) -------------------------------

fn goto_with(app: &mut App, q: &str) {
    app.mode = Mode::Popup(Popup::Goto {
        filter: String::new(),
        sel: 0,
    });
    for c in q.chars() {
        app.on_key(ch(c));
    }
}

fn enter_with(mods: Mods) -> KeyEvent {
    KeyEvent::new(Key::Named(NamedKey::Enter), mods)
}

#[test]
fn goto_ctrl_enter_and_alt_enter_split_next_to_the_current_pane() {
    for mods in [Mods::CTRL, Mods::ALT] {
        let (mut app, mut rxs) = fleet();
        goto_with(&mut app, "~web");
        drain(&mut rxs[0]);
        app.on_key(enter_with(mods));
        let cmds = commands(&mut rxs[0]);
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert_eq!(cmds[0].0, "pane.split");
        assert_eq!(cmds[0].1["pane"], "p1");
        assert_eq!(cmds[0].1["cwd"], "/src/web");
        assert_eq!(cmds[0].1["focus"], true);
        assert!(matches!(app.mode, Mode::Normal), "popup closed");
        // A pane target splits in that pane's cwd.
        goto_with(&mut app, "vite");
        drain(&mut rxs[0]);
        app.on_key(enter_with(mods));
        let cmds = commands(&mut rxs[0]);
        assert_eq!(cmds[0].0, "pane.split");
        assert_eq!(cmds[0].1["cwd"], "/src/api");
    }
}

#[test]
fn goto_plain_enter_still_only_jumps() {
    let (mut app, mut rxs) = fleet();
    goto_with(&mut app, "~web");
    drain(&mut rxs[0]);
    app.on_key(enter_with(Mods::empty()));
    assert!(
        commands(&mut rxs[0])
            .iter()
            .all(|(m, _)| m != "pane.split" && m != "workspace.create")
    );
}

#[test]
fn goto_alt_enter_on_a_path_creates_a_workspace() {
    for mods in [Mods::CTRL, Mods::ALT] {
        let (mut app, mut rxs) = fleet();
        goto_with(&mut app, "/src/new");
        drain(&mut rxs[0]);
        app.on_key(enter_with(mods));
        let cmds = commands(&mut rxs[0]);
        assert_eq!(cmds.len(), 1, "{cmds:?}");
        assert_eq!(cmds[0].0, "workspace.create");
        assert_eq!(cmds[0].1["cwd"], "/src/new");
        assert_eq!(cmds[0].1["focus"], true);
    }
    assert_eq!(goto_path("~"), None);
    assert_eq!(goto_path("login"), None);
}

fn preview_on(app: &mut App, mi: usize) {
    app.machines[mi].model.previews = vec![
        serde_json::from_value(json!({
            "id": "PV", "handle": "v4", "machine": "m0", "pane": "p3", "task": null,
            "port": 5173, "path": "/app", "label": "vite", "url": "http://localhost:5173/app",
            "scheme": "http", "status": "up", "source": "banner", "pid": null,
            "first_seen_ms": 0, "last_seen_ms": 0
        }))
        .unwrap(),
    ];
}

#[test]
fn goto_lists_previews_and_enter_opens_them() {
    let (mut app, mut rxs) = fleet();
    app.caps.kitty_graphics = true;
    preview_on(&mut app, 0);
    // Matched by port, label and URL; `%` narrows to previews.
    let r = goto_ranked(&app, "5173");
    assert_eq!(r[0].0.target, GotoTarget::Preview("PV".into()));
    assert_eq!(r[0].0.label, "v4 :5173/app vite");
    assert!(goto_ranked(&app, "%").iter().all(|(e, _)| e.kind == '%'));
    assert_eq!(goto_ranked(&app, "%vite").len(), 1);
    // A single machine has no machine entries.
    assert!(goto_ranked(&app, "^").is_empty());
    // Drawn with its kind.
    goto_with(&mut app, "%vite");
    let mut g = Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = grid_text(&g);
    assert!(text.contains("%preview ^machine"), "{text}");
    assert!(text.contains("% v4 :5173/app vite"), "{text}");
    assert!(text.contains("preview"), "{text}");
    // enter: a browser pane next to the preview's pane.
    drain(&mut rxs[0]);
    app.on_key(kev(Key::Named(NamedKey::Enter)));
    let cmds = commands(&mut rxs[0]);
    let c = cmds.iter().find(|c| c.0 == "browser.pane.create").unwrap();
    assert_eq!(c.1["preview"], "PV");
    assert_eq!(c.1["pane"], "p3");
    // alt+enter: the profile browser window.
    goto_with(&mut app, "%vite");
    drain(&mut rxs[0]);
    app.on_key(KeyEvent::new(Key::Named(NamedKey::Enter), Mods::ALT));
    let cmds = commands(&mut rxs[0]);
    let c = cmds.iter().find(|c| c.0 == "preview.open").unwrap();
    assert_eq!(c.1["window"], true);
}

#[test]
fn goto_lists_machines_with_several_and_enter_switches() {
    let (mut app, _rxs) = test_app(2);
    for m in app.machines.iter_mut() {
        m.model.workspaces = vec![ws("W1", "api", "/src/api", None)];
        m.model.tabs = vec![tab("T1", "W1", 1, &["p1"])];
        m.model.panes = vec![pane("p1", "T1", "W1", "claude")];
        m.model.runs = vec![test_run("r1", "p1", "claude")];
    }
    app.machines[1].label = "devbox".into();
    let r = goto_ranked(&app, "^devbox");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0.target, GotoTarget::Machine("devbox".into()));
    assert_eq!(r[0].0.mi, 1);
    assert!(r[0].0.label.starts_with("devbox · connected · 1 agent(s)"));
    // Machine entries never pass a state filter.
    app.machines[1]
        .model
        .interactions
        .push(test_interaction("i1", "p1", "rm", 0));
    assert!(
        goto_ranked(&app, "!approve")
            .iter()
            .all(|(e, _)| e.kind != '^')
    );
    goto_with(&mut app, "^devbox");
    app.on_key(kev(Key::Named(NamedKey::Enter)));
    assert_eq!(app.cur, 1);
    assert_eq!(app.focused_pane().as_deref(), Some("p1"));
    // Offline: says so instead of switching.
    app.cur = 0;
    app.machines[1].tx = None;
    app.machines[1].status = "offline".into();
    goto_with(&mut app, "^devbox");
    app.on_key(kev(Key::Named(NamedKey::Enter)));
    assert_eq!(app.cur, 0);
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("devbox is offline")
    );
}

#[test]
fn goto_hint_and_path_row_draw() {
    let (mut app, _rx) = fleet();
    goto_with(&mut app, "/src/new");
    let mut g = Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let text = grid_text(&g);
    assert!(text.contains("alt+enter split"), "{text}");
    assert!(
        text.contains("ctrl+enter / alt+enter: new workspace at /src/new"),
        "{text}"
    );
}

/// The bar cursor sits right after the typed filter, not on its last character.
#[test]
fn filter_cursor_follows_the_last_typed_character() {
    let (app, _rx) = test_app(1);
    let at = |g: &Grid, x: u16, y: u16| g.get(x, y).map(|c| c.text.as_str().to_string());
    for draw in [draw_palette, draw_goto] {
        let mut g = Grid::new(120, 40);
        let (x, y) = draw(&app, &mut g, "abc", 0);
        assert_eq!(at(&g, x - 1, y).as_deref(), Some("c"));
        assert_eq!(at(&g, x - 4, y).as_deref(), Some(" "));
        assert_eq!(at(&g, x - 5, y).as_deref(), Some(">"));
    }
}
