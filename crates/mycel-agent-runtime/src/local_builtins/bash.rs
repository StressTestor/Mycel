use std::{ffi::OsString, sync::Arc, time::Duration};

use mycel_agent_protocol::{
    CommandLanguage, ExecutableToolResult, ToolDefinition, ToolInputDisplay, ToolUpdate,
};
use serde_json::Value;

use crate::{
    BackgroundStatus, ExecutableTool, ToolAccess, ToolError, ToolFuture, ToolInvocation,
    ToolPrepareContext, ToolUpdateSink,
};

use super::{
    base_spec, bash_schema,
    output::{error_result, OutputBuffer},
    path::{resolve_local_path, PathKind},
    process::{run_process, ProcessOutcome, ProcessRequest},
    read::string_argument,
    ForegroundProcessPort, LocalToolConfig,
};

const DEFAULT_TIMEOUT_SECONDS: u64 = 60;

pub struct BashTool {
    config: LocalToolConfig,
    foreground_processes: Option<Arc<dyn ForegroundProcessPort>>,
}

impl BashTool {
    pub fn new(config: LocalToolConfig) -> Self {
        Self {
            config,
            foreground_processes: None,
        }
    }

    pub fn with_foreground_process_port(mut self, port: Arc<dyn ForegroundProcessPort>) -> Self {
        self.foreground_processes = Some(port);
        self
    }
}

impl ExecutableTool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Bash".to_owned(),
            description: "Run a foreground command through the configured non-interactive shell."
                .to_owned(),
            parameters: bash_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<crate::ToolExecutionSpec, ToolError> {
        let command = string_argument(arguments, "command")?;
        let cwd = bash_cwd(&self.config, arguments).map_err(ToolError::Prepare)?;
        let mut spec = base_spec(
            ToolInputDisplay::Command {
                command: command.to_owned(),
                cwd: Some(cwd.to_string_lossy().into_owned()),
                description: None,
                language: Some(CommandLanguage::Bash),
            },
            "Bash",
        );
        // Shell commands may touch arbitrary resources beneath an allowed
        // root. TurnEngine still owns policy authorization before this runs.
        spec.accesses = vec![ToolAccess::All];
        spec.description = Some(format!("Running: {}", preview(command)));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let command = string_argument(&invocation.arguments, "command")?;
            let cwd = match bash_cwd(&self.config, &invocation.arguments) {
                Ok(cwd) => cwd,
                Err(error) => return Ok(error_result(error)),
            };
            let timeout = invocation
                .arguments
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
            let git_prompt =
                std::env::var_os("GIT_TERMINAL_PROMPT").unwrap_or_else(|| OsString::from("0"));
            let Some(port) = self.foreground_processes.clone() else {
                let args = [OsString::from("-c"), OsString::from(command)];
                let env = [
                    ("NO_COLOR", OsString::from("1")),
                    ("TERM", OsString::from("dumb")),
                    ("GIT_TERMINAL_PROMPT", git_prompt),
                    ("SHELL", self.config.shell().as_os_str().to_owned()),
                ];
                let outcome = match run_process(ProcessRequest {
                    program: self.config.shell(),
                    args: &args,
                    cwd: &cwd,
                    env: &env,
                    timeout: Duration::from_secs(timeout),
                    cancellation: &invocation.cancellation,
                    updates: Arc::clone(&invocation.updates),
                    stream_updates: true,
                })
                .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => return Ok(error_result(error)),
                };
                return Ok(render_outcome(outcome, timeout));
            };

            let foreground = match port.register(
                &format!("Running: {}", preview(command)),
                Duration::from_secs(timeout),
            ) {
                Ok(foreground) => foreground,
                Err(error) => return Ok(error_result(error)),
            };
            let task_id = foreground.task_id.clone();
            let process_cancellation = foreground.cancellation.clone();
            let detach = foreground.detach.clone();
            let updates: Arc<dyn ToolUpdateSink> = Arc::new(TeeUpdates {
                foreground: Arc::clone(&invocation.updates),
                background: foreground.updates,
            });
            let shell = self.config.shell().to_path_buf();
            let cwd = cwd.clone();
            let command = command.to_owned();
            let settle_port = Arc::clone(&port);
            let settle_task_id = task_id.clone();
            let (completion_sender, mut completion) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let args = [OsString::from("-c"), OsString::from(command)];
                let env = [
                    ("NO_COLOR", OsString::from("1")),
                    ("TERM", OsString::from("dumb")),
                    ("GIT_TERMINAL_PROMPT", git_prompt),
                    ("SHELL", shell.as_os_str().to_owned()),
                ];
                let outcome = run_process(ProcessRequest {
                    program: &shell,
                    args: &args,
                    cwd: &cwd,
                    env: &env,
                    timeout: Duration::from_secs(timeout),
                    cancellation: &process_cancellation,
                    updates,
                    stream_updates: true,
                })
                .await;
                let (status, reason) = process_settlement(&outcome, timeout);
                let outcome = match settle_port.settle(&settle_task_id, status, reason.as_deref()) {
                    Ok(()) => outcome,
                    Err(error) => Err(error),
                };
                let _ = completion_sender.send(outcome);
            });

            tokio::select! {
                biased;
                _ = detach.cancelled() => {
                    let mut output = OutputBuffer::default();
                    output.push(&format!(
                        "task_id: {task_id}\nstatus: running\nnext_step: The task now runs in the background. Do not wait or poll; continue with the current work."
                    ));
                    Ok(output.into_result(false, Some("Task moved to background".to_owned())))
                }
                result = &mut completion => match result {
                    Ok(Ok(outcome)) => Ok(render_outcome(outcome, timeout)),
                    Ok(Err(error)) => Ok(error_result(error)),
                    Err(_) => Ok(error_result("process completion channel closed unexpectedly")),
                },
                _ = invocation.cancellation.cancelled() => {
                    foreground.cancellation.cancel();
                    match completion.await {
                        Ok(Ok(outcome)) => Ok(render_outcome(outcome, timeout)),
                        Ok(Err(error)) => Ok(error_result(error)),
                        Err(_) => Ok(error_result("process completion channel closed after cancellation")),
                    }
                }
            }
        })
    }
}

struct TeeUpdates {
    foreground: Arc<dyn ToolUpdateSink>,
    background: Arc<dyn ToolUpdateSink>,
}

impl ToolUpdateSink for TeeUpdates {
    fn emit(&self, update: ToolUpdate) {
        self.foreground.emit(update.clone());
        self.background.emit(update);
    }
}

fn process_settlement(
    outcome: &Result<ProcessOutcome, String>,
    timeout: u64,
) -> (BackgroundStatus, Option<String>) {
    match outcome {
        Err(error) => (BackgroundStatus::Failed, Some(error.clone())),
        Ok(outcome) if outcome.cancelled => (
            BackgroundStatus::Killed,
            Some("process cancelled".to_owned()),
        ),
        Ok(outcome) if outcome.timed_out => (
            BackgroundStatus::TimedOut,
            Some(format!("command timed out after {timeout}s")),
        ),
        Ok(outcome) if outcome.exit_code.unwrap_or(-1) != 0 => (
            BackgroundStatus::Failed,
            Some(format!(
                "command exited with code {}",
                outcome.exit_code.unwrap_or(-1)
            )),
        ),
        Ok(_) => (BackgroundStatus::Completed, None),
    }
}

fn render_outcome(outcome: ProcessOutcome, timeout: u64) -> ExecutableToolResult {
    let mut output = OutputBuffer::default();
    output.push(&outcome.combined);
    if outcome.raw_truncated {
        output.push("\n[process output truncated at 10 MiB]");
    }
    if outcome.cancelled {
        return output.into_result(true, Some("Interrupted by user".to_owned()));
    }
    if outcome.timed_out {
        return output.into_result(true, Some(format!("Command timed out after {timeout}s")));
    }
    let exit_code = outcome.exit_code.unwrap_or(-1);
    if exit_code != 0 {
        return output.into_result(true, Some(format!("Command exited with code {exit_code}")));
    }
    output.into_result(false, Some("Command completed".to_owned()))
}

fn bash_cwd(config: &LocalToolConfig, arguments: &Value) -> Result<std::path::PathBuf, String> {
    let cwd = arguments.get("cwd").and_then(Value::as_str).unwrap_or(".");
    resolve_local_path(config, cwd, PathKind::Directory)
}

fn preview(command: &str) -> String {
    let mut preview: String = command.chars().take(50).collect();
    if preview.chars().count() < command.chars().count() {
        preview.push('…');
    }
    preview
}
