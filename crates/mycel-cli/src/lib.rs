//! Rust command contract for the Mycel harness.
//!
//! Parsing, validation, exit semantics, headless rendering, and the production
//! provider/session adapter and injectable terminal host live here.

pub mod cli;
pub mod clipboard;
mod doctor;
pub mod ecology;
pub mod error;
pub mod exit;
pub mod export;
pub mod headless;
mod markdown_export;
pub mod mcp_oauth;
pub mod mcp_service;
pub mod mcp_transport;
pub mod plugin_store;
pub mod production;
pub mod provider_command_runner;
pub mod provider_commands;
pub mod runtime;
mod session_management;
mod system_prompt;
pub mod terminal;
pub mod tui;
mod tui_config;
mod util;
mod workspace_config;

pub use export::{
    ExportConfirmation, FilesystemSessionExportStore, ProcessExportConfirmation,
    SessionExportLookupError, SessionExportStore, SessionExportSummary,
};
pub use production::{
    ConfigSource, EmptyToolRegistryBuilder, FileConfigSource, HomeLocator,
    LocalToolRegistryBuilder, ProcessEnvironmentSource, ProcessHomeLocator,
    ProductionRuntimeAdapter, ProductionRuntimeServices, RuntimeEnvironment, ToolRegistryBuilder,
};
pub use runtime::{
    AdapterOutput, RuntimeAdapter, RuntimeAdapterError, RuntimeCompletion, RuntimeRequest,
};
pub use session_management::{ProcessSessionPicker, SessionPickerPort};

use cli::{Cli, ProcessEnvironment, ValidatedMode};
use error::CliError;
use headless::{HeadlessPipeline, StreamJsonRenderer, TextRenderer};

/// Fully-buffered command result. Buffering keeps the command layer pure and
/// makes stdout/stderr contracts testable without taking ownership of a TTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl ExecutionResult {
    fn empty(exit_code: i32) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code,
        }
    }
}

/// Execute a parsed command through an explicit runtime adapter.
pub fn execute(
    cli: Cli,
    environment: &dyn cli::Environment,
    adapter: &mut dyn RuntimeAdapter,
) -> Result<ExecutionResult, CliError> {
    if let Some(command) = cli.command.clone() {
        let output = adapter.run_command(RuntimeRequest::Command(command))?;
        return Ok(ExecutionResult {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.completion.exit_code(),
        });
    }

    let validated = cli::validate(cli, environment)?;
    match validated.mode {
        ValidatedMode::Interactive(request) => {
            let completion = adapter.run_interactive(&request)?;
            Ok(ExecutionResult::empty(completion.exit_code()))
        }
        ValidatedMode::Prompt(request) => {
            let mut pipeline = match request.output_format {
                cli::OutputFormat::Text => HeadlessPipeline::new(Box::new(TextRenderer::new(None))),
                cli::OutputFormat::StreamJson => {
                    HeadlessPipeline::new(Box::new(StreamJsonRenderer))
                }
            };
            let completion = adapter.run_prompt(&request, &mut pipeline)?;
            if let Some(session_id) = completion.session_id() {
                pipeline.emit_resume_hint(session_id)?;
            }
            let rendered = pipeline.finish()?;
            Ok(ExecutionResult {
                stdout: rendered.stdout,
                stderr: rendered.stderr,
                exit_code: completion.exit_code(),
            })
        }
    }
}

/// Execute against the real process environment.
pub fn execute_process(
    cli: Cli,
    adapter: &mut dyn RuntimeAdapter,
) -> Result<ExecutionResult, CliError> {
    execute(cli, &ProcessEnvironment, adapter)
}
