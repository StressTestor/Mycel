use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryMetadata {
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub error_name: String,
    pub error_message: String,
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessEvent {
    StepStarted,
    AssistantDelta(String),
    ThinkingDelta(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolCallDelta {
        id: String,
        name: Option<String>,
        arguments_part: Option<String>,
    },
    ToolResult {
        id: String,
        output: Value,
    },
    HookResult {
        hook_event: String,
        content: String,
        blocked: bool,
    },
    Retrying(RetryMetadata),
    StepCompleted,
    Progress(String),
    GoalSummary {
        goal_id: Option<String>,
        status: Option<String>,
        reason: Option<String>,
        turns_used: Option<u64>,
        tokens_used: Option<u64>,
        wall_clock_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessRecord {
    Thinking(String),
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        id: String,
        content: String,
    },
    Retry(RetryMetadata),
    Progress(String),
    ResumeHint {
        session_id: String,
    },
    GoalSummary {
        goal_id: Option<String>,
        status: Option<String>,
        reason: Option<String>,
        turns_used: Option<u64>,
        tokens_used: Option<u64>,
        wall_clock_ms: Option<u64>,
    },
}

#[derive(Debug, Default)]
pub struct HeadlessEventReducer {
    assistant: String,
    thinking: String,
    tool_calls: Vec<ToolCall>,
}

impl HeadlessEventReducer {
    pub fn push(&mut self, event: HeadlessEvent) -> Vec<HeadlessRecord> {
        match event {
            HeadlessEvent::StepStarted => Vec::new(),
            HeadlessEvent::AssistantDelta(delta) => {
                self.assistant.push_str(&delta);
                Vec::new()
            }
            HeadlessEvent::ThinkingDelta(delta) => {
                self.thinking.push_str(&delta);
                Vec::new()
            }
            HeadlessEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                let arguments = json_value_as_string(&arguments);
                if let Some(existing) = self.tool_calls.iter_mut().find(|call| call.id == id) {
                    existing.name = name;
                    existing.arguments = arguments;
                } else {
                    self.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments,
                    });
                }
                Vec::new()
            }
            HeadlessEvent::ToolCallDelta {
                id,
                name,
                arguments_part,
            } => {
                let call =
                    if let Some(index) = self.tool_calls.iter().position(|call| call.id == id) {
                        &mut self.tool_calls[index]
                    } else {
                        self.tool_calls.push(ToolCall {
                            id,
                            name: String::new(),
                            arguments: String::new(),
                        });
                        self.tool_calls.last_mut().expect("tool call was inserted")
                    };
                if let Some(name) = name {
                    call.name = name;
                }
                if let Some(arguments_part) = arguments_part {
                    call.arguments.push_str(&arguments_part);
                }
                Vec::new()
            }
            HeadlessEvent::ToolResult { id, output } => {
                let mut records = self.flush_attempt();
                records.push(HeadlessRecord::ToolResult {
                    id,
                    content: json_value_as_string(&output),
                });
                records
            }
            HeadlessEvent::HookResult {
                hook_event,
                content,
                blocked,
            } => {
                let mut records = self.flush_attempt();
                let blocked = if blocked { " blocked" } else { "" };
                let body = if content.trim().is_empty() {
                    "(empty)"
                } else {
                    content.trim()
                };
                records.push(HeadlessRecord::Assistant {
                    content: Some(format!("{hook_event} hook{blocked}\n\n{body}")),
                    tool_calls: Vec::new(),
                });
                records
            }
            HeadlessEvent::Retrying(metadata) => {
                self.discard_attempt();
                vec![HeadlessRecord::Retry(metadata)]
            }
            HeadlessEvent::StepCompleted => self.flush_attempt(),
            HeadlessEvent::Progress(message) => vec![HeadlessRecord::Progress(message)],
            HeadlessEvent::GoalSummary {
                goal_id,
                status,
                reason,
                turns_used,
                tokens_used,
                wall_clock_ms,
            } => {
                let mut records = self.flush_attempt();
                records.push(HeadlessRecord::GoalSummary {
                    goal_id,
                    status,
                    reason,
                    turns_used,
                    tokens_used,
                    wall_clock_ms,
                });
                records
            }
        }
    }

    pub fn finish(&mut self) -> Vec<HeadlessRecord> {
        self.flush_attempt()
    }

    fn flush_attempt(&mut self) -> Vec<HeadlessRecord> {
        let mut records = Vec::new();
        if !self.thinking.is_empty() {
            records.push(HeadlessRecord::Thinking(std::mem::take(&mut self.thinking)));
        }
        if !self.assistant.is_empty() || !self.tool_calls.is_empty() {
            records.push(HeadlessRecord::Assistant {
                content: (!self.assistant.is_empty()).then(|| std::mem::take(&mut self.assistant)),
                tool_calls: std::mem::take(&mut self.tool_calls),
            });
        }
        records
    }

    fn discard_attempt(&mut self) {
        self.assistant.clear();
        self.thinking.clear();
        self.tool_calls.clear();
    }
}

fn json_value_as_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}
