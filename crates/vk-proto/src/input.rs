//! Logical input events (03 §7). Clients decode the host terminal's input into these;
//! the server's single canonical encoder turns them into bytes for each pane's negotiated
//! keyboard mode.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamedKey {
    Enter,
    Tab,
    Backspace,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    /// Modifier keys pressed on their own (only reported to apps that asked for all keys).
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Mods(pub u8);

impl Mods {
    pub const SHIFT: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const CTRL: Mods = Mods(4);
    pub const SUPER: Mods = Mods(8);
    pub const HYPER: Mods = Mods(16);
    pub const META: Mods = Mods(32);

    pub const fn empty() -> Self {
        Mods(0)
    }
    pub const fn contains(self, o: Mods) -> bool {
        self.0 & o.0 == o.0
    }
    pub const fn union(self, o: Mods) -> Mods {
        Mods(self.0 | o.0)
    }
    pub const fn without(self, o: Mods) -> Mods {
        Mods(self.0 & !o.0)
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn shift(self) -> bool {
        self.contains(Self::SHIFT)
    }
    pub fn ctrl(self) -> bool {
        self.contains(Self::CTRL)
    }
    pub fn alt(self) -> bool {
        self.contains(Self::ALT)
    }
    pub fn sup(self) -> bool {
        self.contains(Self::SUPER)
    }
}

impl std::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, o: Mods) -> Mods {
        self.union(o)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum KeyKind {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Mods,
    pub kind: KeyKind,
    /// Kitty "base layout key" used for keybinding matching on non-US layouts.
    pub base_layout_key: Option<char>,
    /// Shifted variant when reported.
    pub shifted: Option<char>,
    /// Associated text (what the user actually typed), when the host reports it.
    pub text: Option<String>,
}

impl KeyEvent {
    pub fn new(key: Key, mods: Mods) -> Self {
        KeyEvent {
            key,
            mods,
            kind: KeyKind::Press,
            base_layout_key: None,
            shifted: None,
            text: None,
        }
    }
    pub fn ch(c: char) -> Self {
        Self::new(Key::Char(c), Mods::empty())
    }
    pub fn named(n: NamedKey) -> Self {
        Self::new(Key::Named(n), Mods::empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    Press,
    Release,
    Drag,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: MouseButton,
    /// Pane-local cell coordinates (0-based).
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    FocusIn,
    FocusOut,
    /// Escape hatch for explicit raw-byte APIs; never produced by key handling.
    Raw(Vec<u8>),
}
