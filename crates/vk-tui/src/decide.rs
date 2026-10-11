//! One guard for every key that decides something (08 §8): the inbox (`y`/`n`/`1-9`/`x`/`X`),
//! the interaction card (`y`/`s`/`n`/`c`/`x`/`enter`/digits), the batch view (`a`/`y`/`n`), the
//! agent list (`enter`), the close/detach confirmation (`y`) and the cloud bring-back review
//! (`a`/`c`/`x`). The rules are the elevation
//! view's (spec 08 §8, [`crate::elevate`]):
//!
//! - only a fresh **press** decides, with no modifier other than shift: never a release (the
//!   kitty keyboard protocol reports them, so the release of the `enter` that opened a card
//!   would otherwise answer it), never a repeat;
//! - the guard follows **what** the keys act on (a target string such as `card:<id>` or
//!   `inbox:<machine>:<kind>:<id>`), never a list index. When the target changes for a reason
//!   the user did not cause (an item arrived, the selected one went away, a card opened from an
//!   async reply), decision keys wait [`ARM_DELAY`] and a frame that showed the new target,
//!   so a key meant for the old target never decides the new one;
//! - while it waits, a decision key typed (or auto-repeated) pushes the wait out again: it
//!   arms only after a pause in typing;
//! - with the kitty protocol, a decision key already held while the guard was not armed does
//!   not count until it was released;
//! - the user's own navigation (moving the selection, opening a surface with a key) is
//!   announced with [`Guard::follow`] and adds no delay.
//!
//! Esc and navigation are never delayed.

use crate::app::{App, Mode, Popup};
use crate::time::{Duration, Instant};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods};

/// Decision keys right after an unexpected change are swallowed.
pub const ARM_DELAY: Duration = Duration::from_millis(600);

/// What a key may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Not a key that acts: a release, a repeat, a modified decision key, or a decision key
    /// held since before the guard armed.
    Ignore,
    /// A decision key before the guard armed: swallowed (the surface shows that keys are not
    /// live yet).
    Wait,
    /// Act on it.
    Go,
}

#[derive(Debug)]
pub struct Guard {
    /// What the keys act on now (`None`: no decision surface, or nothing selected).
    target: RefCell<Option<String>>,
    /// Decision keys count from here.
    armed_at: Cell<Instant>,
    /// A frame showed the current target with the keys live.
    shown: Cell<bool>,
    /// Kitty protocol: decision keys pressed while not armed, or repeating, and not released.
    held: RefCell<HashSet<Key>>,
    /// The arm delay ([`ARM_DELAY`]; tests that are not about the guard use zero).
    pub delay: Duration,
}

impl Default for Guard {
    fn default() -> Self {
        Guard {
            target: RefCell::new(None),
            armed_at: Cell::new(Instant::now()),
            shown: Cell::new(true),
            held: RefCell::new(HashSet::new()),
            delay: ARM_DELAY,
        }
    }
}

impl Guard {
    /// Something the user did not cause changed what the keys act on: wait again.
    pub fn rearm(&self, now: Instant) {
        self.armed_at.set(now + self.delay);
        self.shown.set(self.delay.is_zero());
    }

    /// What the keys act on now. A change not announced with [`Guard::follow`] re-arms.
    pub fn observe(&self, target: Option<&str>, now: Instant) {
        let changed = self.target.borrow().as_deref() != target;
        if changed {
            *self.target.borrow_mut() = target.map(str::to_string);
            if target.is_some() {
                self.rearm(now);
            }
        }
    }

    /// The user moved the selection (or opened or closed a surface) with a key: no delay.
    pub fn follow(&self, target: Option<&str>) {
        *self.target.borrow_mut() = target.map(str::to_string);
    }

    /// The current target, as last observed or followed.
    pub fn target(&self) -> Option<String> {
        self.target.borrow().clone()
    }

    /// Decision keys are live now.
    pub fn armed(&self, now: Instant) -> bool {
        now >= self.armed_at.get() && self.shown.get()
    }

    /// A frame is drawn showing `target`: once armed, the user has seen the keys live.
    pub fn drawn(&self, target: Option<&str>, now: Instant) {
        self.observe(target, now);
        if target.is_some() && now >= self.armed_at.get() {
            self.shown.set(true);
        }
    }

    /// Gate a key on a decision surface whose keys act on `target`. `decides`: this key would
    /// answer, approve, deny, dismiss or confirm something. Other keys only lose their
    /// releases (and pass whatever their modifiers).
    pub fn gate(
        &self,
        target: Option<&str>,
        ev: &KeyEvent,
        now: Instant,
        kitty: bool,
        decides: bool,
    ) -> Gate {
        self.observe(target, now);
        if ev.kind == KeyKind::Release {
            self.held.borrow_mut().remove(&ev.key);
            return Gate::Ignore;
        }
        if !decides {
            return Gate::Go;
        }
        let armed = self.armed(now);
        let was_held = {
            let mut held = self.held.borrow_mut();
            let was = held.contains(&ev.key);
            // A release will come: remember a key pressed while not armed, or repeating
            // (held since before), until it is released.
            if kitty && (!armed || ev.kind == KeyKind::Repeat) {
                held.insert(ev.key);
            }
            was
        };
        if ev.kind != KeyKind::Press || !ev.mods.without(Mods::SHIFT).is_empty() {
            return Gate::Ignore;
        }
        if !armed {
            // Arm only after a pause: a key typed for the pane, or auto-repeating without the
            // kitty protocol, keeps the guard waiting.
            let later = now + self.delay;
            if later > self.armed_at.get() {
                self.armed_at.set(later);
            }
            return Gate::Wait;
        }
        if was_held {
            // Pressed again without a release we saw: this press does not count, the next does.
            self.held.borrow_mut().remove(&ev.key);
            return Gate::Ignore;
        }
        Gate::Go
    }

    /// Redraw when the wait ends, so the surface shows its keys as live (and only then do they
    /// count).
    pub fn deadlines(&self, d: &mut crate::deadline::Deadlines) {
        if self.target.borrow().is_some() && !self.shown.get() {
            d.redraw("decide.arm", self.armed_at.get());
        }
    }
}

// ---- targets -------------------------------------------------------------------------------------

pub fn confirm_target(message: &str) -> String {
    format!("confirm:{message}")
}

pub fn card_target(interaction: &str) -> String {
    format!("card:{interaction}")
}

pub fn inbox_target(k: &crate::inbox::ItemKey) -> String {
    format!("inbox:{}:{}:{}", k.machine, k.kind, k.id)
}

pub fn agents_target(e: &crate::agent_list::Entry) -> String {
    format!("agents:{}:{}", e.mi, e.run)
}

/// What the decision keys of the current mode act on, if it is a decision surface.
pub fn target(app: &App) -> Option<String> {
    match &app.mode {
        Mode::Popup(Popup::Confirm { message, .. }) => Some(confirm_target(message)),
        Mode::Popup(Popup::Card { interaction, .. }) => Some(card_target(interaction)),
        Mode::Popup(Popup::Inbox) => crate::inbox::selected_target(app),
        Mode::Popup(Popup::Batch) => crate::batch::target(app),
        Mode::Popup(Popup::Agents { filter, sel }) => crate::agent_list::target(app, filter, *sel),
        Mode::Popup(Popup::CloudSend) => crate::cloud::review_target(app),
        _ => None,
    }
}

/// Before a key is handled: a change since the last frame or key (an async reply, a refresh)
/// re-arms, so the key cannot act on something that slid into place under it.
pub fn before_key(app: &App) {
    app.decide.observe(target(app).as_deref(), Instant::now());
}

/// After a key was handled: whatever surface and selection it left on screen is the user's own
/// doing, so it adds no delay.
pub fn after_key(app: &App) {
    app.decide.follow(target(app).as_deref());
}

/// Before drawing a frame: a change since the last key (an async reply, a refresh) re-arms.
pub fn before_draw(app: &App) {
    app.decide.drawn(target(app).as_deref(), Instant::now());
}

/// The hint shown while decision keys are not live yet.
pub const WAIT_HINT: &str = "keys are live in a moment…";

/// True when the decision keys of the surface on screen are live.
pub fn live(app: &App) -> bool {
    app.decide.armed(Instant::now())
}

#[cfg(test)]
#[path = "decide_tests.rs"]
mod tests;
