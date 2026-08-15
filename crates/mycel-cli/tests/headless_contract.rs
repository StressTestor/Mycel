use std::collections::HashMap;

use clap::Parser;
use mycel_cli::{
    cli::{Cli, InteractiveRequest, PromptRequest},
    execute,
    exit::{
        GoalStatus, TerminationSignal, GOAL_BLOCKED, GOAL_PAUSED, SIGHUP, SIGINT, SIGQUIT, SIGTERM,
    },
    headless::{HeadlessEvent, HeadlessEventSink, RetryMetadata},
    runtime::{
        AdapterOutput, RuntimeAdapter, RuntimeAdapterError, RuntimeCompletion, RuntimeRequest,
    },
};
use serde_json::{json, Value};

struct ScriptedRuntime {
    events: Vec<HeadlessEvent>,
    completion: RuntimeCompletion,
}

impl RuntimeAdapter for ScriptedRuntime {
    fn run_interactive(
        &mut self,
        _request: &InteractiveRequest,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError> {
        Ok(self.completion.clone())
    }

    fn run_prompt(
        &mut self,
        _request: &PromptRequest,
        events: &mut dyn HeadlessEventSink,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError> {
        for event in std::mem::take(&mut self.events) {
            events.emit(event).map_err(|error| {
                RuntimeAdapterError::failed("headless event", error.to_string())
            })?;
        }
        Ok(self.completion.clone())
    }

    fn run_command(
        &mut self,
        _request: RuntimeRequest,
    ) -> Result<AdapterOutput, RuntimeAdapterError> {
        Ok(AdapterOutput::success("", ""))
    }
}

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("CLI should parse")
}

#[test]
fn text_renderer_separates_thinking_and_assistant_and_hides_tools() {
    let mut runtime = ScriptedRuntime {
        events: vec![
            HeadlessEvent::ThinkingDelta("reason\nline".to_owned()),
            HeadlessEvent::AssistantDelta("answer".to_owned()),
            HeadlessEvent::ToolCall {
                id: "call-1".to_owned(),
                name: "Read".to_owned(),
                arguments: json!({ "path": "README.md" }),
            },
            HeadlessEvent::StepCompleted,
            HeadlessEvent::ToolResult {
                id: "call-1".to_owned(),
                output: json!("contents"),
            },
        ],
        completion: RuntimeCompletion::success_with_session("session-1"),
    };
    let result = execute(
        parse(&["mycel", "--prompt", "hello"]),
        &HashMap::new(),
        &mut runtime,
    )
    .expect("scripted prompt");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "• answer\n\n");
    assert_eq!(
        result.stderr,
        "• reason\n  line\n\nTo resume this session: mycel -r session-1\n"
    );
}

#[test]
fn stream_json_discards_failed_partial_and_emits_openai_tool_messages() {
    let retry = RetryMetadata {
        failed_attempt: 1,
        next_attempt: 2,
        max_attempts: 3,
        delay_ms: 250,
        error_name: "RateLimitError".to_owned(),
        error_message: "slow down".to_owned(),
        status_code: Some(429),
    };
    let mut runtime = ScriptedRuntime {
        events: vec![
            HeadlessEvent::AssistantDelta("failed partial".to_owned()),
            HeadlessEvent::Retrying(retry),
            HeadlessEvent::AssistantDelta("final".to_owned()),
            HeadlessEvent::ToolCallDelta {
                id: "call-7".to_owned(),
                name: Some("Read".to_owned()),
                arguments_part: Some("{\"path\":".to_owned()),
            },
            HeadlessEvent::ToolCallDelta {
                id: "call-7".to_owned(),
                name: None,
                arguments_part: Some("\"README.md\"}".to_owned()),
            },
            HeadlessEvent::ToolResult {
                id: "call-7".to_owned(),
                output: json!({ "ok": true }),
            },
        ],
        completion: RuntimeCompletion::success_with_session("session-7"),
    };
    let result = execute(
        parse(&[
            "mycel",
            "--prompt",
            "hello",
            "--output-format",
            "stream-json",
        ]),
        &HashMap::new(),
        &mut runtime,
    )
    .expect("scripted prompt");

    assert!(result.stderr.is_empty());
    let lines: Vec<Value> = result
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL"))
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["type"], "turn.step.retrying");
    assert_eq!(lines[0]["status_code"], 429);
    assert_eq!(lines[1]["role"], "assistant");
    assert_eq!(lines[1]["content"], "final");
    assert_eq!(lines[1]["tool_calls"][0]["function"]["name"], "Read");
    assert_eq!(
        lines[1]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"README.md\"}"
    );
    assert_eq!(lines[2]["role"], "tool");
    assert_eq!(lines[2]["content"], "{\"ok\":true}");
    assert_eq!(lines[3]["type"], "session.resume_hint");
    assert!(!result.stdout.contains("failed partial"));
}

#[test]
fn hook_results_and_goal_status_use_stable_render_and_exit_contracts() {
    let mut runtime = ScriptedRuntime {
        events: vec![
            HeadlessEvent::HookResult {
                hook_event: "PreToolUse".to_owned(),
                content: " denied ".to_owned(),
                blocked: true,
            },
            HeadlessEvent::GoalSummary {
                goal_id: Some("goal-1".to_owned()),
                status: Some("blocked".to_owned()),
                reason: Some("needs input".to_owned()),
                turns_used: Some(4),
                tokens_used: Some(100),
                wall_clock_ms: Some(500),
            },
        ],
        completion: RuntimeCompletion::Goal {
            status: GoalStatus::Blocked,
            session_id: Some("session-goal".to_owned()),
        },
    };
    let result = execute(
        parse(&["mycel", "--prompt", "/goal do work"]),
        &HashMap::new(),
        &mut runtime,
    )
    .expect("scripted goal");

    assert_eq!(result.exit_code, GOAL_BLOCKED);
    assert_eq!(result.stdout, "• PreToolUse hook blocked\n\n  denied\n\n");
    assert_eq!(
        result.stderr,
        "Goal [blocked]: needs input (turns: 4, tokens: 100)\nTo resume this session: mycel -r session-goal\n"
    );
}

/// The shipped catch-all gate hook returns `{}` on every allowed tool call.
/// That is a successful hook with nothing to say: it must not manufacture an
/// assistant turn reading "PreToolUse hook (empty)" per tool use. Blocked
/// results and results carrying a message (warns) still render.
#[test]
fn successful_hooks_with_no_message_are_not_rendered() {
    let mut runtime = ScriptedRuntime {
        events: vec![
            HeadlessEvent::HookResult {
                hook_event: "PreToolUse".to_owned(),
                content: String::new(),
                blocked: false,
            },
            HeadlessEvent::HookResult {
                hook_event: "PostToolUse".to_owned(),
                content: "   ".to_owned(),
                blocked: false,
            },
            HeadlessEvent::HookResult {
                hook_event: "PreToolUse".to_owned(),
                content: "mycel warn: prefer --force-with-lease".to_owned(),
                blocked: false,
            },
            HeadlessEvent::HookResult {
                hook_event: "PreToolUse".to_owned(),
                content: String::new(),
                blocked: true,
            },
        ],
        completion: RuntimeCompletion::success_with_session("session-hooks"),
    };
    let result = execute(
        parse(&["mycel", "--prompt", "run tools"]),
        &HashMap::new(),
        &mut runtime,
    )
    .expect("scripted hooks");

    assert!(
        !result.stdout.contains("PreToolUse hook\n\n  (empty)")
            && !result.stdout.contains("PostToolUse hook\n\n  (empty)"),
        "silent successful hooks leaked into output: {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("prefer --force-with-lease"),
        "warn message must still render: {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("PreToolUse hook blocked"),
        "blocked hook must still render even without a message: {:?}",
        result.stdout
    );
    assert_eq!(
        result.stdout.matches("hook").count(),
        2,
        "exactly the warn and the block should render: {:?}",
        result.stdout
    );
}

#[test]
fn all_nonstandard_completion_codes_are_stable() {
    assert_eq!(GoalStatus::Blocked.exit_code(), GOAL_BLOCKED);
    assert_eq!(GoalStatus::Paused.exit_code(), GOAL_PAUSED);
    assert_eq!(TerminationSignal::Hangup.exit_code(), SIGHUP);
    assert_eq!(TerminationSignal::Interrupt.exit_code(), SIGINT);
    assert_eq!(TerminationSignal::Quit.exit_code(), SIGQUIT);
    assert_eq!(TerminationSignal::Terminate.exit_code(), SIGTERM);
}
