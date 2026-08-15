use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ToolDefinition;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Think {
        think: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
    },
    ImageUrl {
        #[serde(rename = "imageUrl")]
        image_url: MediaUrl,
    },
    AudioUrl {
        #[serde(rename = "audioUrl")]
        audio_url: MediaUrl,
    },
    VideoUrl {
        #[serde(rename = "videoUrl")]
        video_url: MediaUrl,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallKind {
    #[default]
    Function,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub id: String,
    pub name: String,
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, Value>,
}

impl ToolCall {
    pub fn validate(&self) -> Result<(), MessageError> {
        if self.id.trim().is_empty() {
            return Err(MessageError::EmptyToolCallId);
        }
        if self.name.trim().is_empty() {
            return Err(MessageError::EmptyToolName);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamIndex {
    Number(u64),
    String(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamPart {
    Text {
        text: String,
    },
    Think {
        think: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
    },
    ImageUrl {
        #[serde(rename = "imageUrl")]
        image_url: MediaUrl,
    },
    AudioUrl {
        #[serde(rename = "audioUrl")]
        audio_url: MediaUrl,
    },
    VideoUrl {
        #[serde(rename = "videoUrl")]
        video_url: MediaUrl,
    },
    Function {
        id: String,
        name: String,
        arguments: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extras: BTreeMap<String, Value>,
        #[serde(
            rename = "_streamIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        stream_index: Option<StreamIndex>,
    },
    ToolCallPart {
        #[serde(rename = "argumentsPart")]
        arguments_part: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<StreamIndex>,
    },
}

impl StreamPart {
    pub fn content(self) -> Option<ContentPart> {
        match self {
            Self::Text { text } => Some(ContentPart::Text { text }),
            Self::Think { think, encrypted } => Some(ContentPart::Think { think, encrypted }),
            Self::ImageUrl { image_url } => Some(ContentPart::ImageUrl { image_url }),
            Self::AudioUrl { audio_url } => Some(ContentPart::AudioUrl { audio_url }),
            Self::VideoUrl { video_url } => Some(ContentPart::VideoUrl { video_url }),
            Self::Function { .. } | Self::ToolCallPart { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: Vec<ContentPart>,
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            name: None,
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
            tools: Vec::new(),
        }
    }

    pub fn assistant(content: Vec<ContentPart>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            name: None,
            content,
            tool_calls,
            tool_call_id: None,
            partial: false,
            tools: Vec::new(),
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            name: None,
            content: vec![ContentPart::text(output)],
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            partial: false,
            tools: Vec::new(),
        }
    }

    pub fn text(&self, separator: &str) -> String {
        self.content
            .iter()
            .filter_map(ContentPart::as_text)
            .collect::<Vec<_>>()
            .join(separator)
    }

    pub fn is_tool_declaration_only(&self) -> bool {
        !self.tools.is_empty() && self.content.is_empty() && self.tool_calls.is_empty()
    }

    pub fn validate(&self) -> Result<(), MessageError> {
        if self.role == Role::Tool && self.tool_call_id.as_deref().unwrap_or("").is_empty() {
            return Err(MessageError::ToolMessageMissingCallId);
        }
        if self.role != Role::Tool && self.tool_call_id.is_some() {
            return Err(MessageError::NonToolMessageHasCallId);
        }
        if !self.tool_calls.is_empty() && self.role != Role::Assistant {
            return Err(MessageError::ToolCallsOnNonAssistant);
        }
        if !self.tools.is_empty() && self.role != Role::System {
            return Err(MessageError::ToolsOnNonSystem);
        }
        let mut call_ids = BTreeSet::new();
        for call in &self.tool_calls {
            call.validate()?;
            if !call_ids.insert(&call.id) {
                return Err(MessageError::DuplicateToolCallId(call.id.clone()));
            }
        }
        for tool in &self.tools {
            tool.validate()
                .map_err(|error| MessageError::InvalidTool(error.to_string()))?;
        }
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    #[error("tool message is missing toolCallId")]
    ToolMessageMissingCallId,
    #[error("non-tool message must not contain toolCallId")]
    NonToolMessageHasCallId,
    #[error("tool call id must not be empty")]
    EmptyToolCallId,
    #[error("tool call name must not be empty")]
    EmptyToolName,
    #[error("tool calls may only appear on assistant messages")]
    ToolCallsOnNonAssistant,
    #[error("message-level tool declarations may only appear on system messages")]
    ToolsOnNonSystem,
    #[error("duplicate tool call id {0:?}")]
    DuplicateToolCallId(String),
    #[error("invalid tool definition: {0}")]
    InvalidTool(String),
}
