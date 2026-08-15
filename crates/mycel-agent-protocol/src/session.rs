use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::OptionalNullable;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Main,
    Sub,
    Independent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homedir: Option<String>,
    #[serde(rename = "type")]
    pub agent_type: AgentType,
    #[serde(default, skip_serializing_if = "OptionalNullable::is_missing")]
    pub parent_agent_id: OptionalNullable<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_item: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub created_at: String,
    pub updated_at: String,
    pub title: String,
    pub is_custom_title: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_dirs: Vec<String>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentMeta>,
    #[serde(default)]
    pub custom: BTreeMap<String, Value>,
}

impl SessionMeta {
    pub fn validate(&self) -> Result<(), SessionContractError> {
        if self.created_at.trim().is_empty() || self.updated_at.trim().is_empty() {
            return Err(SessionContractError::MissingTimestamp);
        }
        if self.title.trim().is_empty() {
            return Err(SessionContractError::EmptyTitle);
        }
        for (id, agent) in &self.agents {
            if id.trim().is_empty() {
                return Err(SessionContractError::EmptyAgentId);
            }
            if agent
                .parent_agent_id
                .value()
                .is_some_and(|parent| parent == id)
            {
                return Err(SessionContractError::SelfParent(id.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionWarningSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWarning {
    pub code: String,
    pub message: String,
    pub severity: SessionWarningSeverity,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub work_dir: String,
    pub session_dir: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_dirs: Vec<String>,
}

impl SessionSummary {
    pub fn validate(&self) -> Result<(), SessionContractError> {
        validate_session_id(&self.id)?;
        if self.work_dir.trim().is_empty() {
            return Err(SessionContractError::EmptyWorkDir);
        }
        if self.session_dir.trim().is_empty() {
            return Err(SessionContractError::EmptySessionDir);
        }
        Ok(())
    }
}

/// Append-only session-index line. Deletions are tombstones, not removals,
/// which makes interrupted rewrites recoverable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionIndexLine {
    Entry(SessionIndexEntry),
    Deletion(SessionIndexDeletion),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub session_dir: String,
    pub work_dir: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexDeletion {
    pub session_id: String,
    pub deleted: TrueMarker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrueMarker;

impl Serialize for TrueMarker {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for TrueMarker {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("deleted marker must be true"))
        }
    }
}

pub fn validate_session_id(id: &str) -> Result<(), SessionContractError> {
    if id.trim().is_empty() {
        return Err(SessionContractError::EmptySessionId);
    }
    if id == "." || id == ".." || id.contains('/') || id.contains('\\') || id.contains('\0') {
        return Err(SessionContractError::UnsafeSessionId(id.to_owned()));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionContractError {
    #[error("session timestamps must not be empty")]
    MissingTimestamp,
    #[error("session title must not be empty")]
    EmptyTitle,
    #[error("agent id must not be empty")]
    EmptyAgentId,
    #[error("agent {0:?} cannot be its own parent")]
    SelfParent(String),
    #[error("session id must not be empty")]
    EmptySessionId,
    #[error("unsafe session id {0:?}")]
    UnsafeSessionId(String),
    #[error("work directory must not be empty")]
    EmptyWorkDir,
    #[error("session directory must not be empty")]
    EmptySessionDir,
}
