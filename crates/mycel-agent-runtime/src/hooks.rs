use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};

use mycel_agent_protocol::ExecutableToolResult;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

use crate::{AgentId, CancellationToken, SessionId, ToolCallId};

const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum ToolHookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionResult,
    UserPromptSubmit,
    Stop,
    StopFailure,
    Interrupt,
    SessionStart,
    SessionEnd,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Notification,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookMatcher {
    Any,
    ToolNameGlob(String),
    ToolNameRegex(CompiledToolNameRegex),
    ContentContains(String),
}

impl HookMatcher {
    /// Compiles a JavaScript-style tool-name matcher eagerly. Rust's regex
    /// engine intentionally rejects look-around and backreferences rather
    /// than accepting a pattern it cannot apply faithfully.
    pub fn tool_name_regex(pattern: impl Into<String>) -> Result<Self, HookRunnerError> {
        let pattern = pattern.into();
        let compiled = Regex::new(&pattern).map_err(|error| HookRunnerError::InvalidMatcher {
            pattern: pattern.clone(),
            message: error.to_string(),
        })?;
        Ok(Self::ToolNameRegex(CompiledToolNameRegex {
            pattern,
            compiled,
        }))
    }

    fn matches_values(&self, matcher_value: &str, content: &str) -> bool {
        match self {
            Self::Any => true,
            Self::ToolNameGlob(pattern) => glob_matches(pattern, matcher_value),
            Self::ToolNameRegex(regex) => regex.compiled.is_match(matcher_value),
            Self::ContentContains(needle) => content.contains(needle),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompiledToolNameRegex {
    pattern: String,
    compiled: Regex,
}

impl CompiledToolNameRegex {
    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

impl PartialEq for CompiledToolNameRegex {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
    }
}

impl Eq for CompiledToolNameRegex {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandHookFailMode {
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRegistration {
    pub event: ToolHookEvent,
    pub matcher: HookMatcher,
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Option<Duration>,
    pub fail_mode: CommandHookFailMode,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolHookInput {
    pub hook_event_name: ToolHookEvent,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub turn_id: u64,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub arguments: Value,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ExecutableToolResult>,
}

/// Generic input for hooks that are not tied to a tool call. Additional
/// fields are flattened into the stable JSON object consumed on hook stdin.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LifecycleHookInput {
    pub hook_event_name: ToolHookEvent,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(skip)]
    pub matcher_value: String,
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
}

impl LifecycleHookInput {
    pub fn new(hook_event_name: ToolHookEvent, session_id: SessionId, agent_id: AgentId) -> Self {
        Self {
            hook_event_name,
            session_id,
            agent_id,
            turn_id: None,
            matcher_value: String::new(),
            fields: BTreeMap::new(),
        }
    }

    pub fn with_matcher_value(mut self, matcher_value: impl Into<String>) -> Self {
        self.matcher_value = matcher_value.into();
        self
    }

    pub fn with_turn_id(mut self, turn_id: u64) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    pub fn insert_field(&mut self, name: impl Into<String>, value: Value) {
        self.fields.insert(name.into(), value);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookBlock {
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookExecution {
    pub command: String,
    pub cwd: PathBuf,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub message: Option<String>,
    pub blocked: Option<HookBlock>,
    pub timed_out: bool,
    pub aborted: bool,
    pub output_truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookRunReport {
    pub executions: Vec<HookExecution>,
    pub messages: Vec<String>,
    pub blocked: Option<HookBlock>,
}

#[derive(Clone, Default)]
pub struct HookRunner {
    registrations: Arc<RwLock<Vec<HookRegistration>>>,
}

impl HookRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, registration: HookRegistration) -> Result<(), HookRunnerError> {
        if registration.command.trim().is_empty() {
            return Err(HookRunnerError::EmptyCommand);
        }
        write_lock(&self.registrations).push(registration);
        Ok(())
    }

    pub fn replace_all(&self, registrations: Vec<HookRegistration>) -> Result<(), HookRunnerError> {
        if registrations
            .iter()
            .any(|registration| registration.command.trim().is_empty())
        {
            return Err(HookRunnerError::EmptyCommand);
        }
        *write_lock(&self.registrations) = registrations;
        Ok(())
    }

    pub async fn run(
        &self,
        event: ToolHookEvent,
        input: &ToolHookInput,
        cancellation: &CancellationToken,
    ) -> HookRunReport {
        // The stdin contract with PreToolUse guards (mycel-gate; ARCHITECTURE
        // "gate data flow") is {tool_name, tool_input, cwd}. Mirror `arguments`
        // as `tool_input` here, at the one choke point every tool hook passes
        // through, so no input constructor can silently drop the guard's view.
        let value = serde_json::to_value(input).map(|mut value| {
            if let Some(map) = value.as_object_mut() {
                if !map.contains_key("tool_input") {
                    map.insert("tool_input".to_owned(), input.arguments.clone());
                }
            }
            value
        });
        self.run_serialized(event, &input.tool_name, &input.content, value, cancellation)
            .await
    }

    pub async fn run_lifecycle(
        &self,
        input: &LifecycleHookInput,
        cancellation: &CancellationToken,
    ) -> HookRunReport {
        self.run_serialized(
            input.hook_event_name,
            &input.matcher_value,
            &input.matcher_value,
            serde_json::to_value(input),
            cancellation,
        )
        .await
    }

    async fn run_serialized(
        &self,
        event: ToolHookEvent,
        matcher_value: &str,
        content: &str,
        value: Result<Value, serde_json::Error>,
        cancellation: &CancellationToken,
    ) -> HookRunReport {
        let registrations = read_lock(&self.registrations).clone();
        let mut deduplicated: Vec<EffectiveHook> = Vec::new();
        let mut indexes: BTreeMap<(PathBuf, String), usize> = BTreeMap::new();
        for registration in registrations.into_iter().filter(|registration| {
            registration.event == event
                && registration.matcher.matches_values(matcher_value, content)
        }) {
            let key = (registration.cwd.clone(), registration.command.clone());
            if let Some(index) = indexes.get(&key).copied() {
                let effective = &mut deduplicated[index];
                if registration.fail_mode == CommandHookFailMode::Closed {
                    effective.fail_mode = CommandHookFailMode::Closed;
                }
                effective.timeout = effective
                    .timeout
                    .min(registration.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT));
                continue;
            }
            indexes.insert(key, deduplicated.len());
            deduplicated.push(EffectiveHook {
                command: registration.command,
                cwd: registration.cwd,
                timeout: registration.timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT),
                fail_mode: registration.fail_mode,
            });
        }

        let value = match value {
            Ok(value) => value,
            Err(error) => {
                return HookRunReport {
                    executions: vec![HookExecution::failure(
                        "<serialize>".to_owned(),
                        PathBuf::new(),
                        format!("hook input serialization failed: {error}"),
                        true,
                    )],
                    messages: Vec::new(),
                    blocked: Some(HookBlock {
                        reason: "hook input serialization failed".to_owned(),
                    }),
                };
            }
        };
        let mut tasks = Vec::with_capacity(deduplicated.len());
        let mut failures: Vec<HookExecution> = Vec::new();
        for hook in deduplicated {
            // Each hook sees the payload with `cwd` set to its registration cwd
            // (the agent's cwd in production, and the directory the hook process
            // itself runs in), so guards can resolve relative write targets.
            match encode_hook_payload(&value, &hook.cwd) {
                Ok(encoded) => {
                    let input = Arc::new(encoded);
                    let cancellation = cancellation.clone();
                    tasks.push(tokio::spawn(async move {
                        execute_hook(hook, input, cancellation).await
                    }));
                }
                Err(error) => failures.push(HookExecution::failure(
                    hook.command,
                    hook.cwd,
                    format!("hook input serialization failed: {error}"),
                    true,
                )),
            }
        }

        let mut report = HookRunReport::default();
        let mut executions = failures;
        for task in tasks {
            executions.push(match task.await {
                Ok(execution) => execution,
                Err(error) => HookExecution::failure(
                    "<hook-task>".to_owned(),
                    PathBuf::new(),
                    format!("hook task failed: {error}"),
                    true,
                ),
            });
        }
        for execution in executions {
            if let Some(message) = &execution.message {
                report.messages.push(message.clone());
            }
            if report.blocked.is_none() {
                report.blocked = execution.blocked.clone();
            }
            report.executions.push(execution);
        }
        report
    }
}

#[derive(Clone)]
struct EffectiveHook {
    command: String,
    cwd: PathBuf,
    timeout: Duration,
    fail_mode: CommandHookFailMode,
}

/// Finalize the stdin payload for one hook: set `cwd` (unless the input already
/// carries one) and encode. `Value` serialization cannot realistically fail,
/// but a failure here still fails closed at the call site.
fn encode_hook_payload(value: &Value, cwd: &Path) -> Result<Vec<u8>, serde_json::Error> {
    let mut payload = value.clone();
    if let Some(map) = payload.as_object_mut() {
        if !map.contains_key("cwd") {
            map.insert(
                "cwd".to_owned(),
                Value::String(cwd.to_string_lossy().into_owned()),
            );
        }
    }
    serde_json::to_vec(&payload)
}

async fn execute_hook(
    hook: EffectiveHook,
    input: Arc<Vec<u8>>,
    cancellation: CancellationToken,
) -> HookExecution {
    if cancellation.is_cancelled() {
        return HookExecution::aborted(hook.command, hook.cwd);
    }

    let mut command = shell_command(&hook.command);
    command
        .current_dir(&hook.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HookExecution::failure(
                hook.command,
                hook.cwd,
                format!("hook spawn failed: {error}"),
                hook.fail_mode == CommandHookFailMode::Closed,
            );
        }
    };

    let mut stdin = child.stdin.take();
    let stdin_bytes = Arc::clone(&input);
    let stdin_task = tokio::spawn(async move {
        if let Some(mut stdin) = stdin.take() {
            stdin.write_all(&stdin_bytes).await?;
            stdin.shutdown().await?;
        }
        Ok::<(), std::io::Error>(())
    });
    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(read_bounded(stdout)));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(read_bounded(stderr)));

    enum WaitResult {
        Status(std::process::ExitStatus),
        Failed(String),
        TimedOut,
        Aborted,
    }
    // The wait task owns the child so aborting it drops a kill-on-drop child.
    // Keeping the process and its wait future in different branches would
    // otherwise require two simultaneous mutable borrows of `Child`.
    let mut wait_task = tokio::spawn(async move { child.wait().await });
    let wait_result = tokio::select! {
        _ = cancellation.cancelled() => {
            wait_task.abort();
            let _ = wait_task.await;
            WaitResult::Aborted
        }
        _ = tokio::time::sleep(hook.timeout) => {
            wait_task.abort();
            let _ = wait_task.await;
            WaitResult::TimedOut
        }
        result = &mut wait_task => match result {
            Ok(Ok(status)) => WaitResult::Status(status),
            Ok(Err(error)) => WaitResult::Failed(format!("hook wait failed: {error}")),
            Err(error) => WaitResult::Failed(format!("hook wait task failed: {error}")),
        },
    };

    let stdin_error = match stdin_task.await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(error) => Some(error.to_string()),
    };
    let (stdout, stdout_truncated) = join_capture(stdout_task).await;
    let (stderr, stderr_truncated) = join_capture(stderr_task).await;
    let truncated = stdout_truncated || stderr_truncated;

    match wait_result {
        WaitResult::Aborted => HookExecution {
            command: hook.command,
            cwd: hook.cwd,
            exit_code: None,
            stdout,
            stderr,
            message: None,
            blocked: None,
            timed_out: false,
            aborted: true,
            output_truncated: truncated,
        },
        WaitResult::TimedOut => HookExecution {
            command: hook.command,
            cwd: hook.cwd,
            exit_code: None,
            stdout,
            stderr,
            message: None,
            blocked: (hook.fail_mode == CommandHookFailMode::Closed).then(|| HookBlock {
                reason: "hook timed out".to_owned(),
            }),
            timed_out: true,
            aborted: false,
            output_truncated: truncated,
        },
        WaitResult::Failed(message) => {
            HookExecution::with_capture_failure(hook, stdout, stderr, message, truncated)
        }
        WaitResult::Status(status) => {
            if let Some(error) = stdin_error {
                return HookExecution::with_capture_failure(
                    hook,
                    stdout,
                    stderr,
                    format!("hook stdin failed: {error}"),
                    truncated,
                );
            }
            interpret_status(hook, status, stdout, stderr, truncated)
        }
    }
}

fn interpret_status(
    hook: EffectiveHook,
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    truncated: bool,
) -> HookExecution {
    let code = status.code();
    let block = if code == Some(2) {
        Some(HookBlock {
            reason: nonempty_or(&stderr, "hook denied the tool"),
        })
    } else if !status.success() && hook.fail_mode == CommandHookFailMode::Closed {
        Some(HookBlock {
            reason: nonempty_or(&stderr, "hook failed closed"),
        })
    } else {
        None
    };
    if !status.success() {
        return HookExecution {
            command: hook.command,
            cwd: hook.cwd,
            exit_code: code,
            stdout,
            stderr,
            message: None,
            blocked: block,
            timed_out: false,
            aborted: false,
            output_truncated: truncated,
        };
    }

    let trimmed = stdout.trim();
    let structured = if trimmed.starts_with('{') {
        serde_json::from_str::<StructuredHookOutput>(trimmed).ok()
    } else {
        None
    };
    let message = structured
        .as_ref()
        .and_then(|output| output.message.clone())
        .or_else(|| (!trimmed.is_empty() && structured.is_none()).then(|| trimmed.to_owned()));
    let structured_block = structured.as_ref().and_then(|output| {
        output.denied().then(|| HookBlock {
            reason: output
                .deny_reason()
                .unwrap_or_else(|| "hook denied the tool".to_owned()),
        })
    });
    HookExecution {
        command: hook.command,
        cwd: hook.cwd,
        exit_code: code,
        stdout,
        stderr,
        message,
        blocked: structured_block,
        timed_out: false,
        aborted: false,
        output_truncated: truncated,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StructuredHookOutput {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    permission_decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    // PreToolUse guards (mycel-gate) nest the decision under
    // hookSpecificOutput; both shapes must block or the gate is fail-open.
    #[serde(default)]
    hook_specific_output: Option<HookDecisionOutput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookDecisionOutput {
    #[serde(default)]
    permission_decision: Option<String>,
    #[serde(default)]
    permission_decision_reason: Option<String>,
}

impl StructuredHookOutput {
    fn denied(&self) -> bool {
        self.permission_decision.as_deref() == Some("deny")
            || self
                .hook_specific_output
                .as_ref()
                .is_some_and(|output| output.permission_decision.as_deref() == Some("deny"))
    }

    fn deny_reason(&self) -> Option<String> {
        self.reason.clone().or_else(|| {
            self.hook_specific_output
                .as_ref()
                .and_then(|output| output.permission_decision_reason.clone())
        })
    }
}

impl HookExecution {
    fn failure(command: String, cwd: PathBuf, reason: String, block: bool) -> Self {
        Self {
            command,
            cwd,
            exit_code: None,
            stdout: String::new(),
            stderr: reason.clone(),
            message: None,
            blocked: block.then_some(HookBlock { reason }),
            timed_out: false,
            aborted: false,
            output_truncated: false,
        }
    }

    fn aborted(command: String, cwd: PathBuf) -> Self {
        Self {
            command,
            cwd,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            message: None,
            blocked: None,
            timed_out: false,
            aborted: true,
            output_truncated: false,
        }
    }

    fn with_capture_failure(
        hook: EffectiveHook,
        stdout: String,
        stderr: String,
        reason: String,
        truncated: bool,
    ) -> Self {
        Self {
            command: hook.command,
            cwd: hook.cwd,
            exit_code: None,
            stdout,
            stderr,
            message: None,
            blocked: (hook.fail_mode == CommandHookFailMode::Closed)
                .then_some(HookBlock { reason }),
            timed_out: false,
            aborted: false,
            output_truncated: truncated,
        }
    }
}

async fn read_bounded<R>(mut reader: R) -> (String, bool)
where
    R: AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(remaining)]);
                truncated |= count > remaining;
            }
            Err(error) => {
                let suffix = format!("\n<hook output read error: {error}>");
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&suffix.as_bytes()[..suffix.len().min(remaining)]);
                truncated = true;
                break;
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

async fn join_capture(task: Option<tokio::task::JoinHandle<(String, bool)>>) -> (String, bool) {
    match task {
        Some(task) => task
            .await
            .unwrap_or_else(|error| (format!("<hook output task failed: {error}>"), true)),
        None => (String::new(), false),
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut process = Command::new("cmd.exe");
        process.arg("/D").arg("/S").arg("/C").arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("/bin/sh");
        process.arg("-c").arg(command);
        process
    }
}

fn nonempty_or(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        match token {
            '*' => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            '?' => current[1..].copy_from_slice(&previous[..value.len()]),
            literal => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] == literal;
                }
            }
        }
        previous = current;
    }
    previous[value.len()]
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HookRunnerError {
    #[error("hook command must not be empty")]
    EmptyCommand,
    #[error("invalid hook matcher {pattern:?}: {message}")]
    InvalidMatcher { pattern: String, message: String },
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "mycel-hook-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn input(event: ToolHookEvent) -> ToolHookInput {
        ToolHookInput {
            hook_event_name: event,
            session_id: SessionId::new("s1").expect("session"),
            agent_id: AgentId::main(),
            turn_id: 1,
            tool_call_id: ToolCallId::new("c1").expect("call"),
            tool_name: "Read".to_owned(),
            arguments: json!({"path":"README.md"}),
            content: "README.md".to_owned(),
            result: None,
        }
    }

    #[test]
    fn tool_name_regex_matches_javascript_style_alternation_and_rejects_invalid_patterns() {
        let matcher = HookMatcher::tool_name_regex("Shell|WriteFile").expect("compile matcher");
        let mut hook_input = input(ToolHookEvent::PreToolUse);
        hook_input.tool_name = "Shell".to_owned();
        assert!(matcher.matches_values(&hook_input.tool_name, &hook_input.content));
        hook_input.tool_name = "WriteFile".to_owned();
        assert!(matcher.matches_values(&hook_input.tool_name, &hook_input.content));
        hook_input.tool_name = "Read".to_owned();
        assert!(!matcher.matches_values(&hook_input.tool_name, &hook_input.content));
        assert!(matches!(
            HookMatcher::tool_name_regex("[unterminated"),
            Err(HookRunnerError::InvalidMatcher { .. })
        ));
    }

    #[tokio::test]
    async fn exit_two_blocks_even_when_fail_open() {
        let cwd = temp_dir();
        tokio::fs::create_dir_all(&cwd).await.expect("mkdir");
        let runner = HookRunner::new();
        runner
            .register(HookRegistration {
                event: ToolHookEvent::PreToolUse,
                matcher: HookMatcher::Any,
                command: "printf denied >&2; exit 2".to_owned(),
                cwd: cwd.clone(),
                timeout: None,
                fail_mode: CommandHookFailMode::Open,
            })
            .expect("register");
        let report = runner
            .run(
                ToolHookEvent::PreToolUse,
                &input(ToolHookEvent::PreToolUse),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(report.blocked.expect("blocked").reason, "denied");
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }

    #[tokio::test]
    async fn duplicate_commands_run_once_and_fail_closed_wins() {
        let cwd = temp_dir();
        tokio::fs::create_dir_all(&cwd).await.expect("mkdir");
        let runner = HookRunner::new();
        for fail_mode in [CommandHookFailMode::Open, CommandHookFailMode::Closed] {
            runner
                .register(HookRegistration {
                    event: ToolHookEvent::PreToolUse,
                    matcher: HookMatcher::ToolNameGlob("R*".to_owned()),
                    command: "exit 1".to_owned(),
                    cwd: cwd.clone(),
                    timeout: None,
                    fail_mode,
                })
                .expect("register");
        }
        let report = runner
            .run(
                ToolHookEvent::PreToolUse,
                &input(ToolHookEvent::PreToolUse),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(report.executions.len(), 1);
        assert!(report.blocked.is_some());
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }

    #[tokio::test]
    async fn tool_hook_stdin_carries_tool_input_and_cwd() {
        let cwd = temp_dir();
        tokio::fs::create_dir_all(&cwd).await.expect("mkdir");
        let runner = HookRunner::new();
        runner
            .register(HookRegistration {
                event: ToolHookEvent::PreToolUse,
                matcher: HookMatcher::Any,
                command: "cat > captured.json".to_owned(),
                cwd: cwd.clone(),
                timeout: None,
                fail_mode: CommandHookFailMode::Closed,
            })
            .expect("register");
        let report = runner
            .run(
                ToolHookEvent::PreToolUse,
                &input(ToolHookEvent::PreToolUse),
                &CancellationToken::new(),
            )
            .await;
        assert!(report.blocked.is_none());

        let captured = tokio::fs::read_to_string(cwd.join("captured.json"))
            .await
            .expect("hook stdin captured");
        let payload: Value = serde_json::from_str(&captured).expect("payload is JSON");
        assert_eq!(
            payload["tool_input"]["path"], "README.md",
            "stdin must mirror arguments under tool_input (the PreToolUse guard contract)"
        );
        assert_eq!(
            payload["cwd"],
            cwd.to_string_lossy().as_ref(),
            "stdin must carry the agent cwd for relative write-target resolution"
        );
        assert_eq!(
            payload["arguments"]["path"], "README.md",
            "the native arguments field must remain for existing consumers"
        );
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }

    #[tokio::test]
    async fn nested_hook_specific_output_deny_blocks() {
        let cwd = temp_dir();
        tokio::fs::create_dir_all(&cwd).await.expect("mkdir");
        let runner = HookRunner::new();
        runner
            .register(HookRegistration {
                event: ToolHookEvent::PreToolUse,
                matcher: HookMatcher::Any,
                command: concat!(
                    "printf '%s' '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\",",
                    "\"permissionDecisionReason\":\"floor says no\"}}'"
                )
                .to_owned(),
                cwd: cwd.clone(),
                timeout: None,
                fail_mode: CommandHookFailMode::Open,
            })
            .expect("register");
        let report = runner
            .run(
                ToolHookEvent::PreToolUse,
                &input(ToolHookEvent::PreToolUse),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(
            report.blocked.expect("nested deny must block").reason,
            "floor says no"
        );
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }
}
