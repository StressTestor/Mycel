use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{FinishReason, PermissionMode, TokenUsage, ToolInputDisplay};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageStatus {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_model: BTreeMap<String, TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<TokenUsage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Project,
    User,
    Extra,
    Builtin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillTrigger {
    UserSlash,
    ModelTool,
    NestedSkill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycleStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
    Lost,
}

/// Origin attached to a prompt in the retained CLI runtime.
///
/// The dead v2-only `kind: "task"` spelling is intentionally excluded. The
/// retained engine emits `background_task`, which remains wire-compatible
/// with existing Mycel session logs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PromptOrigin {
    User,
    SkillActivation {
        activation_id: String,
        skill_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_args: Option<String>,
        trigger: SkillTrigger,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_source: Option<SkillSource>,
    },
    PluginCommand {
        activation_id: String,
        plugin_id: String,
        command_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_args: Option<String>,
        trigger: PluginCommandTrigger,
    },
    Injection {
        variant: String,
    },
    ShellCommand {
        phase: ShellCommandPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    CompactionSummary,
    SystemTrigger {
        name: String,
    },
    BackgroundTask {
        task_id: String,
        status: TaskLifecycleStatus,
        notification_id: String,
    },
    CronJob {
        job_id: String,
        cron: String,
        recurring: bool,
        coalesced_count: u64,
        stale: bool,
    },
    CronMissed {
        count: u64,
    },
    HookResult {
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked: Option<bool>,
    },
    Retry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginCommandTrigger {
    UserSlash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellCommandPhase {
    Input,
    Output,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum CronJobOrigin {
    #[serde(rename = "cron_job")]
    CronJob {
        job_id: String,
        cron: String,
        recurring: bool,
        coalesced_count: u64,
        stale: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActor {
    User,
    Model,
    Runtime,
    System,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_budget_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetReport {
    pub token_budget: Option<u64>,
    pub turn_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub remaining_turns: Option<u64>,
    pub remaining_wall_clock_ms: Option<u64>,
    pub token_budget_reached: bool,
    pub turn_budget_reached: bool,
    pub wall_clock_budget_reached: bool,
    pub over_budget: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    pub goal_id: String,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub wall_clock_ms: u64,
    pub budget: GoalBudgetReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChangeKind {
    Lifecycle,
    Completion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalChangeStats {
    pub turns_used: u64,
    pub tokens_used: u64,
    pub wall_clock_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalChange {
    pub kind: GoalChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GoalStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<GoalChangeStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<GoalActor>,
}

/// Stable errors emitted by the retained agent core. This deliberately omits
/// stale protocol-only server, workspace, filesystem, storage, and wire codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentErrorCode {
    #[serde(rename = "config.invalid")]
    ConfigInvalid,
    #[serde(rename = "session.not_found")]
    SessionNotFound,
    #[serde(rename = "session.already_exists")]
    SessionAlreadyExists,
    #[serde(rename = "session.id_invalid")]
    SessionIdInvalid,
    #[serde(rename = "session.id_required")]
    SessionIdRequired,
    #[serde(rename = "session.id_empty")]
    SessionIdEmpty,
    #[serde(rename = "session.title_empty")]
    SessionTitleEmpty,
    #[serde(rename = "session.state_not_found")]
    SessionStateNotFound,
    #[serde(rename = "session.state_invalid")]
    SessionStateInvalid,
    #[serde(rename = "session.fork_active_turn")]
    SessionForkActiveTurn,
    #[serde(rename = "session.export_not_found")]
    SessionExportNotFound,
    #[serde(rename = "session.export_missing_version")]
    SessionExportMissingVersion,
    #[serde(rename = "session.closed")]
    SessionClosed,
    #[serde(rename = "session.permission_mode_invalid")]
    SessionPermissionModeInvalid,
    #[serde(rename = "session.thinking_empty")]
    SessionThinkingEmpty,
    #[serde(rename = "session.model_empty")]
    SessionModelEmpty,
    #[serde(rename = "session.plan_mode_invalid")]
    SessionPlanModeInvalid,
    #[serde(rename = "session.approval_handler_error")]
    SessionApprovalHandlerError,
    #[serde(rename = "session.question_handler_error")]
    SessionQuestionHandlerError,
    #[serde(rename = "session.init_failed")]
    SessionInitFailed,
    #[serde(rename = "agent.not_found")]
    AgentNotFound,
    #[serde(rename = "turn.agent_busy")]
    TurnAgentBusy,
    #[serde(rename = "goal.already_exists")]
    GoalAlreadyExists,
    #[serde(rename = "goal.not_found")]
    GoalNotFound,
    #[serde(rename = "goal.objective_empty")]
    GoalObjectiveEmpty,
    #[serde(rename = "goal.objective_too_long")]
    GoalObjectiveTooLong,
    #[serde(rename = "goal.status_invalid")]
    GoalStatusInvalid,
    #[serde(rename = "goal.metadata_reserved")]
    GoalMetadataReserved,
    #[serde(rename = "goal.not_resumable")]
    GoalNotResumable,
    #[serde(rename = "model.not_configured")]
    ModelNotConfigured,
    #[serde(rename = "model.config_invalid")]
    ModelConfigInvalid,
    #[serde(rename = "auth.login_required")]
    AuthLoginRequired,
    #[serde(rename = "context.overflow")]
    ContextOverflow,
    #[serde(rename = "loop.max_steps_exceeded")]
    LoopMaxStepsExceeded,
    #[serde(rename = "provider.api_error")]
    ProviderApiError,
    #[serde(rename = "provider.filtered")]
    ProviderFiltered,
    #[serde(rename = "provider.rate_limit")]
    ProviderRateLimit,
    #[serde(rename = "provider.auth_error")]
    ProviderAuthError,
    #[serde(rename = "provider.connection_error")]
    ProviderConnectionError,
    #[serde(rename = "skill.not_found")]
    SkillNotFound,
    #[serde(rename = "skill.type_unsupported")]
    SkillTypeUnsupported,
    #[serde(rename = "skill.name_empty")]
    SkillNameEmpty,
    #[serde(rename = "records.write_failed")]
    RecordsWriteFailed,
    #[serde(rename = "compaction.failed")]
    CompactionFailed,
    #[serde(rename = "compaction.unable")]
    CompactionUnable,
    #[serde(rename = "task.task_id_empty")]
    BackgroundTaskIdEmpty,
    #[serde(rename = "mcp.server_not_found")]
    McpServerNotFound,
    #[serde(rename = "mcp.server_disabled")]
    McpServerDisabled,
    #[serde(rename = "mcp.startup_failed")]
    McpStartupFailed,
    #[serde(rename = "mcp.tool_name_collision")]
    McpToolNameCollision,
    #[serde(rename = "plugin.not_found")]
    PluginNotFound,
    #[serde(rename = "plugin.load_failed")]
    PluginLoadFailed,
    #[serde(rename = "request.invalid")]
    RequestInvalid,
    #[serde(rename = "request.work_dir_required")]
    RequestWorkDirRequired,
    #[serde(rename = "request.prompt_input_empty")]
    RequestPromptInputEmpty,
    #[serde(rename = "shell.git_bash_not_found")]
    ShellGitBashNotFound,
    #[serde(rename = "not_implemented")]
    NotImplemented,
    #[serde(rename = "internal")]
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentErrorPayload {
    pub code: AgentErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<AgentErrorPayload>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInfoBase {
    pub task_id: String,
    pub description: String,
    pub status: TaskLifecycleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_notification_suppressed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TaskInfo {
    Process {
        #[serde(flatten)]
        base: TaskInfoBase,
        command: String,
        pid: u32,
        exit_code: Option<i32>,
    },
    Agent {
        #[serde(flatten)]
        base: TaskInfoBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    Question {
        #[serde(flatten)]
        base: TaskInfoBase,
        question_count: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    Workflow {
        #[serde(flatten)]
        base: TaskInfoBase,
        run_id: String,
        workflow_name: String,
        phase_count: u64,
        agent_count: u64,
        source: WorkflowSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest_path: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSource {
    Inline,
    Saved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub compacted_count: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_user_message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_head_user_message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUpdateKind {
    Stdout,
    Stderr,
    Progress,
    Status,
    Custom,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdate {
    pub kind: ToolUpdateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Assistant,
    Thinking,
    ToolCall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionReason {
    Aborted,
    MaxSteps,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentPhase {
    Idle,
    Running {
        turn_id: u64,
        step: u64,
        step_id: String,
        since: u64,
    },
    Streaming {
        turn_id: u64,
        step: u64,
        step_id: String,
        stream: StreamKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        since: u64,
    },
    ToolCall {
        turn_id: u64,
        step: u64,
        tool_call_id: String,
        name: String,
        since: u64,
    },
    Retrying {
        turn_id: u64,
        step: u64,
        step_id: String,
        failed_attempt: u64,
        next_attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
        since: u64,
    },
    AwaitingApproval {
        turn_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval: Option<Value>,
        since: u64,
    },
    Interrupted {
        turn_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
        reason: InterruptionReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        at: u64,
    },
    Ended {
        turn_id: u64,
        reason: TurnEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        at: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolListUpdatedReason {
    #[serde(rename = "mcp.connected")]
    Connected,
    #[serde(rename = "mcp.disconnected")]
    Disconnected,
    #[serde(rename = "mcp.failed")]
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerStatus {
    Pending,
    Connected,
    Failed,
    Disabled,
    NeedsAuth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusPayload {
    pub name: String,
    pub transport: McpTransport,
    pub status: McpServerStatus,
    pub tool_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    #[serde(rename = "error")]
    Error {
        #[serde(flatten)]
        error: AgentErrorPayload,
    },
    #[serde(rename = "warning")]
    Warning {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    #[serde(rename = "agent.status.updated")]
    AgentStatusUpdated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_context_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_usage: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_mode: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_mode: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission: Option<PermissionMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<AgentPhase>,
    },
    #[serde(rename = "session.meta.updated")]
    SessionMetaUpdated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        patch: BTreeMap<String, Value>,
    },
    #[serde(rename = "goal.updated")]
    GoalUpdated {
        snapshot: Option<GoalSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<GoalChange>,
    },
    #[serde(rename = "skill.activated")]
    SkillActivated {
        activation_id: String,
        skill_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_args: Option<String>,
        trigger: SkillTrigger,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_source: Option<SkillSource>,
    },
    #[serde(rename = "plugin_command.activated")]
    PluginCommandActivated {
        activation_id: String,
        plugin_id: String,
        command_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_args: Option<String>,
        trigger: PluginCommandTrigger,
    },
    #[serde(rename = "turn.started")]
    TurnStarted { turn_id: u64, origin: PromptOrigin },
    #[serde(rename = "turn.ended")]
    TurnEnded {
        turn_id: u64,
        reason: TurnEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<AgentErrorPayload>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    #[serde(rename = "turn.step.started")]
    TurnStepStarted {
        turn_id: u64,
        step: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_id: Option<String>,
    },
    #[serde(rename = "turn.step.completed")]
    TurnStepCompleted {
        turn_id: u64,
        step: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_first_token_latency_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_stream_duration_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_request_build_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_server_first_token_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_server_decode_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        llm_client_consume_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_finish_reason: Option<String>,
    },
    #[serde(rename = "turn.step.retrying")]
    TurnStepRetrying {
        turn_id: u64,
        step: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_id: Option<String>,
        failed_attempt: u64,
        next_attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error_name: String,
        error_message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
    },
    #[serde(rename = "turn.step.interrupted")]
    TurnStepInterrupted {
        turn_id: u64,
        step: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_id: Option<String>,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    #[serde(rename = "assistant.delta")]
    AssistantDelta { turn_id: u64, delta: String },
    #[serde(rename = "hook.result")]
    HookResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        hook_event: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked: Option<bool>,
    },
    #[serde(rename = "thinking.delta")]
    ThinkingDelta { turn_id: u64, delta: String },
    #[serde(rename = "tool.call.delta")]
    ToolCallDelta {
        turn_id: u64,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_part: Option<String>,
    },
    #[serde(rename = "tool.call.started")]
    ToolCallStarted {
        turn_id: u64,
        tool_call_id: String,
        name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<ToolInputDisplay>,
    },
    #[serde(rename = "tool.progress")]
    ToolProgress {
        turn_id: u64,
        tool_call_id: String,
        update: ToolUpdate,
    },
    #[serde(rename = "shell.output")]
    ShellOutput {
        command_id: String,
        update: ToolUpdate,
    },
    #[serde(rename = "shell.started")]
    ShellStarted { command_id: String, task_id: String },
    #[serde(rename = "tool.result")]
    ToolResult {
        turn_id: u64,
        tool_call_id: String,
        output: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        synthetic: Option<bool>,
    },
    #[serde(rename = "subagent.spawned")]
    SubagentSpawned {
        subagent_id: String,
        subagent_name: String,
        parent_tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_call_uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_agent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm_index: Option<u64>,
        run_in_background: bool,
    },
    #[serde(rename = "subagent.started")]
    SubagentStarted { subagent_id: String },
    #[serde(rename = "subagent.suspended")]
    SubagentSuspended { subagent_id: String, reason: String },
    #[serde(rename = "subagent.completed")]
    SubagentCompleted {
        subagent_id: String,
        result_summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_tokens: Option<u64>,
    },
    #[serde(rename = "subagent.failed")]
    SubagentFailed { subagent_id: String, error: String },
    #[serde(rename = "compaction.started")]
    CompactionStarted {
        trigger: CompactionTrigger,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction: Option<String>,
    },
    #[serde(rename = "compaction.blocked")]
    CompactionBlocked {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
    },
    #[serde(rename = "compaction.cancelled")]
    CompactionCancelled,
    #[serde(rename = "compaction.completed")]
    CompactionCompleted { result: CompactionResult },
    #[serde(rename = "background.task.started")]
    BackgroundTaskStarted { info: TaskInfo },
    #[serde(rename = "background.task.terminated")]
    BackgroundTaskTerminated { info: TaskInfo },
    #[serde(rename = "cron.fired")]
    CronFired {
        origin: CronJobOrigin,
        prompt: String,
    },
    #[serde(rename = "tool.list.updated")]
    ToolListUpdated {
        reason: ToolListUpdatedReason,
        server_name: String,
    },
    #[serde(rename = "mcp.server.status")]
    McpServerStatus { server: McpServerStatusPayload },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub agent_id: String,
    pub session_id: String,
    #[serde(flatten)]
    pub event: AgentEvent,
}
