//! The brand palette ported from the Python TUI (`THEMES`, `palette_for`,
//! `_resolve_theme`) pulled from `wayfinder_router/demo.html`.
//!
//! `accent` is the local arm (green), `cloud` the hosted arm (amber), and `warn`
//! matches the demo's `.warn`. `bg` is the full-screen fill: the app takes over the
//! terminal, so it owns one. Hex strings are baked into [`Color::Rgb`] at the call site.

use ratatui::style::Color;

/// A resolved brand palette: the colors the chat renders in.
///
/// Mirrors one entry of the Python `THEMES` dict, with each hex string converted to a
/// concrete [`Color`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub cloud: Color,
    pub text: Color,
    pub muted: Color,
    pub line: Color,
    pub warn: Color,
    pub bg: Color,
}

/// The dark palette (the default), from `demo.html`.
pub const DARK: Palette = Palette {
    accent: Color::Rgb(0x19, 0xc8, 0xa4),
    cloud: Color::Rgb(0xe0, 0xa2, 0x5c),
    text: Color::Rgb(0xec, 0xec, 0xec),
    muted: Color::Rgb(0x9a, 0x9a, 0xa6),
    line: Color::Rgb(0x39, 0x39, 0x3d),
    warn: Color::Rgb(0xd9, 0x77, 0x06),
    bg: Color::Rgb(0x16, 0x16, 0x18),
};

/// The light palette, from `demo.html`.
pub const LIGHT: Palette = Palette {
    accent: Color::Rgb(0x10, 0xa3, 0x7f),
    cloud: Color::Rgb(0xbd, 0x6a, 0x13),
    text: Color::Rgb(0x0d, 0x0d, 0x0d),
    muted: Color::Rgb(0x6b, 0x6b, 0x78),
    line: Color::Rgb(0xe2, 0xe2, 0xe6),
    warn: Color::Rgb(0xd9, 0x77, 0x06),
    bg: Color::Rgb(0xff, 0xff, 0xff),
};

/// Map a theme name (incl. `auto`) to a concrete palette key.
///
/// Mirrors the Python `_resolve_theme`: `auto` honors `WAYFINDER_THEME` then defaults to
/// `dark`; an unknown name also falls back to `dark`.
pub fn resolve_theme(theme: &str) -> &'static str {
    let resolved = if theme == "auto" {
        std::env::var("WAYFINDER_THEME")
            .map(|value| value.trim().to_lowercase())
            .unwrap_or_else(|_| "dark".to_owned())
    } else {
        theme.to_owned()
    };
    match resolved.as_str() {
        "light" => "light",
        _ => "dark",
    }
}

/// Resolve a palette. `auto` honors `WAYFINDER_THEME` then defaults to dark.
///
/// Mirrors the Python `palette_for`.
pub fn palette_for(theme: &str) -> Palette {
    match resolve_theme(theme) {
        "light" => LIGHT,
        _ => DARK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_theme_falls_back_to_dark() {
        assert_eq!(palette_for("nonsense"), DARK);
        assert_eq!(resolve_theme("nonsense"), "dark");
    }

    #[test]
    fn light_resolves_to_light() {
        assert_eq!(palette_for("light"), LIGHT);
        assert_eq!(resolve_theme("light"), "light");
    }
}
