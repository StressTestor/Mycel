use std::{collections::BTreeSet, path::PathBuf};

use mycel_agent_protocol::{
    validate_record_sequence, validate_session_id, AgentErrorCode, AgentRecord, ApprovalRequest,
    ApprovalResponse, Event, ExecutableToolResult, LoopContentPart, LoopEvent, McpConfigFile,
    Message, MycelConfig, MycelConfigPatch, PermissionApprovalResultRecord, ProviderRequest,
    ProviderStreamEvent, RecordKind, SessionIndexLine, SessionMeta, SessionSummary,
    StreamAssembler, TokenUsage, ToolDefinition, ToolInputDisplay, WireCompatibility,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

const FIXTURE_FILES: &[&str] = &[
    "domain-cases.json",
    "provider-stream-cases.json",
    "event-cases.json",
    "config-cases.json",
    "permission-loop-cases.json",
    "session-cases.json",
    "record-cases.json",
];

fn fixture_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/parity")
        .join(file)
}

fn load_fixture(file: &str) -> Value {
    let path = fixture_path(file);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn decode_exact<T>(value: &Value, context: &str) -> T
where
    T: DeserializeOwned + serde::Serialize,
{
    let decoded: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|error| panic!("failed to decode {context}: {error}"));
    let encoded = serde_json::to_value(&decoded)
        .unwrap_or_else(|error| panic!("failed to encode {context}: {error}"));
    assert_eq!(encoded, *value, "lossy round trip for {context}");
    decoded
}

fn cases<'a>(fixture: &'a Value, field: &str) -> &'a [Value] {
    fixture[field]
        .as_array()
        .unwrap_or_else(|| panic!("fixture field {field:?} must be an array"))
}

#[test]
fn authoritative_parity_fixture_families_exist() {
    for file in FIXTURE_FILES {
        assert!(
            fixture_path(file).is_file(),
            "missing parity fixture {file}"
        );
    }
}

#[test]
fn message_tool_and_usage_contracts_round_trip() {
    let fixture = load_fixture("domain-cases.json");

    for (index, value) in cases(&fixture, "messages").iter().enumerate() {
        let message: Message = decode_exact(value, &format!("message[{index}]"));
        message
            .validate()
            .unwrap_or_else(|error| panic!("message[{index}] is invalid: {error}"));
    }
    for (index, value) in cases(&fixture, "toolDefinitions").iter().enumerate() {
        let tool: ToolDefinition = decode_exact(value, &format!("toolDefinition[{index}]"));
        tool.validate()
            .unwrap_or_else(|error| panic!("toolDefinition[{index}] is invalid: {error}"));
    }
    for (index, value) in cases(&fixture, "usage").iter().enumerate() {
        let usage: TokenUsage = decode_exact(value, &format!("usage[{index}]"));
        assert!(usage.grand_total() >= usage.output);
    }
}

#[test]
fn malformed_message_tool_and_usage_cases_are_rejected() {
    let fixture = load_fixture("domain-cases.json");

    for case in cases(&fixture, "serdeFailures") {
        let target = case["target"].as_str().expect("serde target");
        let rejected = match target {
            "message" => serde_json::from_value::<Message>(case["value"].clone()).is_err(),
            "tool" => serde_json::from_value::<ToolDefinition>(case["value"].clone()).is_err(),
            "usage" => serde_json::from_value::<TokenUsage>(case["value"].clone()).is_err(),
            other => panic!("unknown serde failure target {other}"),
        };
        assert!(rejected, "{} unexpectedly decoded", case["name"]);
    }

    for case in cases(&fixture, "validationFailures") {
        let target = case["target"].as_str().expect("validation target");
        let error = match target {
            "message" => serde_json::from_value::<Message>(case["value"].clone())
                .expect("validation fixture must decode")
                .validate()
                .expect_err("message should fail validation")
                .to_string(),
            "tool" => serde_json::from_value::<ToolDefinition>(case["value"].clone())
                .expect("validation fixture must decode")
                .validate()
                .expect_err("tool should fail validation")
                .to_string(),
            other => panic!("unknown validation failure target {other}"),
        };
        assert_eq!(error, case["error"].as_str().expect("expected error"));
    }
}

#[test]
fn provider_requests_and_streams_match_normalized_results() {
    let fixture = load_fixture("provider-stream-cases.json");

    for (index, value) in cases(&fixture, "requests").iter().enumerate() {
        let request: ProviderRequest = decode_exact(value, &format!("providerRequest[{index}]"));
        request
            .validate()
            .unwrap_or_else(|error| panic!("providerRequest[{index}] is invalid: {error}"));
    }

    for case in cases(&fixture, "streams") {
        let name = case["name"].as_str().expect("stream name");
        let mut assembler = StreamAssembler::default();
        for (index, value) in case["events"]
            .as_array()
            .expect("stream events")
            .iter()
            .enumerate()
        {
            let event: ProviderStreamEvent =
                decode_exact(value, &format!("{name}.events[{index}]"));
            assembler
                .push(event)
                .unwrap_or_else(|error| panic!("{name}.events[{index}] failed: {error}"));
        }
        let result = assembler
            .finish()
            .unwrap_or_else(|error| panic!("{name} failed to finish: {error}"));
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            case["expected"],
            "{name}"
        );
    }
}

#[test]
fn malformed_provider_streams_fail_at_the_recorded_boundary() {
    let fixture = load_fixture("provider-stream-cases.json");
    for case in cases(&fixture, "failures") {
        let name = case["name"].as_str().expect("failure name");
        let expected = case["error"].as_str().expect("failure error");
        let mut assembler = StreamAssembler::default();
        let mut push_error = None;
        for value in case["events"].as_array().expect("failure events") {
            let event: ProviderStreamEvent = serde_json::from_value(value.clone())
                .unwrap_or_else(|error| panic!("{name} fixture did not decode: {error}"));
            if let Err(error) = assembler.push(event) {
                push_error = Some(error.to_string());
                break;
            }
        }
        let actual = match push_error {
            Some(error) => error,
            None => assembler
                .finish()
                .expect_err("malformed stream unexpectedly finished")
                .to_string(),
        };
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn all_public_event_tags_and_tool_displays_round_trip() {
    const EXPECTED_EVENT_TAGS: &[&str] = &[
        "agent.status.updated",
        "assistant.delta",
        "background.task.started",
        "background.task.terminated",
        "compaction.blocked",
        "compaction.cancelled",
        "compaction.completed",
        "compaction.started",
        "cron.fired",
        "error",
        "goal.updated",
        "hook.result",
        "mcp.server.status",
        "plugin_command.activated",
        "session.meta.updated",
        "shell.output",
        "shell.started",
        "skill.activated",
        "subagent.completed",
        "subagent.failed",
        "subagent.spawned",
        "subagent.started",
        "subagent.suspended",
        "thinking.delta",
        "tool.call.delta",
        "tool.call.started",
        "tool.list.updated",
        "tool.progress",
        "tool.result",
        "turn.ended",
        "turn.started",
        "turn.step.completed",
        "turn.step.interrupted",
        "turn.step.retrying",
        "turn.step.started",
        "warning",
    ];

    let fixture = load_fixture("event-cases.json");
    let mut tags = BTreeSet::new();
    for (index, value) in cases(&fixture, "events").iter().enumerate() {
        let tag = value["type"].as_str().expect("event type");
        assert!(tags.insert(tag), "duplicate event fixture {tag}");
        let _: Event = decode_exact(value, &format!("event[{index}]/{tag}"));
    }
    assert_eq!(tags.into_iter().collect::<Vec<_>>(), EXPECTED_EVENT_TAGS);

    for (index, value) in cases(&fixture, "toolInputDisplays").iter().enumerate() {
        let _: ToolInputDisplay = decode_exact(value, &format!("toolInputDisplay[{index}]"));
    }
}

#[test]
fn malformed_events_are_rejected() {
    let fixture = load_fixture("event-cases.json");
    for case in cases(&fixture, "failures") {
        assert!(
            serde_json::from_value::<Event>(case["value"].clone()).is_err(),
            "{} unexpectedly decoded",
            case["name"]
        );
    }
}

#[test]
fn retained_error_code_golden_is_exhaustive_and_rejects_protocol_bloat() {
    let fixture = load_fixture("event-cases.json");
    let values = cases(&fixture, "errorCodes");
    assert_eq!(values.len(), 58, "retained agent error-code count drifted");

    let mut codes = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let code = value.as_str().expect("error code must be a string");
        assert!(codes.insert(code), "duplicate error code {code}");
        let _: AgentErrorCode = decode_exact(value, &format!("errorCode[{index}]/{code}"));
    }
    assert_eq!(codes.len(), 58);

    for value in cases(&fixture, "obsoleteErrorCodes") {
        assert!(
            serde_json::from_value::<AgentErrorCode>(value.clone()).is_err(),
            "obsolete protocol-only code {value} unexpectedly decoded"
        );
    }
}

#[test]
fn normalized_config_and_mcp_transports_round_trip() {
    let fixture = load_fixture("config-cases.json");
    for (index, value) in cases(&fixture, "mycel").iter().enumerate() {
        let config: MycelConfig = decode_exact(value, &format!("mycelConfig[{index}]"));
        config
            .validate_runtime()
            .unwrap_or_else(|error| panic!("mycelConfig[{index}] is invalid: {error}"));
    }
    for (index, value) in cases(&fixture, "mcp").iter().enumerate() {
        let _: McpConfigFile = decode_exact(value, &format!("mcpConfig[{index}]"));
    }
    for case in cases(&fixture, "patches") {
        let name = case["name"].as_str().expect("patch name");
        let base: MycelConfig = decode_exact(&case["base"], &format!("{name}.base"));
        let patch: MycelConfigPatch = decode_exact(&case["patch"], &format!("{name}.patch"));
        let merged = base
            .merged(patch)
            .unwrap_or_else(|error| panic!("{name} failed to merge: {error}"));
        assert_eq!(
            serde_json::to_value(merged).unwrap(),
            case["expected"],
            "{name}"
        );
    }
}

#[test]
fn malformed_and_semantically_invalid_configs_are_rejected() {
    let fixture = load_fixture("config-cases.json");
    for case in cases(&fixture, "serdeFailures") {
        let rejected = match case["target"].as_str().expect("config target") {
            "mycel" => serde_json::from_value::<MycelConfig>(case["value"].clone()).is_err(),
            "mcp" => serde_json::from_value::<McpConfigFile>(case["value"].clone()).is_err(),
            "patch" => serde_json::from_value::<MycelConfigPatch>(case["value"].clone()).is_err(),
            other => panic!("unknown config target {other}"),
        };
        assert!(rejected, "{} unexpectedly decoded", case["name"]);
    }
    for case in cases(&fixture, "validationFailures") {
        let config: MycelConfig =
            serde_json::from_value(case["value"].clone()).expect("validation config must decode");
        let error = if case["runtimeOnly"].as_bool().unwrap_or(false) {
            config
                .validate_runtime()
                .expect_err("runtime config should fail")
        } else {
            config.validate().expect_err("config should fail")
        };
        assert_eq!(error.to_string(), case["error"].as_str().unwrap());
    }
}

#[test]
fn permission_and_loop_contracts_round_trip() {
    const EXPECTED_LOOP_TAGS: &[&str] = &[
        "content.part",
        "step.begin",
        "step.end",
        "step.retrying",
        "text.delta",
        "thinking.delta",
        "tool.call",
        "tool.call.delta",
        "tool.progress",
        "tool.result",
        "turn.interrupted",
    ];

    let fixture = load_fixture("permission-loop-cases.json");
    for (index, value) in cases(&fixture, "approvalRequests").iter().enumerate() {
        let _: ApprovalRequest = decode_exact(value, &format!("approvalRequest[{index}]"));
    }
    for (index, value) in cases(&fixture, "approvalResponses").iter().enumerate() {
        let _: ApprovalResponse = decode_exact(value, &format!("approvalResponse[{index}]"));
    }
    for (index, value) in cases(&fixture, "approvalRecords").iter().enumerate() {
        let _: PermissionApprovalResultRecord =
            decode_exact(value, &format!("approvalRecord[{index}]"));
    }
    for (index, value) in cases(&fixture, "executableToolResults").iter().enumerate() {
        let _: ExecutableToolResult =
            decode_exact(value, &format!("executableToolResult[{index}]"));
    }
    for (index, value) in cases(&fixture, "loopContentParts").iter().enumerate() {
        let _: LoopContentPart = decode_exact(value, &format!("loopContentPart[{index}]"));
    }

    let mut tags = BTreeSet::new();
    for (index, case) in cases(&fixture, "loopEvents").iter().enumerate() {
        let value = &case["value"];
        let tag = value["type"].as_str().expect("loop event type");
        assert!(tags.insert(tag), "duplicate loop event fixture {tag}");
        let event: LoopEvent = decode_exact(value, &format!("loopEvent[{index}]/{tag}"));
        assert_eq!(
            event.is_recorded(),
            case["recorded"].as_bool().expect("recorded flag"),
            "{tag}"
        );
    }
    assert_eq!(tags.into_iter().collect::<Vec<_>>(), EXPECTED_LOOP_TAGS);
}

#[test]
fn malformed_permission_and_loop_contracts_are_rejected() {
    let fixture = load_fixture("permission-loop-cases.json");
    for case in cases(&fixture, "serdeFailures") {
        let rejected = match case["target"].as_str().expect("contract target") {
            "request" => serde_json::from_value::<ApprovalRequest>(case["value"].clone()).is_err(),
            "response" => {
                serde_json::from_value::<ApprovalResponse>(case["value"].clone()).is_err()
            }
            "record" => {
                serde_json::from_value::<PermissionApprovalResultRecord>(case["value"].clone())
                    .is_err()
            }
            "loop" => serde_json::from_value::<LoopEvent>(case["value"].clone()).is_err(),
            "toolResult" => {
                serde_json::from_value::<ExecutableToolResult>(case["value"].clone()).is_err()
            }
            other => panic!("unknown contract target {other}"),
        };
        assert!(rejected, "{} unexpectedly decoded", case["name"]);
    }
}

#[test]
fn session_metadata_summaries_and_index_lines_round_trip() {
    let fixture = load_fixture("session-cases.json");
    for (index, value) in cases(&fixture, "metadata").iter().enumerate() {
        let metadata: SessionMeta = decode_exact(value, &format!("sessionMeta[{index}]"));
        metadata
            .validate()
            .unwrap_or_else(|error| panic!("sessionMeta[{index}] is invalid: {error}"));
    }
    for (index, value) in cases(&fixture, "summaries").iter().enumerate() {
        let summary: SessionSummary = decode_exact(value, &format!("sessionSummary[{index}]"));
        summary
            .validate()
            .unwrap_or_else(|error| panic!("sessionSummary[{index}] is invalid: {error}"));
    }
    for (index, value) in cases(&fixture, "indexLines").iter().enumerate() {
        let _: SessionIndexLine = decode_exact(value, &format!("sessionIndexLine[{index}]"));
    }
}

#[test]
fn malformed_sessions_and_unsafe_ids_are_rejected() {
    let fixture = load_fixture("session-cases.json");
    for case in cases(&fixture, "serdeFailures") {
        let rejected = match case["target"].as_str().expect("session target") {
            "metadata" => serde_json::from_value::<SessionMeta>(case["value"].clone()).is_err(),
            "summary" => serde_json::from_value::<SessionSummary>(case["value"].clone()).is_err(),
            "index" => serde_json::from_value::<SessionIndexLine>(case["value"].clone()).is_err(),
            other => panic!("unknown session target {other}"),
        };
        assert!(rejected, "{} unexpectedly decoded", case["name"]);
    }
    for case in cases(&fixture, "validationFailures") {
        let target = case["target"].as_str().expect("session validation target");
        let error = match target {
            "metadata" => serde_json::from_value::<SessionMeta>(case["value"].clone())
                .expect("metadata validation fixture must decode")
                .validate()
                .expect_err("metadata should fail")
                .to_string(),
            "summary" => serde_json::from_value::<SessionSummary>(case["value"].clone())
                .expect("summary validation fixture must decode")
                .validate()
                .expect_err("summary should fail")
                .to_string(),
            "id" => validate_session_id(case["value"].as_str().expect("session id"))
                .expect_err("session id should fail")
                .to_string(),
            other => panic!("unknown session validation target {other}"),
        };
        assert_eq!(error, case["error"].as_str().unwrap());
    }
}

#[test]
fn all_durable_record_tags_round_trip_and_validate() {
    const EXPECTED_RECORD_TAGS: &[&str] = &[
        "config.update",
        "context.append_loop_event",
        "context.append_message",
        "context.apply_compaction",
        "context.clear",
        "context.undo",
        "context.update_token_count",
        "forked",
        "full_compaction.begin",
        "full_compaction.cancel",
        "full_compaction.complete",
        "goal.clear",
        "goal.create",
        "goal.update",
        "llm.request",
        "llm.tools_snapshot",
        "mcp.tools_discovered",
        "metadata",
        "micro_compaction.apply",
        "permission.record_approval_result",
        "permission.set_mode",
        "plan_mode.cancel",
        "plan_mode.enter",
        "plan_mode.exit",
        "swarm_mode.enter",
        "swarm_mode.exit",
        "tools.register_user_tool",
        "tools.set_active_tools",
        "tools.unregister_user_tool",
        "tools.update_store",
        "turn.cancel",
        "turn.prompt",
        "turn.steer",
        "usage.record",
    ];

    let fixture = load_fixture("record-cases.json");
    let mut tags = BTreeSet::new();
    for (index, value) in cases(&fixture, "records").iter().enumerate() {
        let record: AgentRecord = decode_exact(value, &format!("record[{index}]"));
        record
            .validate()
            .unwrap_or_else(|error| panic!("record[{index}] is invalid: {error}"));
        assert!(
            tags.insert(record.record_type.clone()),
            "duplicate record tag"
        );
    }
    assert_eq!(
        tags.into_iter().collect::<Vec<_>>(),
        EXPECTED_RECORD_TAGS
            .iter()
            .map(|tag| (*tag).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        RecordKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<BTreeSet<_>>(),
        EXPECTED_RECORD_TAGS.iter().copied().collect()
    );

    let forward: AgentRecord = decode_exact(&fixture["forwardRecord"], "forwardRecord");
    assert_eq!(forward.kind(), None);
    forward
        .validate()
        .expect("future record must remain readable");
}

#[test]
fn record_sequence_compatibility_and_malformed_records_match_the_wire_contract() {
    let fixture = load_fixture("record-cases.json");
    for case in cases(&fixture, "sequences") {
        let records: Vec<AgentRecord> =
            serde_json::from_value(case["records"].clone()).expect("record sequence must decode");
        let actual = validate_record_sequence(&records);
        match case["compatibility"].as_str().expect("compatibility") {
            "current" => assert_eq!(actual.unwrap(), WireCompatibility::Current),
            "migration" => assert!(matches!(
                actual.unwrap(),
                WireCompatibility::NeedsMigration { .. }
            )),
            "newer" => assert!(matches!(actual.unwrap(), WireCompatibility::Newer { .. })),
            "error" => assert_eq!(
                actual.expect_err("sequence should fail").to_string(),
                case["error"].as_str().expect("sequence error")
            ),
            other => panic!("unknown compatibility {other}"),
        }
    }

    for case in cases(&fixture, "recordFailures") {
        let record: AgentRecord = serde_json::from_value(case["value"].clone())
            .expect("record failure fixture must decode");
        assert_eq!(
            record
                .validate()
                .expect_err("record should fail")
                .to_string(),
            case["error"].as_str().unwrap()
        );
    }
}
