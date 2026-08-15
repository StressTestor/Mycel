use serde_json::{json, Map, Value};

use super::{HeadlessError, HeadlessRecord, HeadlessRenderer, RetryMetadata, ToolCall};

const BULLET: &str = "• ";
const INDENT: &str = "  ";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedOutput {
    pub stdout: String,
    pub stderr: String,
}

impl RenderedOutput {
    pub fn append(&mut self, other: Self) {
        self.stdout.push_str(&other.stdout);
        self.stderr.push_str(&other.stderr);
    }
}

#[derive(Debug, Clone)]
pub struct TextRenderer {
    columns: Option<usize>,
}

impl TextRenderer {
    pub const fn new(columns: Option<usize>) -> Self {
        Self { columns }
    }
}

impl HeadlessRenderer for TextRenderer {
    fn render(&mut self, record: HeadlessRecord) -> Result<RenderedOutput, HeadlessError> {
        let mut output = RenderedOutput::default();
        match record {
            HeadlessRecord::Thinking(content) => {
                output.stderr = format_block(&content, self.columns);
            }
            HeadlessRecord::Assistant { content, .. } => {
                if let Some(content) = content {
                    output.stdout = format_block(&content, self.columns);
                }
            }
            HeadlessRecord::ToolResult { .. } | HeadlessRecord::Retry(_) => {}
            HeadlessRecord::Progress(message) => {
                output.stderr.push_str(&message);
                if !message.ends_with('\n') {
                    output.stderr.push('\n');
                }
            }
            HeadlessRecord::ResumeHint { session_id } => {
                output.stderr = format!("To resume this session: mycel -r {session_id}\n");
            }
            HeadlessRecord::GoalSummary {
                status,
                reason,
                turns_used,
                tokens_used,
                ..
            } => {
                output.stderr = match status {
                    None => "Goal: no goal found.\n".to_owned(),
                    Some(status) => {
                        let reason = reason
                            .map(|reason| format!(": {reason}"))
                            .unwrap_or_default();
                        format!(
                            "Goal [{status}]{reason} (turns: {}, tokens: {})\n",
                            turns_used.unwrap_or(0),
                            tokens_used.unwrap_or(0)
                        )
                    }
                };
            }
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StreamJsonRenderer;

impl HeadlessRenderer for StreamJsonRenderer {
    fn render(&mut self, record: HeadlessRecord) -> Result<RenderedOutput, HeadlessError> {
        let value = match record {
            HeadlessRecord::Thinking(_) | HeadlessRecord::Progress(_) => {
                return Ok(RenderedOutput::default())
            }
            HeadlessRecord::Assistant {
                content,
                tool_calls,
            } => assistant_message(content, tool_calls),
            HeadlessRecord::ToolResult { id, content } => json!({
                "role": "tool",
                "tool_call_id": id,
                "content": content,
            }),
            HeadlessRecord::Retry(metadata) => retry_message(metadata),
            HeadlessRecord::ResumeHint { session_id } => {
                let command = format!("mycel -r {session_id}");
                let content = format!("To resume this session: {command}");
                json!({
                    "role": "meta",
                    "type": "session.resume_hint",
                    "session_id": session_id,
                    "command": command,
                    "content": content,
                })
            }
            HeadlessRecord::GoalSummary {
                goal_id,
                status,
                reason,
                turns_used,
                tokens_used,
                wall_clock_ms,
            } => json!({
                "type": "goal.summary",
                "goalId": goal_id,
                "status": status,
                "reason": reason,
                "turnsUsed": turns_used,
                "tokensUsed": tokens_used,
                "wallClockMs": wall_clock_ms,
            }),
        };
        let line = serde_json::to_string(&value).map_err(|error| {
            HeadlessError::new(format!("failed to encode stream-json: {error}"))
        })?;
        Ok(RenderedOutput {
            stdout: format!("{line}\n"),
            stderr: String::new(),
        })
    }
}

fn assistant_message(content: Option<String>, tool_calls: Vec<ToolCall>) -> Value {
    let mut message = Map::new();
    message.insert("role".to_owned(), Value::String("assistant".to_owned()));
    if let Some(content) = content {
        message.insert("content".to_owned(), Value::String(content));
    }
    if !tool_calls.is_empty() {
        message.insert(
            "tool_calls".to_owned(),
            Value::Array(
                tool_calls
                    .into_iter()
                    .map(|call| {
                        json!({
                            "type": "function",
                            "id": call.id,
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments,
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
    Value::Object(message)
}

fn retry_message(metadata: RetryMetadata) -> Value {
    let mut message = Map::new();
    message.insert("role".to_owned(), Value::String("meta".to_owned()));
    message.insert(
        "type".to_owned(),
        Value::String("turn.step.retrying".to_owned()),
    );
    message.insert("failed_attempt".to_owned(), json!(metadata.failed_attempt));
    message.insert("next_attempt".to_owned(), json!(metadata.next_attempt));
    message.insert("max_attempts".to_owned(), json!(metadata.max_attempts));
    message.insert("delay_ms".to_owned(), json!(metadata.delay_ms));
    message.insert("error_name".to_owned(), json!(metadata.error_name));
    message.insert("error_message".to_owned(), json!(metadata.error_message));
    if let Some(status_code) = metadata.status_code {
        message.insert("status_code".to_owned(), json!(status_code));
    }
    Value::Object(message)
}

fn format_block(content: &str, columns: Option<usize>) -> String {
    if content.is_empty() {
        return String::new();
    }
    let wrap_width = columns.filter(|columns| *columns > INDENT.len() + 1);
    let mut output = String::from(BULLET);
    let mut at_line_start = false;
    let mut line_width = BULLET.len();
    for character in content.chars() {
        if at_line_start && character != '\n' {
            output.push_str(INDENT);
            at_line_start = false;
            line_width = INDENT.len();
        }
        let width = if character == '\t' { 4 } else { 1 };
        if wrap_width
            .is_some_and(|max| !at_line_start && character != '\n' && line_width + width > max)
        {
            output.push('\n');
            output.push_str(INDENT);
            line_width = INDENT.len();
        }
        output.push(character);
        if character == '\n' {
            at_line_start = true;
            line_width = 0;
        } else {
            line_width += width;
        }
    }
    if at_line_start {
        output.push('\n');
    } else {
        output.push_str("\n\n");
    }
    output
}
