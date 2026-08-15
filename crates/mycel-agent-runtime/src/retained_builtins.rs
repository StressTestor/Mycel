//! Retained, non-network built-ins that sit above the provider-neutral tool
//! runtime. Permission checks and hooks remain centralized in `TurnEngine`;
//! these implementations own validation and their concrete local effects.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    future::Future,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use mycel_agent_protocol::{
    ContentPart, DisplayListItem, ExecutableToolOutput, ExecutableToolResult, FileOperation,
    MediaUrl, PlanReviewOption, PromptOrigin, SkillTrigger as ProtocolSkillTrigger, ToolDefinition,
    ToolInputDisplay, ToolUpdate, ToolUpdateKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::AsyncReadExt;

use crate::{
    skills::{
        SkillActivation, SkillFileSystem, SkillRegistry, SkillTrigger as RuntimeSkillTrigger,
    },
    ExecutableTool, FileAccessMode, LocalToolConfig, OrchestrationError, OrchestrationPorts,
    PlanPolicy, Question, QuestionOption, QuestionRequest, RequestId, SessionHandle, ToolAccess,
    ToolError, ToolExecutionSpec, ToolFuture, ToolInvocation, ToolPrepareContext, ToolRegistry,
    ToolRegistryError,
};

const STATE_SCOPE_PREFIX: &str = "retained-builtins";
const MAX_TODOS: usize = 128;
const MAX_TODO_TITLE_CHARS: usize = 500;
const MAX_PLAN_BYTES: u64 = 256 * 1024;
const MAX_QUESTION_CHARS: usize = 2_000;
const MAX_OPTION_LABEL_CHARS: usize = 160;
const MAX_OPTION_DESCRIPTION_CHARS: usize = 1_000;
const MAX_SKILL_NAME_CHARS: usize = 160;
const MAX_SKILL_ARGS_CHARS: usize = 4_096;
const MAX_SKILL_ARGUMENTS: usize = 64;
const MAX_SKILL_ARGUMENT_CHARS: usize = 512;
const MAX_SKILL_PROMPT_BYTES: usize = 256 * 1024;
const MAX_MEDIA_BYTES: u64 = 10 * 1024 * 1024;
const HARD_MAX_MEDIA_BYTES: u64 = 20 * 1024 * 1024;

pub const RETAINED_BUILTIN_TOOL_NAMES: [&str; 7] = [
    "AskUserQuestion",
    "TodoList",
    "EnterPlanMode",
    "ExitPlanMode",
    "Skill",
    "SetGoalBudget",
    "ReadMediaFile",
];

pub type BuiltinPortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub title: String,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBuiltinSnapshot {
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub plan_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_file: Option<PathBuf>,
}

/// Canonical session-state seam for plan mode and todos.
///
/// Mutation futures must not resolve successfully until their durable record
/// is committed. If an implementation publishes a live event, it must do so
/// only after that commit. `snapshot` is asynchronous so canonical session
/// reducers do not need a parallel cache merely to satisfy this tool layer.
pub trait SessionBuiltinStatePort: Send + Sync {
    fn snapshot<'a>(&'a self) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>>;

    fn replace_todos<'a>(
        &'a self,
        todos: Vec<TodoItem>,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>>;

    fn enter_plan_mode<'a>(
        &'a self,
        plan_file: Option<PathBuf>,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>>;

    fn exit_plan_mode<'a>(
        &'a self,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>>;
}

/// Append-only implementation of [`SessionBuiltinStatePort`]. Production
/// hosts whose canonical session reducer lives elsewhere can provide their own
/// adapter while retaining the exact same tool executors.
pub struct DurableSessionBuiltinState {
    scope: String,
    ports: OrchestrationPorts,
    state: Mutex<SessionBuiltinSnapshot>,
}

impl DurableSessionBuiltinState {
    pub fn open(session_id: &str, ports: OrchestrationPorts) -> Result<Self, RetainedBuiltinError> {
        if session_id.is_empty()
            || session_id.len() > 160
            || session_id.chars().any(char::is_control)
        {
            return Err(RetainedBuiltinError::InvalidSessionId);
        }
        let scope = format!("{STATE_SCOPE_PREFIX}:{session_id}");
        let state = ports.restore(&scope)?;
        validate_session_snapshot(&state).map_err(RetainedBuiltinError::InvalidRestoredState)?;
        Ok(Self {
            scope,
            ports,
            state: Mutex::new(state),
        })
    }

    fn transition(
        &self,
        state: &mut SessionBuiltinSnapshot,
        action: &str,
        next: SessionBuiltinSnapshot,
        detail: Value,
    ) -> Result<SessionBuiltinSnapshot, String> {
        validate_session_snapshot(&next)?;
        let event = self
            .ports
            .persist(&self.scope, action, None, &next, detail)
            .map_err(|error| error.to_string())?;
        *state = next.clone();
        self.ports.publish(event);
        Ok(next)
    }
}

impl SessionBuiltinStatePort for DurableSessionBuiltinState {
    fn snapshot<'a>(&'a self) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move { Ok(lock(&self.state).clone()) })
    }

    fn replace_todos<'a>(
        &'a self,
        todos: Vec<TodoItem>,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            validate_todos(&todos)?;
            let mut state = lock(&self.state);
            let mut next = state.clone();
            next.todos = todos;
            let count = next.todos.len();
            self.transition(&mut state, "todos.replaced", next, json!({"count": count}))
        })
    }

    fn enter_plan_mode<'a>(
        &'a self,
        plan_file: Option<PathBuf>,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            let mut state = lock(&self.state);
            let mut next = state.clone();
            if next.plan_mode {
                return Err("plan mode is already active".to_owned());
            }
            next.plan_mode = true;
            next.plan_file = plan_file;
            let has_plan_file = next.plan_file.is_some();
            self.transition(
                &mut state,
                "plan.entered",
                next,
                json!({"hasPlanFile": has_plan_file}),
            )
        })
    }

    fn exit_plan_mode<'a>(
        &'a self,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            let mut state = lock(&self.state);
            let mut next = state.clone();
            if !next.plan_mode {
                return Err("plan mode is not active".to_owned());
            }
            next.plan_mode = false;
            self.transition(&mut state, "plan.exited", next, Value::Null)
        })
    }
}

impl SessionBuiltinStatePort for SessionHandle {
    fn snapshot<'a>(&'a self) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            let session = SessionHandle::snapshot(self).await;
            let todos = match session.state.tool_store.get("todos") {
                Some(value) => serde_json::from_value(value.clone())
                    .map_err(|_| "canonical todo state is malformed".to_owned())?,
                None => Vec::new(),
            };
            let plan_file = session
                .state
                .tool_store
                .get("plan_file")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let snapshot = SessionBuiltinSnapshot {
                todos,
                plan_mode: session.state.plan_mode,
                plan_file,
            };
            validate_session_snapshot(&snapshot)?;
            Ok(snapshot)
        })
    }

    fn replace_todos<'a>(
        &'a self,
        todos: Vec<TodoItem>,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            validate_todos(&todos)?;
            let value =
                serde_json::to_value(todos).map_err(|_| "todo state is invalid".to_owned())?;
            self.set_tool_store_value("todos", value)
                .await
                .map_err(|error| safe_error("todo state commit failed", &error.to_string()))?;
            SessionBuiltinStatePort::snapshot(self).await
        })
    }

    fn enter_plan_mode<'a>(
        &'a self,
        plan_file: Option<PathBuf>,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            if SessionHandle::snapshot(self).await.state.plan_mode {
                return Err("plan mode is already active".to_owned());
            }
            let plan_file = plan_file.map(|path| path.to_string_lossy().into_owned());
            SessionHandle::enter_plan_mode(self, plan_file)
                .await
                .map_err(|error| safe_error("plan mode commit failed", &error.to_string()))?;
            SessionBuiltinStatePort::snapshot(self).await
        })
    }

    fn exit_plan_mode<'a>(
        &'a self,
    ) -> BuiltinPortFuture<'a, Result<SessionBuiltinSnapshot, String>> {
        Box::pin(async move {
            if !SessionHandle::snapshot(self).await.state.plan_mode {
                return Err("plan mode is not active".to_owned());
            }
            SessionHandle::exit_plan_mode(self)
                .await
                .map_err(|error| safe_error("plan mode exit commit failed", &error.to_string()))?;
            SessionBuiltinStatePort::snapshot(self).await
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GoalBudgetLimits {
    pub turn_budget: Option<u64>,
    pub token_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GoalBudgetSnapshot {
    pub has_goal: bool,
    pub turns_used: u64,
    pub tokens_used: u64,
    pub wall_clock_ms: u64,
    pub limits: GoalBudgetLimits,
    pub over_budget: bool,
}

/// Goal-driver seam. A successful `set_budget` must be durable before it
/// returns and before any related live event is emitted. Non-`None` limits in
/// the request are merged into the current goal; existing limits of other
/// kinds are preserved.
pub trait GoalBudgetPort: Send + Sync {
    fn snapshot(&self) -> Result<GoalBudgetSnapshot, String>;

    fn set_budget<'a>(
        &'a self,
        limits: GoalBudgetLimits,
    ) -> BuiltinPortFuture<'a, Result<GoalBudgetSnapshot, String>>;
}

/// Object-safe activation seam over the generic skill registry.
pub trait SkillActivationPort: Send + Sync {
    fn activate(
        &self,
        id: &str,
        arguments: &[String],
        trigger: RuntimeSkillTrigger,
        session_id: &str,
    ) -> Result<SkillActivation, String>;
}

pub struct SkillRegistryActivationPort<F: SkillFileSystem> {
    registry: Arc<RwLock<SkillRegistry<F>>>,
}

impl<F: SkillFileSystem> SkillRegistryActivationPort<F> {
    pub fn new(registry: Arc<RwLock<SkillRegistry<F>>>) -> Self {
        Self { registry }
    }
}

impl<F: SkillFileSystem> SkillActivationPort for SkillRegistryActivationPort<F> {
    fn activate(
        &self,
        id: &str,
        arguments: &[String],
        trigger: RuntimeSkillTrigger,
        session_id: &str,
    ) -> Result<SkillActivation, String> {
        read_lock(&self.registry)
            .activate(id, arguments, trigger, session_id)
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaCapabilities {
    pub image_input: bool,
    pub video_input: bool,
}

impl MediaCapabilities {
    pub const fn images() -> Self {
        Self {
            image_input: true,
            video_input: false,
        }
    }

    pub const fn images_and_video() -> Self {
        Self {
            image_input: true,
            video_input: true,
        }
    }
}

#[derive(Clone)]
pub struct ReadMediaConfig {
    local: LocalToolConfig,
    capabilities: MediaCapabilities,
    max_bytes: u64,
}

impl ReadMediaConfig {
    pub fn new(local: LocalToolConfig, capabilities: MediaCapabilities) -> Result<Self, String> {
        if !capabilities.image_input && !capabilities.video_input {
            return Err("ReadMediaFile needs image or video input capability".to_owned());
        }
        Ok(Self {
            local,
            capabilities,
            max_bytes: MAX_MEDIA_BYTES,
        })
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Result<Self, String> {
        if max_bytes == 0 || max_bytes > HARD_MAX_MEDIA_BYTES {
            return Err(format!(
                "media byte limit must be between 1 and {HARD_MAX_MEDIA_BYTES}"
            ));
        }
        self.max_bytes = max_bytes;
        Ok(self)
    }
}

#[derive(Clone)]
pub struct RetainedBuiltinConfig {
    session: SessionHandle,
    state: Arc<dyn SessionBuiltinStatePort>,
    plan_file: Option<PathBuf>,
    local: Option<LocalToolConfig>,
    skills: Option<Arc<dyn SkillActivationPort>>,
    skill_depth: u8,
    goal_budget: Option<Arc<dyn GoalBudgetPort>>,
    media: Option<ReadMediaConfig>,
}

impl RetainedBuiltinConfig {
    pub fn new(session: SessionHandle, state: Arc<dyn SessionBuiltinStatePort>) -> Self {
        Self {
            session,
            state,
            plan_file: None,
            local: None,
            skills: None,
            skill_depth: 0,
            goal_budget: None,
            media: None,
        }
    }

    pub fn with_plan_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.plan_file = Some(path.into());
        self
    }

    pub fn with_local_tools(mut self, local: LocalToolConfig) -> Self {
        self.local = Some(local);
        self
    }

    pub fn with_skills(mut self, skills: Arc<dyn SkillActivationPort>, depth: u8) -> Self {
        self.skills = Some(skills);
        self.skill_depth = depth;
        self
    }

    pub fn with_goal_budget(mut self, goal_budget: Arc<dyn GoalBudgetPort>) -> Self {
        self.goal_budget = Some(goal_budget);
        self
    }

    pub fn with_media(mut self, media: ReadMediaConfig) -> Self {
        if self.local.is_none() {
            self.local = Some(media.local.clone());
        }
        self.media = Some(media);
        self
    }
}

pub fn register_retained_builtins(
    registry: &ToolRegistry,
    config: RetainedBuiltinConfig,
) -> Result<(), ToolRegistryError> {
    let exit_plan_file = config.plan_file.clone();
    let mut tools: Vec<Arc<dyn ExecutableTool>> = vec![
        Arc::new(AskUserQuestionTool::new(config.session.clone())),
        Arc::new(TodoListTool::new(Arc::clone(&config.state))),
        Arc::new(EnterPlanModeTool::new(
            Arc::clone(&config.state),
            config.plan_file,
            config.local.clone(),
        )),
        Arc::new(ExitPlanModeTool::new(
            Arc::clone(&config.state),
            config.local,
            exit_plan_file,
        )),
    ];
    if let Some(skills) = config.skills {
        tools.push(Arc::new(SkillTool::new(
            config.session,
            skills,
            config.skill_depth,
        )));
    }
    if let Some(goal_budget) = config.goal_budget {
        tools.push(Arc::new(SetGoalBudgetTool::new(goal_budget)));
    }
    if let Some(media) = config.media {
        tools.push(Arc::new(ReadMediaTool::new(media)));
    }
    registry.replace_batch(&BTreeSet::new(), tools)
}

#[derive(Debug, thiserror::Error)]
pub enum RetainedBuiltinError {
    #[error("session id is invalid")]
    InvalidSessionId,
    #[error("retained built-in state is invalid: {0}")]
    InvalidRestoredState(String),
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}

struct AskUserQuestionTool {
    session: SessionHandle,
}

impl AskUserQuestionTool {
    fn new(session: SessionHandle) -> Self {
        Self { session }
    }
}

impl ExecutableTool for AskUserQuestionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "AskUserQuestion".to_owned(),
            description: "Ask the user one to four bounded, structured questions.".to_owned(),
            parameters: ask_user_schema(),
            deferred: false,
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        crate::validate_json_schema(&ask_user_schema(), arguments)?;
        let input: AskUserInput = parse_arguments(arguments)?;
        validate_questions(&input.questions).map_err(invalid_arguments)
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let input: AskUserInput = parse_arguments(arguments)?;
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::Generic {
                summary: format!("Ask {} user question(s)", input.questions.len()),
                detail: None,
            },
            "AskUserQuestion",
        );
        spec.accesses = vec![ToolAccess::None];
        spec.description = Some("Asking the user structured questions".to_owned());
        spec.approval_rule = Some("AskUserQuestion".to_owned());
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let input: AskUserInput = parse_arguments(&invocation.arguments)?;
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("AskUserQuestion"));
            }
            let mut prompts = BTreeMap::new();
            let questions = input
                .questions
                .into_iter()
                .enumerate()
                .map(|(index, question)| {
                    let id = format!("q{}", index + 1);
                    prompts.insert(
                        id.clone(),
                        (question.question.clone(), question.multi_select),
                    );
                    Question {
                        id,
                        prompt: question.question,
                        options: question
                            .options
                            .into_iter()
                            .map(|option| QuestionOption {
                                label: option.label,
                                description: nonempty(option.description),
                            })
                            .collect(),
                        multiple: question.multi_select,
                    }
                })
                .collect();
            let request = QuestionRequest {
                request_id: RequestId::generate(),
                agent_id: invocation.context.agent_id.clone(),
                questions,
            };
            let response = tokio::select! {
                _ = invocation.cancellation.cancelled() => {
                    return Err(cancelled("AskUserQuestion"));
                }
                response = self.session.ask(request) => response,
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    return Ok(error_result(safe_error(
                        "question failed",
                        &error.to_string(),
                    )))
                }
            };
            if response.answers.is_empty() {
                return Ok(error_result(
                    "User dismissed the question without answering.",
                ));
            }
            let mut answers = Map::new();
            let mut seen = BTreeSet::new();
            for answer in response.answers {
                if !seen.insert(answer.question_id.clone()) {
                    return Ok(error_result("question port returned a duplicate answer id"));
                }
                let Some((prompt, multiple)) = prompts.get(&answer.question_id) else {
                    return Ok(error_result("question port returned an unknown answer id"));
                };
                let answer_text = answer.text.and_then(nonempty);
                let value = if *multiple {
                    if let Some(text) = answer_text {
                        let mut selected = answer.selected_labels;
                        selected.push(text);
                        json!(selected)
                    } else {
                        json!(answer.selected_labels)
                    }
                } else if let Some(label) = answer.selected_labels.first() {
                    json!(label)
                } else if let Some(text) = answer_text {
                    json!(text)
                } else {
                    Value::Null
                };
                answers.insert(prompt.clone(), value);
            }
            invocation
                .updates
                .emit(completed_update("questions answered"));
            text_json(json!({"answers": answers}))
        })
    }
}

#[derive(Deserialize)]
struct AskUserInput {
    questions: Vec<AskQuestionInput>,
}

#[derive(Deserialize)]
struct AskQuestionInput {
    question: String,
    #[serde(default)]
    header: String,
    options: Vec<AskOptionInput>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Deserialize)]
struct AskOptionInput {
    label: String,
    #[serde(default)]
    description: String,
}

struct TodoListTool {
    state: Arc<dyn SessionBuiltinStatePort>,
}

impl TodoListTool {
    fn new(state: Arc<dyn SessionBuiltinStatePort>) -> Self {
        Self { state }
    }
}

impl ExecutableTool for TodoListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "TodoList".to_owned(),
            description: "Read, replace, or clear the current session todo list.".to_owned(),
            parameters: todo_schema(),
            deferred: false,
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        crate::validate_json_schema(&todo_schema(), arguments)?;
        let input: TodoInput = parse_arguments(arguments)?;
        if let Some(todos) = &input.todos {
            validate_todos(todos).map_err(invalid_arguments)?;
        }
        Ok(())
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let input: TodoInput = parse_arguments(arguments)?;
        let items = input
            .todos
            .unwrap_or_default()
            .into_iter()
            .map(|todo| DisplayListItem {
                title: todo.title,
                status: todo.status.as_str().to_owned(),
            })
            .collect();
        let mut spec = ToolExecutionSpec::new(ToolInputDisplay::TodoList { items }, "TodoList");
        spec.accesses = vec![ToolAccess::None];
        spec.description = Some("Updating session todos".to_owned());
        spec.approval_rule = Some("TodoList".to_owned());
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let input: TodoInput = parse_arguments(&invocation.arguments)?;
            let snapshot = if let Some(todos) = input.todos {
                tokio::select! {
                    _ = invocation.cancellation.cancelled() => {
                        return Err(cancelled("TodoList"));
                    }
                    result = self.state.replace_todos(todos) => result,
                }
            } else {
                tokio::select! {
                    _ = invocation.cancellation.cancelled() => {
                        return Err(cancelled("TodoList"));
                    }
                    result = self.state.snapshot() => result,
                }
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => return Ok(error_result(safe_error("todo update failed", &error))),
            };
            invocation
                .updates
                .emit(completed_update("todo state committed"));
            text_json(json!({"todos": snapshot.todos}))
        })
    }
}

#[derive(Deserialize)]
struct TodoInput {
    todos: Option<Vec<TodoItem>>,
}

struct EnterPlanModeTool {
    state: Arc<dyn SessionBuiltinStatePort>,
    plan_file: Option<PathBuf>,
    local: Option<LocalToolConfig>,
}

impl EnterPlanModeTool {
    fn new(
        state: Arc<dyn SessionBuiltinStatePort>,
        plan_file: Option<PathBuf>,
        local: Option<LocalToolConfig>,
    ) -> Self {
        Self {
            state,
            plan_file,
            local,
        }
    }

    fn resolved_plan_file(&self) -> Result<Option<PathBuf>, String> {
        match (&self.plan_file, &self.local) {
            (Some(path), Some(local)) => resolve_writable_local_path(local, path).map(Some),
            (Some(_), None) => {
                Err("plan file requires configured local workspace roots".to_owned())
            }
            (None, _) => Ok(None),
        }
    }
}

impl ExecutableTool for EnterPlanModeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "EnterPlanMode".to_owned(),
            description: "Enter read-mostly planning mode for the current session.".to_owned(),
            parameters: empty_object_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let plan_file = self.resolved_plan_file().map_err(ToolError::Prepare)?;
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::Generic {
                summary: "Enter plan mode".to_owned(),
                detail: plan_file
                    .as_ref()
                    .map(|path| json!({"planFile": path.to_string_lossy()})),
            },
            "EnterPlanMode",
        );
        spec.accesses = vec![ToolAccess::None];
        spec.description = Some("Entering plan mode".to_owned());
        spec.approval_rule = Some("EnterPlanMode".to_owned());
        spec.plan_policy = PlanPolicy::Allowed;
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("EnterPlanMode"));
            }
            let plan_file = match self.resolved_plan_file() {
                Ok(path) => path,
                Err(error) => {
                    return Ok(error_result(safe_error("cannot enter plan mode", &error)))
                }
            };
            let snapshot = tokio::select! {
                _ = invocation.cancellation.cancelled() => {
                    return Err(cancelled("EnterPlanMode"));
                }
                result = self.state.enter_plan_mode(plan_file.clone()) => result,
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Ok(error_result(safe_error("cannot enter plan mode", &error)))
                }
            };
            invocation
                .updates
                .emit(completed_update("plan mode committed"));
            let suffix = snapshot
                .plan_file
                .map(|path| format!(" Write the plan to {}.", path.to_string_lossy()))
                .unwrap_or_else(|| " No plan file is configured in this host.".to_owned());
            Ok(text_result(format!("Plan mode is active.{suffix}")))
        })
    }
}

struct ExitPlanModeTool {
    state: Arc<dyn SessionBuiltinStatePort>,
    local: Option<LocalToolConfig>,
    plan_file: Option<PathBuf>,
}

impl ExitPlanModeTool {
    fn new(
        state: Arc<dyn SessionBuiltinStatePort>,
        local: Option<LocalToolConfig>,
        plan_file: Option<PathBuf>,
    ) -> Self {
        Self {
            state,
            local,
            plan_file,
        }
    }

    fn resolve_plan_path(&self) -> Result<PathBuf, String> {
        let local = self
            .local
            .as_ref()
            .ok_or_else(|| "plan file requires configured local workspace roots".to_owned())?;
        let path = self
            .plan_file
            .as_ref()
            .ok_or_else(|| "no plan file is configured".to_owned())?;
        resolve_existing_local_file(local, path)
    }

    fn read_plan_for_display(&self) -> Result<(PathBuf, String), String> {
        let path = self.resolve_plan_path()?;
        let plan = read_small_text_sync(&path, MAX_PLAN_BYTES)?;
        if plan.trim().is_empty() {
            return Err("plan file is empty".to_owned());
        }
        Ok((path, plan))
    }
}

impl ExecutableTool for ExitPlanModeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ExitPlanMode".to_owned(),
            description: "Submit the current plan for review and exit plan mode.".to_owned(),
            parameters: exit_plan_schema(),
            deferred: false,
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        crate::validate_json_schema(&exit_plan_schema(), arguments)?;
        let input: ExitPlanInput = parse_arguments(arguments)?;
        validate_plan_options(input.options.as_deref()).map_err(invalid_arguments)
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let input: ExitPlanInput = parse_arguments(arguments)?;
        let mut spec = match self.read_plan_for_display() {
            Ok((path, plan)) => {
                let options = input
                    .options
                    .filter(|options| options.len() >= 2)
                    .map(|options| {
                        options
                            .into_iter()
                            .map(|option| PlanReviewOption {
                                label: option.label,
                                description: option.description,
                            })
                            .collect()
                    });
                let mut spec = ToolExecutionSpec::new(
                    ToolInputDisplay::PlanReview {
                        plan,
                        path: Some(path.to_string_lossy().into_owned()),
                        options,
                    },
                    "ExitPlanMode",
                );
                spec.accesses = vec![ToolAccess::file(path, FileAccessMode::Read)];
                spec
            }
            Err(_) => ToolExecutionSpec::new(
                ToolInputDisplay::Generic {
                    summary: "Exit plan mode".to_owned(),
                    detail: None,
                },
                "ExitPlanMode",
            ),
        };
        spec.description = Some("Submitting the plan and exiting plan mode".to_owned());
        spec.approval_rule = Some("ExitPlanMode".to_owned());
        spec.plan_policy = PlanPolicy::ExitReview;
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("ExitPlanMode"));
            }
            let snapshot = tokio::select! {
                _ = invocation.cancellation.cancelled() => {
                    return Err(cancelled("ExitPlanMode"));
                }
                result = self.state.snapshot() => result,
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Ok(error_result(safe_error("cannot inspect plan mode", &error)))
                }
            };
            if !snapshot.plan_mode {
                return Ok(error_result(
                    "ExitPlanMode can only be called while plan mode is active.",
                ));
            }
            let candidate =
                match snapshot.plan_file {
                    Some(path) => path,
                    None => match self.resolve_plan_path() {
                        Ok(path) => path,
                        Err(_) => return Ok(error_result(
                            "No plan file is configured. Write a plan before exiting plan mode.",
                        )),
                    },
                };
            let Some(local) = self.local.as_ref() else {
                return Ok(error_result(
                    "Plan file cannot be read without configured local workspace roots.",
                ));
            };
            let path = match resolve_existing_local_file(local, &candidate) {
                Ok(path) => path,
                Err(error) => return Ok(error_result(safe_error("plan path rejected", &error))),
            };
            let plan = match read_small_text_async(&path, MAX_PLAN_BYTES, &invocation.cancellation)
                .await
            {
                Ok(plan) if !plan.trim().is_empty() => plan,
                Ok(_) => return Ok(error_result("The current plan file is empty.")),
                Err(error) => {
                    return Ok(error_result(safe_error(
                        "failed to read the plan file",
                        &error,
                    )))
                }
            };
            let exited = tokio::select! {
                _ = invocation.cancellation.cancelled() => {
                    return Err(cancelled("ExitPlanMode"));
                }
                result = self.state.exit_plan_mode() => result,
            };
            if let Err(error) = exited {
                return Ok(error_result(safe_error("failed to exit plan mode", &error)));
            }
            invocation
                .updates
                .emit(completed_update("plan mode exit committed"));
            Ok(text_result(format!(
                "Exited plan mode. Plan saved to {} ({} characters).",
                path.to_string_lossy(),
                plan.chars().count()
            )))
        })
    }
}

#[derive(Deserialize)]
struct ExitPlanInput {
    options: Option<Vec<ExitPlanOptionInput>>,
}

#[derive(Deserialize)]
struct ExitPlanOptionInput {
    label: String,
    #[serde(default)]
    description: String,
}

struct SkillTool {
    session: SessionHandle,
    skills: Arc<dyn SkillActivationPort>,
    depth: u8,
}

impl SkillTool {
    fn new(session: SessionHandle, skills: Arc<dyn SkillActivationPort>, depth: u8) -> Self {
        Self {
            session,
            skills,
            depth,
        }
    }
}

impl ExecutableTool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Skill".to_owned(),
            description: "Activate one retained Mycel skill in the current session.".to_owned(),
            parameters: skill_schema(),
            deferred: false,
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        crate::validate_json_schema(&skill_schema(), arguments)?;
        let input: SkillInput = parse_arguments(arguments)?;
        parse_skill_arguments(input.args.as_deref().unwrap_or_default())?;
        Ok(())
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let input: SkillInput = parse_arguments(arguments)?;
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::SkillCall {
                skill_name: input.skill.clone(),
                args: input.args.clone(),
            },
            "Skill",
        );
        spec.accesses = vec![ToolAccess::None];
        spec.description = Some(format!("Activating skill {}", input.skill));
        spec.approval_rule = Some("Skill".to_owned());
        spec.rule_subject = Some(input.skill);
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            if self.depth >= 3 {
                return Ok(error_result(
                    "Skill activation depth limit reached; recursive skill activation is denied.",
                ));
            }
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("Skill"));
            }
            let input: SkillInput = parse_arguments(&invocation.arguments)?;
            let parsed = parse_skill_arguments(input.args.as_deref().unwrap_or_default())?;
            let trigger = if self.depth == 0 {
                RuntimeSkillTrigger::ModelTool
            } else {
                RuntimeSkillTrigger::NestedSkill
            };
            let activation = match self.skills.activate(
                &input.skill,
                &parsed,
                trigger,
                self.session.id().as_str(),
            ) {
                Ok(activation) => activation,
                Err(error) => {
                    return Ok(error_result(safe_error("skill activation failed", &error)))
                }
            };
            if activation.prompt.len() > MAX_SKILL_PROMPT_BYTES {
                return Ok(error_result(
                    "skill activation exceeds the context byte limit",
                ));
            }
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("Skill"));
            }
            let activation_id = RequestId::generate().into_string();
            let origin = PromptOrigin::SkillActivation {
                activation_id,
                skill_name: activation.id.clone(),
                skill_args: input.args.clone().and_then(nonempty),
                trigger: match trigger {
                    RuntimeSkillTrigger::ModelTool => ProtocolSkillTrigger::ModelTool,
                    RuntimeSkillTrigger::NestedSkill => ProtocolSkillTrigger::NestedSkill,
                    RuntimeSkillTrigger::UserSlash => ProtocolSkillTrigger::UserSlash,
                },
                skill_type: Some(activation.kind.to_string()),
                skill_path: None,
                skill_source: None,
            };
            let append = tokio::select! {
                _ = invocation.cancellation.cancelled() => {
                    return Err(cancelled("Skill"));
                }
                result = self.session.append_user_message(activation.prompt, origin) => result,
            };
            if let Err(error) = append {
                return Ok(error_result(safe_error(
                    "skill activation could not be recorded",
                    &error.to_string(),
                )));
            }
            invocation
                .updates
                .emit(completed_update("skill activation committed"));
            Ok(text_result(format!(
                "Activated skill {} ({}, {}).",
                activation.id, activation.kind, activation.trigger
            )))
        })
    }
}

#[derive(Deserialize)]
struct SkillInput {
    skill: String,
    args: Option<String>,
}

struct SetGoalBudgetTool {
    goal: Arc<dyn GoalBudgetPort>,
}

impl SetGoalBudgetTool {
    fn new(goal: Arc<dyn GoalBudgetPort>) -> Self {
        Self { goal }
    }
}

impl ExecutableTool for SetGoalBudgetTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "SetGoalBudget".to_owned(),
            description: "Set one user-provided hard budget on the current goal.".to_owned(),
            parameters: goal_budget_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let input: GoalBudgetInput = parse_arguments(arguments)?;
        let normalized = normalize_goal_budget(input).ok();
        let snapshot = self.goal.snapshot().map_err(ToolError::Prepare)?;
        let label = normalized
            .as_ref()
            .map(|budget| budget.label.clone())
            .unwrap_or_else(|| "requested limit".to_owned());
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::Generic {
                summary: format!("Set goal budget: {label}"),
                detail: None,
            },
            "SetGoalBudget",
        );
        spec.accesses = vec![ToolAccess::None];
        spec.description = Some(format!("Setting goal budget: {label}"));
        spec.approval_rule = Some("SetGoalBudget".to_owned());
        spec.stop_batch_after_this =
            normalized.is_some_and(|budget| would_be_over_budget(snapshot, budget.limits));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let input: GoalBudgetInput = parse_arguments(&invocation.arguments)?;
            let normalized = match normalize_goal_budget(input) {
                Ok(normalized) => normalized,
                Err(message) => return Ok(text_result(format!("Goal budget not set: {message}."))),
            };
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("SetGoalBudget"));
            }
            let before = match self.goal.snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Ok(error_result(safe_error(
                        "goal budget lookup failed",
                        &error,
                    )))
                }
            };
            if !before.has_goal {
                return Ok(text_result("Goal budget not set: no current goal."));
            }
            let snapshot = tokio::select! {
                _ = invocation.cancellation.cancelled() => {
                    return Err(cancelled("SetGoalBudget"));
                }
                result = self.goal.set_budget(normalized.limits) => result,
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Ok(error_result(safe_error(
                        "goal budget update failed",
                        &error,
                    )))
                }
            };
            invocation
                .updates
                .emit(completed_update("goal budget committed"));
            let mut result = text_result(if snapshot.over_budget {
                format!(
                    "Goal budget set: {}. The goal has already reached this budget and will stop now.",
                    normalized.label
                )
            } else {
                format!("Goal budget set: {}.", normalized.label)
            });
            result.stop_turn = snapshot.over_budget;
            Ok(result)
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GoalBudgetUnit {
    Turns,
    Tokens,
    Milliseconds,
    Seconds,
    Minutes,
    Hours,
}

#[derive(Deserialize)]
struct GoalBudgetInput {
    value: f64,
    unit: GoalBudgetUnit,
}

struct NormalizedGoalBudget {
    limits: GoalBudgetLimits,
    label: String,
}

struct ReadMediaTool {
    config: ReadMediaConfig,
}

impl ReadMediaTool {
    fn new(config: ReadMediaConfig) -> Self {
        Self { config }
    }
}

impl ExecutableTool for ReadMediaTool {
    fn definition(&self) -> ToolDefinition {
        let supported = match (
            self.config.capabilities.image_input,
            self.config.capabilities.video_input,
        ) {
            (true, true) => "images and videos",
            (true, false) => "images",
            (false, true) => "videos",
            (false, false) => "no media",
        };
        ToolDefinition {
            name: "ReadMediaFile".to_owned(),
            description: format!(
                "Read a bounded local media file for a model that accepts {supported}."
            ),
            parameters: media_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        self.validate_arguments(arguments)?;
        let input: MediaInput = parse_arguments(arguments)?;
        let path = resolve_existing_local_file(&self.config.local, Path::new(&input.path))
            .map_err(ToolError::Prepare)?;
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::FileIo {
                operation: FileOperation::Read,
                path: path.to_string_lossy().into_owned(),
                detail: Some("media".to_owned()),
                content: None,
                before: None,
                after: None,
            },
            "ReadMediaFile",
        );
        spec.accesses = vec![ToolAccess::file(path.clone(), FileAccessMode::Read)];
        spec.description = Some(format!("Reading media: {}", input.path));
        spec.approval_rule = Some("ReadMediaFile".to_owned());
        spec.rule_subject = Some(path.to_string_lossy().into_owned());
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let input: MediaInput = parse_arguments(&invocation.arguments)?;
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("ReadMediaFile"));
            }
            let path = match resolve_existing_local_file(&self.config.local, Path::new(&input.path))
            {
                Ok(path) => path,
                Err(error) => return Ok(error_result(safe_error("media path rejected", &error))),
            };
            let bytes = match read_media_bytes(
                &path,
                self.config.max_bytes,
                &invocation.cancellation,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(error) => return Ok(error_result(safe_error("failed to read media", &error))),
            };
            if bytes.is_empty() {
                return Ok(error_result("media file is empty"));
            }
            let media = match sniff_media(&bytes) {
                Some(media) => media,
                None if looks_textual(&bytes) => {
                    return Ok(error_result("file is text; use Read instead"))
                }
                None => return Ok(error_result("unsupported image or video format")),
            };
            match media.kind {
                MediaKind::Image if !self.config.capabilities.image_input => {
                    return Ok(error_result(
                        "the current model does not support image input",
                    ))
                }
                MediaKind::Video if !self.config.capabilities.video_input => {
                    return Ok(error_result(
                        "the current model does not support video input",
                    ))
                }
                _ => {}
            }
            if invocation.cancellation.is_cancelled() {
                return Err(cancelled("ReadMediaFile"));
            }
            let encoded = BASE64_STANDARD.encode(&bytes);
            let url = format!("data:{};base64,{encoded}", media.mime);
            let content = match media.kind {
                MediaKind::Image => ContentPart::ImageUrl {
                    image_url: MediaUrl { url, id: None },
                },
                MediaKind::Video => ContentPart::VideoUrl {
                    video_url: MediaUrl { url, id: None },
                },
            };
            invocation.updates.emit(completed_update(&format!(
                "read {} bytes of {}",
                bytes.len(),
                media.mime
            )));
            Ok(ExecutableToolResult {
                output: ExecutableToolOutput::Parts(vec![
                    ContentPart::text(format!(
                        "<media path=\"{}\" mime=\"{}\" bytes=\"{}\">",
                        escape_media_path(&input.path),
                        media.mime,
                        bytes.len()
                    )),
                    content,
                    ContentPart::text("</media>"),
                ]),
                is_error: false,
                stop_turn: false,
                message: None,
                note: None,
                truncated: false,
            })
        })
    }
}

#[derive(Deserialize)]
struct MediaInput {
    path: String,
}

#[derive(Clone, Copy)]
enum MediaKind {
    Image,
    Video,
}

#[derive(Clone, Copy)]
struct SniffedMedia {
    kind: MediaKind,
    mime: &'static str,
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn ask_user_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "questions":{
                "type":"array", "minItems":1, "maxItems":4,
                "items":{
                    "type":"object",
                    "properties":{
                        "question":{"type":"string","minLength":1,"maxLength":MAX_QUESTION_CHARS},
                        "header":{"type":"string","maxLength":12},
                        "options":{
                            "type":"array", "minItems":2, "maxItems":4,
                            "items":{
                                "type":"object",
                                "properties":{
                                    "label":{"type":"string","minLength":1,"maxLength":MAX_OPTION_LABEL_CHARS},
                                    "description":{"type":"string","maxLength":MAX_OPTION_DESCRIPTION_CHARS}
                                },
                                "required":["label"],
                                "additionalProperties":false
                            }
                        },
                        "multi_select":{"type":"boolean"}
                    },
                    "required":["question","options"],
                    "additionalProperties":false
                }
            }
        },
        "required":["questions"],
        "additionalProperties":false
    })
}

fn todo_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "todos":{
                "type":"array", "maxItems":MAX_TODOS,
                "items":{
                    "type":"object",
                    "properties":{
                        "title":{"type":"string","minLength":1,"maxLength":MAX_TODO_TITLE_CHARS},
                        "status":{"type":"string","enum":["pending","in_progress","done"]}
                    },
                    "required":["title","status"],
                    "additionalProperties":false
                }
            }
        },
        "additionalProperties":false
    })
}

fn exit_plan_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "options":{
                "type":"array", "minItems":1, "maxItems":3,
                "items":{
                    "type":"object",
                    "properties":{
                        "label":{"type":"string","minLength":1,"maxLength":80},
                        "description":{"type":"string","maxLength":1_000}
                    },
                    "required":["label"],
                    "additionalProperties":false
                }
            }
        },
        "additionalProperties":false
    })
}

fn skill_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "skill":{"type":"string","minLength":1,"maxLength":MAX_SKILL_NAME_CHARS},
            "args":{"type":"string","maxLength":MAX_SKILL_ARGS_CHARS}
        },
        "required":["skill"],
        "additionalProperties":false
    })
}

fn goal_budget_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "value":{"type":"number","exclusiveMinimum":0},
            "unit":{"type":"string","enum":["turns","tokens","milliseconds","seconds","minutes","hours"]}
        },
        "required":["value","unit"],
        "additionalProperties":false
    })
}

fn media_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "path":{"type":"string","minLength":1,"maxLength":4_096}
        },
        "required":["path"],
        "additionalProperties":false
    })
}

fn validate_session_snapshot(snapshot: &SessionBuiltinSnapshot) -> Result<(), String> {
    validate_todos(&snapshot.todos)?;
    if snapshot
        .plan_file
        .as_ref()
        .is_some_and(|path| path.as_os_str().len() > 4_096)
    {
        return Err("plan file path exceeds its limit".to_owned());
    }
    Ok(())
}

fn validate_todos(todos: &[TodoItem]) -> Result<(), String> {
    if todos.len() > MAX_TODOS {
        return Err(format!("todo list exceeds {MAX_TODOS} items"));
    }
    let mut in_progress = 0;
    for todo in todos {
        let title = todo.title.trim();
        if title.is_empty() || title.chars().count() > MAX_TODO_TITLE_CHARS {
            return Err("todo titles must be non-empty and bounded".to_owned());
        }
        if todo.status == TodoStatus::InProgress {
            in_progress += 1;
        }
    }
    if in_progress > 1 {
        return Err("at most one todo may be in progress".to_owned());
    }
    Ok(())
}

fn validate_questions(questions: &[AskQuestionInput]) -> Result<(), String> {
    let mut prompts = BTreeSet::new();
    for question in questions {
        if question.question.trim().is_empty() {
            return Err("question text cannot be blank".to_owned());
        }
        if question.header.chars().count() > 12 {
            return Err("question headers cannot exceed 12 characters".to_owned());
        }
        if !prompts.insert(question.question.clone()) {
            return Err("question texts must be unique".to_owned());
        }
        let mut labels = BTreeSet::new();
        for option in &question.options {
            if option.label.trim().is_empty() {
                return Err("question option labels cannot be blank".to_owned());
            }
            if !labels.insert(option.label.clone()) {
                return Err("option labels must be unique within each question".to_owned());
            }
        }
    }
    Ok(())
}

fn validate_plan_options(options: Option<&[ExitPlanOptionInput]>) -> Result<(), String> {
    let reserved = ["approve", "reject", "reject and exit", "revise"];
    let mut labels = BTreeSet::new();
    for option in options.unwrap_or_default() {
        let normalized = option.label.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err("plan option labels cannot be blank".to_owned());
        }
        if reserved.contains(&normalized.as_str()) {
            return Err("plan option label is reserved for approval controls".to_owned());
        }
        if !labels.insert(normalized) {
            return Err("plan option labels must be unique".to_owned());
        }
    }
    Ok(())
}

fn parse_skill_arguments(input: &str) -> Result<Vec<String>, ToolError> {
    if input.chars().count() > MAX_SKILL_ARGS_CHARS {
        return Err(invalid_arguments("skill arguments exceed their limit"));
    }
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut token_started = false;
    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            token_started = true;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            token_started = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            token_started = true;
        } else if character.is_whitespace() {
            push_skill_argument(&mut arguments, &mut current, &mut token_started)?;
        } else {
            current.push(character);
            token_started = true;
        }
    }
    if escaped || quote.is_some() {
        return Err(invalid_arguments(
            "skill arguments contain an unterminated quote or escape",
        ));
    }
    push_skill_argument(&mut arguments, &mut current, &mut token_started)?;
    Ok(arguments)
}

fn push_skill_argument(
    arguments: &mut Vec<String>,
    current: &mut String,
    token_started: &mut bool,
) -> Result<(), ToolError> {
    if !*token_started {
        return Ok(());
    }
    if arguments.len() >= MAX_SKILL_ARGUMENTS {
        return Err(invalid_arguments("skill has too many arguments"));
    }
    if current.chars().count() > MAX_SKILL_ARGUMENT_CHARS {
        return Err(invalid_arguments("a skill argument exceeds its limit"));
    }
    arguments.push(std::mem::take(current));
    *token_started = false;
    Ok(())
}

fn normalize_goal_budget(input: GoalBudgetInput) -> Result<NormalizedGoalBudget, String> {
    if !input.value.is_finite() || input.value <= 0.0 {
        return Err("budget value must be a finite positive number".to_owned());
    }
    let (limits, value, unit) = match input.unit {
        GoalBudgetUnit::Turns => {
            let value = rounded_bounded_u64(input.value, "turn")?;
            (
                GoalBudgetLimits {
                    turn_budget: Some(value),
                    ..GoalBudgetLimits::default()
                },
                value,
                "turn",
            )
        }
        GoalBudgetUnit::Tokens => {
            let value = rounded_bounded_u64(input.value, "token")?;
            (
                GoalBudgetLimits {
                    token_budget: Some(value),
                    ..GoalBudgetLimits::default()
                },
                value,
                "token",
            )
        }
        unit => {
            let factor = match unit {
                GoalBudgetUnit::Milliseconds => 1.0,
                GoalBudgetUnit::Seconds => 1_000.0,
                GoalBudgetUnit::Minutes => 60_000.0,
                GoalBudgetUnit::Hours => 3_600_000.0,
                GoalBudgetUnit::Turns | GoalBudgetUnit::Tokens => unreachable!(),
            };
            let milliseconds = input.value * factor;
            if !milliseconds.is_finite() || !(1_000.0..=86_400_000.0).contains(&milliseconds) {
                return Err("time budget must be between 1 second and 24 hours".to_owned());
            }
            let value = milliseconds.round() as u64;
            (
                GoalBudgetLimits {
                    wall_clock_budget_ms: Some(value),
                    ..GoalBudgetLimits::default()
                },
                value,
                "millisecond",
            )
        }
    };
    let plural = if value == 1 { unit } else { pluralize(unit) };
    Ok(NormalizedGoalBudget {
        limits,
        label: format!("{value} {plural}"),
    })
}

fn rounded_bounded_u64(value: f64, kind: &str) -> Result<u64, String> {
    let rounded = value.round().max(1.0);
    if rounded > u64::MAX as f64 {
        return Err(format!("{kind} budget is too large"));
    }
    Ok(rounded as u64)
}

fn pluralize(unit: &str) -> &'static str {
    match unit {
        "turn" => "turns",
        "token" => "tokens",
        "millisecond" => "milliseconds",
        _ => "units",
    }
}

fn would_be_over_budget(snapshot: GoalBudgetSnapshot, new: GoalBudgetLimits) -> bool {
    if !snapshot.has_goal {
        return false;
    }
    let turns = new.turn_budget.or(snapshot.limits.turn_budget);
    let tokens = new.token_budget.or(snapshot.limits.token_budget);
    let wall = new
        .wall_clock_budget_ms
        .or(snapshot.limits.wall_clock_budget_ms);
    turns.is_some_and(|limit| snapshot.turns_used >= limit)
        || tokens.is_some_and(|limit| snapshot.tokens_used >= limit)
        || wall.is_some_and(|limit| snapshot.wall_clock_ms >= limit)
}

fn resolve_existing_local_file(config: &LocalToolConfig, input: &Path) -> Result<PathBuf, String> {
    if input.as_os_str().is_empty() {
        return Err("path cannot be empty".to_owned());
    }
    let candidate = normalized_candidate(config, input)?;
    if is_sensitive_path(&candidate) {
        return Err("access to a sensitive path is denied".to_owned());
    }
    let exact_file = exact_allowed_file(config, &candidate);
    let root = match (lexical_root(config, &candidate), exact_file) {
        (Some(root), _) => root,
        (None, Some(file)) => file
            .parent()
            .ok_or_else(|| "allowed plan file has no parent".to_owned())?,
        (None, None) => {
            return Err(
                "path is outside the configured workspace roots and exact file grants".to_owned(),
            )
        }
    };
    reject_symlinks(root, &candidate)?;
    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|error| safe_io_error("cannot resolve local file", &error))?;
    require_real_root_or_file(config, &canonical)?;
    let metadata = std::fs::symlink_metadata(&canonical)
        .map_err(|error| safe_io_error("cannot inspect local file", &error))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("path is not a regular file".to_owned());
    }
    Ok(canonical)
}

fn resolve_writable_local_path(config: &LocalToolConfig, input: &Path) -> Result<PathBuf, String> {
    if input.as_os_str().is_empty() {
        return Err("path cannot be empty".to_owned());
    }
    let candidate = normalized_candidate(config, input)?;
    if is_sensitive_path(&candidate) {
        return Err("access to a sensitive path is denied".to_owned());
    }
    let exact_file = exact_allowed_file(config, &candidate);
    let root = match (lexical_root(config, &candidate), exact_file) {
        (Some(root), _) => root,
        (None, Some(file)) => file
            .parent()
            .ok_or_else(|| "allowed plan file has no parent".to_owned())?,
        (None, None) => {
            return Err(
                "path is outside the configured workspace roots and exact file grants".to_owned(),
            )
        }
    };
    reject_symlinks(root, &candidate)?;
    match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err("plan path is not a regular file".to_owned());
            }
            let canonical = std::fs::canonicalize(&candidate)
                .map_err(|error| safe_io_error("cannot resolve plan file", &error))?;
            require_real_root_or_file(config, &canonical)?;
            Ok(canonical)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let ancestor = nearest_existing_ancestor(&candidate)?;
            let canonical = std::fs::canonicalize(&ancestor)
                .map_err(|error| safe_io_error("cannot resolve plan parent", &error))?;
            if exact_file.is_none() {
                require_real_root(config, &canonical)?;
            } else if candidate.parent() != Some(canonical.as_path()) {
                return Err("allowed plan path has an unexpected parent".to_owned());
            }
            Ok(candidate)
        }
        Err(error) => Err(safe_io_error("cannot inspect plan file", &error)),
    }
}

fn normalized_candidate(config: &LocalToolConfig, input: &Path) -> Result<PathBuf, String> {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        config.cwd().join(input)
    };
    normalize_absolute(&absolute)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("path did not resolve to an absolute path".to_owned());
    }
    let mut prefix = None;
    let mut parts = Vec::<OsString>::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_owned()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err("path escapes the filesystem root".to_owned());
                }
            }
        }
    }
    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    normalized.push(std::path::MAIN_SEPARATOR.to_string());
    normalized.extend(parts);
    Ok(normalized)
}

fn roots(config: &LocalToolConfig) -> impl Iterator<Item = &Path> {
    std::iter::once(config.cwd()).chain(config.additional_dirs().iter().map(PathBuf::as_path))
}

fn lexical_root<'a>(config: &'a LocalToolConfig, candidate: &Path) -> Option<&'a Path> {
    roots(config).find(|root| candidate.starts_with(root))
}

fn require_real_root(config: &LocalToolConfig, canonical: &Path) -> Result<(), String> {
    if roots(config).any(|root| canonical.starts_with(root)) {
        Ok(())
    } else {
        Err("path resolves outside the configured workspace roots".to_owned())
    }
}

fn exact_allowed_file<'a>(config: &'a LocalToolConfig, candidate: &Path) -> Option<&'a Path> {
    config
        .allowed_writable_files()
        .iter()
        .map(PathBuf::as_path)
        .find(|allowed| *allowed == candidate)
}

fn require_real_root_or_file(config: &LocalToolConfig, canonical: &Path) -> Result<(), String> {
    if exact_allowed_file(config, canonical).is_some() {
        Ok(())
    } else {
        require_real_root(config, canonical)
    }
}

fn reject_symlinks(root: &Path, candidate: &Path) -> Result<(), String> {
    let relative = candidate
        .strip_prefix(root)
        .map_err(|_| "path is outside its workspace root".to_owned())?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("refusing to access media or plan data through a symlink".to_owned())
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => return Err(safe_io_error("cannot inspect local path", &error)),
        }
    }
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut current = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "path has no parent directory".to_owned())?;
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => return Ok(current),
            Ok(_) => return Err("plan parent is not a directory".to_owned()),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if !current.pop() {
                    return Err("plan path has no existing parent".to_owned());
                }
            }
            Err(error) => return Err(safe_io_error("cannot inspect plan parent", &error)),
        }
    }
}

fn is_sensitive_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".env.example" | ".env.sample" | ".env.template"
    ) || matches!(
        lower.as_str(),
        "id_rsa.pub" | "id_ed25519.pub" | "id_ecdsa.pub"
    ) {
        return false;
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    const PREFIXES: [&str; 4] = ["id_rsa", "id_ed25519", "id_ecdsa", "credentials"];
    const DOT_VARIANTS: [&str; 10] = [
        ".bak",
        ".backup",
        ".copy",
        ".disabled",
        ".key",
        ".old",
        ".orig",
        ".pem",
        ".save",
        ".tmp",
    ];
    for prefix in PREFIXES {
        if lower == prefix {
            return true;
        }
        if let Some(suffix) = lower.strip_prefix(prefix) {
            if suffix.starts_with(['-', '_']) || DOT_VARIANTS.contains(&suffix) {
                return true;
            }
        }
    }
    let comparable = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    [".aws/credentials", ".gcp/credentials"]
        .iter()
        .any(|suffix| {
            comparable.ends_with(&format!("/{suffix}"))
                || comparable.contains(&format!("/{suffix}/"))
        })
}

fn read_small_text_sync(path: &Path, limit: u64) -> Result<String, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| safe_io_error("cannot inspect plan file", &error))?;
    if metadata.len() > limit {
        return Err("plan file exceeds its byte limit".to_owned());
    }
    let bytes =
        std::fs::read(path).map_err(|error| safe_io_error("cannot read plan file", &error))?;
    String::from_utf8(bytes).map_err(|_| "plan file is not UTF-8 text".to_owned())
}

async fn read_small_text_async(
    path: &Path,
    limit: u64,
    cancellation: &crate::CancellationToken,
) -> Result<String, String> {
    let bytes = read_limited_bytes(path, limit, cancellation).await?;
    String::from_utf8(bytes).map_err(|_| "plan file is not UTF-8 text".to_owned())
}

async fn read_media_bytes(
    path: &Path,
    limit: u64,
    cancellation: &crate::CancellationToken,
) -> Result<Vec<u8>, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| safe_io_error("cannot inspect media file", &error))?;
    if metadata.len() > limit {
        return Err(format!("media file exceeds the {limit} byte limit"));
    }
    read_limited_bytes(path, limit, cancellation).await
}

async fn read_limited_bytes(
    path: &Path,
    limit: u64,
    cancellation: &crate::CancellationToken,
) -> Result<Vec<u8>, String> {
    let read = async {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|error| safe_io_error("cannot open local file", &error))?;
        let mut reader = file.take(limit.saturating_add(1));
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| safe_io_error("cannot read local file", &error))?;
        if bytes.len() as u64 > limit {
            return Err(format!("file exceeds the {limit} byte limit"));
        }
        Ok(bytes)
    };
    tokio::select! {
        _ = cancellation.cancelled() => Err("operation cancelled".to_owned()),
        result = read => result,
    }
}

fn sniff_media(bytes: &[u8]) -> Option<SniffedMedia> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(SniffedMedia {
            kind: MediaKind::Image,
            mime: "image/png",
        });
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(SniffedMedia {
            kind: MediaKind::Image,
            mime: "image/jpeg",
        });
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(SniffedMedia {
            kind: MediaKind::Image,
            mime: "image/gif",
        });
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(SniffedMedia {
            kind: MediaKind::Image,
            mime: "image/webp",
        });
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        let mime = match brand {
            b"qt  " => "video/quicktime",
            b"isom" | b"iso2" | b"mp41" | b"mp42" | b"avc1" | b"M4V " => "video/mp4",
            _ => return None,
        };
        return Some(SniffedMedia {
            kind: MediaKind::Video,
            mime,
        });
    }
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3])
        && bytes[..bytes.len().min(128)]
            .windows(4)
            .any(|window| window.eq_ignore_ascii_case(b"webm"))
    {
        return Some(SniffedMedia {
            kind: MediaKind::Video,
            mime: "video/webm",
        });
    }
    None
}

fn looks_textual(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(4_096)];
    !sample.contains(&0) && std::str::from_utf8(sample).is_ok()
}

fn escape_media_path(path: &str) -> String {
    path.chars()
        .take(4_096)
        .flat_map(|character| match character {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '"' => "&quot;".chars().collect(),
            '<' => "&lt;".chars().collect(),
            '>' => "&gt;".chars().collect(),
            other => vec![other],
        })
        .collect()
}

fn safe_io_error(context: &str, error: &std::io::Error) -> String {
    format!("{context}: {}", error.kind())
}

fn safe_error(context: &str, error: &str) -> String {
    let summary = error
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(500)
        .collect::<String>();
    format!("{context}: {summary}")
}

fn completed_update(text: &str) -> ToolUpdate {
    ToolUpdate {
        kind: ToolUpdateKind::Progress,
        text: Some(text.chars().take(500).collect()),
        percent: Some(100.0),
        custom_kind: None,
        custom_data: None,
    }
}

fn text_result(text: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(text.into()),
        is_error: false,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

fn text_json(value: Value) -> Result<ExecutableToolResult, ToolError> {
    let text = serde_json::to_string(&value)
        .map_err(|_| ToolError::Execute("tool output could not be serialized".to_owned()))?;
    if text.len() > 128 * 1024 {
        return Ok(error_result("tool output exceeds its byte limit"));
    }
    Ok(text_result(text))
}

fn error_result(text: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(text.into()),
        is_error: true,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, ToolError> {
    serde_json::from_value(value.clone()).map_err(|error| ToolError::InvalidArguments {
        path: "$".to_owned(),
        message: error.to_string(),
    })
}

fn invalid_arguments(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        path: "$".to_owned(),
        message: message.into(),
    }
}

fn cancelled(tool: &str) -> ToolError {
    ToolError::Execute(format!("{tool} cancelled"))
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
