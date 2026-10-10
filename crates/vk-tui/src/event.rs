//! Input events at the host boundary. Native clients retain crossterm's public types.
#[cfg(target_arch = "wasm32")]
pub use browser::*;
#[cfg(not(target_arch = "wasm32"))]
pub use crossterm::event::*;
#[cfg(target_arch = "wasm32")]
mod browser {
    pub use vk_proto::input::KeyEvent;
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Key(KeyEvent),
        Paste(String),
        Mouse(MouseEvent),
        Resize(u16, u16),
        FocusGained,
        FocusLost,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MouseButton {
        Left,
        Right,
        Middle,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MouseEventKind {
        Down(MouseButton),
        Up(MouseButton),
        Drag(MouseButton),
        Moved,
        ScrollDown,
        ScrollUp,
        ScrollLeft,
        ScrollRight,
    }
    bitflags::bitflags! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
        pub struct KeyModifiers: u8 { const SHIFT = 1; const CONTROL = 2; const ALT = 4; const SUPER = 8; const HYPER = 16; const META = 32; }
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MouseEvent {
        pub kind: MouseEventKind,
        pub column: u16,
        pub row: u16,
        pub modifiers: KeyModifiers,
    }
}
