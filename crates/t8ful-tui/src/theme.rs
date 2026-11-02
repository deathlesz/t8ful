//! Color palettes for the full-screen interface.
//!
//! The default `pi` palette is a fixed 24-bit scheme with a transparent base so
//! the terminal's own background shows through. `ansi` is the escape hatch for
//! terminals that cannot display true color, and for light backgrounds.

use ratatui::style::Color;

/// A selectable palette name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Pi,
    Ansi,
}

impl ThemeName {
    /// Parse a configured theme name, ignoring case and surrounding space.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pi" => Some(Self::Pi),
            "ansi" => Some(Self::Ansi),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Ansi => "ansi",
        }
    }
}

/// Every color the interface draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub name: ThemeName,
    pub bg: Option<Color>,
    pub fg: Color,
    pub dim: Color,
    pub rule: Color,
    pub accent: Color,
    pub title: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub tool: Color,
    pub user_bg: Option<Color>,
    pub tool_bg: Option<Color>,
    pub overlay: Color,
}

impl Theme {
    /// Fixed 24-bit palette, pinned so tests can compare exact colors.
    pub const fn pi() -> Self {
        Self {
            name: ThemeName::Pi,
            bg: None,
            fg: rgb(0xc9d4d6),
            dim: rgb(0x6b7c85),
            rule: rgb(0x34424b),
            accent: rgb(0x7aa2f7),
            title: rgb(0xbb9af7),
            success: rgb(0x9ece6a),
            warning: rgb(0xe0af68),
            error: rgb(0xf7768e),
            tool: rgb(0xe06c75),
            user_bg: Some(rgb(0x263230)),
            tool_bg: Some(rgb(0x212a2f)),
            overlay: rgb(0x1a2026),
        }
    }

    /// Terminal-native palette. Bands fall back to a colored gutter because
    /// the terminal's background cannot be tinted reliably.
    pub const fn ansi() -> Self {
        Self {
            name: ThemeName::Ansi,
            bg: Some(Color::Reset),
            fg: Color::Gray,
            dim: Color::DarkGray,
            rule: Color::DarkGray,
            accent: Color::Cyan,
            title: Color::Magenta,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            tool: Color::Blue,
            user_bg: None,
            tool_bg: None,
            overlay: Color::Black,
        }
    }

    pub const fn for_name(name: ThemeName) -> Self {
        match name {
            ThemeName::Pi => Self::pi(),
            ThemeName::Ansi => Self::ansi(),
        }
    }
}

/// Build a fixed RGB color from `0xRRGGBB`.
const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::{Theme, ThemeName};

    #[test]
    fn the_pi_palette_matches_the_pinned_colors() {
        let theme = Theme::pi();

        assert_eq!(theme.name, ThemeName::Pi);
        assert!(theme.bg.is_none());
        assert_eq!(theme.fg, Color::Rgb(0xc9, 0xd4, 0xd6));
        assert_eq!(theme.dim, Color::Rgb(0x6b, 0x7c, 0x85));
        assert_eq!(theme.rule, Color::Rgb(0x34, 0x42, 0x4b));
        assert_eq!(theme.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(theme.title, Color::Rgb(0xbb, 0x9a, 0xf7));
        assert_eq!(theme.success, Color::Rgb(0x9e, 0xce, 0x6a));
        assert_eq!(theme.warning, Color::Rgb(0xe0, 0xaf, 0x68));
        assert_eq!(theme.error, Color::Rgb(0xf7, 0x76, 0x8e));
        assert_eq!(theme.tool, Color::Rgb(0xe0, 0x6c, 0x75));
        assert_eq!(theme.user_bg, Some(Color::Rgb(0x26, 0x32, 0x30)));
        assert_eq!(theme.tool_bg, Some(Color::Rgb(0x21, 0x2a, 0x2f)));
        assert_eq!(theme.overlay, Color::Rgb(0x1a, 0x20, 0x26));
    }

    #[test]
    fn the_pi_palette_keeps_the_terminal_background_transparent() {
        let theme = Theme::pi();

        assert!(theme.bg.is_none());
        assert!(theme.user_bg.is_some());
        assert!(theme.tool_bg.is_some());
    }

    #[test]
    fn the_ansi_palette_uses_terminal_colors_and_no_bands() {
        let theme = Theme::ansi();

        assert_eq!(theme.name, ThemeName::Ansi);
        assert_eq!(theme.bg, Some(Color::Reset));
        assert_eq!(theme.fg, Color::Gray);
        assert_eq!(theme.accent, Color::Cyan);
        assert_eq!(theme.user_bg, None);
        assert_eq!(theme.tool_bg, None);
    }

    #[test]
    fn theme_names_parse_case_insensitively_and_unknown_names_are_rejected() {
        assert_eq!(ThemeName::parse("pi"), Some(ThemeName::Pi));
        assert_eq!(ThemeName::parse(" ANSI "), Some(ThemeName::Ansi));
        assert_eq!(ThemeName::parse("solarized"), None);
        assert_eq!(Theme::for_name(ThemeName::Ansi).name, ThemeName::Ansi);
    }
}
