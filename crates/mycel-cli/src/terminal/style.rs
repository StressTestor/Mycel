//! Styled spans and their terminal encoding.
//!
//! A `StyledLine` is a run of `(text, style)` spans that renders to exactly one
//! terminal line: SGR-coded, padded or truncated to a target column width. Colors
//! are 24-bit RGB with a nearest-256 downgrade for terminals without truecolor.
//! The `DifferentialRenderer` keeps diffing final strings, so one styled line maps
//! to one string and the diff layer is unchanged.

use super::unicode::{truncate_to_width, visible_width};

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

/// Bold/italic/underline text attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A foreground color plus an optional background and text attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Option<Color>,
    pub attrs: Attrs,
}

impl Style {
    /// A style with the given foreground and default everything else.
    pub fn fg(color: Color) -> Self {
        Self {
            fg: color,
            ..Self::default()
        }
    }

    /// Set the background color.
    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Enable bold.
    pub fn bold(mut self) -> Self {
        self.attrs.bold = true;
        self
    }

    /// Enable italic.
    pub fn italic(mut self) -> Self {
        self.attrs.italic = true;
        self
    }

    /// Enable underline.
    pub fn underline(mut self) -> Self {
        self.attrs.underline = true;
        self
    }

    /// The `\x1b[...m` prefix that turns this style on.
    fn sgr_prefix(&self, truecolor: bool) -> String {
        let mut codes: Vec<String> = Vec::new();
        if self.attrs.bold {
            codes.push("1".to_owned());
        }
        if self.attrs.italic {
            codes.push("3".to_owned());
        }
        if self.attrs.underline {
            codes.push("4".to_owned());
        }
        codes.push(self.fg.fg_sgr(truecolor));
        if let Some(bg) = self.bg {
            codes.push(bg.bg_sgr(truecolor));
        }
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// A run of text carrying a single style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// One terminal line as an ordered list of styled spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledLine(pub Vec<Span>);

impl StyledLine {
    /// Render to a single terminal line: each span's SGR, its text, a reset
    /// between spans, then padding or truncation to `width` and a final reset.
    /// A line with no spans renders empty so the differ treats it as blank.
    pub fn render(&self, width: usize, truecolor: bool) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let mut body = String::new();
        for span in &self.0 {
            body.push_str(&span.style.sgr_prefix(truecolor));
            body.push_str(&span.text);
            body.push_str("\x1b[0m");
        }
        let visible = visible_width(&body);
        let mut out = if visible > width {
            truncate_to_width(&body, width, "")
        } else {
            body.push_str(&" ".repeat(width - visible));
            body
        };
        out.push_str("\x1b[0m");
        out
    }
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

    #[test]
    fn styled_line_pads_to_width_and_wraps_sgr() {
        let line = StyledLine(vec![Span::new(
            "hi",
            Style::fg(Color::Rgb(Rgb::from_hex("#55a868"))),
        )]);
        let out = line.render(6, true);
        assert!(out.starts_with("\x1b[38;2;85;168;104m"));
        assert!(out.contains("hi"));
        assert!(out.trim_end_matches("\x1b[0m").ends_with("    ")); // padded 2 -> 6
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn styled_line_truncates_to_width() {
        let line = StyledLine(vec![Span::new("abcdef", Style::default())]);
        // 3 visible cells max; ANSI stripped length of the text region == 3
        let out = line.render(3, true);
        assert!(out.contains("abc") && !out.contains("abcd"));
    }
}
