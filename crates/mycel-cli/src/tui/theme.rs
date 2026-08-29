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

    /// Fill the 13 TUI-only roles from the base roles by fixed rules. The rules
    /// are anchored so that applying them to amanita's base roles reproduces its
    /// explicit overrides within tolerance. Amanita ships overrides and does not
    /// call this; the other six themes do.
    pub fn derive_tui_roles(&mut self) {
        self.panel_bg = lighten(self.bg, 0.02);
        self.border = blend(self.fg, self.bg, 0.80);
        self.value = blend(self.fg, self.bright, 0.5);
        self.secondary = self.prompt;
        self.muted = self.dim;
        self.faint = blend(self.dim, self.bg, 0.5);
        self.accent_dim = darken(self.accent, 0.45);
        self.diff_bg = blend(self.bg, self.accent, 0.06);
        self.diff_add = blend(self.fg, self.bright, 0.4);
        self.diff_del = self.dim;
        self.deny_border = darken(self.accent, 0.55);
        self.deny_bg = darken(self.accent, 0.88);
        self.selection = lighten(self.bg, 0.06);
    }

    /// Every built-in theme name, amanita first.
    pub const ALL: [&'static str; 7] = [
        "amanita",
        "hacker",
        "foxfire",
        "cordyceps",
        "phosphor",
        "amber",
        "synthwave",
    ];

    /// Resolve a theme by name, or `None` if the name is not built in.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "amanita" => Some(Self::amanita()),
            "hacker" => Some(Self::hacker()),
            "foxfire" => Some(Self::foxfire()),
            "cordyceps" => Some(Self::cordyceps()),
            "phosphor" => Some(Self::phosphor()),
            "amber" => Some(Self::amber()),
            "synthwave" => Some(Self::synthwave()),
            _ => None,
        }
    }

    pub fn hacker() -> Self {
        themed(
            "hacker",
            true,
            "v0.4.2 — substrate ecology · fail-closed // trust nothing, verify everything",
            [
                Rgb::from_hex("#060109"), // bg
                Rgb::from_hex("#9fb4c8"), // fg
                Rgb::from_hex("#eaf7ff"), // bright
                Rgb::from_hex("#5a6b80"), // dim
                Rgb::from_hex("#333f52"), // dimmer
                Rgb::from_hex("#ff2d78"), // cap
                Rgb::from_hex("#7d1043"), // rim
                Rgb::from_hex("#22e6ff"), // speck
                Rgb::from_hex("#6f7f96"), // stem
                Rgb::from_hex("#12855f"), // thread
                Rgb::from_hex("#0c5340"), // thread2
                Rgb::from_hex("#ff2d78"), // accent
                Rgb::from_hex("#39ff88"), // ok
                Rgb::from_hex("#22e6ff"), // prompt
                Rgb::from_hex("#241033"), // frame
                Rgb::from_hex("#2a1440"), // ground
            ],
        )
    }

    pub fn foxfire() -> Self {
        themed(
            "foxfire",
            true,
            "v0.4.2 — substrate ecology · fail-closed by design",
            [
                Rgb::from_hex("#02070a"), // bg
                Rgb::from_hex("#9fc2b8"), // fg
                Rgb::from_hex("#e2fff6"), // bright
                Rgb::from_hex("#527066"), // dim
                Rgb::from_hex("#31453f"), // dimmer
                Rgb::from_hex("#2ee6a8"), // cap
                Rgb::from_hex("#14735a"), // rim
                Rgb::from_hex("#c8fff0"), // speck
                Rgb::from_hex("#5e7a72"), // stem
                Rgb::from_hex("#1f6e58"), // thread
                Rgb::from_hex("#123f33"), // thread2
                Rgb::from_hex("#2ee6a8"), // accent
                Rgb::from_hex("#54ffb0"), // ok
                Rgb::from_hex("#66f0c8"), // prompt
                Rgb::from_hex("#0f2a24"), // frame
                Rgb::from_hex("#123028"), // ground
            ],
        )
    }

    pub fn cordyceps() -> Self {
        themed(
            "cordyceps",
            false,
            "v0.4.2 — substrate ecology · fail-closed by design",
            [
                Rgb::from_hex("#0a0404"), // bg
                Rgb::from_hex("#bfb3a4"), // fg
                Rgb::from_hex("#ecdfd0"), // bright
                Rgb::from_hex("#6e6055"), // dim
                Rgb::from_hex("#443a33"), // dimmer
                Rgb::from_hex("#b8402e"), // cap
                Rgb::from_hex("#6e2417"), // rim
                Rgb::from_hex("#e8d9c4"), // speck
                Rgb::from_hex("#8f8468"), // stem
                Rgb::from_hex("#54432e"), // thread
                Rgb::from_hex("#382c1e"), // thread2
                Rgb::from_hex("#c74b32"), // accent
                Rgb::from_hex("#8faf5c"), // ok
                Rgb::from_hex("#a08b6a"), // prompt
                Rgb::from_hex("#241412"), // frame
                Rgb::from_hex("#2a1a14"), // ground
            ],
        )
    }

    pub fn phosphor() -> Self {
        themed(
            "phosphor",
            true,
            "v0.4.2 — substrate ecology · fail-closed by design",
            [
                Rgb::from_hex("#020803"), // bg
                Rgb::from_hex("#7dcf96"), // fg
                Rgb::from_hex("#d8ffe4"), // bright
                Rgb::from_hex("#3f7a52"), // dim
                Rgb::from_hex("#26492f"), // dimmer
                Rgb::from_hex("#33ff66"), // cap
                Rgb::from_hex("#1a8f3c"), // rim
                Rgb::from_hex("#ccffdd"), // speck
                Rgb::from_hex("#2a9e52"), // stem
                Rgb::from_hex("#155c30"), // thread
                Rgb::from_hex("#0d3d20"), // thread2
                Rgb::from_hex("#33ff66"), // accent
                Rgb::from_hex("#33ff66"), // ok
                Rgb::from_hex("#33ff66"), // prompt
                Rgb::from_hex("#123420"), // frame
                Rgb::from_hex("#143a24"), // ground
            ],
        )
    }

    pub fn amber() -> Self {
        themed(
            "amber",
            true,
            "v0.4.2 — substrate ecology · fail-closed by design",
            [
                Rgb::from_hex("#0a0602"), // bg
                Rgb::from_hex("#d1a35c"), // fg
                Rgb::from_hex("#ffe6bb"), // bright
                Rgb::from_hex("#7a5c2e"), // dim
                Rgb::from_hex("#4a3820"), // dimmer
                Rgb::from_hex("#ffb000"), // cap
                Rgb::from_hex("#8f5e00"), // rim
                Rgb::from_hex("#ffe8b3"), // speck
                Rgb::from_hex("#b8842e"), // stem
                Rgb::from_hex("#5c451f"), // thread
                Rgb::from_hex("#3d2e14"), // thread2
                Rgb::from_hex("#ffb000"), // accent
                Rgb::from_hex("#ffb000"), // ok
                Rgb::from_hex("#ffb000"), // prompt
                Rgb::from_hex("#2e2210"), // frame
                Rgb::from_hex("#332611"), // ground
            ],
        )
    }

    pub fn synthwave() -> Self {
        themed(
            "synthwave",
            true,
            "v0.4.2 — substrate ecology · fail-closed // wake up, operator",
            [
                Rgb::from_hex("#0c0518"), // bg
                Rgb::from_hex("#b3a6d9"), // fg
                Rgb::from_hex("#f2ecff"), // bright
                Rgb::from_hex("#6a5c99"), // dim
                Rgb::from_hex("#3d3366"), // dimmer
                Rgb::from_hex("#ff3ea5"), // cap
                Rgb::from_hex("#8f1f66"), // rim
                Rgb::from_hex("#ffde59"), // speck
                Rgb::from_hex("#8f7bd9"), // stem
                Rgb::from_hex("#5b3fd1"), // thread
                Rgb::from_hex("#372680"), // thread2
                Rgb::from_hex("#ff3ea5"), // accent
                Rgb::from_hex("#3ef0c0"), // ok
                Rgb::from_hex("#3edbff"), // prompt
                Rgb::from_hex("#2a1a4d"), // frame
                Rgb::from_hex("#301f57"), // ground
            ],
        )
    }
}

/// Build a theme from its base roles (order: bg, fg, bright, dim, dimmer, cap,
/// rim, speck, stem, thread, thread2, accent, ok, prompt, frame, ground) and
/// derive its TUI-only roles.
fn themed(name: &'static str, glow: bool, tag: &'static str, base: [Rgb; 16]) -> Theme {
    let [bg, fg, bright, dim, dimmer, cap, rim, speck, stem, thread, thread2, accent, ok, prompt, frame, ground] =
        base;
    let mut theme = Theme {
        name,
        glow,
        tag,
        bg,
        fg,
        bright,
        dim,
        dimmer,
        cap,
        rim,
        speck,
        stem,
        thread,
        thread2,
        accent,
        ok,
        prompt,
        frame,
        ground,
        // Placeholders; derive_tui_roles fills these from the base roles.
        panel_bg: Rgb::default(),
        border: Rgb::default(),
        value: Rgb::default(),
        secondary: Rgb::default(),
        muted: Rgb::default(),
        faint: Rgb::default(),
        accent_dim: Rgb::default(),
        diff_bg: Rgb::default(),
        diff_add: Rgb::default(),
        diff_del: Rgb::default(),
        deny_border: Rgb::default(),
        deny_bg: Rgb::default(),
        selection: Rgb::default(),
    };
    theme.derive_tui_roles();
    theme
}

/// Linear interpolation of one channel from `a` to `b` by `t` in `0.0..=1.0`.
fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Mix `a` toward `b` by `t`: `a + (b - a) * t` per channel.
pub fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    Rgb {
        r: lerp(a.r, b.r, t),
        g: lerp(a.g, b.g, t),
        b: lerp(a.b, b.b, t),
    }
}

/// Darken toward black by `f`: `c * (1 - f)` per channel.
pub fn darken(c: Rgb, f: f32) -> Rgb {
    blend(c, Rgb { r: 0, g: 0, b: 0 }, f)
}

/// Lighten toward white by `f`: `c + (255 - c) * f` per channel.
pub fn lighten(c: Rgb, f: f32) -> Rgb {
    blend(
        c,
        Rgb {
            r: 255,
            g: 255,
            b: 255,
        },
        f,
    )
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

    fn close(a: Rgb, b: Rgb, tol: i16) -> bool {
        (a.r as i16 - b.r as i16).abs() <= tol
            && (a.g as i16 - b.g as i16).abs() <= tol
            && (a.b as i16 - b.b as i16).abs() <= tol
    }

    #[test]
    fn derivation_stays_in_family_with_amanita_for_every_role() {
        // Amanita ships explicit TUI overrides; the other six themes derive
        // theirs. Re-derive amanita's from its base roles and confirm every one
        // of the 13 derived roles lands in-family with the real override. This
        // is a coarse anchor (per-theme fidelity is eyeballed when components
        // render them), but it catches a rule that produces a garbage color.
        let real = Theme::amanita();
        let mut d = real.clone();
        d.derive_tui_roles();
        const TOL: i16 = 36;
        assert!(close(d.panel_bg, real.panel_bg, TOL), "panel_bg");
        assert!(close(d.border, real.border, TOL), "border");
        assert!(close(d.value, real.value, TOL), "value");
        assert!(close(d.secondary, real.secondary, TOL), "secondary");
        assert!(close(d.muted, real.muted, TOL), "muted");
        assert!(close(d.faint, real.faint, TOL), "faint");
        assert!(close(d.accent_dim, real.accent_dim, TOL), "accent_dim");
        assert!(close(d.diff_bg, real.diff_bg, TOL), "diff_bg");
        assert!(close(d.diff_add, real.diff_add, TOL), "diff_add");
        assert!(close(d.diff_del, real.diff_del, TOL), "diff_del");
        assert!(close(d.deny_border, real.deny_border, TOL), "deny_border");
        assert!(close(d.deny_bg, real.deny_bg, TOL), "deny_bg");
        assert!(close(d.selection, real.selection, TOL), "selection");
    }

    #[test]
    fn nonamanita_themes_carry_their_source_values() {
        // One distinctive base role per theme, to catch a transposed or
        // mistyped hex in the hand-copied startup values.
        assert_eq!(Theme::hacker().accent, Rgb::from_hex("#ff2d78"));
        assert_eq!(Theme::foxfire().ok, Rgb::from_hex("#54ffb0"));
        assert_eq!(Theme::cordyceps().rim, Rgb::from_hex("#6e2417"));
        assert_eq!(Theme::phosphor().bg, Rgb::from_hex("#020803"));
        assert_eq!(Theme::amber().accent, Rgb::from_hex("#ffb000"));
        assert_eq!(Theme::synthwave().speck, Rgb::from_hex("#ffde59"));
    }

    #[test]
    fn every_theme_resolves_and_is_distinct_bg() {
        let mut seen = std::collections::HashSet::new();
        for name in Theme::ALL {
            let t = Theme::by_name(name).unwrap();
            assert_eq!(t.name, name);
            // accent + bg are set (non-zero struct, all roles present by construction)
            seen.insert((t.bg.r, t.bg.g, t.bg.b));
        }
        assert_eq!(seen.len(), 7, "themes must have distinct backgrounds");
        assert!(Theme::by_name("nope").is_none());
    }
}
