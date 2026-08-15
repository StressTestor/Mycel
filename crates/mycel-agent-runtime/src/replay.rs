use std::collections::{BTreeMap, BTreeSet};

use mycel_agent_protocol::{
    validate_record_sequence, AgentRecord, ApprovalDecision, ApprovalScope, LoopEvent,
    PermissionApprovalResultRecord, PermissionMode, RecordClass, RecordKind, TokenUsage,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{CanonicalContext, CompactionRecord, ContextEntry, ContextError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactionState {
    #[default]
    Idle,
    Running,
    Completed,
}

/// All state rebuilt by the pure record reducer in the first runtime tranche.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentState {
    pub context: CanonicalContext,
    pub permission_mode: PermissionMode,
    pub session_approval_rules: BTreeSet<String>,
    pub turn_sequence: u64,
    pub plan_mode: bool,
    pub swarm_mode: bool,
    pub config: BTreeMap<String, Value>,
    pub active_tools: BTreeSet<String>,
    pub user_tools: BTreeMap<String, BTreeMap<String, Value>>,
    pub tool_store: BTreeMap<String, Value>,
    pub usage_by_model: BTreeMap<String, TokenUsage>,
    pub compaction: CompactionState,
    pub goal: Option<BTreeMap<String, Value>>,
    pub last_tools_snapshot_hash: Option<String>,
    pub mcp_discovery_hashes: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    ObservabilityCursor,
    IgnoredFutureRecord,
    IgnoredLegacyNoOp,
}

impl AgentState {
    /// Performs trust-boundary validation that can fail before a record is
    /// appended. `apply` remains authoritative, but a session never knowingly
    /// writes an event its current state cannot reduce.
    pub fn validate_apply(&self, record: &AgentRecord) -> Result<(), ReplayError> {
        record.validate().map_err(ReplayError::InvalidSequence)?;
        match record.kind() {
            Some(RecordKind::ContextAppendMessage) => {
                let entry: ContextEntry = required(record, "message")?;
                self.context.validate_message(&entry)?;
            }
            Some(RecordKind::ContextAppendLoopEvent) => {
                let event: LoopEvent = required(record, "event")?;
                self.context.validate_loop_event(&event)?;
            }
            Some(RecordKind::PermissionSetMode) => {
                let _: PermissionMode = required(record, "mode")?;
            }
            Some(RecordKind::PermissionRecordApprovalResult) => {
                let _: PermissionApprovalResultRecord = payload(record)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Applies exactly one record to memory. This method has no I/O or callback
    /// parameters by design, making replay incapable of external side effects.
    pub fn apply(&mut self, record: &AgentRecord) -> Result<ApplyOutcome, ReplayError> {
        let Some(kind) = record.kind() else {
            return Ok(ApplyOutcome::IgnoredFutureRecord);
        };
        if kind == RecordKind::MicroCompactionApply {
            return Ok(ApplyOutcome::IgnoredLegacyNoOp);
        }
        if kind.class() == RecordClass::Observability {
            match kind {
                RecordKind::LlmToolsSnapshot => {
                    self.last_tools_snapshot_hash = Some(required_string(record, "hash")?);
                }
                RecordKind::McpToolsDiscovered => {
                    self.mcp_discovery_hashes.insert(
                        required_string(record, "serverName")?,
                        required_string(record, "hash")?,
                    );
                }
                RecordKind::LlmRequest => {}
                _ => unreachable!("record class is exhaustive"),
            }
            return Ok(ApplyOutcome::ObservabilityCursor);
        }

        match kind {
            RecordKind::Metadata => {}
            RecordKind::Forked => self.goal = None,
            RecordKind::TurnPrompt | RecordKind::TurnSteer => {
                self.turn_sequence = self.turn_sequence.saturating_add(1);
            }
            RecordKind::TurnCancel => {
                if let Some(turn_id) = optional_u64(record, "turnId")? {
                    self.turn_sequence = self.turn_sequence.max(turn_id);
                }
            }
            RecordKind::ConfigUpdate => {
                self.config.extend(record.payload.clone());
            }
            RecordKind::PermissionSetMode => {
                self.permission_mode = required(record, "mode")?;
            }
            RecordKind::PermissionRecordApprovalResult => {
                let approval: PermissionApprovalResultRecord = payload(record)?;
                if approval.result.decision == ApprovalDecision::Approved
                    && approval.result.scope == Some(ApprovalScope::Session)
                {
                    if let Some(rule) = approval.session_approval_rule {
                        self.session_approval_rules.insert(rule);
                    }
                }
                self.turn_sequence = self.turn_sequence.max(approval.turn_id);
            }
            RecordKind::FullCompactionBegin => self.compaction = CompactionState::Running,
            RecordKind::FullCompactionCancel => self.compaction = CompactionState::Idle,
            RecordKind::FullCompactionComplete => self.compaction = CompactionState::Completed,
            RecordKind::PlanModeEnter => {
                let plan_file = record
                    .payload
                    .contains_key("planFile")
                    .then(|| required::<Option<String>>(record, "planFile"))
                    .transpose()?;
                self.plan_mode = true;
                match plan_file {
                    Some(Some(path)) => {
                        self.tool_store
                            .insert("plan_file".to_owned(), Value::String(path));
                    }
                    Some(None) => {
                        self.tool_store.remove("plan_file");
                    }
                    None => {}
                }
            }
            RecordKind::PlanModeCancel | RecordKind::PlanModeExit => self.plan_mode = false,
            RecordKind::SwarmModeEnter => self.swarm_mode = true,
            RecordKind::SwarmModeExit => self.swarm_mode = false,
            RecordKind::ToolsRegisterUserTool => {
                let name = required_string(record, "name")?;
                self.user_tools.insert(name, record.payload.clone());
            }
            RecordKind::ToolsUnregisterUserTool => {
                self.user_tools.remove(&required_string(record, "name")?);
            }
            RecordKind::ToolsSetActiveTools => {
                self.active_tools = required::<Vec<String>>(record, "names")?
                    .into_iter()
                    .collect();
            }
            RecordKind::ToolsUpdateStore => {
                self.tool_store.insert(
                    required_string(record, "key")?,
                    record
                        .payload
                        .get("value")
                        .cloned()
                        .ok_or_else(|| missing(record, "value"))?,
                );
            }
            RecordKind::UsageRecord => {
                let model = required_string(record, "model")?;
                let usage: TokenUsage = required(record, "usage")?;
                self.usage_by_model
                    .entry(model)
                    .and_modify(|current| *current = current.saturating_add(usage))
                    .or_insert(usage);
            }
            RecordKind::ContextAppendMessage => {
                let entry: ContextEntry = required(record, "message")?;
                self.context.append_message(entry)?;
            }
            RecordKind::ContextAppendLoopEvent => {
                let event: LoopEvent = required(record, "event")?;
                observe_turn_id(&mut self.turn_sequence, &event);
                self.context.append_loop_event(&event)?;
            }
            RecordKind::ContextUpdateTokenCount => {
                self.context
                    .update_token_count(required(record, "tokenCount")?);
            }
            RecordKind::ContextClear => self.context.clear(),
            RecordKind::ContextApplyCompaction => {
                let summary = record
                    .payload
                    .get("contextSummary")
                    .or_else(|| record.payload.get("summary"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| missing(record, "summary"))?
                    .to_owned();
                self.context.apply_compaction(CompactionRecord {
                    context_summary: summary,
                    compacted_count: required::<u64>(record, "compactedCount")?
                        .try_into()
                        .unwrap_or(usize::MAX),
                    tokens_after: required(record, "tokensAfter")?,
                    kept_user_message_count: optional_u64(record, "keptUserMessageCount")?
                        .and_then(|value| value.try_into().ok()),
                    kept_head_user_message_count: optional_u64(record, "keptHeadUserMessageCount")?
                        .and_then(|value| value.try_into().ok()),
                })?;
            }
            RecordKind::ContextUndo => {
                let count = required::<u64>(record, "count")?
                    .try_into()
                    .unwrap_or(usize::MAX);
                self.context.undo(count);
            }
            RecordKind::GoalCreate => self.goal = Some(record.payload.clone()),
            RecordKind::GoalUpdate => {
                if let Some(goal) = &mut self.goal {
                    goal.extend(record.payload.clone());
                }
            }
            RecordKind::GoalClear => self.goal = None,
            RecordKind::MicroCompactionApply
            | RecordKind::LlmToolsSnapshot
            | RecordKind::LlmRequest
            | RecordKind::McpToolsDiscovered => unreachable!("handled above"),
        }
        Ok(ApplyOutcome::Applied)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayResult {
    pub state: AgentState,
    pub applied_records: usize,
    pub observability_records: usize,
    pub ignored_records: usize,
}

pub fn replay_records(records: &[AgentRecord]) -> Result<ReplayResult, ReplayError> {
    validate_record_sequence(records).map_err(ReplayError::InvalidSequence)?;
    let mut state = AgentState::default();
    let mut applied_records = 0usize;
    let mut observability_records = 0usize;
    let mut ignored_records = 0usize;
    for record in records {
        match state.apply(record)? {
            ApplyOutcome::Applied => applied_records += 1,
            ApplyOutcome::ObservabilityCursor => observability_records += 1,
            ApplyOutcome::IgnoredFutureRecord | ApplyOutcome::IgnoredLegacyNoOp => {
                ignored_records += 1;
            }
        }
    }
    state.context.validate_tail_invariant()?;
    Ok(ReplayResult {
        state,
        applied_records,
        observability_records,
        ignored_records,
    })
}

fn observe_turn_id(turn_sequence: &mut u64, event: &LoopEvent) {
    let turn_id = match event {
        LoopEvent::StepBegin { turn_id, .. }
        | LoopEvent::StepEnd { turn_id, .. }
        | LoopEvent::ContentPart { turn_id, .. }
        | LoopEvent::ToolCall { turn_id, .. }
        | LoopEvent::StepRetrying { turn_id, .. } => Some(turn_id),
        _ => None,
    };
    if let Some(turn_id) = turn_id.and_then(|value| value.parse::<u64>().ok()) {
        *turn_sequence = (*turn_sequence).max(turn_id);
    }
}

fn payload<T: DeserializeOwned>(record: &AgentRecord) -> Result<T, ReplayError> {
    serde_json::from_value(Value::Object(record.payload.clone().into_iter().collect())).map_err(
        |source| ReplayError::InvalidPayload {
            record_type: record.record_type.clone(),
            message: source.to_string(),
        },
    )
}

fn required<T: DeserializeOwned>(record: &AgentRecord, field: &str) -> Result<T, ReplayError> {
    serde_json::from_value(
        record
            .payload
            .get(field)
            .cloned()
            .ok_or_else(|| missing(record, field))?,
    )
    .map_err(|source| ReplayError::InvalidField {
        record_type: record.record_type.clone(),
        field: field.to_owned(),
        message: source.to_string(),
    })
}

fn required_string(record: &AgentRecord, field: &str) -> Result<String, ReplayError> {
    required(record, field)
}

fn optional_u64(record: &AgentRecord, field: &str) -> Result<Option<u64>, ReplayError> {
    record
        .payload
        .get(field)
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|source| ReplayError::InvalidField {
                record_type: record.record_type.clone(),
                field: field.to_owned(),
                message: source.to_string(),
            })
        })
        .transpose()
}

fn missing(record: &AgentRecord, field: &str) -> ReplayError {
    ReplayError::MissingField {
        record_type: record.record_type.clone(),
        field: field.to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("invalid record sequence: {0}")]
    InvalidSequence(mycel_agent_protocol::RecordError),
    #[error("record {record_type:?} is missing field {field:?}")]
    MissingField { record_type: String, field: String },
    #[error("invalid field {field:?} in record {record_type:?}: {message}")]
    InvalidField {
        record_type: String,
        field: String,
        message: String,
    },
    #[error("invalid payload in record {record_type:?}: {message}")]
    InvalidPayload {
        record_type: String,
        message: String,
    },
    #[error(transparent)]
    Context(#[from] ContextError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mycel_agent_protocol::{RecordKind, CURRENT_WIRE_VERSION};
    use serde_json::json;

    use super::*;

    fn record(kind: RecordKind, payload: Value) -> AgentRecord {
        AgentRecord {
            record_type: kind.as_str().to_owned(),
            time: Some(1),
            payload: payload
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        }
    }

    fn metadata() -> AgentRecord {
        record(
            RecordKind::Metadata,
            json!({"protocol_version":CURRENT_WIRE_VERSION.to_string(),"created_at":1}),
        )
    }

    #[test]
    fn replay_is_deterministic_and_restores_permission_and_context() {
        let records = vec![
            metadata(),
            record(RecordKind::PermissionSetMode, json!({"mode":"yolo"})),
            record(
                RecordKind::ContextAppendMessage,
                json!({"message": {
                    "role":"user",
                    "content":[{"type":"text","text":"hello"}],
                    "toolCalls":[],
                    "origin":{"kind":"user"}
                }}),
            ),
            record(
                RecordKind::ContextUpdateTokenCount,
                json!({"tokenCount":42}),
            ),
        ];
        let first = replay_records(&records).expect("first replay");
        let second = replay_records(&records).expect("second replay");
        assert_eq!(first, second);
        assert_eq!(first.state.permission_mode, PermissionMode::Yolo);
        assert_eq!(first.state.context.history().len(), 1);
        assert_eq!(first.state.context.token_count(), 42);
    }

    #[test]
    fn replay_advances_turn_sequence_from_internal_loop_events() {
        let records = vec![
            metadata(),
            record(
                RecordKind::ContextAppendLoopEvent,
                json!({"event": {
                    "type":"step.begin",
                    "uuid":"step",
                    "turnId":"9",
                    "step":1
                }}),
            ),
        ];
        let result = replay_records(&records).expect("replay");
        assert_eq!(result.state.turn_sequence, 9);
    }

    #[test]
    fn future_records_are_ignored_without_data_loss_errors() {
        let mut future = BTreeMap::new();
        future.insert("data".to_owned(), json!(true));
        let result = replay_records(&[
            metadata(),
            AgentRecord {
                record_type: "future.record".to_owned(),
                time: Some(2),
                payload: future,
            },
        ])
        .expect("replay");
        assert_eq!(result.ignored_records, 1);
    }
}
