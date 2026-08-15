use mycel_cli::{
    terminal::{Cursor, InputDecoder, VirtualTerminal},
    tui::{
        FrameKind, HistoryEntry, InputMode, LogicalAction, QueuedInput, SessionPhase,
        SessionReducer, SubmissionMode, ToolFrameStatus, TranscriptEvent, TranscriptFrame,
        TranscriptReducer,
    },
};
use serde_json::{json, Value};

#[test]
fn raw_key_sequences_match_editor_session_state_corpus() {
    let cases: Value =
        serde_json::from_str(include_str!("../fixtures/parity/key-editor-cases.json"))
            .expect("key fixture JSON");
    for case in cases.as_array().expect("case array") {
        let name = string(case, "name");
        let initial = &case["initial"];
        let mut reducer = SessionReducer {
            phase: parse_phase(
                initial
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or("idle"),
            ),
            input_mode: parse_input_mode(
                initial
                    .get("input_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("prompt"),
            ),
            ..SessionReducer::default()
        };
        if let Some(text) = initial.get("text").and_then(Value::as_str) {
            reducer.editor.replace_without_undo(text);
        }
        if let Some(history) = initial.get("history").and_then(Value::as_array) {
            reducer.editor = mycel_cli::tui::EditorState::with_history(
                history
                    .iter()
                    .map(|entry| HistoryEntry {
                        text: entry.as_str().expect("history string").to_owned(),
                    })
                    .collect(),
            );
        }
        if let Some(queue) = initial.get("queue").and_then(Value::as_array) {
            reducer.queue = queue.iter().map(parse_queue).collect();
        }

        let mut decoder = InputDecoder::default();
        for chunk in case["chunks"].as_array().expect("chunks") {
            for event in decoder.feed(chunk.as_str().expect("chunk string").as_bytes()) {
                reducer.apply(event);
            }
        }
        for event in decoder.flush() {
            reducer.apply(event);
        }

        let actual = json!({
            "text": reducer.editor.text(),
            "phase": phase_name(reducer.phase),
            "input_mode": input_mode_name(reducer.input_mode),
            "queue": reducer.queue.iter().map(queue_json).collect::<Vec<_>>(),
            "actions": reducer.actions.iter().map(action_json).collect::<Vec<_>>(),
        });
        assert_eq!(actual, case["expected"], "key parity case: {name}");
    }
}

#[test]
fn event_traces_match_logical_transcript_frame_corpus() {
    let cases: Value = serde_json::from_str(include_str!(
        "../fixtures/parity/event-transcript-cases.json"
    ))
    .expect("event fixture JSON");
    for case in cases.as_array().expect("case array") {
        let name = string(case, "name");
        let mut reducer = TranscriptReducer::default();
        for action in case["actions"].as_array().expect("actions") {
            let at = action["at"].as_u64().expect("timestamp");
            if action.get("tick").and_then(Value::as_bool) == Some(true) {
                reducer.tick(at);
            } else {
                reducer.push(parse_transcript_event(&action["event"]), at);
            }
        }
        reducer.finish();
        let actual: Vec<Value> = reducer.frames().iter().map(frame_json).collect();
        assert_eq!(
            Value::Array(actual),
            case["expected"],
            "event parity case: {name}"
        );
    }
}

#[test]
fn ansi_bytes_match_viewport_cursor_golden_corpus() {
    let cases: Value =
        serde_json::from_str(include_str!("../fixtures/parity/ansi-viewport-cases.json"))
            .expect("ANSI fixture JSON");
    for case in cases.as_array().expect("case array") {
        let name = string(case, "name");
        let width = case["width"].as_u64().expect("width") as usize;
        let height = case["height"].as_u64().expect("height") as usize;
        let mut terminal = VirtualTerminal::new(width, height);
        for chunk in case["chunks"].as_array().expect("chunks") {
            let bytes = fixture_bytes(chunk);
            terminal.feed(&bytes);
        }
        let expected_lines: Vec<String> = case["expected_lines"]
            .as_array()
            .expect("expected lines")
            .iter()
            .map(|line| line.as_str().expect("line string").to_owned())
            .collect();
        assert_eq!(terminal.lines(), expected_lines, "ANSI parity case: {name}");
        let expected_cursor = Cursor {
            row: case["expected_cursor"]["row"].as_u64().expect("row") as usize,
            column: case["expected_cursor"]["column"].as_u64().expect("column") as usize,
        };
        assert_eq!(
            terminal.cursor(),
            expected_cursor,
            "ANSI cursor case: {name}"
        );
    }
}

fn fixture_bytes(value: &Value) -> Vec<u8> {
    if let Some(text) = value.as_str() {
        return text.as_bytes().to_vec();
    }
    let hex = value["hex"].as_str().expect("chunk string or hex object");
    assert_eq!(hex.len() % 2, 0, "hex chunk must have complete bytes");
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex byte"))
        .collect()
}

fn parse_transcript_event(value: &Value) -> TranscriptEvent {
    match string(value, "type") {
        "turn.started" => TranscriptEvent::TurnStarted,
        "turn.ended" => TranscriptEvent::TurnEnded,
        "step.completed" => TranscriptEvent::StepCompleted,
        "thinking.delta" => TranscriptEvent::ThinkingDelta(string(value, "text").to_owned()),
        "assistant.delta" => TranscriptEvent::AssistantDelta(string(value, "text").to_owned()),
        "tool.started" => TranscriptEvent::ToolStarted {
            id: string(value, "id").to_owned(),
            name: string(value, "name").to_owned(),
            preview: value
                .get("preview")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "tool.progress" => TranscriptEvent::ToolProgress {
            id: string(value, "id").to_owned(),
            text: string(value, "text").to_owned(),
        },
        "tool.result" => TranscriptEvent::ToolResult {
            id: string(value, "id").to_owned(),
            output: string(value, "output").to_owned(),
            failed: value
                .get("failed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "hook.result" => TranscriptEvent::HookResult {
            name: string(value, "name").to_owned(),
            content: string(value, "content").to_owned(),
            blocked: value
                .get("blocked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "retrying" => TranscriptEvent::Retrying {
            failed_attempt: value["failed_attempt"].as_u64().expect("failed attempt") as u32,
            next_attempt: value["next_attempt"].as_u64().expect("next attempt") as u32,
        },
        "subagent.state" => TranscriptEvent::SubagentState {
            id: string(value, "id").to_owned(),
            name: string(value, "name").to_owned(),
            state: string(value, "state").to_owned(),
            detail: value
                .get("detail")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "background.state" => TranscriptEvent::BackgroundTaskState {
            id: string(value, "id").to_owned(),
            kind: string(value, "kind").to_owned(),
            state: string(value, "state").to_owned(),
            description: string(value, "description").to_owned(),
        },
        "goal.state" => TranscriptEvent::GoalState {
            status: string(value, "status").to_owned(),
            objective: string(value, "objective").to_owned(),
            detail: value
                .get("detail")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "mcp.state" => TranscriptEvent::McpServerState {
            name: string(value, "name").to_owned(),
            status: string(value, "status").to_owned(),
            detail: value
                .get("detail")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "compaction.started" => TranscriptEvent::CompactionStarted {
            instruction: value
                .get("instruction")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "compaction.completed" => TranscriptEvent::CompactionCompleted {
            tokens_before: value["tokens_before"].as_u64().expect("tokens before"),
            tokens_after: value["tokens_after"].as_u64().expect("tokens after"),
            summary: string(value, "summary").to_owned(),
        },
        "compaction.cancelled" => TranscriptEvent::CompactionCancelled,
        "compaction.blocked" => TranscriptEvent::CompactionBlocked {
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        other => panic!("unknown transcript event: {other}"),
    }
}

fn frame_json(frame: &TranscriptFrame) -> Value {
    let mut value = json!({
        "kind": match frame.kind {
            FrameKind::User => "user",
            FrameKind::Thinking => "thinking",
            FrameKind::Assistant => "assistant",
            FrameKind::Tool => "tool",
            FrameKind::Hook => "hook",
            FrameKind::Status => "status",
            FrameKind::Subagent => "subagent",
            FrameKind::BackgroundTask => "background_task",
            FrameKind::Goal => "goal",
            FrameKind::Mcp => "mcp",
            FrameKind::Compaction => "compaction",
        },
        "text": frame.text,
        "streaming": frame.streaming,
    });
    if let Some(tool_id) = &frame.tool_id {
        value["tool_id"] = json!(tool_id);
    }
    if let Some(status) = frame.tool_status {
        value["tool_status"] = json!(match status {
            ToolFrameStatus::Running => "running",
            ToolFrameStatus::Completed => "completed",
            ToolFrameStatus::Failed => "failed",
        });
    }
    if let Some(entity_id) = &frame.entity_id {
        value["entity_id"] = json!(entity_id);
    }
    if let Some(state) = &frame.state {
        value["state"] = json!(state);
    }
    value
}

fn action_json(action: &LogicalAction) -> Value {
    match action {
        LogicalAction::Submit(input) => json!({
            "kind": "submit",
            "text": input.text,
            "mode": submission_mode_name(input.mode),
        }),
        LogicalAction::Newline => json!({ "kind": "newline" }),
        LogicalAction::Cancel => json!({ "kind": "cancel" }),
        LogicalAction::Clear => json!({ "kind": "clear" }),
        LogicalAction::Queue(input) => json!({
            "kind": "queue",
            "text": input.text,
            "mode": submission_mode_name(input.mode),
        }),
        LogicalAction::Steer(messages) => json!({ "kind": "steer", "messages": messages }),
        LogicalAction::Detach => json!({ "kind": "detach" }),
        LogicalAction::TogglePlan(enabled) => json!({ "kind": "toggle_plan", "enabled": enabled }),
        LogicalAction::PasteMedia => json!({ "kind": "paste_media" }),
        LogicalAction::ExitArmed => json!({ "kind": "exit_armed" }),
    }
}

fn parse_queue(value: &Value) -> QueuedInput {
    QueuedInput {
        text: string(value, "text").to_owned(),
        mode: parse_submission_mode(string(value, "mode")),
    }
}

fn queue_json(value: &QueuedInput) -> Value {
    json!({ "text": value.text, "mode": submission_mode_name(value.mode) })
}

fn parse_phase(value: &str) -> SessionPhase {
    match value {
        "busy" => SessionPhase::Busy,
        "compacting" => SessionPhase::Compacting,
        "shell" => SessionPhase::Shell,
        _ => SessionPhase::Idle,
    }
}

fn phase_name(value: SessionPhase) -> &'static str {
    match value {
        SessionPhase::Idle => "idle",
        SessionPhase::Busy => "busy",
        SessionPhase::Compacting => "compacting",
        SessionPhase::Shell => "shell",
    }
}

fn parse_input_mode(value: &str) -> InputMode {
    if value == "shell" {
        InputMode::Shell
    } else {
        InputMode::Prompt
    }
}

fn input_mode_name(value: InputMode) -> &'static str {
    match value {
        InputMode::Prompt => "prompt",
        InputMode::Shell => "shell",
    }
}

fn parse_submission_mode(value: &str) -> SubmissionMode {
    if value == "shell" {
        SubmissionMode::Shell
    } else {
        SubmissionMode::Prompt
    }
}

fn submission_mode_name(value: SubmissionMode) -> &'static str {
    match value {
        SubmissionMode::Prompt => "prompt",
        SubmissionMode::Shell => "shell",
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing string {key}"))
}
