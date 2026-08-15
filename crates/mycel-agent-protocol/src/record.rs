use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CURRENT_WIRE_VERSION: WireVersion = WireVersion { major: 1, minor: 4 };

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WireVersion {
    pub major: u16,
    pub minor: u16,
}

impl fmt::Display for WireVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

impl FromStr for WireVersion {
    type Err = RecordError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (major, minor) = value
            .split_once('.')
            .ok_or_else(|| RecordError::InvalidProtocolVersion(value.to_owned()))?;
        let major = major
            .parse()
            .map_err(|_| RecordError::InvalidProtocolVersion(value.to_owned()))?;
        let minor = minor
            .parse()
            .map_err(|_| RecordError::InvalidProtocolVersion(value.to_owned()))?;
        Ok(Self { major, minor })
    }
}

impl Serialize for WireVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WireVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

macro_rules! record_kinds {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum RecordKind {
            $($variant),+
        }

        impl RecordKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }

        impl FromStr for RecordKind {
            type Err = UnknownRecordKind;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(UnknownRecordKind(value.to_owned())),
                }
            }
        }

        impl Serialize for RecordKind {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for RecordKind {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

record_kinds! {
    Metadata => "metadata",
    Forked => "forked",
    TurnPrompt => "turn.prompt",
    TurnSteer => "turn.steer",
    TurnCancel => "turn.cancel",
    ConfigUpdate => "config.update",
    PermissionSetMode => "permission.set_mode",
    PermissionRecordApprovalResult => "permission.record_approval_result",
    FullCompactionBegin => "full_compaction.begin",
    PlanModeEnter => "plan_mode.enter",
    PlanModeCancel => "plan_mode.cancel",
    PlanModeExit => "plan_mode.exit",
    SwarmModeEnter => "swarm_mode.enter",
    SwarmModeExit => "swarm_mode.exit",
    ToolsRegisterUserTool => "tools.register_user_tool",
    ToolsUnregisterUserTool => "tools.unregister_user_tool",
    ToolsSetActiveTools => "tools.set_active_tools",
    UsageRecord => "usage.record",
    FullCompactionCancel => "full_compaction.cancel",
    FullCompactionComplete => "full_compaction.complete",
    MicroCompactionApply => "micro_compaction.apply",
    ContextAppendMessage => "context.append_message",
    ContextAppendLoopEvent => "context.append_loop_event",
    ContextUpdateTokenCount => "context.update_token_count",
    ContextClear => "context.clear",
    ContextApplyCompaction => "context.apply_compaction",
    ContextUndo => "context.undo",
    ToolsUpdateStore => "tools.update_store",
    GoalCreate => "goal.create",
    GoalUpdate => "goal.update",
    GoalClear => "goal.clear",
    LlmToolsSnapshot => "llm.tools_snapshot",
    LlmRequest => "llm.request",
    McpToolsDiscovered => "mcp.tools_discovered",
}

impl RecordKind {
    pub const fn class(self) -> RecordClass {
        match self {
            Self::LlmToolsSnapshot | Self::LlmRequest | Self::McpToolsDiscovered => {
                RecordClass::Observability
            }
            _ => RecordClass::State,
        }
    }

    pub const fn is_legacy_decode_only(self) -> bool {
        matches!(self, Self::MicroCompactionApply)
    }

    fn required_fields(self) -> &'static [&'static str] {
        match self {
            Self::Metadata => &["protocol_version", "created_at"],
            Self::TurnPrompt | Self::TurnSteer => &["input", "origin"],
            Self::ConfigUpdate => &[],
            Self::PermissionSetMode => &["mode"],
            Self::PermissionRecordApprovalResult => {
                &["turnId", "toolCallId", "toolName", "action", "result"]
            }
            Self::FullCompactionBegin => &["source"],
            Self::PlanModeEnter => &["id"],
            Self::SwarmModeEnter => &["trigger"],
            Self::ToolsRegisterUserTool => &["name"],
            Self::ToolsUnregisterUserTool => &["name"],
            Self::ToolsSetActiveTools => &["names"],
            Self::UsageRecord => &["model", "usage"],
            Self::MicroCompactionApply => &["cutoff"],
            Self::ContextAppendMessage => &["message"],
            Self::ContextAppendLoopEvent => &["event"],
            Self::ContextUpdateTokenCount => &["tokenCount"],
            Self::ContextApplyCompaction => {
                &["summary", "compactedCount", "tokensBefore", "tokensAfter"]
            }
            Self::ContextUndo => &["count"],
            Self::ToolsUpdateStore => &["key", "value"],
            Self::GoalCreate => &["goalId", "objective"],
            Self::LlmToolsSnapshot => &["hash", "tools"],
            Self::LlmRequest => &[
                "kind",
                "provider",
                "model",
                "toolSelect",
                "systemPromptHash",
                "toolsHash",
                "messageCount",
            ],
            Self::McpToolsDiscovered => &["serverName", "hash", "tools", "enabledNames"],
            Self::Forked
            | Self::TurnCancel
            | Self::PlanModeCancel
            | Self::PlanModeExit
            | Self::SwarmModeExit
            | Self::FullCompactionCancel
            | Self::FullCompactionComplete
            | Self::ContextClear
            | Self::GoalUpdate
            | Self::GoalClear => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordClass {
    State,
    Observability,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    #[serde(rename = "type")]
    pub record_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<u64>,
    #[serde(flatten)]
    pub payload: BTreeMap<String, Value>,
}

impl AgentRecord {
    pub fn new(kind: RecordKind, payload: BTreeMap<String, Value>) -> Self {
        Self {
            record_type: kind.as_str().to_owned(),
            time: None,
            payload,
        }
    }

    /// Returns `None` for a future record type. Unknown records are retained
    /// verbatim so newer session logs can be opened without data loss.
    pub fn kind(&self) -> Option<RecordKind> {
        self.record_type.parse().ok()
    }

    pub fn validate(&self) -> Result<(), RecordError> {
        if self.record_type.trim().is_empty() {
            return Err(RecordError::EmptyRecordType);
        }
        let Some(kind) = self.kind() else {
            return Ok(());
        };
        for field in kind.required_fields() {
            if !self.payload.contains_key(*field) {
                return Err(RecordError::MissingField {
                    record_type: self.record_type.clone(),
                    field,
                });
            }
        }
        if kind == RecordKind::Metadata {
            let version = self
                .payload
                .get("protocol_version")
                .and_then(Value::as_str)
                .ok_or(RecordError::MetadataVersionNotString)?;
            version.parse::<WireVersion>()?;
            if !self.payload.get("created_at").is_some_and(Value::is_number) {
                return Err(RecordError::MetadataCreatedAtNotNumber);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireCompatibility {
    Current,
    NeedsMigration { from: WireVersion },
    Newer { found: WireVersion },
}

/// Validates ordering and the protocol header without rejecting future record
/// tags. Record payload migrations happen in the persistence layer, where the
/// original JSON can be rewritten atomically.
pub fn validate_record_sequence(records: &[AgentRecord]) -> Result<WireCompatibility, RecordError> {
    let first = records.first().ok_or(RecordError::EmptyLog)?;
    if first.kind() != Some(RecordKind::Metadata) {
        return Err(RecordError::MetadataNotFirst);
    }
    for (index, record) in records.iter().enumerate() {
        record.validate()?;
        if index != 0 && record.kind() == Some(RecordKind::Metadata) {
            return Err(RecordError::DuplicateMetadata(index + 1));
        }
    }
    let version = first
        .payload
        .get("protocol_version")
        .and_then(Value::as_str)
        .ok_or(RecordError::MetadataVersionNotString)?
        .parse::<WireVersion>()?;
    if version == CURRENT_WIRE_VERSION {
        Ok(WireCompatibility::Current)
    } else if version.major == CURRENT_WIRE_VERSION.major && version < CURRENT_WIRE_VERSION {
        Ok(WireCompatibility::NeedsMigration { from: version })
    } else if version > CURRENT_WIRE_VERSION {
        Ok(WireCompatibility::Newer { found: version })
    } else {
        Err(RecordError::UnsupportedProtocolVersion(version))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownRecordKind(pub String);

impl fmt::Display for UnknownRecordKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown record type {:?}", self.0)
    }
}

impl std::error::Error for UnknownRecordKind {}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("agent record log is empty")]
    EmptyLog,
    #[error("metadata record must be first")]
    MetadataNotFirst,
    #[error("metadata record repeated at line {0}")]
    DuplicateMetadata(usize),
    #[error("record type must not be empty")]
    EmptyRecordType,
    #[error("record {record_type:?} is missing required field {field:?}")]
    MissingField {
        record_type: String,
        field: &'static str,
    },
    #[error("metadata protocol_version must be a string")]
    MetadataVersionNotString,
    #[error("metadata created_at must be a number")]
    MetadataCreatedAtNotNumber,
    #[error("invalid protocol version {0:?}")]
    InvalidProtocolVersion(String),
    #[error("unsupported protocol version {0}")]
    UnsupportedProtocolVersion(WireVersion),
}
