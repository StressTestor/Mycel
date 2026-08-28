//! Styled spans and their terminal encoding.
//!
//! A `StyledLine` is a run of `(text, style)` spans that renders to exactly one
//! terminal line: SGR-coded, padded or truncated to a target column width. Colors
//! are 24-bit RGB with a nearest-256 downgrade for terminals without truecolor.
//! The `DifferentialRenderer` keeps diffing final strings, so one styled line maps
//! to one string and the diff layer is unchanged.

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Parse a `#rrggbb` hex string. Malformed channels fall back to 0.
    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        let channel = |start: usize| {
            hex.get(start..start + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .unwrap_or(0)
        };
        Self {
            r: channel(0),
            g: channel(2),
            b: channel(4),
        }
    }
}

/// A terminal color: an explicit RGB value or the terminal default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    Rgb(Rgb),
    #[default]
    Reset,
}

impl Color {
    /// SGR parameters that set this color as the foreground.
    pub fn fg_sgr(self, truecolor: bool) -> String {
        self.sgr(truecolor, 38, 39)
    }

    /// SGR parameters that set this color as the background.
    pub fn bg_sgr(self, truecolor: bool) -> String {
        self.sgr(truecolor, 48, 49)
    }

    fn sgr(self, truecolor: bool, base: u16, reset: u16) -> String {
        match self {
            Self::Reset => reset.to_string(),
            Self::Rgb(rgb) if truecolor => format!("{base};2;{};{};{}", rgb.r, rgb.g, rgb.b),
            Self::Rgb(rgb) => format!("{base};5;{}", nearest_256(rgb)),
        }
    }
}

/// Map an RGB value to the nearest index in the 6x6x6 color cube (16..=231).
fn nearest_256(rgb: Rgb) -> u16 {
    let level = |value: u8| (value as u16 * 5 + 127) / 255;
    16 + 36 * level(rgb.r) + 6 * level(rgb.g) + level(rgb.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parses_and_emits_truecolor_and_256() {
        let c = Color::Rgb(Rgb::from_hex("#e05a1e"));
        assert_eq!(c.fg_sgr(true), "38;2;224;90;30");
        // 256 downgrade lands in the 6x6x6 color cube (16..=231)
        let idx: u16 = c
            .fg_sgr(false)
            .strip_prefix("38;5;")
            .unwrap()
            .parse()
            .unwrap();
        assert!((16..=231).contains(&idx));
    }
}
