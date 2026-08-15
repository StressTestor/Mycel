use mycel_agent_protocol::{ExecutableToolOutput, ExecutableToolResult};

pub(crate) const MAX_RESULT_CHARS: usize = 50_000;
pub(crate) const MAX_RESULT_LINE_CHARS: usize = 2_000;
const MARKER: &str = "[...truncated]";

#[derive(Debug)]
pub(crate) struct OutputBuffer {
    text: String,
    max_chars: usize,
    max_line_chars: usize,
    chars: usize,
    truncated: bool,
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self::new(MAX_RESULT_CHARS, MAX_RESULT_LINE_CHARS)
    }
}

impl OutputBuffer {
    pub(crate) fn new(max_chars: usize, max_line_chars: usize) -> Self {
        Self {
            text: String::new(),
            max_chars,
            max_line_chars,
            chars: 0,
            truncated: false,
        }
    }

    pub(crate) fn push(&mut self, input: &str) {
        if input.is_empty() || self.truncated && self.chars >= self.max_chars {
            return;
        }
        for segment in input.split_inclusive(['\n', '\r']) {
            if self.chars >= self.max_chars {
                self.mark_truncated();
                break;
            }
            let has_break = segment.ends_with(['\n', '\r']);
            let body = if has_break {
                &segment[..segment.len() - 1]
            } else {
                segment
            };
            let mut rendered = take_chars(body, self.max_line_chars);
            if rendered.chars().count() < body.chars().count() {
                rendered = fit_marker(rendered, self.max_line_chars);
                self.truncated = true;
            }
            if has_break {
                rendered.push_str(&segment[segment.len() - 1..]);
            }
            let rendered_chars = rendered.chars().count();
            let remaining = self.max_chars.saturating_sub(self.chars);
            if rendered_chars > remaining {
                self.text.push_str(&take_chars(&rendered, remaining));
                self.chars += remaining;
                self.mark_truncated();
                break;
            }
            self.text.push_str(&rendered);
            self.chars += rendered_chars;
        }
    }

    fn mark_truncated(&mut self) {
        if !self.text.ends_with(MARKER)
            && self.chars + MARKER.len() <= self.max_chars + MARKER.len()
        {
            self.text.push_str(MARKER);
            self.chars += MARKER.chars().count();
        }
        self.truncated = true;
    }

    pub(crate) fn into_result(
        self,
        is_error: bool,
        message: Option<String>,
    ) -> ExecutableToolResult {
        let mut text = self.text;
        if text.is_empty() {
            if let Some(message) = &message {
                text.push_str(message);
            }
        } else if self.truncated {
            if let Some(message) = &message {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(message);
            }
        }
        ExecutableToolResult {
            output: ExecutableToolOutput::Text(text),
            is_error,
            stop_turn: false,
            message,
            note: None,
            truncated: self.truncated,
        }
    }
}

pub(super) fn text_result(text: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(text.into()),
        is_error: false,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

pub(crate) fn error_result(text: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(text.into()),
        is_error: true,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn fit_marker(mut prefix: String, limit: usize) -> String {
    let keep = limit.saturating_sub(MARKER.chars().count());
    prefix = take_chars(&prefix, keep);
    prefix.push_str(MARKER);
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_lines_and_total_output_at_unicode_boundaries() {
        let mut output = OutputBuffer::new(12, 6);
        output.push("ééééééé\nsecond line");
        let result = output.into_result(false, None);
        let ExecutableToolOutput::Text(text) = result.output else {
            panic!("text output")
        };
        assert!(result.truncated);
        assert!(text.is_char_boundary(text.len()));
        assert!(text.contains("[...truncated]"));
    }
}
