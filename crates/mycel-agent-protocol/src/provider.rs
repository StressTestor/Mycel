use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    pin::Pin,
};

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    ContentPart, Message, OptionalNullable, StreamIndex, StreamPart, ToolCall, ToolCallKind,
    ToolDefinition,
};

pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<ProviderStreamEvent, ProviderError>> + Send>>;

pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderEventStream, ProviderError>> + Send + 'a>>;

/// Injectable model boundary used by the runtime and by deterministic parity
/// tests. Dropping the returned future or stream must cancel transport work.
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
        auth: &'a ProviderRequestAuth,
    ) -> ProviderFuture<'a>;
}

#[derive(Clone, Default)]
pub struct ProviderRequestAuth {
    pub api_key: Option<SecretString>,
    pub headers: BTreeMap<String, SecretString>,
}

impl fmt::Debug for ProviderRequestAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRequestAuth")
            .field("api_key", &self.api_key)
            .field("headers", &self.headers)
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Authentication,
    RateLimit,
    Connection,
    InvalidRequest,
    Filtered,
    MalformedResponse,
    EmptyResponse,
    Cancelled,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
    pub status_code: Option<u16>,
    pub retry_after_ms: Option<u64>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: matches!(
                kind,
                ProviderErrorKind::RateLimit | ProviderErrorKind::Connection
            ),
            status_code: None,
            retry_after_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Completed,
    ToolCalls,
    Truncated,
    Filtered,
    Paused,
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_other: u64,
    pub output: u64,
    pub input_cache_read: u64,
    pub input_cache_creation: u64,
}

impl TokenUsage {
    pub fn input_total(self) -> u64 {
        self.input_other
            .saturating_add(self.input_cache_read)
            .saturating_add(self.input_cache_creation)
    }

    pub fn grand_total(self) -> u64 {
        self.input_total().saturating_add(self.output)
    }

    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input_other: self.input_other.saturating_add(other.input_other),
            output: self.output.saturating_add(other.output),
            input_cache_read: self.input_cache_read.saturating_add(other.input_cache_read),
            input_cache_creation: self
                .input_cache_creation
                .saturating_add(other.input_cache_creation),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema {
        #[serde(rename = "jsonSchema")]
        json_schema: JsonSchemaFormat,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonSchemaFormat {
    pub name: String,
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThinkingEffort(String);

impl ThinkingEffort {
    pub fn new(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ProtocolError::EmptyThinkingEffort);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRequest {
    pub provider: String,
    pub model: String,
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub history: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<ThinkingEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

impl ProviderRequest {
    pub fn wire_tools(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.iter().filter(|tool| !tool.deferred)
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.provider.trim().is_empty() {
            return Err(ProtocolError::EmptyProvider);
        }
        if self.model.trim().is_empty() {
            return Err(ProtocolError::EmptyModel);
        }
        for tool in &self.tools {
            tool.validate()
                .map_err(|error| ProtocolError::InvalidTool(error.to_string()))?;
        }
        let mut pending_tool_calls = BTreeSet::new();
        for message in &self.history {
            message
                .validate()
                .map_err(|error| ProtocolError::InvalidMessage(error.to_string()))?;
            for call in &message.tool_calls {
                if !pending_tool_calls.insert(call.id.clone()) {
                    return Err(ProtocolError::DuplicateToolCallId(call.id.clone()));
                }
            }
            if let Some(tool_call_id) = &message.tool_call_id {
                if !pending_tool_calls.remove(tool_call_id) {
                    return Err(ProtocolError::OrphanToolResult(tool_call_id.clone()));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    ResponseStart {
        id: Option<String>,
        #[serde(
            rename = "traceId",
            default,
            skip_serializing_if = "OptionalNullable::is_missing"
        )]
        trace_id: OptionalNullable<String>,
    },
    Part {
        part: StreamPart,
    },
    Usage {
        usage: TokenUsage,
    },
    Finish {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<FinishReason>,
        #[serde(rename = "rawReason", default, skip_serializing_if = "Option::is_none")]
        raw_reason: Option<String>,
    },
    ResponseEnd,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateResult {
    pub id: Option<String>,
    pub message: Message,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<FinishReason>,
    pub raw_finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "OptionalNullable::is_missing")]
    pub trace_id: OptionalNullable<String>,
}

impl GenerateResult {
    /// Convert an aggregate response into the canonical provider event stream.
    /// This is the compatibility bridge used by runtimes when a provider has
    /// not implemented incremental delivery yet.
    pub fn into_stream_events(self) -> Vec<ProviderStreamEvent> {
        let Self {
            id,
            message,
            usage,
            finish_reason,
            raw_finish_reason,
            trace_id,
        } = self;
        let mut events = Vec::with_capacity(
            message
                .content
                .len()
                .saturating_add(message.tool_calls.len())
                .saturating_add(4),
        );
        events.push(ProviderStreamEvent::ResponseStart { id, trace_id });
        events.extend(message.content.into_iter().map(|part| {
            let part = match part {
                ContentPart::Text { text } => StreamPart::Text { text },
                ContentPart::Think { think, encrypted } => StreamPart::Think { think, encrypted },
                ContentPart::ImageUrl { image_url } => StreamPart::ImageUrl { image_url },
                ContentPart::AudioUrl { audio_url } => StreamPart::AudioUrl { audio_url },
                ContentPart::VideoUrl { video_url } => StreamPart::VideoUrl { video_url },
            };
            ProviderStreamEvent::Part { part }
        }));
        events.extend(
            message
                .tool_calls
                .into_iter()
                .map(|call| ProviderStreamEvent::Part {
                    part: StreamPart::Function {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                        extras: call.extras,
                        stream_index: None,
                    },
                }),
        );
        if let Some(usage) = usage {
            events.push(ProviderStreamEvent::Usage { usage });
        }
        if finish_reason.is_some() || raw_finish_reason.is_some() {
            events.push(ProviderStreamEvent::Finish {
                reason: finish_reason,
                raw_reason: raw_finish_reason,
            });
        }
        events.push(ProviderStreamEvent::ResponseEnd);
        events
    }
}

#[derive(Default)]
pub struct StreamAssembler {
    id: Option<String>,
    trace_id: OptionalNullable<String>,
    content: Vec<ContentPart>,
    tool_calls: Vec<ToolCall>,
    pending: Option<StreamPart>,
    tool_call_indexes: BTreeMap<StreamIndex, usize>,
    usage: Option<TokenUsage>,
    finish_reason: Option<FinishReason>,
    raw_finish_reason: Option<String>,
    started: bool,
    ended: bool,
}

impl StreamAssembler {
    pub fn push(&mut self, event: ProviderStreamEvent) -> Result<(), ProtocolError> {
        if self.ended {
            return Err(ProtocolError::EventAfterEnd);
        }
        match event {
            ProviderStreamEvent::ResponseStart { id, trace_id } => {
                if self.started {
                    return Err(ProtocolError::DuplicateStart);
                }
                self.started = true;
                self.id = id;
                self.trace_id = trace_id;
            }
            ProviderStreamEvent::Part { part } => self.push_part(part)?,
            ProviderStreamEvent::Usage { usage } => self.usage = Some(usage),
            ProviderStreamEvent::Finish { reason, raw_reason } => {
                self.finish_reason = reason;
                self.raw_finish_reason = raw_reason;
            }
            ProviderStreamEvent::ResponseEnd => {
                self.flush_pending()?;
                self.ended = true;
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<GenerateResult, ProtocolError> {
        self.flush_pending()?;
        if self.content.is_empty() && self.tool_calls.is_empty() {
            return Err(ProtocolError::EmptyResponse);
        }
        let has_think = self
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::Think { .. }));
        let has_text = self
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::Text { text } if !text.trim().is_empty()));
        if has_think && !has_text && self.tool_calls.is_empty() {
            return Err(ProtocolError::ThinkingOnlyResponse);
        }
        Ok(GenerateResult {
            id: self.id,
            message: Message::assistant(self.content, self.tool_calls),
            usage: self.usage,
            finish_reason: self.finish_reason,
            raw_finish_reason: self.raw_finish_reason,
            trace_id: self.trace_id,
        })
    }

    fn push_part(&mut self, part: StreamPart) -> Result<(), ProtocolError> {
        if let StreamPart::ToolCallPart {
            arguments_part,
            index: Some(index),
        } = &part
        {
            if !pending_has_index(self.pending.as_ref(), index) {
                if let Some(position) = self.tool_call_indexes.get(index).copied() {
                    if let Some(arguments_part) = arguments_part {
                        append_arguments(&mut self.tool_calls[position].arguments, arguments_part);
                    }
                    return Ok(());
                }
            }
        }

        if let Some(pending) = self.pending.as_mut() {
            if merge_stream_part(pending, &part) {
                return Ok(());
            }
            self.flush_pending()?;
        }
        self.pending = Some(part);
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<(), ProtocolError> {
        let Some(part) = self.pending.take() else {
            return Ok(());
        };
        match part {
            StreamPart::Text { text } => self.content.push(ContentPart::Text { text }),
            StreamPart::Think { think, encrypted } => {
                self.content.push(ContentPart::Think { think, encrypted });
            }
            StreamPart::ImageUrl { image_url } => {
                self.content.push(ContentPart::ImageUrl { image_url });
            }
            StreamPart::AudioUrl { audio_url } => {
                self.content.push(ContentPart::AudioUrl { audio_url });
            }
            StreamPart::VideoUrl { video_url } => {
                self.content.push(ContentPart::VideoUrl { video_url });
            }
            StreamPart::Function {
                id,
                name,
                arguments,
                extras,
                stream_index,
            } => {
                let call = ToolCall {
                    kind: ToolCallKind::Function,
                    id,
                    name,
                    arguments,
                    extras,
                };
                call.validate()
                    .map_err(|error| ProtocolError::InvalidToolCall(error.to_string()))?;
                let position = self.tool_calls.len();
                self.tool_calls.push(call);
                if let Some(index) = stream_index {
                    self.tool_call_indexes.insert(index, position);
                }
            }
            // Preserves the accepted wire contract: a delta with no preceding tool
            // call is inert and never becomes executable output.
            StreamPart::ToolCallPart { .. } => {}
        }
        Ok(())
    }
}

fn pending_has_index(pending: Option<&StreamPart>, index: &StreamIndex) -> bool {
    matches!(
        pending,
        Some(StreamPart::Function {
            stream_index: Some(pending_index),
            ..
        }) if pending_index == index
    )
}

fn merge_stream_part(target: &mut StreamPart, source: &StreamPart) -> bool {
    match (target, source) {
        (StreamPart::Text { text }, StreamPart::Text { text: more }) => {
            text.push_str(more);
            true
        }
        (
            StreamPart::Think { think, encrypted },
            StreamPart::Think {
                think: more,
                encrypted: source_encrypted,
            },
        ) if encrypted.is_none() => {
            think.push_str(more);
            if let Some(value) = source_encrypted {
                *encrypted = Some(value.clone());
            }
            true
        }
        (
            StreamPart::Function { arguments, .. },
            StreamPart::ToolCallPart { arguments_part, .. },
        ) => {
            if let Some(arguments_part) = arguments_part {
                append_arguments(arguments, arguments_part);
            }
            true
        }
        _ => false,
    }
}

fn append_arguments(arguments: &mut Option<String>, part: &str) {
    match arguments {
        Some(arguments) => arguments.push_str(part),
        None => *arguments = Some(part.to_owned()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("provider name must not be empty")]
    EmptyProvider,
    #[error("model name must not be empty")]
    EmptyModel,
    #[error("thinking effort must not be empty")]
    EmptyThinkingEffort,
    #[error("invalid tool definition: {0}")]
    InvalidTool(String),
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    #[error("invalid streamed tool call: {0}")]
    InvalidToolCall(String),
    #[error("duplicate tool call id {0:?} in request history")]
    DuplicateToolCallId(String),
    #[error("tool result references unknown call id {0:?}")]
    OrphanToolResult(String),
    #[error("provider stream contains a duplicate response start")]
    DuplicateStart,
    #[error("provider stream emitted an event after response_end")]
    EventAfterEnd,
    #[error("provider returned no content or tool calls")]
    EmptyResponse,
    #[error("provider returned thinking without text or tool calls")]
    ThinkingOnlyResponse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_compatibility_stream_round_trips_through_the_canonical_assembler() {
        let aggregate = GenerateResult {
            id: Some("response-1".to_owned()),
            message: Message::assistant(
                vec![
                    ContentPart::Think {
                        think: "plan".to_owned(),
                        encrypted: Some("signature".to_owned()),
                    },
                    ContentPart::text("answer"),
                ],
                vec![ToolCall {
                    kind: ToolCallKind::Function,
                    id: "call-1".to_owned(),
                    name: "Read".to_owned(),
                    arguments: Some(r#"{"path":"README.md"}"#.to_owned()),
                    extras: BTreeMap::new(),
                }],
            ),
            usage: Some(TokenUsage {
                input_other: 4,
                output: 2,
                input_cache_read: 1,
                input_cache_creation: 0,
            }),
            finish_reason: Some(FinishReason::ToolCalls),
            raw_finish_reason: Some("tool_calls".to_owned()),
            trace_id: OptionalNullable::Value("trace-1".to_owned()),
        };
        let expected = aggregate.clone();
        let mut assembler = StreamAssembler::default();
        for event in aggregate.into_stream_events() {
            assembler.push(event).expect("compatibility event");
        }
        assert_eq!(assembler.finish().expect("assembled"), expected);
    }
}
