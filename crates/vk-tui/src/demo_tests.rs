use super::*;
use crate::drafts::tests::fleet_n;

#[test]
fn focus_selects_a_workspace_on_another_machine() {
    let (mut app, _rx) = fleet_n(2);
    run(&mut app, "focus m1 api");
    assert_eq!(app.cur, 1);
    assert_eq!(app.focused_ws().map(|w| w.id), Some("W1".into()));
    assert!(app.toasts.is_empty());
}

#[test]
fn pane_focuses_a_pane_by_handle_or_id() {
    let (mut app, _rx) = fleet_n(2);
    run(&mut app, "pane m0 w1:p2");
    assert_eq!((app.cur, app.focused_pane().as_deref()), (0, Some("p2")));
    run(&mut app, "pane m1 p1");
    assert_eq!((app.cur, app.focused_pane().as_deref()), (1, Some("p1")));
}

#[test]
fn unknown_targets_and_commands_only_toast() {
    let (mut app, _rx) = fleet_n(2);
    let before = (app.cur, app.focused_pane());
    for line in [
        "focus nowhere api",
        "focus m0 missing",
        "pane m0 w9:p9",
        "jump m0 api",
        "",
    ] {
        run(&mut app, line);
    }
    assert_eq!((app.cur, app.focused_pane()), before);
    assert!(!app.toasts.is_empty());
}

#[test]
fn the_target_is_the_rest_of_the_line() {
    let (mut app, _rx) = fleet_n(2);
    app.machines[1].model.workspaces[0].name = Some("My Project".into());
    run(&mut app, "focus m1 My Project");
    assert_eq!(app.cur, 1);
    assert!(app.toasts.is_empty());
}

#[test]
fn only_a_private_fifo_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("plain");
    std::fs::write(&file, "focus m0 api\n").unwrap();
    assert_eq!(open_fifo(&file).unwrap_err(), "not a FIFO");
    let fifo = dir.path().join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o622) }, 0);
    std::fs::set_permissions(&fifo, std::os::unix::fs::PermissionsExt::from_mode(0o622)).unwrap();
    assert!(
        open_fifo(&fifo)
            .unwrap_err()
            .contains("writable only by you")
    );
    std::fs::set_permissions(&fifo, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    assert!(open_fifo(&fifo).is_ok());
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&fifo, &link).unwrap();
    assert!(open_fifo(&link).is_err(), "symlinks are not followed");
}

#[test]
fn the_reader_sends_lines_and_stops_with_the_session() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_reader(open_fifo(&fifo).unwrap(), tx);
    let mut w = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    w.write_all(b"focus m0 api\npane m0 w1:").unwrap();
    w.write_all(b"p2\n").unwrap();
    let next = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<Incoming>| {
        for _ in 0..200 {
            if let Ok(Incoming::Demo(l)) = rx.try_recv() {
                return l;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("no line from the reader");
    };
    assert_eq!(next(&mut rx), "focus m0 api");
    assert_eq!(next(&mut rx), "pane m0 w1:p2");
    drop(rx);
}
