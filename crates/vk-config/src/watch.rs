//! Debounced config file watcher. A config that fails to parse is reported as
//! [`ReloadEvent::Rejected`] and never replaces the current one.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::load::{ConfigError, Warning};
use crate::types::Config;

#[derive(Debug)]
pub enum ReloadEvent {
    /// A new valid config. `changed` is `Config::diff(old, new)`; it is empty when the file
    /// was rewritten without semantic changes (no event is emitted in that case).
    Reloaded {
        config: Box<Config>,
        warnings: Vec<Warning>,
        changed: Vec<String>,
    },
    /// The file failed to parse or validate; the previously applied config stays in force.
    Rejected(ConfigError),
}

/// Keeps the watcher alive; dropping it stops watching.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Watch `path` (its parent directory, so atomic-rename saves are seen) and emit reload
/// events after `debounce` of quiet. `current` is the config presently applied; events are
/// only produced when the new config differs from the last accepted one.
pub fn watch(
    path: impl AsRef<Path>,
    current: Config,
    debounce: Duration,
) -> notify::Result<(ConfigWatcher, Receiver<ReloadEvent>)> {
    let path: PathBuf = path.as_ref().to_path_buf();
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = path.file_name().map(|n| n.to_os_string());

    let (fs_tx, fs_rx) = mpsc::channel::<()>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ev_tx, ev_rx) = mpsc::channel::<ReloadEvent>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev
                .paths
                .iter()
                .any(|p| p.file_name().map(|n| n.to_os_string()) == file_name)
        {
            let _ = fs_tx.send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    let thread_path = path;
    let thread = std::thread::spawn(move || {
        let mut current = current;
        loop {
            // Wait for the first event or a stop request.
            loop {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                match fs_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(()) => break,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            // Debounce: keep draining until `debounce` of quiet.
            loop {
                match fs_rx.recv_timeout(debounce) {
                    Ok(()) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            match Config::load(&thread_path) {
                Ok((cfg, warnings)) => {
                    let changed = Config::diff(&current, &cfg);
                    if !changed.is_empty() {
                        current = cfg.clone();
                        if ev_tx
                            .send(ReloadEvent::Reloaded {
                                config: Box::new(cfg),
                                warnings,
                                changed,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    if ev_tx.send(ReloadEvent::Rejected(e)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    Ok((
        ConfigWatcher {
            _watcher: watcher,
            stop: stop_tx,
            thread: Some(thread),
        },
        ev_rx,
    ))
}
