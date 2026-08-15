use std::{error::Error, fmt};

use crate::{
    cli::{Command, InteractiveRequest, PromptRequest},
    exit::{GoalStatus, TerminationSignal, ERROR, SUCCESS},
    headless::HeadlessEventSink,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRequest {
    Command(Command),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCompletion {
    Success {
        session_id: Option<String>,
    },
    Failure,
    Goal {
        status: GoalStatus,
        session_id: Option<String>,
    },
    Signal(TerminationSignal),
}

impl RuntimeCompletion {
    pub fn success() -> Self {
        Self::Success { session_id: None }
    }

    pub fn success_with_session(session_id: impl Into<String>) -> Self {
        Self::Success {
            session_id: Some(session_id.into()),
        }
    }

    pub const fn failure() -> Self {
        Self::Failure
    }

    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Success { .. } => SUCCESS,
            Self::Failure => ERROR,
            Self::Goal { status, .. } => status.exit_code(),
            Self::Signal(signal) => signal.exit_code(),
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Success { session_id } | Self::Goal { session_id, .. } => session_id.as_deref(),
            Self::Failure | Self::Signal(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterOutput {
    pub stdout: String,
    pub stderr: String,
    pub completion: RuntimeCompletion,
}

impl AdapterOutput {
    pub fn success(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            completion: RuntimeCompletion::success(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAdapterError {
    Failed {
        operation: &'static str,
        message: String,
    },
}

impl RuntimeAdapterError {
    pub fn failed(operation: &'static str, message: impl Into<String>) -> Self {
        Self::Failed {
            operation,
            message: message.into(),
        }
    }

    pub const fn exit_code(&self) -> i32 {
        ERROR
    }
}

impl fmt::Display for RuntimeAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed { operation, message } => {
                write!(formatter, "runtime {operation} failed: {message}")
            }
        }
    }
}

impl Error for RuntimeAdapterError {}

/// Boundary between the CLI contract and its production runtime integration.
pub trait RuntimeAdapter {
    fn run_interactive(
        &mut self,
        request: &InteractiveRequest,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError>;

    fn run_prompt(
        &mut self,
        request: &PromptRequest,
        events: &mut dyn HeadlessEventSink,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError>;

    fn run_command(
        &mut self,
        request: RuntimeRequest,
    ) -> Result<AdapterOutput, RuntimeAdapterError>;
}
