//! Chrome colours (08 §11 `[theme]`). Default: catppuccin mocha tokens.
//!
//! Built-in palettes: `catppuccin` (dark), `catppuccin-latte` (light), `tokyo-night`,
//! `dracula`, `nord`, `gruvbox` (dark) and `terminal` (only the host's own colours). A theme
//! file `themes/<name>.toml` next to the config file adds more: flat colour tokens, plus an
//! optional `base = "<built-in name>"`. `[theme.custom]` overrides single tokens on top of
//! any theme. `NO_COLOR` turns every colour off and keeps bold, dim and reverse video.

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
    /// Mark a selected row with reverse video, not with the `selection` colour. The `terminal`
    /// theme and `NO_COLOR` use it: no fixed colour is right on both light and dark hosts.
    pub sel_inverse: bool,
    /// Dim text and unfocused borders with the DIM attribute, not with a fixed grey.
    pub dim_attr: bool,
}

/// The theme names the chrome knows without a theme file.
pub const BUILTIN: &[&str] = &[
    "catppuccin",
    "catppuccin-latte",
    "tokyo-night",
    "dracula",
    "nord",
    "gruvbox",
    "terminal",
];

const fn rgb(v: u32) -> Color {
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
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
            sel_inverse: false,
            dim_attr: false,
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
            // Bright black is invisible on some palettes (solarized): dim the host's own
            // foreground instead.
            muted: Color::Default,
            accent: Color::Indexed(5),
            red: Color::Indexed(1),
            yellow: Color::Indexed(3),
            green: Color::Indexed(2),
            blue: Color::Indexed(4),
            border: Color::Default,
            // Black on a black background is unreadable on light hosts: reverse video.
            selection: Color::Default,
            sel_inverse: true,
            dim_attr: true,
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
            sel_inverse: false,
            dim_attr: false,
        }
    }
    pub fn tokyo_night() -> Self {
        Theme {
            bg: Color::Default,
            panel: rgb(0x16161e),
            fg: rgb(0xc0caf5),
            muted: rgb(0x565f89),
            accent: rgb(0xbb9af7),
            red: rgb(0xf7768e),
            yellow: rgb(0xe0af68),
            green: rgb(0x9ece6a),
            blue: rgb(0x7aa2f7),
            border: rgb(0x3b4261),
            selection: rgb(0x283457),
            sel_inverse: false,
            dim_attr: false,
        }
    }
    pub fn dracula() -> Self {
        Theme {
            bg: Color::Default,
            panel: rgb(0x21222c),
            fg: rgb(0xf8f8f2),
            muted: rgb(0x6272a4),
            accent: rgb(0xbd93f9),
            red: rgb(0xff5555),
            yellow: rgb(0xf1fa8c),
            green: rgb(0x50fa7b),
            blue: rgb(0x8be9fd),
            border: rgb(0x383a4a),
            selection: rgb(0x44475a),
            sel_inverse: false,
            dim_attr: false,
        }
    }
    pub fn nord() -> Self {
        Theme {
            bg: Color::Default,
            panel: rgb(0x2e3440),
            fg: rgb(0xd8dee9),
            muted: rgb(0x7b88a1),
            accent: rgb(0x88c0d0),
            red: rgb(0xbf616a),
            yellow: rgb(0xebcb8b),
            green: rgb(0xa3be8c),
            blue: rgb(0x81a1c1),
            border: rgb(0x434c5e),
            selection: rgb(0x3b4252),
            sel_inverse: false,
            dim_attr: false,
        }
    }
    /// Gruvbox, dark variant.
    pub fn gruvbox() -> Self {
        Theme {
            bg: Color::Default,
            panel: rgb(0x282828),
            fg: rgb(0xebdbb2),
            muted: rgb(0x928374),
            accent: rgb(0xd3869b),
            red: rgb(0xfb4934),
            yellow: rgb(0xfabd2f),
            green: rgb(0xb8bb26),
            blue: rgb(0x83a598),
            border: rgb(0x504945),
            selection: rgb(0x3c3836),
            sel_inverse: false,
            dim_attr: false,
        }
    }
    /// No colour at all (`NO_COLOR`): bold, dim and reverse video carry the meaning.
    pub fn mono() -> Self {
        Theme {
            bg: Color::Default,
            panel: Color::Default,
            fg: Color::Default,
            muted: Color::Default,
            accent: Color::Default,
            red: Color::Default,
            yellow: Color::Default,
            green: Color::Default,
            blue: Color::Default,
            border: Color::Default,
            selection: Color::Default,
            sel_inverse: true,
            dim_attr: true,
        }
    }
    /// A built-in palette by name (a few aliases are accepted).
    pub fn builtin(name: &str) -> Option<Self> {
        Some(match name {
            "catppuccin" | "catppuccin-mocha" | "mocha" => Self::default(),
            "catppuccin-latte" | "latte" => Self::latte(),
            "tokyo-night" | "tokyonight" => Self::tokyo_night(),
            "dracula" => Self::dracula(),
            "nord" => Self::nord(),
            "gruvbox" | "gruvbox-dark" => Self::gruvbox(),
            "terminal" => Self::terminal(),
            _ => return None,
        })
    }

    /// The chrome theme for a name from a preview or a test: built-in palettes only. An unknown
    /// name gives the default palette. `NO_COLOR` wins over every name.
    pub fn named(name: &str) -> Self {
        if no_color() {
            return Self::mono();
        }
        Self::builtin(name).unwrap_or_default()
    }

    /// The chrome theme for the configuration: the named theme (built-in, or
    /// `themes/<name>.toml` next to the config file), then `[theme.custom]` on top. The second
    /// value lists problems to show the user once: an unknown theme, a bad token.
    pub fn build(name: &str, cfg: &vk_config::Config) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        if no_color() {
            return (Self::mono(), warnings);
        }
        let mut theme = match Self::builtin(name) {
            Some(t) => t,
            None => match load_theme_file(name) {
                Ok(Some((t, w))) => {
                    warnings.extend(w);
                    t
                }
                Ok(None) => {
                    warnings.push(format!(
                        "unknown theme `{name}`: using catppuccin (built-in: {})",
                        BUILTIN.join(", ")
                    ));
                    Self::default()
                }
                Err(e) => {
                    warnings.push(format!("theme `{name}`: {e}; using catppuccin"));
                    Self::default()
                }
            },
        };
        warnings.extend(
            theme.apply_tokens(
                cfg.theme
                    .custom
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            ),
        );
        (theme, warnings)
    }

    /// Set colour tokens (`accent = "#f5c2e7"`). Returns one message per bad token.
    pub fn apply_tokens<'a>(
        &mut self,
        tokens: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<String> {
        let mut bad = Vec::new();
        for (k, v) in tokens {
            // `base` names a theme (read by the theme file loader), not a colour.
            if k == "base" {
                continue;
            }
            let Some(c) = parse_color(v) else {
                bad.push(format!(
                    "theme token `{k}`: `{v}` is not a colour (use #rrggbb)"
                ));
                continue;
            };
            match k {
                "bg" => self.bg = c,
                "panel" | "panel_bg" => self.panel = c,
                "fg" => self.fg = c,
                "muted" => self.muted = c,
                "accent" => self.accent = c,
                "red" => self.red = c,
                "yellow" => self.yellow = c,
                "green" => self.green = c,
                "blue" => self.blue = c,
                "border" => self.border = c,
                "selection" => {
                    self.selection = c;
                    // A chosen selection colour replaces reverse video.
                    self.sel_inverse = false;
                }
                other => bad.push(format!("theme token `{other}` is unknown")),
            }
        }
        bad
    }

    /// `style` as the background of a selected row.
    pub fn with_sel(&self, style: Style) -> Style {
        if self.sel_inverse {
            Style {
                attrs: style.attrs | attr::INVERSE,
                ..style
            }
        } else {
            Style {
                bg: self.selection,
                ..style
            }
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
        Style {
            attrs: if self.dim_attr { attr::DIM } else { 0 },
            ..self.s(self.muted)
        }
    }
    pub fn bold(&self, fg: Color) -> Style {
        Style {
            attrs: attr::BOLD,
            ..self.s(fg)
        }
    }
    pub fn sel(&self, fg: Color) -> Style {
        if self.sel_inverse {
            return Style {
                fg,
                bg: self.panel,
                ul: Color::Default,
                attrs: attr::INVERSE,
            };
        }
        Style {
            fg,
            bg: self.selection,
            ul: Color::Default,
            attrs: 0,
        }
    }
    pub fn rev(&self) -> Style {
        if self.accent == Color::Default {
            // No accent colour (`NO_COLOR`): reverse video marks the cursor.
            return Style {
                fg: Color::Default,
                bg: Color::Default,
                ul: Color::Default,
                attrs: attr::BOLD | attr::INVERSE,
            };
        }
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
            // A theme without a border colour dims the host's foreground instead.
            attrs: if self.dim_attr && !focused {
                attr::DIM
            } else {
                0
            },
        }
    }
}

/// `NO_COLOR` is set and not empty (https://no-color.org).
pub fn no_color() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

/// `#rrggbb`, `#rgb`, `reset`/`default` (the host colour), or a palette index `0`..`255`.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("reset") || s.eq_ignore_ascii_case("default") {
        return Some(Color::Default);
    }
    if let Some(h) = s.strip_prefix('#') {
        if !h.is_ascii() {
            return None;
        }
        return match h.len() {
            6 => u32::from_str_radix(h, 16).ok().map(rgb),
            3 => {
                let v = u32::from_str_radix(h, 16).ok()?;
                let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
                Some(Color::Rgb((r * 17) as u8, (g * 17) as u8, (b * 17) as u8))
            }
            _ => None,
        };
    }
    s.parse::<u8>().ok().map(Color::Indexed)
}

/// `themes/<name>.toml` next to the config file. `Ok(None)`: no such file.
#[cfg(not(target_arch = "wasm32"))]
fn load_theme_file(name: &str) -> Result<Option<(Theme, Vec<String>)>, String> {
    // The name becomes a file name: no separators, no dots at the start.
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Ok(None);
    }
    let Some(dir) = vk_config::config_path().parent().map(|p| p.join("themes")) else {
        return Ok(None);
    };
    let path = dir.join(format!("{name}.toml"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse_theme_file(&text).map(Some)
}

#[cfg(target_arch = "wasm32")]
fn load_theme_file(_: &str) -> Result<Option<(Theme, Vec<String>)>, String> {
    Ok(None)
}

/// The text of a theme file: flat colour tokens and an optional `base` built-in name.
pub fn parse_theme_file(text: &str) -> Result<(Theme, Vec<String>), String> {
    let doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let base = doc
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("catppuccin");
    let mut theme = Theme::builtin(base).ok_or_else(|| format!("unknown base theme `{base}`"))?;
    let tokens: Vec<(String, String)> = doc
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
        .collect();
    let warnings = theme.apply_tokens(tokens.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    Ok((theme, warnings))
}
