//! Small dependency-free terminal grapheme/width layer.
//!
//! This covers the sequences terminals most often expose to an editor: combining
//! marks, variation selectors, emoji modifiers, ZWJ emoji, keycaps, and paired
//! regional indicators. It intentionally does not claim complete UAX #29 or
//! Unicode-version conformance; that requires a generated table dependency.

pub fn graphemes(input: &str) -> Graphemes<'_> {
    Graphemes { rest: input }
}

pub struct Graphemes<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Graphemes<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let boundary = next_boundary(self.rest);
        let (cluster, rest) = self.rest.split_at(boundary);
        self.rest = rest;
        Some(cluster)
    }
}

pub fn visible_width(input: &str) -> usize {
    ansi_segments(input)
        .filter_map(|segment| match segment {
            AnsiSegment::Text(text) => Some(graphemes(text).map(grapheme_width).sum::<usize>()),
            AnsiSegment::Escape(_) => None,
        })
        .sum()
}

pub fn grapheme_width(cluster: &str) -> usize {
    if cluster.is_empty() {
        return 0;
    }
    let mut has_visible = false;
    let mut wide = false;
    let mut regional_indicators = 0usize;
    for character in cluster.chars() {
        if is_zero_width(character) {
            continue;
        }
        has_visible = true;
        if is_regional_indicator(character) {
            regional_indicators += 1;
        }
        wide |= is_wide(character);
    }
    if !has_visible {
        0
    } else if wide || regional_indicators >= 2 || cluster.contains('\u{200d}') {
        2
    } else {
        1
    }
}

pub fn wrap_text(input: &str, max_width: usize) -> Vec<String> {
    if input.is_empty() {
        return vec![String::new()];
    }
    if max_width == 0 {
        return input.split('\n').map(str::to_owned).collect();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut width = 0usize;
    for cluster in graphemes(input) {
        if cluster == "\n" {
            lines.push(std::mem::take(&mut line));
            width = 0;
            continue;
        }
        let cluster_width = grapheme_width(cluster);
        if !line.is_empty() && width + cluster_width > max_width {
            lines.push(std::mem::take(&mut line));
            width = 0;
        }
        // An indivisible wide grapheme at width=1 is kept intact so wrapping
        // always advances and cannot loop.
        line.push_str(cluster);
        width += cluster_width;
    }
    lines.push(line);
    lines
}

pub fn truncate_to_width(input: &str, max_width: usize, ellipsis: &str) -> String {
    if visible_width(input) <= max_width {
        return input.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let clipped_ellipsis = take_width(ellipsis, max_width);
    let available = max_width.saturating_sub(visible_width(&clipped_ellipsis));
    let mut output = take_width(input, available);
    output.push_str(&clipped_ellipsis);
    output
}

fn take_width(input: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0usize;
    let mut styled = false;
    'segments: for segment in ansi_segments(input) {
        match segment {
            AnsiSegment::Escape(sequence) => {
                styled = true;
                output.push_str(sequence);
            }
            AnsiSegment::Text(text) => {
                for cluster in graphemes(text) {
                    let cluster_width = grapheme_width(cluster);
                    if width + cluster_width > max_width {
                        break 'segments;
                    }
                    output.push_str(cluster);
                    width += cluster_width;
                }
            }
        }
    }
    if styled && !output.ends_with("\x1b[0m") {
        output.push_str("\x1b[0m");
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiSegment<'a> {
    Text(&'a str),
    Escape(&'a str),
}

fn ansi_segments(input: &str) -> impl Iterator<Item = AnsiSegment<'_>> {
    let mut offset = 0usize;
    std::iter::from_fn(move || {
        if offset >= input.len() {
            return None;
        }
        let bytes = input.as_bytes();
        if bytes[offset] != 0x1b {
            let end = bytes[offset..]
                .iter()
                .position(|byte| *byte == 0x1b)
                .map_or(input.len(), |relative| offset + relative);
            let segment = AnsiSegment::Text(&input[offset..end]);
            offset = end;
            return Some(segment);
        }
        let start = offset;
        offset = ansi_escape_end(bytes, offset);
        Some(AnsiSegment::Escape(&input[start..offset]))
    })
}

fn ansi_escape_end(bytes: &[u8], start: usize) -> usize {
    let Some(kind) = bytes.get(start + 1).copied() else {
        return bytes.len();
    };
    match kind {
        b'[' => bytes[start + 2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
            .map_or(bytes.len(), |relative| start + 3 + relative),
        b']' => control_string_end(bytes, start + 2, true),
        b'P' | b'_' | b'^' => control_string_end(bytes, start + 2, false),
        _ => (start + 2).min(bytes.len()),
    }
}

fn control_string_end(bytes: &[u8], mut offset: usize, bell_terminated: bool) -> usize {
    while offset < bytes.len() {
        if bell_terminated && bytes[offset] == 0x07 {
            return offset + 1;
        }
        if bytes[offset] == 0x1b && bytes.get(offset + 1) == Some(&b'\\') {
            return offset + 2;
        }
        offset += 1;
    }
    bytes.len()
}

fn next_boundary(input: &str) -> usize {
    let mut chars = input.char_indices();
    let (_, first) = chars.next().expect("input is non-empty");
    let mut boundary = first.len_utf8();
    if first == '\r' {
        if let Some((index, '\n')) = chars.next() {
            return index + '\n'.len_utf8();
        }
        return boundary;
    }

    let mut regional_count = usize::from(is_regional_indicator(first));
    let mut after_joiner = false;
    for (index, character) in chars {
        let include = if after_joiner {
            after_joiner = false;
            true
        } else if is_grapheme_extend(character) {
            true
        } else if character == '\u{200d}' {
            after_joiner = true;
            true
        } else if regional_count == 1 && is_regional_indicator(character) {
            regional_count += 1;
            true
        } else {
            false
        };
        if !include {
            return index;
        }
        boundary = index + character.len_utf8();
    }
    boundary
}

fn is_grapheme_extend(character: char) -> bool {
    let value = character as u32;
    is_combining(value)
        || matches!(value, 0xfe00..=0xfe0f | 0xe0100..=0xe01ef)
        || matches!(value, 0x1f3fb..=0x1f3ff)
        || matches!(value, 0xe0020..=0xe007f)
}

fn is_zero_width(character: char) -> bool {
    let value = character as u32;
    character.is_control()
        || character == '\u{200d}'
        || is_combining(value)
        || matches!(value, 0xfe00..=0xfe0f | 0xe0100..=0xe01ef)
        || matches!(value, 0x1f3fb..=0x1f3ff)
        || matches!(value, 0xe0020..=0xe007f)
}

fn is_combining(value: u32) -> bool {
    matches!(
        value,
        0x0300..=0x036f
            | 0x0483..=0x0489
            | 0x0591..=0x05bd
            | 0x05bf
            | 0x05c1..=0x05c2
            | 0x05c4..=0x05c5
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06ed
            | 0x0711
            | 0x0730..=0x074a
            | 0x07a6..=0x07b0
            | 0x07eb..=0x07f3
            | 0x0816..=0x082d
            | 0x0859..=0x085b
            | 0x08d3..=0x0903
            | 0x093a..=0x094f
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x0981..=0x09cd
            | 0x0a01..=0x0a4d
            | 0x0a70..=0x0a71
            | 0x0a81..=0x0acd
            | 0x0b01..=0x0bcd
            | 0x0c00..=0x0ccd
            | 0x0d00..=0x0d4d
            | 0x0e31
            | 0x0e34..=0x0e3a
            | 0x0e47..=0x0e4e
            | 0x0eb1
            | 0x0eb4..=0x0ebc
            | 0x0ec8..=0x0ecd
            | 0x0f18..=0x0f19
            | 0x0f35
            | 0x0f37
            | 0x0f39
            | 0x0f71..=0x0f84
            | 0x0f86..=0x0f87
            | 0x102b..=0x103e
            | 0x1056..=0x1059
            | 0x135d..=0x135f
            | 0x1712..=0x1715
            | 0x1732..=0x1734
            | 0x17b4..=0x17d3
            | 0x180b..=0x180f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

fn is_regional_indicator(character: char) -> bool {
    matches!(character as u32, 0x1f1e6..=0x1f1ff)
}

fn is_wide(character: char) -> bool {
    let value = character as u32;
    matches!(
        value,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2600..=0x27bf
            | 0x2e80..=0x303e
            | 0x3040..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f000..=0x1faff
            | 0x20000..=0x3fffd
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_sequences_are_zero_width_and_truncation_restores_style() {
        let styled = "\x1b[38;5;114massistant: 界abc\x1b[0m";
        assert_eq!(visible_width(styled), 16);
        let clipped = truncate_to_width(styled, 12, "");
        assert_eq!(visible_width(&clipped), 11);
        assert!(clipped.starts_with("\x1b[38;5;114m"));
        assert!(clipped.ends_with("\x1b[0m"));
    }

    #[test]
    fn osc_payloads_do_not_affect_visible_width() {
        assert_eq!(
            visible_width("a\x1b]8;;https://example.test\x07b\x1b]8;;\x07"),
            2
        );
    }
}
