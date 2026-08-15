use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ContentPart, FinishReason, TokenUsage, ToolInputDisplay, ToolUpdate};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopStepStopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Filtered,
    Paused,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopInterruptReason {
    Aborted,
    MaxSteps,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecutableToolOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableToolResult {
    pub output: ExecutableToolOutput,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stop_turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum LoopEvent {
    #[serde(rename = "step.begin")]
    StepBegin {
        uuid: String,
        turn_id: String,
        step: u64,
    },
    #[serde(rename = "step.end")]
    StepEnd {
        uuid: String,
        turn_id: String,
        step: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<LoopStepStopReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_first_token_latency_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_stream_duration_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_request_build_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_server_first_token_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_server_decode_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_client_consume_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_finish_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    #[serde(rename = "content.part")]
    ContentPart {
        uuid: String,
        turn_id: String,
        step: u64,
        step_uuid: String,
        part: LoopContentPart,
    },
    #[serde(rename = "tool.call")]
    ToolCall {
        uuid: String,
        turn_id: String,
        step: u64,
        step_uuid: String,
        tool_call_id: String,
        name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ToolInputDisplay>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extras: BTreeMap<String, Value>,
    },
    #[serde(rename = "tool.result")]
    ToolResult {
        parent_uuid: String,
        tool_call_id: String,
        result: ExecutableToolResult,
    },
    #[serde(rename = "turn.interrupted")]
    TurnInterrupted {
        reason: LoopInterruptReason,
        attempted_steps: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_step: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    #[serde(rename = "step.retrying")]
    StepRetrying {
        turn_id: String,
        step: u64,
        step_uuid: String,
        failed_attempt: u64,
        next_attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error_name: String,
        error_message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
    #[serde(rename = "text.delta")]
    TextDelta { delta: String },
    #[serde(rename = "thinking.delta")]
    ThinkingDelta { delta: String },
    #[serde(rename = "tool.call.delta")]
    ToolCallDelta {
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_part: Option<String>,
    },
    #[serde(rename = "tool.progress")]
    ToolProgress {
        tool_call_id: String,
        update: ToolUpdate,
    },
}

impl LoopEvent {
    pub const fn is_recorded(&self) -> bool {
        matches!(
            self,
            Self::StepBegin { .. }
                | Self::StepEnd { .. }
                | Self::ContentPart { .. }
                | Self::ToolCall { .. }
                | Self::ToolResult { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoopContentPart {
    Text {
        text: String,
    },
    Think {
        think: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
    },
}

fn is_false(value: &bool) -> bool {
    !value
}
