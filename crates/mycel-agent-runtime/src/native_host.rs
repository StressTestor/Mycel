//! In-process native child-agent host for orchestration tools.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use mycel_agent_protocol::{ContentPart, PromptOrigin, Role, ThinkingEffort};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    CancellationToken, CapabilitySet, NativeAgentFuture, NativeAgentOperation, NativeAgentRequest,
    NativeAgentResult, NativeStopFuture, NativeSubagentHost, OrchestrationError,
    OrchestrationPorts, Runtime, SessionHandle, SessionId, SessionOptions, TurnEngine, TurnInput,
    TurnOutcomeReason, WorkerProfile,
};

const NATIVE_HOST_SCOPE: &str = "native-child-host";
const DEFAULT_MAX_DEPTH: u8 = 3;
const DEFAULT_MAX_PROMPT_CHARS: usize = 128 * 1024;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 200_000;
const MAX_SYSTEM_PROMPT_CHARS: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeChildStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl NativeChildStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeChildState {
    pub agent_id: String,
    pub parent_agent_id: String,
    pub session_id: SessionId,
    pub profile: WorkerProfile,
    pub depth: u8,
    pub status: NativeChildStatus,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeChildBoard {
    pub children: BTreeMap<String, NativeChildState>,
}

#[derive(Clone, Debug)]
pub struct NativeChildContext {
    pub agent_id: String,
    pub parent_agent_id: String,
    pub session_id: SessionId,
    pub profile: WorkerProfile,
    pub depth: u8,
    pub resume: bool,
}

/// Factory for canonical child session options. Implementations configure the
/// child's permission, question, and hook ports. The returned ID must equal
/// `context.session_id`.
pub trait NativeSessionOptionsFactory: Send + Sync {
    fn build(&self, context: &NativeChildContext) -> Result<SessionOptions, String>;
}

/// Fully constructed child turn runtime. `effective_capabilities` is checked
/// against the requested worker profile before the provider is called, and
/// every registered tool must be named by its `tools` set.
pub struct NativeTurnRuntime {
    pub engine: Arc<TurnEngine>,
    pub effective_capabilities: CapabilitySet,
    pub system_prompt: String,
    pub thinking_effort: Option<ThinkingEffort>,
    pub max_completion_tokens: Option<u64>,
    pub metadata: BTreeMap<String, Value>,
}

pub trait NativeTurnEngineFactory: Send + Sync {
    fn build(&self, context: &NativeChildContext) -> Result<NativeTurnRuntime, String>;
}

pub struct NativeChildHostDependencies {
    pub runtime: Runtime,
    pub ports: OrchestrationPorts,
    pub sessions: Arc<dyn NativeSessionOptionsFactory>,
    pub turns: Arc<dyn NativeTurnEngineFactory>,
}

pub struct NativeChildHostOptions {
    pub session_namespace: String,
    pub root_agent_id: String,
    pub root_capabilities: CapabilitySet,
    pub root_allow_delegation: bool,
    pub max_depth: u8,
    pub max_prompt_chars: usize,
    pub max_output_chars: usize,
}

impl NativeChildHostOptions {
    pub fn new(
        session_namespace: impl Into<String>,
        root_agent_id: impl Into<String>,
        root_capabilities: CapabilitySet,
    ) -> Self {
        Self {
            session_namespace: session_namespace.into(),
            root_agent_id: root_agent_id.into(),
            root_capabilities,
            root_allow_delegation: true,
            max_depth: DEFAULT_MAX_DEPTH,
            max_prompt_chars: DEFAULT_MAX_PROMPT_CHARS,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
        }
    }
}

#[derive(Clone)]
pub struct NativeChildAgentHost {
    inner: Arc<NativeChildHostInner>,
}

struct NativeChildHostInner {
    dependencies: NativeChildHostDependencies,
    options: NativeChildHostOptions,
    state: Mutex<NativeChildBoard>,
    active: Mutex<BTreeMap<String, ActiveChild>>,
}

#[derive(Clone)]
struct ActiveChild {
    cancellation: CancellationToken,
    session: SessionHandle,
}

impl NativeChildAgentHost {
    pub fn open(
        dependencies: NativeChildHostDependencies,
        options: NativeChildHostOptions,
    ) -> Result<Self, NativeChildHostError> {
        validate_options(&options)?;
        let state = dependencies.ports.restore(NATIVE_HOST_SCOPE)?;
        validate_board(&state, &options)?;
        let host = Self {
            inner: Arc::new(NativeChildHostInner {
                dependencies,
                options,
                state: Mutex::new(state),
                active: Mutex::new(BTreeMap::new()),
            }),
        };
        host.reconcile_lost()?;
        Ok(host)
    }

    pub fn snapshot(&self) -> NativeChildBoard {
        lock(&self.inner.state).clone()
    }

    /// Returns the active IDs a same-process composition may pass to
    /// orchestration reducers. A freshly opened host deliberately returns an
    /// empty set because persisted running work has already been marked lost.
    pub fn active_agent_ids(&self) -> BTreeSet<String> {
        lock(&self.inner.active).keys().cloned().collect()
    }

    fn reconcile_lost(&self) -> Result<(), NativeChildHostError> {
        let mut state = lock(&self.inner.state);
        let lost = state
            .children
            .values()
            .filter(|child| child.status == NativeChildStatus::Running)
            .map(|child| child.agent_id.clone())
            .collect::<Vec<_>>();
        if lost.is_empty() {
            return Ok(());
        }
        let now = self.inner.dependencies.ports.now_ms();
        let mut next = state.clone();
        for id in &lost {
            let child = next.children.get_mut(id).expect("selected child");
            child.status = NativeChildStatus::Lost;
            child.ended_at_ms = Some(now);
            child.reason = Some("runtime restarted without the child executor".to_owned());
        }
        self.commit(
            &mut state,
            next,
            "reconciled_lost",
            None,
            serde_json::json!({"agentIds": lost}),
        )
    }

    fn commit(
        &self,
        state: &mut NativeChildBoard,
        next: NativeChildBoard,
        action: &str,
        entity_id: Option<&str>,
        detail: Value,
    ) -> Result<(), NativeChildHostError> {
        validate_board(&next, &self.inner.options)?;
        let event = self.inner.dependencies.ports.persist(
            NATIVE_HOST_SCOPE,
            action,
            entity_id,
            &next,
            detail,
        )?;
        *state = next;
        self.inner.dependencies.ports.publish(event);
        Ok(())
    }

    fn begin(&self, request: &NativeAgentRequest) -> Result<NativeChildContext, String> {
        validate_request(request, &self.inner.options)?;
        let mut state = lock(&self.inner.state);
        match &request.operation {
            NativeAgentOperation::Spawn { profile } => {
                if state.children.contains_key(&request.agent_id) {
                    return Err("native child agent id already exists".to_owned());
                }
                let (parent_capabilities, parent_allows_delegation, parent_depth) =
                    if request.parent_agent_id == self.inner.options.root_agent_id {
                        (
                            self.inner.options.root_capabilities.clone(),
                            self.inner.options.root_allow_delegation,
                            0,
                        )
                    } else {
                        let parent = state
                            .children
                            .get(&request.parent_agent_id)
                            .ok_or_else(|| "native child parent was not found".to_owned())?;
                        if parent.status != NativeChildStatus::Running {
                            return Err("native child parent is not running".to_owned());
                        }
                        (
                            parent.profile.capabilities.clone(),
                            parent.profile.allow_delegation,
                            parent.depth,
                        )
                    };
                if !parent_allows_delegation || !parent_capabilities.can_spawn_subagents {
                    return Err("native child recursion is denied by its parent".to_owned());
                }
                if !profile.capabilities.is_subset_of(&parent_capabilities) {
                    return Err("native child capabilities exceed its parent".to_owned());
                }
                let depth = parent_depth.saturating_add(1);
                if depth > self.inner.options.max_depth {
                    return Err("native child depth limit reached".to_owned());
                }
                if depth == self.inner.options.max_depth
                    && (profile.allow_delegation
                        || profile.capabilities.can_spawn_subagents
                        || profile.capabilities.can_swarm
                        || profile.capabilities.can_workflow)
                {
                    return Err("native child at the depth ceiling must be nonrecursive".to_owned());
                }
                let session_id =
                    child_session_id(&self.inner.options.session_namespace, &request.agent_id)?;
                if state
                    .children
                    .values()
                    .any(|child| child.session_id == session_id)
                {
                    return Err("native child session id collision".to_owned());
                }
                let now = self.inner.dependencies.ports.now_ms();
                let child = NativeChildState {
                    agent_id: request.agent_id.clone(),
                    parent_agent_id: request.parent_agent_id.clone(),
                    session_id: session_id.clone(),
                    profile: profile.clone(),
                    depth,
                    status: NativeChildStatus::Running,
                    started_at_ms: now,
                    ended_at_ms: None,
                    reason: None,
                };
                let mut next = state.clone();
                next.children.insert(request.agent_id.clone(), child);
                self.commit(
                    &mut state,
                    next,
                    "spawned",
                    Some(&request.agent_id),
                    serde_json::json!({"parentAgentId": request.parent_agent_id, "depth": depth}),
                )
                .map_err(|error| error.to_string())?;
                Ok(NativeChildContext {
                    agent_id: request.agent_id.clone(),
                    parent_agent_id: request.parent_agent_id.clone(),
                    session_id,
                    profile: profile.clone(),
                    depth,
                    resume: false,
                })
            }
            NativeAgentOperation::Resume { agent_id } => {
                if agent_id != &request.agent_id {
                    return Err("native resume id does not match the request".to_owned());
                }
                let child = state
                    .children
                    .get(agent_id)
                    .cloned()
                    .ok_or_else(|| "native child to resume was not found".to_owned())?;
                if !child.status.is_terminal() {
                    return Err("native child is already running".to_owned());
                }
                if child.parent_agent_id != request.parent_agent_id {
                    return Err("native child resume parent does not match".to_owned());
                }
                let (parent_capabilities, parent_allows_delegation, parent_depth) =
                    if child.parent_agent_id == self.inner.options.root_agent_id {
                        (
                            &self.inner.options.root_capabilities,
                            self.inner.options.root_allow_delegation,
                            0,
                        )
                    } else {
                        let parent = state
                            .children
                            .get(&child.parent_agent_id)
                            .ok_or_else(|| "native child resume parent was not found".to_owned())?;
                        if parent.status != NativeChildStatus::Running {
                            return Err("native child resume parent is not running".to_owned());
                        }
                        (
                            &parent.profile.capabilities,
                            parent.profile.allow_delegation,
                            parent.depth,
                        )
                    };
                if !parent_allows_delegation
                    || !parent_capabilities.can_spawn_subagents
                    || !child.profile.capabilities.is_subset_of(parent_capabilities)
                    || child.depth != parent_depth.saturating_add(1)
                {
                    return Err("native child resume is no longer authorized".to_owned());
                }
                if child.depth == self.inner.options.max_depth
                    && (child.profile.allow_delegation
                        || child.profile.capabilities.can_spawn_subagents
                        || child.profile.capabilities.can_swarm
                        || child.profile.capabilities.can_workflow)
                {
                    return Err("native child resume is recursive at the depth ceiling".to_owned());
                }
                let now = self.inner.dependencies.ports.now_ms();
                let mut next = state.clone();
                let resumed = next.children.get_mut(agent_id).expect("checked child");
                resumed.status = NativeChildStatus::Running;
                resumed.started_at_ms = now;
                resumed.ended_at_ms = None;
                resumed.reason = None;
                self.commit(&mut state, next, "resumed", Some(agent_id), Value::Null)
                    .map_err(|error| error.to_string())?;
                Ok(NativeChildContext {
                    agent_id: child.agent_id,
                    parent_agent_id: child.parent_agent_id,
                    session_id: child.session_id,
                    profile: child.profile,
                    depth: child.depth,
                    resume: true,
                })
            }
        }
    }

    fn settle(
        &self,
        agent_id: &str,
        status: NativeChildStatus,
        reason: Option<&str>,
    ) -> Result<(), String> {
        let mut state = lock(&self.inner.state);
        let Some(current) = state.children.get(agent_id) else {
            return Ok(());
        };
        if current.status != NativeChildStatus::Running {
            return Ok(());
        }
        let mut next = state.clone();
        let child = next.children.get_mut(agent_id).expect("checked child");
        child.status = status;
        child.ended_at_ms = Some(self.inner.dependencies.ports.now_ms());
        child.reason = reason.map(bounded_reason);
        self.commit(
            &mut state,
            next,
            match status {
                NativeChildStatus::Completed => "completed",
                NativeChildStatus::Failed => "failed",
                NativeChildStatus::Cancelled => "cancelled",
                NativeChildStatus::Lost => "lost",
                NativeChildStatus::Running => "running",
            },
            Some(agent_id),
            Value::Null,
        )
        .map_err(|error| error.to_string())
    }

    async fn execute_inner(self, request: NativeAgentRequest) -> Result<NativeAgentResult, String> {
        let context = self.begin(&request)?;
        if request.cancellation.is_cancelled() {
            self.settle(
                &context.agent_id,
                NativeChildStatus::Cancelled,
                Some("cancelled before child session startup"),
            )?;
            return Err("native child cancelled".to_owned());
        }
        let options = match self.inner.dependencies.sessions.build(&context) {
            Ok(options) if options.id == context.session_id => options,
            Ok(_) => {
                self.settle(
                    &context.agent_id,
                    NativeChildStatus::Failed,
                    Some("session factory returned the wrong session id"),
                )?;
                return Err("native child session factory returned the wrong id".to_owned());
            }
            Err(error) => {
                let error = bounded_error("native child session factory failed", &error);
                self.settle(&context.agent_id, NativeChildStatus::Failed, Some(&error))?;
                return Err(error);
            }
        };
        let session_result = if context.resume {
            match self
                .inner
                .dependencies
                .runtime
                .get_session(&context.session_id)
                .await
            {
                Some(session) => Ok(session),
                None => {
                    self.inner
                        .dependencies
                        .runtime
                        .resume_session(options)
                        .await
                }
            }
        } else {
            self.inner
                .dependencies
                .runtime
                .create_session(options)
                .await
        };
        let session = match session_result {
            Ok(session) => session,
            Err(error) => {
                let operation = if context.resume { "resume" } else { "creation" };
                let error = bounded_error(
                    &format!("native child {operation} failed"),
                    &error.to_string(),
                );
                self.settle(&context.agent_id, NativeChildStatus::Failed, Some(&error))?;
                return Err(error);
            }
        };
        let turn = match self.inner.dependencies.turns.build(&context) {
            Ok(turn) => turn,
            Err(error) => {
                let error = bounded_error("native child turn factory failed", &error);
                session.cancel();
                let _ = session.close().await;
                self.settle(&context.agent_id, NativeChildStatus::Failed, Some(&error))?;
                return Err(error);
            }
        };
        if let Err(error) = validate_turn_runtime(&turn, &context, self.inner.options.max_depth) {
            session.cancel();
            let close_error = session.close().await.err();
            let error = match close_error {
                Some(close) => format!("{error}; child session close failed: {close}"),
                None => error,
            };
            let error = bounded_error("native child turn runtime rejected", &error);
            self.settle(&context.agent_id, NativeChildStatus::Failed, Some(&error))?;
            return Err(error);
        }
        let before = session.snapshot().await.state.context.history().len();
        let activation_error = {
            let state = lock(&self.inner.state);
            match state.children.get(&context.agent_id) {
                Some(child) if child.status == NativeChildStatus::Running => {
                    let mut active = lock(&self.inner.active);
                    if active.contains_key(&context.agent_id) {
                        Some("duplicate active native child")
                    } else {
                        active.insert(
                            context.agent_id.clone(),
                            ActiveChild {
                                cancellation: request.cancellation.clone(),
                                session: session.clone(),
                            },
                        );
                        None
                    }
                }
                Some(_) => Some("native child was stopped during startup"),
                None => Some("native child state disappeared during startup"),
            }
        };
        if let Some(error) = activation_error {
            session.cancel();
            let _ = session.close().await;
            self.settle(&context.agent_id, NativeChildStatus::Failed, Some(error))?;
            return Err(error.to_owned());
        }
        let input = TurnInput {
            content: vec![ContentPart::text(request.prompt)],
            origin: PromptOrigin::SystemTrigger {
                name: "native_subagent".to_owned(),
            },
            system_prompt: turn.system_prompt,
            thinking_effort: turn.thinking_effort,
            max_completion_tokens: turn.max_completion_tokens,
            metadata: turn.metadata,
        };
        let outcome = turn
            .engine
            .run_turn(&session, input, request.cancellation.clone())
            .await;
        let owns_completion = lock(&self.inner.active).remove(&context.agent_id).is_some();
        if !owns_completion {
            return Err("native child was stopped".to_owned());
        }
        let cancelled = request.cancellation.is_cancelled();
        let result = match outcome {
            Ok(outcome)
                if matches!(
                    outcome.reason,
                    TurnOutcomeReason::Completed
                        | TurnOutcomeReason::MaxTokens
                        | TurnOutcomeReason::ToolStopped
                ) =>
            {
                let snapshot = session.snapshot().await;
                let output = render_child_output(
                    &snapshot.state.context.history()[before..],
                    self.inner.options.max_output_chars,
                );
                Ok(NativeAgentResult { output })
            }
            Ok(outcome) => Err(format!("native child stopped with {:?}", outcome.reason)),
            Err(error) => Err(bounded_error(
                "native child turn failed",
                &error.to_string(),
            )),
        };
        let close = session.close().await;
        match (result, close) {
            (Ok(result), Ok(())) => {
                self.settle(&context.agent_id, NativeChildStatus::Completed, None)?;
                Ok(result)
            }
            (result, close) => {
                let error = match (result.err(), close.err()) {
                    (Some(error), Some(close)) => {
                        format!("{error}; child session close failed: {close}")
                    }
                    (Some(error), None) => error,
                    (None, Some(close)) => format!("native child session close failed: {close}"),
                    (None, None) => "native child failed".to_owned(),
                };
                let error = bounded_error("native child failed", &error);
                self.settle(
                    &context.agent_id,
                    if cancelled {
                        NativeChildStatus::Cancelled
                    } else {
                        NativeChildStatus::Failed
                    },
                    Some(&error),
                )?;
                Err(error)
            }
        }
    }

    async fn stop_inner(self, agent_id: String, reason: String) -> Result<(), String> {
        let active = lock(&self.inner.active).remove(&agent_id);
        let close_result = if let Some(active) = active {
            active.cancellation.cancel();
            active.session.cancel();
            active
                .session
                .close()
                .await
                .map_err(|error| bounded_error("native child stop failed", &error.to_string()))
        } else if let Some(child) = self.snapshot().children.get(&agent_id) {
            if let Some(session) = self
                .inner
                .dependencies
                .runtime
                .get_session(&child.session_id)
                .await
            {
                session.cancel();
                session
                    .close()
                    .await
                    .map_err(|error| bounded_error("native child stop failed", &error.to_string()))
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        let settle_result = self.settle(
            &agent_id,
            NativeChildStatus::Cancelled,
            Some(&bounded_reason(&reason)),
        );
        match (close_result, settle_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(close), Ok(())) => Err(close),
            (Ok(()), Err(settle)) => Err(settle),
            (Err(close), Err(settle)) => Err(format!("{close}; {settle}")),
        }
    }
}

impl NativeSubagentHost for NativeChildAgentHost {
    fn execute(&self, request: NativeAgentRequest) -> NativeAgentFuture {
        let host = self.clone();
        Box::pin(async move { host.execute_inner(request).await })
    }

    fn stop(&self, agent_id: String, reason: String) -> NativeStopFuture {
        let host = self.clone();
        Box::pin(async move { host.stop_inner(agent_id, reason).await })
    }
}

fn validate_options(options: &NativeChildHostOptions) -> Result<(), NativeChildHostError> {
    if options.session_namespace.is_empty()
        || options.session_namespace.len() > 160
        || options.session_namespace.chars().any(char::is_control)
    {
        return Err(NativeChildHostError::Config(
            "native child session namespace is invalid".to_owned(),
        ));
    }
    if options.root_agent_id.is_empty() || options.root_agent_id.len() > 160 {
        return Err(NativeChildHostError::Config(
            "native child root agent id is invalid".to_owned(),
        ));
    }
    if options.max_depth == 0 || options.max_depth > 8 {
        return Err(NativeChildHostError::Config(
            "native child max depth must be between 1 and 8".to_owned(),
        ));
    }
    if options.max_prompt_chars == 0 || options.max_prompt_chars > 1024 * 1024 {
        return Err(NativeChildHostError::Config(
            "native child prompt limit is invalid".to_owned(),
        ));
    }
    if options.max_output_chars == 0 || options.max_output_chars > 1024 * 1024 {
        return Err(NativeChildHostError::Config(
            "native child output limit is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_board(
    board: &NativeChildBoard,
    options: &NativeChildHostOptions,
) -> Result<(), NativeChildHostError> {
    if board.children.len() > 10_000 {
        return Err(NativeChildHostError::State(
            "native child count exceeds its safety limit".to_owned(),
        ));
    }
    let mut sessions = BTreeSet::new();
    for (id, child) in &board.children {
        if id != &child.agent_id || id.is_empty() || id.len() > 160 {
            return Err(NativeChildHostError::State(
                "native child state has an invalid agent id".to_owned(),
            ));
        }
        if child.depth == 0 || child.depth > options.max_depth {
            return Err(NativeChildHostError::State(
                "native child state has an invalid depth".to_owned(),
            ));
        }
        if child.profile.name.trim().is_empty()
            || child.profile.name.len() > 160
            || child.profile.name.chars().any(char::is_control)
        {
            return Err(NativeChildHostError::State(
                "native child state has an invalid profile name".to_owned(),
            ));
        }
        let (parent_capabilities, parent_allows_delegation, parent_depth) =
            if child.parent_agent_id == options.root_agent_id {
                (&options.root_capabilities, options.root_allow_delegation, 0)
            } else {
                let parent = board.children.get(&child.parent_agent_id).ok_or_else(|| {
                    NativeChildHostError::State(
                        "native child state references a missing parent".to_owned(),
                    )
                })?;
                (
                    &parent.profile.capabilities,
                    parent.profile.allow_delegation,
                    parent.depth,
                )
            };
        if !parent_allows_delegation
            || !parent_capabilities.can_spawn_subagents
            || !child.profile.capabilities.is_subset_of(parent_capabilities)
            || child.depth != parent_depth.saturating_add(1)
        {
            return Err(NativeChildHostError::State(
                "native child state violates its parent capability boundary".to_owned(),
            ));
        }
        if child.depth == options.max_depth
            && (child.profile.allow_delegation
                || child.profile.capabilities.can_spawn_subagents
                || child.profile.capabilities.can_swarm
                || child.profile.capabilities.can_workflow)
        {
            return Err(NativeChildHostError::State(
                "native child state is recursive at the depth ceiling".to_owned(),
            ));
        }
        if !sessions.insert(child.session_id.clone()) {
            return Err(NativeChildHostError::State(
                "native child state has duplicate session ids".to_owned(),
            ));
        }
        if child
            .reason
            .as_ref()
            .is_some_and(|reason| reason.len() > 1_000)
        {
            return Err(NativeChildHostError::State(
                "native child state has an oversized reason".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_request(
    request: &NativeAgentRequest,
    options: &NativeChildHostOptions,
) -> Result<(), String> {
    if request.agent_id.is_empty()
        || request.agent_id.len() > 160
        || request.agent_id.chars().any(char::is_control)
    {
        return Err("native child agent id is invalid".to_owned());
    }
    if request.parent_agent_id.is_empty() || request.parent_agent_id.len() > 160 {
        return Err("native child parent id is invalid".to_owned());
    }
    if request.description.chars().count() > 2_000 {
        return Err("native child description exceeds its limit".to_owned());
    }
    if request.prompt.is_empty() || request.prompt.chars().count() > options.max_prompt_chars {
        return Err("native child prompt is empty or exceeds its limit".to_owned());
    }
    Ok(())
}

fn validate_turn_runtime(
    turn: &NativeTurnRuntime,
    context: &NativeChildContext,
    max_depth: u8,
) -> Result<(), String> {
    if turn.system_prompt.chars().count() > MAX_SYSTEM_PROMPT_CHARS {
        return Err("native child system prompt exceeds its limit".to_owned());
    }
    if !turn
        .effective_capabilities
        .is_subset_of(&context.profile.capabilities)
    {
        return Err("native child turn factory escalated capabilities".to_owned());
    }
    if context.depth == max_depth
        && (turn.effective_capabilities.can_spawn_subagents
            || turn.effective_capabilities.can_swarm
            || turn.effective_capabilities.can_workflow)
    {
        return Err("native child turn runtime is recursive at the depth ceiling".to_owned());
    }
    let allowed = &turn.effective_capabilities.tools;
    if turn
        .engine
        .tool_definitions()
        .iter()
        .any(|definition| !allowed.contains(&definition.name))
    {
        return Err("native child turn runtime registered an undeclared tool".to_owned());
    }
    Ok(())
}

fn child_session_id(namespace: &str, agent_id: &str) -> Result<SessionId, String> {
    let hash = stable_hash(&format!("{namespace}\0{agent_id}"));
    SessionId::new(format!("child-{hash:016x}")).map_err(|error| error.to_string())
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn render_child_output(entries: &[crate::ContextEntry], maximum: usize) -> String {
    let mut output = String::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.message.role == Role::Assistant)
    {
        for part in &entry.message.content {
            if let ContentPart::Text { text } = part {
                append_bounded(&mut output, text, maximum);
            }
        }
    }
    output
}

fn append_bounded(output: &mut String, text: &str, maximum: usize) {
    let remaining = maximum.saturating_sub(output.chars().count());
    if remaining == 0 {
        return;
    }
    if !output.is_empty() && !text.is_empty() {
        output.push('\n');
    }
    output.extend(text.chars().take(remaining));
}

fn bounded_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(1_000)
        .collect()
}

fn bounded_error(context: &str, error: &str) -> String {
    format!("{context}: {}", bounded_reason(error))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, thiserror::Error)]
pub enum NativeChildHostError {
    #[error("native child host configuration failed: {0}")]
    Config(String),
    #[error("native child host state is invalid: {0}")]
    State(String),
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}
