//! Scripted control for recordings and UI tests. With `VIBEKE_TUI_DEMO_CONTROL=<fifo>`, each
//! line written to the FIFO runs one command, as if the user had selected the row in the
//! sidebar. The sidebar selection follows, unlike an outside `workspace.focus` call, which only
//! moves that client's focus:
//!
//! - `focus <machine> <workspace>`: focus a workspace (name, handle or id) on a machine (label).
//! - `pane <machine> <pane>`: focus a pane (handle or id).
//!
//! The last argument is the rest of the line, so names may contain spaces. The caller creates
//! the FIFO. It must belong to the user who runs the TUI, and nobody else may write to it: a
//! writer decides where the next keystrokes go.
use crate::app::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::app::Incoming;

/// Read commands from the control FIFO, if one is configured.
#[cfg(not(target_arch = "wasm32"))]
pub fn start(inc: tokio::sync::mpsc::UnboundedSender<Incoming>) {
    let Ok(path) = std::env::var("VIBEKE_TUI_DEMO_CONTROL") else {
        return;
    };
    let file = match open_fifo(std::path::Path::new(&path)) {
        Ok(f) => f,
        Err(why) => {
            let _ = inc.send(Incoming::Demo(format!(
                "# VIBEKE_TUI_DEMO_CONTROL ignored: {why}"
            )));
            return;
        }
    };
    spawn_reader(file, inc);
}

/// Send each line of `file` as [`Incoming::Demo`]. Waits for input with a timeout, so the thread
/// ends soon after the session does.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_reader(file: std::fs::File, inc: tokio::sync::mpsc::UnboundedSender<Incoming>) {
    std::thread::spawn(move || {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let mut file = file;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !inc.is_closed() {
            let mut fd = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            if unsafe { libc::poll(&mut fd, 1, 500) } <= 0 {
                continue;
            }
            let n = match file.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return,
            };
            buf.extend_from_slice(&chunk[..n]);
            while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..i]).into_owned();
                buf.drain(..=i);
                if inc.send(Incoming::Demo(line)).is_err() {
                    return;
                }
            }
        }
    });
}

/// Open `path` if it is a FIFO of this user that nobody else can write to. The opened file is
/// checked (not the path), so a swapped path cannot pass. Read and write, so the reader never
/// sees end-of-file when a writer leaves; opening never blocks.
#[cfg(not(target_arch = "wasm32"))]
fn open_fifo(path: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.file_type().is_fifo() {
        return Err("not a FIFO".into());
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    if m.uid() != unsafe { libc::getuid() } || m.mode() & 0o022 != 0 {
        return Err("the FIFO must belong to you and be writable only by you".into());
    }
    Ok(file)
}

pub fn run(app: &mut App, line: &str) {
    if let Some(note) = line.strip_prefix("# ") {
        return app.toast(note.to_string());
    }
    // Command, machine, and the rest of the line as the target.
    let words: Vec<&str> = line
        .trim()
        .splitn(3, char::is_whitespace)
        .map(str::trim)
        .collect();
    let machine = |app: &App, label: &str| app.machines.iter().position(|m| m.label == label);
    match words.as_slice() {
        ["focus", label, ws] => {
            let Some(mi) = machine(app, label) else {
                return app.toast(format!("demo: no machine {label}"));
            };
            let id = app.machines[mi]
                .model
                .workspaces
                .iter()
                .find(|w| {
                    w.id == *ws
                        || w.handle == *ws
                        || w.name.as_deref() == Some(*ws)
                        || w.auto_name == *ws
                })
                .map(|w| w.id.clone());
            match id {
                Some(id) => {
                    crate::nav::focus_workspace(app, mi, &id);
                }
                None => app.toast(format!("demo: no workspace {ws} on {label}")),
            }
        }
        ["pane", label, pane] => {
            let Some(mi) = machine(app, label) else {
                return app.toast(format!("demo: no machine {label}"));
            };
            let id = app.machines[mi]
                .model
                .panes
                .iter()
                .find(|p| p.id == *pane || p.handle == *pane)
                .map(|p| p.id.clone());
            match id {
                Some(id) => app.focus_pane(mi, &id),
                None => app.toast(format!("demo: no pane {pane} on {label}")),
            }
        }
        _ => app.toast(format!("demo: unknown command {line}")),
    }
    app.dirty = true;
}

#[cfg(test)]
#[path = "demo_tests.rs"]
mod tests;
