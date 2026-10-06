//! Chrome colours (08 §11 `[theme]`). Default: catppuccin mocha tokens.

use vk_proto::render::{Color, Style, attr};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub red: Color,
    pub yellow: Color,
    pub green: Color,
    pub blue: Color,
    pub border: Color,
    pub selection: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            bg: Color::Default,
            panel: Color::Rgb(0x18, 0x18, 0x25),
            fg: Color::Rgb(0xcd, 0xd6, 0xf4),
            muted: Color::Rgb(0x6c, 0x70, 0x86),
            accent: Color::Rgb(0xcb, 0xa6, 0xf7),
            red: Color::Rgb(0xf3, 0x8b, 0xa8),
            yellow: Color::Rgb(0xf9, 0xe2, 0xaf),
            green: Color::Rgb(0xa6, 0xe3, 0xa1),
            blue: Color::Rgb(0x89, 0xb4, 0xfa),
            border: Color::Rgb(0x45, 0x47, 0x5a),
            selection: Color::Rgb(0x31, 0x32, 0x44),
        }
    }
}

impl Theme {
    /// The `terminal` theme: only ANSI colours, follows the host palette.
    pub fn terminal() -> Self {
        Theme {
            bg: Color::Default,
            panel: Color::Default,
            fg: Color::Default,
            muted: Color::Indexed(8),
            accent: Color::Indexed(5),
            red: Color::Indexed(1),
            yellow: Color::Indexed(3),
            green: Color::Indexed(2),
            blue: Color::Indexed(4),
            border: Color::Indexed(8),
            selection: Color::Indexed(0),
        }
    }
    /// Catppuccin latte: the light counterpart (`theme.light_name` default, theme auto).
    pub fn latte() -> Self {
        Theme {
            bg: Color::Default,
            panel: Color::Rgb(0xe6, 0xe9, 0xef),
            fg: Color::Rgb(0x4c, 0x4f, 0x69),
            muted: Color::Rgb(0x8c, 0x8f, 0xa1),
            accent: Color::Rgb(0x88, 0x39, 0xef),
            red: Color::Rgb(0xd2, 0x0f, 0x39),
            yellow: Color::Rgb(0xdf, 0x8e, 0x1d),
            green: Color::Rgb(0x40, 0xa0, 0x2b),
            blue: Color::Rgb(0x1e, 0x66, 0xf5),
            border: Color::Rgb(0xbc, 0xc0, 0xcc),
            selection: Color::Rgb(0xcc, 0xd0, 0xda),
        }
    }
    pub fn named(name: &str) -> Self {
        match name {
            "terminal" => Self::terminal(),
            "catppuccin-latte" | "latte" => Self::latte(),
            _ => Self::default(),
        }
    }
    pub fn s(&self, fg: Color) -> Style {
        Style {
            fg,
            bg: self.panel,
            ul: Color::Default,
            attrs: 0,
        }
    }
    pub fn text(&self) -> Style {
        self.s(self.fg)
    }
    pub fn dim(&self) -> Style {
        self.s(self.muted)
    }
    pub fn bold(&self, fg: Color) -> Style {
        Style {
            attrs: attr::BOLD,
            ..self.s(fg)
        }
    }
    pub fn sel(&self, fg: Color) -> Style {
        Style {
            fg,
            bg: self.selection,
            ul: Color::Default,
            attrs: 0,
        }
    }
    pub fn rev(&self) -> Style {
        Style {
            fg: self.panel_or_black(),
            bg: self.accent,
            ul: Color::Default,
            attrs: attr::BOLD,
        }
    }
    fn panel_or_black(&self) -> Color {
        match self.panel {
            Color::Default => Color::Indexed(0),
            c => c,
        }
    }
    pub fn border(&self, focused: bool) -> Style {
        Style {
            fg: if focused { self.accent } else { self.border },
            bg: Color::Default,
            ul: Color::Default,
            attrs: 0,
        }
    }
}
