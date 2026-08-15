use std::{fs, path::PathBuf};

use mycel_agent_protocol::PermissionMode;
use mycel_agent_runtime::{
    CommandHookFailMode, HookMatcher, HookRegistration, HookRunner, Runtime, SessionId,
    SessionOptions, ToolHookEvent,
};
use serde_json::Value;

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mycel-lifecycle-{}",
            mycel_agent_runtime::RequestId::generate()
        ));
        fs::create_dir(&path).expect("temporary root");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn configured_hooks(cwd: &std::path::Path, capture: &std::path::Path) -> HookRunner {
    let runner = HookRunner::new();
    let command = format!(
        "{{ cat; printf '\\n'; }} >> {}",
        shell_quote(&capture.to_string_lossy())
    );
    for (event, matcher) in [
        (ToolHookEvent::SessionStart, "startup|resume"),
        (ToolHookEvent::SessionEnd, "^exit$"),
    ] {
        runner
            .register(HookRegistration {
                event,
                matcher: HookMatcher::tool_name_regex(matcher).expect("matcher"),
                command: command.clone(),
                cwd: cwd.to_owned(),
                timeout: None,
                fail_mode: CommandHookFailMode::Closed,
            })
            .expect("register hook");
    }
    runner
}

fn options(id: &SessionId, hooks: HookRunner) -> SessionOptions {
    let mut options = SessionOptions::new(id.clone());
    options.initial_permission_mode = PermissionMode::Manual;
    options.hooks = hooks;
    options
}

#[tokio::test]
async fn session_start_resume_and_end_hooks_are_dispatched_with_flattened_payloads() {
    let root = TempRoot::new();
    let capture = root.0.join("events.jsonl");
    let runtime = Runtime::new(root.0.join("sessions"));
    let id = SessionId::new("hook-session").expect("session id");

    let session = runtime
        .create_session(options(&id, configured_hooks(&root.0, &capture)))
        .await
        .expect("create session");
    session.close().await.expect("close created session");
    let resumed = runtime
        .resume_session(options(&id, configured_hooks(&root.0, &capture)))
        .await
        .expect("resume session");
    resumed.close().await.expect("close resumed session");

    let events: Vec<Value> = fs::read_to_string(&capture)
        .expect("capture")
        .lines()
        .map(|line| serde_json::from_str(line).expect("hook JSON"))
        .collect();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["hook_event_name"], "SessionStart");
    assert_eq!(events[0]["source"], "startup");
    assert_eq!(events[1]["hook_event_name"], "SessionEnd");
    assert_eq!(events[1]["reason"], "exit");
    assert_eq!(events[2]["source"], "resume");
    assert_eq!(events[3]["reason"], "exit");
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
