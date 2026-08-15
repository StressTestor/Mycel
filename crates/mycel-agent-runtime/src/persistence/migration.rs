use std::collections::BTreeMap;

use mycel_agent_protocol::{AgentRecord, WireVersion, CURRENT_WIRE_VERSION};
#[cfg(test)]
use serde_json::Map;
use serde_json::Value;

/// Applies the durable v1.0 through v1.4 record migrations without executing
/// runtime behavior. The caller rewrites only after replay succeeds.
pub fn migrate_records(
    records: &[AgentRecord],
    from: WireVersion,
) -> Result<Vec<AgentRecord>, MigrationError> {
    if from == CURRENT_WIRE_VERSION {
        return Ok(records.to_vec());
    }
    if from.major != CURRENT_WIRE_VERSION.major || from > CURRENT_WIRE_VERSION {
        return Err(MigrationError::UnsupportedVersion(from));
    }

    let mut migrated = records.to_vec();
    let mut version = from;
    while version < CURRENT_WIRE_VERSION {
        migrated = match (version.major, version.minor) {
            (1, 0) => migrated.into_iter().map(migrate_v1_0_to_v1_1).collect(),
            (1, 1) => migrated.into_iter().map(migrate_v1_1_to_v1_2).collect(),
            (1, 2) => migrated,
            (1, 3) => migrated.into_iter().map(migrate_v1_3_to_v1_4).collect(),
            _ => return Err(MigrationError::MissingStep(version)),
        };
        version.minor += 1;
    }

    let metadata = migrated
        .first_mut()
        .ok_or(MigrationError::MissingMetadata)?;
    metadata.payload.insert(
        "protocol_version".to_owned(),
        Value::String(CURRENT_WIRE_VERSION.to_string()),
    );
    Ok(migrated)
}

fn migrate_v1_0_to_v1_1(mut record: AgentRecord) -> AgentRecord {
    if record.record_type != "context.append_message" {
        return record;
    }
    let Some(message) = record
        .payload
        .get_mut("message")
        .and_then(Value::as_object_mut)
    else {
        return record;
    };
    let Some(tool_calls) = message.get_mut("toolCalls").and_then(Value::as_array_mut) else {
        return record;
    };
    for call in tool_calls {
        let Some(call) = call.as_object_mut() else {
            continue;
        };
        let Some(function) = call
            .remove("function")
            .and_then(|value| value.as_object().cloned())
        else {
            continue;
        };
        if let Some(name) = function.get("name") {
            call.insert("name".to_owned(), name.clone());
        }
        if let Some(arguments) = function.get("arguments") {
            call.insert("arguments".to_owned(), arguments.clone());
        }
    }
    record
}

fn migrate_v1_1_to_v1_2(mut record: AgentRecord) -> AgentRecord {
    if record.record_type != "permission.record_approval_result"
        || record.payload.contains_key("sessionApprovalRule")
    {
        return record;
    }
    let approved_for_session = record
        .payload
        .get("result")
        .and_then(Value::as_object)
        .is_some_and(|result| {
            result.get("decision").and_then(Value::as_str) == Some("approved")
                && result.get("scope").and_then(Value::as_str) == Some("session")
        });
    if !approved_for_session {
        return record;
    }
    let action = record
        .payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let pattern = match action {
        "run command in plan mode" | "run background command" => None,
        "run command" => Some("Bash"),
        "stop background task" => Some("TaskStop"),
        "edit file" | "edit file outside of working directory" | "write file" => Some("Write"),
        _ => record.payload.get("toolName").and_then(Value::as_str),
    };
    if let Some(pattern) = pattern {
        record.payload.insert(
            "sessionApprovalRule".to_owned(),
            Value::String(pattern.to_owned()),
        );
    }
    record
}

fn migrate_v1_3_to_v1_4(record: AgentRecord) -> AgentRecord {
    match record.record_type.as_str() {
        "goal.create" => select_payload(
            record,
            "goal.create",
            &["goalId", "objective", "completionCriterion"],
        ),
        "goal.update" => select_payload(
            record,
            "goal.update",
            &[
                "status",
                "reason",
                "turnsUsed",
                "tokensUsed",
                "wallClockMs",
                "actor",
            ],
        ),
        "goal.account_usage" => {
            select_payload(record, "goal.update", &["tokensUsed", "wallClockMs"])
        }
        "goal.continuation" => select_payload(record, "goal.update", &["turnsUsed"]),
        "goal.clear" => select_payload(record, "goal.clear", &[]),
        _ => record,
    }
}

fn select_payload(mut record: AgentRecord, record_type: &str, fields: &[&str]) -> AgentRecord {
    let mut payload = BTreeMap::new();
    for field in fields {
        if let Some(value) = record.payload.remove(*field) {
            payload.insert((*field).to_owned(), value);
        }
    }
    record.record_type = record_type.to_owned();
    record.payload = payload;
    record
}

/// Converts a flattened record to a JSON object. Kept private to this module's
/// tests so migration fixtures can be expressed in the original wire shape.
#[cfg(test)]
fn record_from_object(mut object: Map<String, Value>) -> AgentRecord {
    let record_type = object
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .expect("fixture type");
    let time = object.remove("time").and_then(|value| value.as_u64());
    AgentRecord {
        record_type,
        time,
        payload: object.into_iter().collect(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MigrationError {
    #[error("record log has no metadata record")]
    MissingMetadata,
    #[error("unsupported wire version {0}")]
    UnsupportedVersion(WireVersion),
    #[error("missing migration step from wire version {0}")]
    MissingStep(WireVersion),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn metadata(version: &str) -> AgentRecord {
        record_from_object(
            json!({"type":"metadata","protocol_version":version,"created_at":1})
                .as_object()
                .expect("object")
                .clone(),
        )
    }

    #[test]
    fn migration_flattens_calls_and_restores_only_safe_approvals() {
        let message = record_from_object(
            json!({
                "type":"context.append_message",
                "message": {
                    "role":"assistant",
                    "content":[],
                    "toolCalls":[{"type":"function","id":"c1","function":{"name":"Bash","arguments":"{}"}}]
                }
            })
            .as_object()
            .expect("object")
            .clone(),
        );
        let approval = record_from_object(
            json!({
                "type":"permission.record_approval_result",
                "turnId":1,
                "toolCallId":"c1",
                "toolName":"Bash",
                "action":"run command",
                "result":{"decision":"approved","scope":"session"}
            })
            .as_object()
            .expect("object")
            .clone(),
        );
        let migrated = migrate_records(
            &[metadata("1.0"), message, approval],
            WireVersion { major: 1, minor: 0 },
        )
        .expect("migration");

        assert_eq!(
            migrated[1].payload["message"]["toolCalls"][0]["name"],
            "Bash"
        );
        assert!(migrated[1].payload["message"]["toolCalls"][0]
            .get("function")
            .is_none());
        assert_eq!(migrated[2].payload["sessionApprovalRule"], "Bash");
        assert_eq!(migrated[0].payload["protocol_version"], "1.4");
    }

    #[test]
    fn migration_never_broadens_background_command_approval() {
        let approval = record_from_object(
            json!({
                "type":"permission.record_approval_result",
                "turnId":1,
                "toolCallId":"c1",
                "toolName":"Bash",
                "action":"run background command",
                "result":{"decision":"approved","scope":"session"}
            })
            .as_object()
            .expect("object")
            .clone(),
        );
        let migrated = migrate_records(
            &[metadata("1.1"), approval],
            WireVersion { major: 1, minor: 1 },
        )
        .expect("migration");
        assert!(!migrated[1].payload.contains_key("sessionApprovalRule"));
    }

    #[test]
    fn v1_4_goal_migration_removes_redundant_ids_and_normalizes_usage() {
        let create = record_from_object(
            json!({
                "type":"goal.create",
                "goalId":"g1",
                "objective":"ship",
                "completionCriterion":"tests pass",
                "legacy":true
            })
            .as_object()
            .expect("object")
            .clone(),
        );
        let usage = record_from_object(
            json!({
                "type":"goal.account_usage",
                "goalId":"g1",
                "tokensUsed":12,
                "wallClockMs":34
            })
            .as_object()
            .expect("object")
            .clone(),
        );
        let migrated = migrate_records(
            &[metadata("1.3"), create, usage],
            WireVersion { major: 1, minor: 3 },
        )
        .expect("migration");
        assert!(!migrated[1].payload.contains_key("legacy"));
        assert_eq!(migrated[2].record_type, "goal.update");
        assert!(!migrated[2].payload.contains_key("goalId"));
        assert_eq!(migrated[2].payload["tokensUsed"], 12);
    }
}
