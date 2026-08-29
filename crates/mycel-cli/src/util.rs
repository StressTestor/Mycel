//! Small crate-wide helpers.

/// A long id shortened for display: its first 8 characters. Char-based so a
/// non-ASCII id shortens instead of falling back to (or panicking on) a
/// byte-slice that lands off a UTF-8 boundary.
pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_takes_eight_chars_even_for_non_ascii() {
        assert_eq!(short_id("0123456789abcdef"), "01234567");
        assert_eq!(short_id("short"), "short");
        // Multi-byte chars: byte index 8 is not a char boundary here.
        assert_eq!(short_id("αβγδεζηθικλ"), "αβγδεζηθ");
        assert_eq!(short_id("αβγδεζηθικλ").chars().count(), 8);
    }
}
