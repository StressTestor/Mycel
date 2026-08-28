//! Theme model: named color roles with seven built-in themes.
//!
//! A `Theme` is a set of named color roles the components draw from. Amanita is
//! the default and is pixel-exact from the mockup; the other six themes carry
//! their base roles verbatim from the design's startup model and derive their
//! TUI-only roles from fixed rules (see `derive_tui_roles`).

use crate::terminal::style::Rgb;

/// A full set of named color roles. Base roles come from the design's startup
/// model; TUI-only roles are pixel-exact for amanita and derived for the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: &'static str,
    pub glow: bool,
    pub tag: &'static str,

    // Base roles (defined for all seven themes by the startup model).
    pub bg: Rgb,
    pub fg: Rgb,
    pub bright: Rgb,
    pub dim: Rgb,
    pub dimmer: Rgb,
    pub cap: Rgb,
    pub rim: Rgb,
    pub speck: Rgb,
    pub stem: Rgb,
    pub thread: Rgb,
    pub thread2: Rgb,
    pub accent: Rgb,
    pub ok: Rgb,
    pub prompt: Rgb,
    pub frame: Rgb,
    pub ground: Rgb,

    // TUI-only roles (named by the mockup, absent from the startup model).
    pub panel_bg: Rgb,
    pub border: Rgb,
    pub value: Rgb,
    pub secondary: Rgb,
    pub muted: Rgb,
    pub faint: Rgb,
    pub accent_dim: Rgb,
    pub diff_bg: Rgb,
    pub diff_add: Rgb,
    pub diff_del: Rgb,
    pub deny_border: Rgb,
    pub deny_bg: Rgb,
    pub selection: Rgb,
}

impl Theme {
    /// The default theme, pixel-exact from `mycel-tui-mockup.html`. Its TUI-only
    /// roles are explicit overrides so amanita is never an approximation.
    pub fn amanita() -> Self {
        Self {
            name: "amanita",
            glow: false,
            tag: "v0.4.2 — substrate ecology · fail-closed by design",
            bg: Rgb::from_hex("#050705"),
            fg: Rgb::from_hex("#b7beb3"),
            bright: Rgb::from_hex("#dde3d8"),
            dim: Rgb::from_hex("#626d61"),
            dimmer: Rgb::from_hex("#3f483f"),
            cap: Rgb::from_hex("#e05a1e"),
            rim: Rgb::from_hex("#8a3c18"),
            speck: Rgb::from_hex("#f2ece2"),
            stem: Rgb::from_hex("#9aa79a"),
            thread: Rgb::from_hex("#4f5c4c"),
            thread2: Rgb::from_hex("#39423a"),
            accent: Rgb::from_hex("#e05a1e"),
            ok: Rgb::from_hex("#55a868"),
            prompt: Rgb::from_hex("#8ba18c"),
            frame: Rgb::from_hex("#1e241e"),
            ground: Rgb::from_hex("#232823"),
            panel_bg: Rgb::from_hex("#0a0c0a"),
            border: Rgb::from_hex("#2c332c"),
            value: Rgb::from_hex("#c3cabe"),
            secondary: Rgb::from_hex("#8ba18c"),
            muted: Rgb::from_hex("#626d61"),
            faint: Rgb::from_hex("#4a544a"),
            accent_dim: Rgb::from_hex("#8a3c18"),
            diff_bg: Rgb::from_hex("#0d0f0c"),
            diff_add: Rgb::from_hex("#a8b0a3"),
            diff_del: Rgb::from_hex("#7d8579"),
            deny_border: Rgb::from_hex("#6b3111"),
            deny_bg: Rgb::from_hex("#140a04"),
            selection: Rgb::from_hex("#2a332a"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amanita_matches_mockup_exactly() {
        let t = Theme::amanita();
        assert_eq!(t.panel_bg, Rgb::from_hex("#0a0c0a"));
        assert_eq!(t.accent, Rgb::from_hex("#e05a1e"));
        assert_eq!(t.ok, Rgb::from_hex("#55a868"));
        assert_eq!(t.border, Rgb::from_hex("#2c332c"));
        assert_eq!(t.deny_border, Rgb::from_hex("#6b3111"));
        assert_eq!(t.diff_add, Rgb::from_hex("#a8b0a3"));
        assert!(!t.glow);
    }
}
