//! Background jobs for work that must not block the event loop: clipboard tools (`pbcopy`,
//! `osascript`, `wl-copy`, `xclip`), image scaling and encoding.
//!
//! [`spawn`] runs `work` on a worker thread and hands its result to `done` on the event loop
//! (as [`crate::app::Incoming::Job`]), so `done` may use the whole [`App`]. Without an
//! installed channel (unit tests, the browser client) `work` and `done` run at once, in
//! order, which keeps tests deterministic.

use crate::app::App;
#[cfg(not(target_arch = "wasm32"))]
use crate::app::Incoming;

/// A finished job, waiting for the event loop.
pub struct Job(Box<dyn FnOnce(&mut App) + Send>);

impl Job {
    pub fn run(self, app: &mut App) {
        (self.0)(app)
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Job")
    }
}

#[cfg(not(target_arch = "wasm32"))]
static SINK: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<Incoming>> =
    std::sync::OnceLock::new();

/// Where finished jobs go: called once by the run loop.
#[cfg(not(target_arch = "wasm32"))]
pub fn install(tx: tokio::sync::mpsc::UnboundedSender<Incoming>) {
    let _ = SINK.set(tx);
}

/// Run `work` off the event loop, then `done(app, result)` on it. The loop repaints after.
pub fn spawn<R: Send + 'static>(
    app: &mut App,
    work: impl FnOnce() -> R + Send + 'static,
    done: impl FnOnce(&mut App, R) + Send + 'static,
) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(tx) = SINK.get() {
        let tx = tx.clone();
        // The closure moves into the thread; if the thread cannot start, the work is lost
        // with it, so a failed spawn reports through the same channel.
        let spawned = std::thread::Builder::new()
            .name("vk-job".into())
            .spawn(move || {
                let result = work();
                let job = Job(Box::new(move |app: &mut App| {
                    done(app, result);
                    app.dirty = true;
                }));
                let _ = tx.send(Incoming::Job(job));
            });
        if spawned.is_err() {
            app.toast("could not start a background task");
        }
        return;
    }
    let result = work();
    done(app, result);
}
