use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use mycel_agent_protocol::{
    AgentEvent, AgentRecord, CompactionResult as ProtocolCompactionResult, CompactionTrigger,
    ContentPart, Event, LoopContentPart, LoopEvent, Message, PermissionApprovalResultRecord,
    PermissionMode, PermissionRule, PromptOrigin, RecordKind, Role, TokenUsage,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{
    replay_records, AgentId, AgentState, ApprovalPort, Authorization, CancellationToken,
    ContextEntry, EventBus, EventBusError, EventReceiver, HookRunReport, HookRunner,
    LifecycleHookInput, PermissionEngine, PortError, PreToolPermissionPort, QuestionPort,
    QuestionRequest, QuestionResponse, RecordLog, RecordLogError, ReplayError, SessionId,
    ToolHookEvent, ToolPermissionRequest,
};

const DEFAULT_EVENT_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    root: PathBuf,
    active: Mutex<BTreeMap<SessionId, Arc<SessionInner>>>,
}

impl Runtime {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                root: root.into(),
                active: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub async fn create_session(
        &self,
        options: SessionOptions,
    ) -> Result<SessionHandle, SessionError> {
        let mut active = self.inner.active.lock().await;
        if let Some(existing) = active.get(&options.id) {
            return Ok(SessionHandle {
                inner: Arc::clone(existing),
            });
        }
        let record_path = self.record_path(&options.id);
        let (records, read) = RecordLog::open(&record_path).await?;
        if !read.records.is_empty() {
            return Err(SessionError::AlreadyExists(options.id));
        }
        records.ensure_metadata().await?;
        let initial_mode = options.initial_permission_mode;
        let inner = Arc::new(SessionInner::new(
            options,
            records,
            AgentState::default(),
            VecDeque::new(),
            None,
            Arc::downgrade(&self.inner),
        )?);
        let handle = SessionHandle { inner };
        if initial_mode != PermissionMode::Manual {
            handle.set_permission_mode(initial_mode).await?;
        }
        active.insert(handle.inner.id.clone(), Arc::clone(&handle.inner));
        drop(active);
        handle
            .run_lifecycle_hook(
                ToolHookEvent::SessionStart,
                "startup",
                BTreeMap::from([("source".to_owned(), json!("startup"))]),
                &CancellationToken::new(),
            )
            .await;
        Ok(handle)
    }

    pub async fn resume_session(
        &self,
        options: SessionOptions,
    ) -> Result<SessionHandle, SessionError> {
        let mut active = self.inner.active.lock().await;
        if let Some(existing) = active.get(&options.id) {
            return Ok(SessionHandle {
                inner: Arc::clone(existing),
            });
        }
        let record_path = self.record_path(&options.id);
        if !tokio::fs::try_exists(&record_path)
            .await
            .map_err(|source| SessionError::RecordIo {
                path: record_path.clone(),
                message: source.to_string(),
            })?
        {
            return Err(SessionError::NotFound(options.id));
        }
        let (records, read) = RecordLog::open(&record_path).await?;
        if read.records.is_empty() {
            return Err(SessionError::EmptyRecordLog(record_path));
        }
        let prepared = read.prepare_replay()?;
        let pending_steers = recover_pending_steers(&prepared.records)?;
        let replay = replay_records(&prepared.records)?;
        if prepared.rewrite_after_replay {
            // Migration cannot replace the source until every record has been
            // reduced successfully.
            records.rewrite(&prepared.records).await?;
        }
        let mut state = replay.state;
        if !prepared
            .records
            .iter()
            .any(|record| record.kind() == Some(RecordKind::PermissionSetMode))
        {
            state.permission_mode = options.initial_permission_mode;
        }
        let mut closure_preview = state.context.clone();
        let resume_closures = closure_preview.finish_resume()?;
        let inner = Arc::new(SessionInner::new(
            options,
            records,
            state,
            pending_steers,
            prepared.warning,
            Arc::downgrade(&self.inner),
        )?);
        let handle = SessionHandle { inner };
        if handle.snapshot().await.state.compaction == crate::CompactionState::Running {
            // A process can die after `full_compaction.begin` or after applying
            // the folded context but before the completion marker. The durable
            // context is already authoritative; close only the orphaned
            // lifecycle so resume never reports a compactor that cannot exist.
            handle.cancel_compaction().await?;
        }
        // Closing a genuinely interrupted tail is a new durable mutation, not
        // a replay side effect. It happens only after migration rewrite.
        for event in resume_closures {
            handle.append_loop_event(event).await?;
        }
        active.insert(handle.inner.id.clone(), Arc::clone(&handle.inner));
        drop(active);
        handle
            .run_lifecycle_hook(
                ToolHookEvent::SessionStart,
                "resume",
                BTreeMap::from([("source".to_owned(), json!("resume"))]),
                &CancellationToken::new(),
            )
            .await;
        Ok(handle)
    }

    /// Clone a closed session's complete durable history into a new session.
    ///
    /// Forking an active source is rejected so the copied log cannot race an
    /// in-flight durable append. The target must not already exist. The fork
    /// is resumed through the normal replay path after its log is committed,
    /// so every migration and replay invariant is applied to the copy.
    pub async fn fork_session(
        &self,
        source: &SessionId,
        options: SessionOptions,
    ) -> Result<SessionHandle, SessionError> {
        let target = options.id.clone();
        self.fork_session_records(source, &target).await?;
        self.resume_session(options).await
    }

    /// Copy a closed source log into a new durable session without opening
    /// the target. Hosts use this when a fork is immediately handed to a new
    /// runtime composition with different dialog and tool ports.
    pub async fn fork_session_records(
        &self,
        source: &SessionId,
        target: &SessionId,
    ) -> Result<(), SessionError> {
        if source == target {
            return Err(SessionError::ForkSameSession(source.clone()));
        }
        {
            let active = self.inner.active.lock().await;
            if active.contains_key(source) {
                return Err(SessionError::ForkSourceActive(source.clone()));
            }
            if active.contains_key(target) {
                return Err(SessionError::AlreadyExists(target.clone()));
            }
        }

        let source_path = self.record_path(source);
        if !tokio::fs::try_exists(&source_path)
            .await
            .map_err(|source| SessionError::RecordIo {
                path: source_path.clone(),
                message: source.to_string(),
            })?
        {
            return Err(SessionError::NotFound(source.clone()));
        }
        let target_path = self.record_path(target);
        if tokio::fs::try_exists(&target_path)
            .await
            .map_err(|source| SessionError::RecordIo {
                path: target_path.clone(),
                message: source.to_string(),
            })?
        {
            return Err(SessionError::AlreadyExists(target.clone()));
        }

        let (_, source_read) = RecordLog::open(&source_path).await?;
        if source_read.records.is_empty() {
            return Err(SessionError::EmptyRecordLog(source_path));
        }
        let prepared = source_read.prepare_replay()?;
        // Validate the complete fork before creating its path.
        replay_records(&prepared.records)?;
        let (target_log, target_read) = RecordLog::open(&target_path).await?;
        if !target_read.records.is_empty() {
            return Err(SessionError::AlreadyExists(target.clone()));
        }
        target_log.rewrite(&prepared.records).await?;
        target_log.close().await?;
        Ok(())
    }

    pub async fn get_session(&self, id: &SessionId) -> Option<SessionHandle> {
        self.inner
            .active
            .lock()
            .await
            .get(id)
            .cloned()
            .map(|inner| SessionHandle { inner })
    }

    pub async fn active_session_ids(&self) -> Vec<SessionId> {
        self.inner.active.lock().await.keys().cloned().collect()
    }

    fn record_path(&self, id: &SessionId) -> PathBuf {
        self.inner
            .root
            .join(id.as_str())
            .join("agents")
            .join("main")
            .join("records.jsonl")
    }
}

pub struct SessionOptions {
    pub id: SessionId,
    pub initial_permission_mode: PermissionMode,
    pub permission_rules: Vec<PermissionRule>,
    pub approval_port: Option<Arc<dyn ApprovalPort>>,
    pub question_port: Option<Arc<dyn QuestionPort>>,
    pub pre_tool_permission_port: Option<Arc<dyn PreToolPermissionPort>>,
    pub hooks: HookRunner,
    pub event_capacity: usize,
}

impl SessionOptions {
    pub fn new(id: SessionId) -> Self {
        Self {
            id,
            initial_permission_mode: PermissionMode::Manual,
            permission_rules: Vec::new(),
            approval_port: None,
            question_port: None,
            pre_tool_permission_port: None,
            hooks: HookRunner::new(),
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    id: SessionId,
    main_agent_id: AgentId,
    records: Arc<RecordLog>,
    mutation: Mutex<SessionMutation>,
    steer_gate: Mutex<()>,
    turn_gate: Arc<Mutex<()>>,
    permission: PermissionEngine,
    events: EventBus,
    cancellation: CancellationToken,
    approval_port: Option<Arc<dyn ApprovalPort>>,
    question_port: Option<Arc<dyn QuestionPort>>,
    hooks: HookRunner,
    warning: Option<String>,
    runtime: Weak<RuntimeInner>,
}

struct SessionMutation {
    state: AgentState,
    pending_steers: VecDeque<PendingSteer>,
    active_turn: bool,
    closed: bool,
    session_end_hook_ran: bool,
    poisoned: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingSteer {
    content: Vec<ContentPart>,
    origin: PromptOrigin,
}

impl SessionInner {
    fn new(
        options: SessionOptions,
        records: Arc<RecordLog>,
        state: AgentState,
        pending_steers: VecDeque<PendingSteer>,
        warning: Option<String>,
        runtime: Weak<RuntimeInner>,
    ) -> Result<Self, SessionError> {
        let permission = PermissionEngine::new(
            state.permission_mode,
            options.permission_rules,
            state.session_approval_rules.iter().cloned(),
            options.pre_tool_permission_port,
        );
        Ok(Self {
            id: options.id,
            main_agent_id: AgentId::main(),
            records,
            mutation: Mutex::new(SessionMutation {
                state,
                pending_steers,
                active_turn: false,
                closed: false,
                session_end_hook_ran: false,
                poisoned: None,
            }),
            steer_gate: Mutex::new(()),
            turn_gate: Arc::new(Mutex::new(())),
            permission,
            events: EventBus::new(options.event_capacity)?,
            cancellation: CancellationToken::new(),
            approval_port: options.approval_port,
            question_port: options.question_port,
            hooks: options.hooks,
            warning,
            runtime,
        })
    }
}

impl SessionHandle {
    pub fn id(&self) -> &SessionId {
        &self.inner.id
    }

    pub fn main_agent_id(&self) -> &AgentId {
        &self.inner.main_agent_id
    }

    pub fn record_path(&self) -> &Path {
        self.inner.records.path()
    }

    pub fn warning(&self) -> Option<&str> {
        self.inner.warning.as_deref()
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.inner.cancellation.clone()
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.inner.events.subscribe()
    }

    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        let mutation = self.inner.mutation.lock().await;
        SessionSnapshot {
            id: self.inner.id.clone(),
            main_agent_id: self.inner.main_agent_id.clone(),
            state: mutation.state.clone(),
            closed: mutation.closed,
            poisoned: mutation.poisoned.clone(),
            warning: self.inner.warning.clone(),
        }
    }

    pub async fn append_context(&self, entry: ContextEntry) -> Result<(), SessionError> {
        let record = record_with_value(
            RecordKind::ContextAppendMessage,
            json!({ "message": entry }),
        )?;
        self.commit(record, None).await.map(|_| ())
    }

    pub async fn append_user_message(
        &self,
        text: impl Into<String>,
        origin: PromptOrigin,
    ) -> Result<(), SessionError> {
        self.append_context(ContextEntry::user(text, origin)).await
    }

    /// Durably buffer user input for the active turn. The turn engine flushes
    /// buffered messages at the next provider-step boundary. Completion and
    /// steering share one gate so an accepted steer cannot race behind the
    /// terminal record and disappear.
    pub async fn steer(
        &self,
        content: Vec<ContentPart>,
        origin: PromptOrigin,
    ) -> Result<(), SessionError> {
        if content.is_empty() {
            return Err(SessionError::EmptyTurnInput);
        }
        let _gate = self.inner.steer_gate.lock().await;
        {
            let mutation = self.inner.mutation.lock().await;
            if !mutation.active_turn {
                return Err(SessionError::NoActiveTurn);
            }
        }
        let pending = PendingSteer {
            content: content.clone(),
            origin: origin.clone(),
        };
        let record = record_with_value(
            RecordKind::TurnSteer,
            json!({ "input": content, "origin": origin }),
        )?;
        self.commit_guarded(record, None, CommitGuard::None, Some(pending))
            .await
            .map(|_| ())
    }

    /// Remove the newest `count` real user turns and their trailing model/tool
    /// context. Undo is durable and cannot race an active turn.
    pub async fn undo_context(&self, count: usize) -> Result<usize, SessionError> {
        if count == 0 || count > 10_000 {
            return Err(SessionError::InvalidUndoCount);
        }
        let _gate = self.inner.steer_gate.lock().await;
        let before = {
            let mutation = self.inner.mutation.lock().await;
            if mutation.active_turn {
                return Err(SessionError::ActiveTurnMutation);
            }
            mutation.state.context.history().len()
        };
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        let record = record_with_value(RecordKind::ContextUndo, json!({ "count": count }))?;
        self.commit(record, None).await?;
        let after = self
            .inner
            .mutation
            .lock()
            .await
            .state
            .context
            .history()
            .len();
        Ok(before.saturating_sub(after))
    }

    pub async fn append_loop_event(&self, event: LoopEvent) -> Result<(), SessionError> {
        if !event.is_recorded() {
            return Err(SessionError::LiveOnlyLoopEvent);
        }
        let live_event = loop_event_to_agent_event(&event);
        let record = record_with_value(
            RecordKind::ContextAppendLoopEvent,
            json!({ "event": event }),
        )?;
        self.commit(record, live_event).await.map(|_| ())
    }

    /// Persist a loop event whose live projection was already emitted from an
    /// incremental provider stream. This prevents final assembly from
    /// replaying assistant/thinking deltas a second time.
    pub(crate) async fn append_streamed_loop_event(
        &self,
        event: LoopEvent,
    ) -> Result<(), SessionError> {
        if !event.is_recorded() {
            return Err(SessionError::LiveOnlyLoopEvent);
        }
        let record = record_with_value(
            RecordKind::ContextAppendLoopEvent,
            json!({ "event": event }),
        )?;
        self.commit(record, None).await.map(|_| ())
    }

    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), SessionError> {
        let record = record_with_value(RecordKind::PermissionSetMode, json!({ "mode": mode }))?;
        self.commit(
            record,
            Some(AgentEvent::AgentStatusUpdated {
                model: None,
                thinking_effort: None,
                context_tokens: None,
                max_context_tokens: None,
                context_usage: None,
                plan_mode: None,
                swarm_mode: None,
                permission: Some(mode),
                usage: None,
                phase: None,
            }),
        )
        .await
        .map(|_| ())
    }

    /// Replaces one canonical tool-store value through the session record log.
    /// The future resolves only after the value is durable and reduced into
    /// [`SessionSnapshot::state`].
    pub async fn set_tool_store_value(
        &self,
        key: impl Into<String>,
        value: Value,
    ) -> Result<(), SessionError> {
        let key = key.into();
        if key.trim().is_empty() || key.chars().any(char::is_control) {
            return Err(SessionError::InvalidRecord(
                "tool store key must be non-empty and contain no control characters".to_owned(),
            ));
        }
        let record = record_with_value(
            RecordKind::ToolsUpdateStore,
            json!({ "key": key, "value": value }),
        )?;
        self.commit(record, None).await.map(|_| ())
    }

    /// Enters canonical plan mode and records the optional plan path in the
    /// same durable transition. The live status is published only after the
    /// record has been appended and reduced.
    pub async fn enter_plan_mode(&self, plan_file: Option<String>) -> Result<(), SessionError> {
        let record = record_with_value(
            RecordKind::PlanModeEnter,
            json!({ "id": crate::RequestId::generate(), "planFile": plan_file }),
        )?;
        self.commit_guarded(
            record,
            Some(agent_status_plan_mode(true)),
            CommitGuard::PlanInactive,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Leaves canonical plan mode. The retained plan path remains available
    /// for review or re-entry, matching the retained built-in state contract.
    pub async fn exit_plan_mode(&self) -> Result<(), SessionError> {
        let record = record_with_value(RecordKind::PlanModeExit, json!({}))?;
        self.commit_guarded(
            record,
            Some(agent_status_plan_mode(false)),
            CommitGuard::PlanActive,
            None,
        )
        .await
        .map(|_| ())
    }

    /// Sets the canonical session swarm mode. The transition is durable and
    /// replayed before any later provider turn so CLI toggles cannot diverge
    /// from the state observed by orchestration and status surfaces.
    pub async fn set_swarm_mode(
        &self,
        enabled: bool,
        trigger: impl Into<String>,
    ) -> Result<(), SessionError> {
        let trigger = trigger.into();
        if enabled
            && (trigger.trim().is_empty()
                || trigger.chars().any(char::is_control)
                || trigger.chars().count() > 64)
        {
            return Err(SessionError::InvalidSwarmTrigger);
        }
        let (record, event, guard) = if enabled {
            (
                record_with_value(RecordKind::SwarmModeEnter, json!({ "trigger": trigger }))?,
                agent_status_swarm_mode(true),
                CommitGuard::SwarmInactive,
            )
        } else {
            (
                record_with_value(RecordKind::SwarmModeExit, json!({}))?,
                agent_status_swarm_mode(false),
                CommitGuard::SwarmActive,
            )
        };
        self.commit_guarded(record, Some(event), guard, None)
            .await
            .map(|_| ())
    }

    pub async fn authorize_tool(
        &self,
        request: &ToolPermissionRequest,
    ) -> Result<Authorization, SessionError> {
        self.ensure_open().await?;
        let authorization = self
            .inner
            .permission
            .authorize(request, self.inner.approval_port.as_deref())
            .await?;
        if let Some(response) = authorization.approval_response.clone() {
            let approval = PermissionApprovalResultRecord {
                turn_id: request.turn_id,
                tool_call_id: request.tool_call_id.as_str().to_owned(),
                tool_name: request.tool_name.clone(),
                action: request.action.clone(),
                session_approval_rule: authorization.remember_session_rule.clone(),
                result: response,
            };
            let record = record_with_value(
                RecordKind::PermissionRecordApprovalResult,
                serde_json::to_value(approval).map_err(SessionError::Serialize)?,
            )?;
            self.commit(record, None).await?;
        }
        Ok(authorization)
    }

    pub async fn ask(&self, request: QuestionRequest) -> Result<QuestionResponse, SessionError> {
        self.ensure_open().await?;
        if let Some(port) = self.inner.question_port.as_deref() {
            return Ok(port.ask(request).await?);
        }
        if self.inner.permission.mode() == PermissionMode::Auto {
            return Ok(QuestionResponse {
                answers: Vec::new(),
            });
        }
        Err(PortError::new(
            "question requires an interactive question port, but none is configured",
        )
        .into())
    }

    pub(crate) async fn acquire_turn(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.inner.turn_gate).lock_owned().await
    }

    pub(crate) fn try_acquire_turn(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        Arc::clone(&self.inner.turn_gate).try_lock_owned().ok()
    }

    pub(crate) async fn begin_compaction(
        &self,
        trigger: CompactionTrigger,
        instruction: Option<String>,
    ) -> Result<(), SessionError> {
        let source = match trigger {
            CompactionTrigger::Manual => "manual",
            CompactionTrigger::Auto => "auto",
        };
        let mut payload = BTreeMap::from([("source".to_owned(), Value::String(source.to_owned()))]);
        if let Some(instruction) = &instruction {
            payload.insert("instruction".to_owned(), Value::String(instruction.clone()));
        }
        let record = AgentRecord::new(RecordKind::FullCompactionBegin, payload);
        record
            .validate()
            .map_err(|error| SessionError::InvalidRecord(error.to_string()))?;
        self.commit(
            record,
            Some(AgentEvent::CompactionStarted {
                trigger,
                instruction,
            }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn apply_compaction(
        &self,
        result: &ProtocolCompactionResult,
        context_summary: &str,
    ) -> Result<(), SessionError> {
        let mut payload = serde_json::to_value(result)
            .map_err(SessionError::Serialize)?
            .as_object()
            .cloned()
            .ok_or(SessionError::RecordPayloadNotObject)?;
        payload.insert(
            "contextSummary".to_owned(),
            Value::String(context_summary.to_owned()),
        );
        let record = AgentRecord::new(
            RecordKind::ContextApplyCompaction,
            payload.into_iter().collect(),
        );
        record
            .validate()
            .map_err(|error| SessionError::InvalidRecord(error.to_string()))?;
        self.commit(record, None).await.map(|_| ())
    }

    pub(crate) async fn complete_compaction(
        &self,
        result: ProtocolCompactionResult,
    ) -> Result<(), SessionError> {
        let record = record_with_value(RecordKind::FullCompactionComplete, json!({}))?;
        self.commit(record, Some(AgentEvent::CompactionCompleted { result }))
            .await
            .map(|_| ())
    }

    pub(crate) async fn cancel_compaction(&self) -> Result<(), SessionError> {
        let running =
            self.inner.mutation.lock().await.state.compaction == crate::CompactionState::Running;
        if !running {
            return Ok(());
        }
        let record = record_with_value(RecordKind::FullCompactionCancel, json!({}))?;
        self.commit(record, Some(AgentEvent::CompactionCancelled))
            .await
            .map(|_| ())
    }

    pub(crate) fn publish_compaction_blocked(&self, turn_id: Option<u64>) {
        self.publish_live(AgentEvent::CompactionBlocked { turn_id });
    }

    pub(crate) async fn begin_turn(
        &self,
        content: Vec<ContentPart>,
        origin: PromptOrigin,
    ) -> Result<u64, SessionError> {
        if content.is_empty() {
            return Err(SessionError::EmptyTurnInput);
        }
        let _steer_gate = self.inner.steer_gate.lock().await;
        self.flush_pending_steers_locked().await?;
        let record = record_with_value(
            RecordKind::TurnPrompt,
            json!({ "input": content, "origin": origin }),
        )?;
        let receipt = self.commit(record, None).await?;
        self.publish_live(AgentEvent::TurnStarted {
            turn_id: receipt.turn_sequence,
            origin: origin.clone(),
        });
        self.append_context(ContextEntry {
            message: Message {
                role: Role::User,
                name: None,
                content,
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
                tools: Vec::new(),
            },
            origin: Some(origin),
            is_error: false,
            tool_call_displays: BTreeMap::new(),
            note: None,
        })
        .await?;
        self.inner.mutation.lock().await.active_turn = true;
        Ok(receipt.turn_sequence)
    }

    pub(crate) async fn flush_pending_steers(&self) -> Result<usize, SessionError> {
        let _steer_gate = self.inner.steer_gate.lock().await;
        self.flush_pending_steers_locked().await
    }

    async fn flush_pending_steers_locked(&self) -> Result<usize, SessionError> {
        let mut flushed = 0usize;
        loop {
            let pending = {
                let mut mutation = self.inner.mutation.lock().await;
                mutation.pending_steers.pop_front()
            };
            let Some(pending) = pending else {
                return Ok(flushed);
            };
            let entry = ContextEntry {
                message: Message {
                    role: Role::User,
                    name: None,
                    content: pending.content.clone(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    partial: false,
                    tools: Vec::new(),
                },
                origin: Some(pending.origin.clone()),
                is_error: false,
                tool_call_displays: BTreeMap::new(),
                note: None,
            };
            if let Err(error) = self.append_context(entry).await {
                self.inner
                    .mutation
                    .lock()
                    .await
                    .pending_steers
                    .push_front(pending);
                return Err(error);
            }
            flushed = flushed.saturating_add(1);
        }
    }

    pub(crate) async fn record_usage(
        &self,
        model: &str,
        usage: TokenUsage,
    ) -> Result<(), SessionError> {
        let record = record_with_value(
            RecordKind::UsageRecord,
            json!({ "model": model, "usage": usage }),
        )?;
        self.commit(record, None).await.map(|_| ())
    }

    pub(crate) async fn record_turn_cancel(
        &self,
        turn_id: u64,
        reason: &str,
    ) -> Result<(), SessionError> {
        let record = record_with_value(
            RecordKind::TurnCancel,
            json!({ "turnId": turn_id, "reason": reason }),
        )?;
        self.commit(record, None).await.map(|_| ())
    }

    pub(crate) fn publish_live(&self, event: AgentEvent) {
        self.inner.events.publish(Event {
            agent_id: self.inner.main_agent_id.as_str().to_owned(),
            session_id: self.inner.id.as_str().to_owned(),
            event,
        });
    }

    /// Appends a runtime-owned observability record and only then publishes
    /// its live projection. These records deliberately use forward-compatible
    /// tags: older reducers retain them verbatim and ignore their state.
    pub(crate) async fn append_observation(
        &self,
        record_type: &'static str,
        payload: Value,
        event: AgentEvent,
    ) -> Result<(), SessionError> {
        self.append_observation_record(record_type, payload).await?;
        self.publish_live(event);
        Ok(())
    }

    /// Append a terminal record unless an accepted steer is waiting. With
    /// `continue_for_steer`, pending input is flushed while the completion gate
    /// remains held and the caller must run another provider step.
    pub(crate) async fn append_terminal_observation(
        &self,
        record_type: &'static str,
        payload: Value,
        event: AgentEvent,
        continue_for_steer: bool,
    ) -> Result<bool, SessionError> {
        let _steer_gate = self.inner.steer_gate.lock().await;
        if continue_for_steer && self.flush_pending_steers_locked().await? > 0 {
            return Ok(false);
        }
        self.append_observation(record_type, payload, event).await?;
        self.inner.mutation.lock().await.active_turn = false;
        Ok(true)
    }

    /// Persists an observability record without publishing its live
    /// projection. Runtime adapters use this with `publish_live` to preserve
    /// an explicit durable-before-live boundary.
    pub(crate) async fn append_observation_record(
        &self,
        record_type: &'static str,
        payload: Value,
    ) -> Result<(), SessionError> {
        let payload = payload
            .as_object()
            .cloned()
            .ok_or(SessionError::RecordPayloadNotObject)?
            .into_iter()
            .collect();
        let record = AgentRecord {
            record_type: record_type.to_owned(),
            time: None,
            payload,
        };
        self.commit(record, None).await.map(|_| ())
    }

    pub fn cancel(&self) -> bool {
        self.inner.cancellation.cancel()
    }

    pub async fn close(&self) -> Result<(), SessionError> {
        let mut mutation = self.inner.mutation.lock().await;
        if mutation.closed {
            return Ok(());
        }
        if !mutation.session_end_hook_ran {
            // Latch before invoking the external process: a later record-log
            // close failure must not run an irreversible ingest hook twice.
            mutation.session_end_hook_ran = true;
            self.run_lifecycle_hook(
                ToolHookEvent::SessionEnd,
                "exit",
                BTreeMap::from([("reason".to_owned(), json!("exit"))]),
                &CancellationToken::new(),
            )
            .await;
        }
        self.inner.cancellation.cancel();
        self.inner.records.close().await?;
        mutation.closed = true;
        drop(mutation);
        if let Some(runtime) = self.inner.runtime.upgrade() {
            let mut active = runtime.active.lock().await;
            if active
                .get(&self.inner.id)
                .is_some_and(|current| Arc::ptr_eq(current, &self.inner))
            {
                active.remove(&self.inner.id);
            }
        }
        Ok(())
    }

    pub async fn run_lifecycle_hook(
        &self,
        event: ToolHookEvent,
        matcher_value: impl Into<String>,
        fields: BTreeMap<String, Value>,
        cancellation: &CancellationToken,
    ) -> HookRunReport {
        let mut input = LifecycleHookInput::new(
            event,
            self.inner.id.clone(),
            self.inner.main_agent_id.clone(),
        )
        .with_matcher_value(matcher_value);
        input.fields = fields;
        self.inner.hooks.run_lifecycle(&input, cancellation).await
    }

    async fn ensure_open(&self) -> Result<(), SessionError> {
        let mutation = self.inner.mutation.lock().await;
        if mutation.closed {
            return Err(SessionError::Closed(self.inner.id.clone()));
        }
        if let Some(message) = &mutation.poisoned {
            return Err(SessionError::StatePoisoned {
                id: self.inner.id.clone(),
                message: message.clone(),
            });
        }
        Ok(())
    }

    async fn commit(
        &self,
        record: AgentRecord,
        event: Option<AgentEvent>,
    ) -> Result<CommitReceipt, SessionError> {
        self.commit_guarded(record, event, CommitGuard::None, None)
            .await
    }

    async fn commit_guarded(
        &self,
        record: AgentRecord,
        event: Option<AgentEvent>,
        guard: CommitGuard,
        pending_steer: Option<PendingSteer>,
    ) -> Result<CommitReceipt, SessionError> {
        let mut mutation = self.inner.mutation.lock().await;
        if mutation.closed {
            return Err(SessionError::Closed(self.inner.id.clone()));
        }
        if let Some(message) = &mutation.poisoned {
            return Err(SessionError::StatePoisoned {
                id: self.inner.id.clone(),
                message: message.clone(),
            });
        }
        match guard {
            CommitGuard::None => {}
            CommitGuard::PlanInactive if mutation.state.plan_mode => {
                return Err(SessionError::PlanModeAlreadyActive);
            }
            CommitGuard::PlanActive if !mutation.state.plan_mode => {
                return Err(SessionError::PlanModeNotActive);
            }
            CommitGuard::SwarmInactive if mutation.state.swarm_mode => {
                return Err(SessionError::SwarmModeAlreadyActive);
            }
            CommitGuard::SwarmActive if !mutation.state.swarm_mode => {
                return Err(SessionError::SwarmModeNotActive);
            }
            CommitGuard::PlanInactive
            | CommitGuard::PlanActive
            | CommitGuard::SwarmInactive
            | CommitGuard::SwarmActive => {}
        }
        mutation.state.validate_apply(&record)?;
        // Holding the mutation lock across append establishes one total order
        // for concurrent callers: durable record, in-memory reduction, event.
        self.inner.records.append(record.clone()).await?;
        if let Err(error) = mutation.state.apply(&record) {
            mutation.poisoned = Some(error.to_string());
            return Err(SessionError::Replay(error));
        }
        if let Some(pending) = pending_steer {
            mutation.pending_steers.push_back(pending);
        }
        self.inner
            .permission
            .set_mode(mutation.state.permission_mode);
        for approval in &mutation.state.session_approval_rules {
            self.inner
                .permission
                .remember_session_approval(approval.clone());
        }
        let receipt = CommitReceipt {
            turn_sequence: mutation.state.turn_sequence,
        };
        drop(mutation);
        if let Some(event) = event {
            self.publish_live(event);
        }
        Ok(receipt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CommitReceipt {
    turn_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitGuard {
    None,
    PlanInactive,
    PlanActive,
    SwarmInactive,
    SwarmActive,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub main_agent_id: AgentId,
    pub state: AgentState,
    pub closed: bool,
    pub poisoned: Option<String>,
    pub warning: Option<String>,
}

fn recover_pending_steers(records: &[AgentRecord]) -> Result<VecDeque<PendingSteer>, SessionError> {
    let mut pending = VecDeque::new();
    for record in records {
        match record.kind() {
            Some(RecordKind::TurnSteer) => {
                let content = serde_json::from_value(
                    record.payload.get("input").cloned().ok_or_else(|| {
                        SessionError::InvalidRecord("turn.steer record omitted input".to_owned())
                    })?,
                )
                .map_err(|error| SessionError::InvalidRecord(error.to_string()))?;
                let origin = serde_json::from_value(
                    record.payload.get("origin").cloned().ok_or_else(|| {
                        SessionError::InvalidRecord("turn.steer record omitted origin".to_owned())
                    })?,
                )
                .map_err(|error| SessionError::InvalidRecord(error.to_string()))?;
                pending.push_back(PendingSteer { content, origin });
            }
            Some(RecordKind::ContextAppendMessage) => {
                let entry: ContextEntry = serde_json::from_value(
                    record.payload.get("message").cloned().ok_or_else(|| {
                        SessionError::InvalidRecord(
                            "context.append_message record omitted message".to_owned(),
                        )
                    })?,
                )
                .map_err(|error| SessionError::InvalidRecord(error.to_string()))?;
                let matches_front = pending.front().is_some_and(|steer| {
                    entry.message.role == Role::User
                        && entry.message.content == steer.content
                        && entry.origin.as_ref() == Some(&steer.origin)
                });
                if matches_front {
                    pending.pop_front();
                }
            }
            _ => {}
        }
    }
    Ok(pending)
}

fn record_with_value(kind: RecordKind, value: Value) -> Result<AgentRecord, SessionError> {
    let payload = value
        .as_object()
        .cloned()
        .ok_or(SessionError::RecordPayloadNotObject)?
        .into_iter()
        .collect();
    let record = AgentRecord::new(kind, payload);
    record
        .validate()
        .map_err(|error| SessionError::InvalidRecord(error.to_string()))?;
    Ok(record)
}

fn agent_status_plan_mode(plan_mode: bool) -> AgentEvent {
    AgentEvent::AgentStatusUpdated {
        model: None,
        thinking_effort: None,
        context_tokens: None,
        max_context_tokens: None,
        context_usage: None,
        plan_mode: Some(plan_mode),
        swarm_mode: None,
        permission: None,
        usage: None,
        phase: None,
    }
}

fn agent_status_swarm_mode(swarm_mode: bool) -> AgentEvent {
    AgentEvent::AgentStatusUpdated {
        model: None,
        thinking_effort: None,
        context_tokens: None,
        max_context_tokens: None,
        context_usage: None,
        plan_mode: None,
        swarm_mode: Some(swarm_mode),
        permission: None,
        usage: None,
        phase: None,
    }
}

fn loop_event_to_agent_event(event: &LoopEvent) -> Option<AgentEvent> {
    match event {
        LoopEvent::StepBegin {
            turn_id,
            step,
            uuid,
        } => Some(AgentEvent::TurnStepStarted {
            turn_id: turn_id.parse().unwrap_or_default(),
            step: *step,
            step_id: Some(uuid.clone()),
        }),
        LoopEvent::StepEnd {
            turn_id,
            step,
            uuid,
            usage,
            finish_reason,
            llm_first_token_latency_ms,
            llm_stream_duration_ms,
            llm_request_build_ms,
            llm_server_first_token_ms,
            llm_server_decode_ms,
            llm_client_consume_ms,
            provider_finish_reason,
            raw_finish_reason,
            ..
        } => Some(AgentEvent::TurnStepCompleted {
            turn_id: turn_id.parse().unwrap_or_default(),
            step: *step,
            step_id: Some(uuid.clone()),
            usage: *usage,
            finish_reason: finish_reason.map(|reason| format!("{reason:?}").to_lowercase()),
            llm_first_token_latency_ms: *llm_first_token_latency_ms,
            llm_stream_duration_ms: *llm_stream_duration_ms,
            llm_request_build_ms: *llm_request_build_ms,
            llm_server_first_token_ms: *llm_server_first_token_ms,
            llm_server_decode_ms: *llm_server_decode_ms,
            llm_client_consume_ms: *llm_client_consume_ms,
            provider_finish_reason: *provider_finish_reason,
            raw_finish_reason: raw_finish_reason.clone(),
        }),
        LoopEvent::ContentPart { turn_id, part, .. } => match part {
            LoopContentPart::Text { text } => Some(AgentEvent::AssistantDelta {
                turn_id: turn_id.parse().unwrap_or_default(),
                delta: text.clone(),
            }),
            LoopContentPart::Think { think, .. } => Some(AgentEvent::ThinkingDelta {
                turn_id: turn_id.parse().unwrap_or_default(),
                delta: think.clone(),
            }),
        },
        LoopEvent::ToolCall {
            turn_id,
            tool_call_id,
            name,
            args,
            description,
            display,
            ..
        } => Some(AgentEvent::ToolCallStarted {
            turn_id: turn_id.parse().unwrap_or_default(),
            tool_call_id: tool_call_id.clone(),
            name: name.clone(),
            args: args.clone(),
            description: description.clone(),
            display: display.clone(),
        }),
        // The durable tool-result shape intentionally has no turn id. The
        // loop publishes the live ToolResult event while it still owns that
        // association; manufacturing turn 0 here would corrupt consumers.
        LoopEvent::ToolResult { .. } => None,
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session already exists: {0}")]
    AlreadyExists(SessionId),
    #[error("session not found: {0}")]
    NotFound(SessionId),
    #[error("session cannot be forked while it is active: {0}")]
    ForkSourceActive(SessionId),
    #[error("session cannot be forked onto itself: {0}")]
    ForkSameSession(SessionId),
    #[error("session is closed: {0}")]
    Closed(SessionId),
    #[error("session {id} state is poisoned after a durable reducer failure: {message}")]
    StatePoisoned { id: SessionId, message: String },
    #[error("session record log is empty: {0}")]
    EmptyRecordLog(PathBuf),
    #[error("record I/O failed for {path}: {message}")]
    RecordIo { path: PathBuf, message: String },
    #[error("record payload must be a JSON object")]
    RecordPayloadNotObject,
    #[error("invalid record: {0}")]
    InvalidRecord(String),
    #[error("live-only loop event cannot be appended to durable context")]
    LiveOnlyLoopEvent,
    #[error("turn input must not be empty")]
    EmptyTurnInput,
    #[error("there is no active turn to steer")]
    NoActiveTurn,
    #[error("undo count must be between 1 and 10000")]
    InvalidUndoCount,
    #[error("session context cannot be mutated while a turn is active")]
    ActiveTurnMutation,
    #[error("plan mode is already active")]
    PlanModeAlreadyActive,
    #[error("plan mode is not active")]
    PlanModeNotActive,
    #[error("swarm mode is already active")]
    SwarmModeAlreadyActive,
    #[error("swarm mode is not active")]
    SwarmModeNotActive,
    #[error("swarm trigger must be non-empty, at most 64 characters, and contain no controls")]
    InvalidSwarmTrigger,
    #[error("serialization failed: {0}")]
    Serialize(serde_json::Error),
    #[error(transparent)]
    Records(#[from] RecordLogError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Context(#[from] crate::ContextError),
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    EventBus(#[from] EventBusError),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex as StdMutex,
    };

    use mycel_agent_protocol::{
        ApprovalDecision, ApprovalRequest, ApprovalResponse, ApprovalScope, FileOperation,
        ToolInputDisplay,
    };

    use crate::{PermissionVerdict, PortFuture, RequestId, ToolCallId};

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mycel-session-{name}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn options(id: &str) -> SessionOptions {
        SessionOptions::new(SessionId::new(id).expect("id"))
    }

    #[tokio::test]
    async fn session_round_trip_restores_canonical_state() {
        let root = temp_root("roundtrip");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");
        session
            .append_user_message("hello", PromptOrigin::User)
            .await
            .expect("message");
        session
            .set_permission_mode(PermissionMode::Yolo)
            .await
            .expect("mode");
        session.close().await.expect("close");

        let resumed = runtime.resume_session(options("s1")).await.expect("resume");
        let snapshot = resumed.snapshot().await;
        assert_eq!(snapshot.state.context.history().len(), 1);
        assert_eq!(
            snapshot.state.context.history()[0].message.text(""),
            "hello"
        );
        assert_eq!(snapshot.state.permission_mode, PermissionMode::Yolo);
        resumed.close().await.expect("close resumed");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fork_clones_closed_durable_history_and_rejects_active_sources() {
        let root = temp_root("fork");
        let runtime = Runtime::new(&root);
        let source = runtime
            .create_session(options("source"))
            .await
            .expect("create source");
        source
            .append_user_message("branch point", PromptOrigin::User)
            .await
            .expect("message");

        assert!(matches!(
            runtime.fork_session(source.id(), options("active-fork")).await,
            Err(SessionError::ForkSourceActive(id)) if id.as_str() == "source"
        ));
        source.close().await.expect("close source");

        let source_id = SessionId::new("source").expect("source id");
        let fork = runtime
            .fork_session(&source_id, options("fork"))
            .await
            .expect("fork");
        let snapshot = fork.snapshot().await;
        assert_eq!(snapshot.state.context.history().len(), 1);
        assert_eq!(
            snapshot.state.context.history()[0].message.text(""),
            "branch point"
        );
        assert!(matches!(
            runtime.fork_session(fork.id(), options("fork")).await,
            Err(SessionError::ForkSameSession(id)) if id.as_str() == "fork"
        ));
        fork.close().await.expect("close fork");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn steer_is_active_turn_only_durable_and_recovered_before_the_next_prompt() {
        let root = temp_root("steer-recovery");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");
        assert!(matches!(
            session
                .steer(vec![ContentPart::text("too early")], PromptOrigin::User)
                .await,
            Err(SessionError::NoActiveTurn)
        ));

        let turn = session.acquire_turn().await;
        session
            .begin_turn(vec![ContentPart::text("initial")], PromptOrigin::User)
            .await
            .expect("begin turn");
        session
            .steer(vec![ContentPart::text("redirect")], PromptOrigin::User)
            .await
            .expect("steer");
        let records = crate::read_record_file(session.record_path())
            .await
            .expect("read steer record");
        assert_eq!(
            records.records.last().and_then(AgentRecord::kind),
            Some(RecordKind::TurnSteer)
        );
        drop(turn);
        session.close().await.expect("close");

        let resumed = runtime.resume_session(options("s1")).await.expect("resume");
        let resumed_turn = resumed.acquire_turn().await;
        resumed
            .begin_turn(vec![ContentPart::text("next")], PromptOrigin::User)
            .await
            .expect("begin resumed turn");
        let history = resumed.snapshot().await.state.context.provider_history();
        assert_eq!(
            history
                .iter()
                .map(|message| message.text(""))
                .collect::<Vec<_>>(),
            ["initial", "redirect", "next"]
        );
        drop(resumed_turn);
        resumed.close().await.expect("close resumed");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn undo_context_is_idle_only_durable_and_replayed() {
        let root = temp_root("undo");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");
        session
            .append_user_message("keep", PromptOrigin::User)
            .await
            .expect("first message");
        session
            .append_user_message("remove", PromptOrigin::User)
            .await
            .expect("second message");
        assert_eq!(session.undo_context(1).await.expect("undo"), 1);
        assert!(matches!(
            session.undo_context(0).await,
            Err(SessionError::InvalidUndoCount)
        ));
        let records = crate::read_record_file(session.record_path())
            .await
            .expect("read undo record");
        assert_eq!(
            records.records.last().and_then(AgentRecord::kind),
            Some(RecordKind::ContextUndo)
        );
        session.close().await.expect("close");

        let resumed = runtime.resume_session(options("s1")).await.expect("resume");
        assert_eq!(
            resumed.snapshot().await.state.context.history()[0]
                .message
                .text(""),
            "keep"
        );
        assert_eq!(resumed.snapshot().await.state.context.history().len(), 1);
        let turn = resumed.acquire_turn().await;
        resumed
            .begin_turn(vec![ContentPart::text("active")], PromptOrigin::User)
            .await
            .expect("begin active turn");
        assert!(matches!(
            resumed.undo_context(1).await,
            Err(SessionError::ActiveTurnMutation)
        ));
        drop(turn);
        resumed.close().await.expect("close resumed");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn active_create_returns_the_same_session() {
        let root = temp_root("active");
        let runtime = Runtime::new(&root);
        let first = runtime.create_session(options("s1")).await.expect("first");
        let second = runtime.create_session(options("s1")).await.expect("second");
        assert!(first.same_instance(&second));
        first.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn event_is_published_only_after_the_record_is_readable() {
        let root = temp_root("event-order");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");
        let mut events = session.subscribe();
        session
            .set_permission_mode(PermissionMode::Auto)
            .await
            .expect("mode");
        let event = events.recv().await.expect("event");
        assert!(matches!(
            event.event,
            AgentEvent::AgentStatusUpdated {
                permission: Some(PermissionMode::Auto),
                ..
            }
        ));
        let read = crate::read_record_file(session.record_path())
            .await
            .expect("read");
        assert_eq!(
            read.records.last().and_then(AgentRecord::kind),
            Some(RecordKind::PermissionSetMode)
        );
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn plan_mode_and_todos_are_canonical_durable_session_state() {
        let root = temp_root("plan-todos");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");
        let todos = json!([
            {"title":"port retained state", "status":"in_progress"},
            {"title":"verify replay", "status":"pending"}
        ]);
        session
            .set_tool_store_value("todos", todos.clone())
            .await
            .expect("store todos");
        let stored = crate::read_record_file(session.record_path())
            .await
            .expect("read todo record");
        assert_eq!(
            stored.records.last().and_then(AgentRecord::kind),
            Some(RecordKind::ToolsUpdateStore)
        );

        let mut events = session.subscribe();
        session
            .enter_plan_mode(Some("PLAN.md".to_owned()))
            .await
            .expect("enter plan mode");
        let entered = events.recv().await.expect("plan event");
        assert!(matches!(
            entered.event,
            AgentEvent::AgentStatusUpdated {
                plan_mode: Some(true),
                ..
            }
        ));
        let entered_records = crate::read_record_file(session.record_path())
            .await
            .expect("read entered record");
        assert_eq!(
            entered_records.records.last().and_then(AgentRecord::kind),
            Some(RecordKind::PlanModeEnter)
        );
        let snapshot = session.snapshot().await;
        assert!(snapshot.state.plan_mode);
        assert_eq!(snapshot.state.tool_store.get("todos"), Some(&todos));
        assert_eq!(
            snapshot.state.tool_store.get("plan_file"),
            Some(&Value::String("PLAN.md".to_owned()))
        );

        session.exit_plan_mode().await.expect("exit plan mode");
        let exited = events.recv().await.expect("exit event");
        assert!(matches!(
            exited.event,
            AgentEvent::AgentStatusUpdated {
                plan_mode: Some(false),
                ..
            }
        ));
        session.close().await.expect("close");

        let resumed = runtime.resume_session(options("s1")).await.expect("resume");
        let resumed_snapshot = resumed.snapshot().await;
        assert!(!resumed_snapshot.state.plan_mode);
        assert_eq!(resumed_snapshot.state.tool_store.get("todos"), Some(&todos));
        assert_eq!(
            resumed_snapshot.state.tool_store.get("plan_file"),
            Some(&Value::String("PLAN.md".to_owned()))
        );
        resumed.close().await.expect("close resumed");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn concurrent_plan_transitions_are_guarded_by_canonical_state() {
        let root = temp_root("plan-concurrency");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");

        let (first, second) =
            tokio::join!(session.enter_plan_mode(None), session.enter_plan_mode(None));
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let rejected = first.err().or_else(|| second.err()).expect("one rejected");
        assert!(matches!(rejected, SessionError::PlanModeAlreadyActive));
        assert!(session.snapshot().await.state.plan_mode);

        let (first, second) = tokio::join!(session.exit_plan_mode(), session.exit_plan_mode());
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let rejected = first.err().or_else(|| second.err()).expect("one rejected");
        assert!(matches!(rejected, SessionError::PlanModeNotActive));
        assert!(!session.snapshot().await.state.plan_mode);

        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn swarm_mode_is_durable_evented_and_concurrency_guarded() {
        let root = temp_root("swarm-mode");
        let runtime = Runtime::new(&root);
        let session = runtime.create_session(options("s1")).await.expect("create");
        let mut events = session.subscribe();

        let (first, second) = tokio::join!(
            session.set_swarm_mode(true, "manual"),
            session.set_swarm_mode(true, "manual")
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let rejected = first.err().or_else(|| second.err()).expect("one rejected");
        assert!(matches!(rejected, SessionError::SwarmModeAlreadyActive));
        assert!(session.snapshot().await.state.swarm_mode);
        assert!(matches!(
            events.recv().await.expect("swarm event").event,
            AgentEvent::AgentStatusUpdated {
                swarm_mode: Some(true),
                ..
            }
        ));
        assert!(matches!(
            session.set_swarm_mode(true, "\n").await,
            Err(SessionError::InvalidSwarmTrigger)
        ));

        session
            .set_swarm_mode(false, "manual")
            .await
            .expect("disable");
        session.close().await.expect("close");
        let resumed = runtime.resume_session(options("s1")).await.expect("resume");
        assert!(!resumed.snapshot().await.state.swarm_mode);
        let records = crate::read_record_file(resumed.record_path())
            .await
            .expect("records");
        assert!(records
            .records
            .iter()
            .any(|record| record.kind() == Some(RecordKind::SwarmModeEnter)));
        assert_eq!(
            records.records.last().and_then(AgentRecord::kind),
            Some(RecordKind::SwarmModeExit)
        );
        resumed.close().await.expect("close resumed");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    struct ApprovalMock {
        requests: StdMutex<Vec<ApprovalRequest>>,
    }

    impl ApprovalPort for ApprovalMock {
        fn request_approval<'a>(
            &'a self,
            request: ApprovalRequest,
        ) -> PortFuture<'a, Result<ApprovalResponse, PortError>> {
            self.requests.lock().expect("requests").push(request);
            Box::pin(async {
                Ok(ApprovalResponse {
                    decision: ApprovalDecision::Approved,
                    scope: Some(ApprovalScope::Session),
                    feedback: None,
                    selected_label: None,
                })
            })
        }
    }

    fn bash_request() -> ToolPermissionRequest {
        ToolPermissionRequest {
            turn_id: 3,
            tool_call_id: ToolCallId::new("call-1").expect("id"),
            tool_name: "Bash".to_owned(),
            action: "run command".to_owned(),
            display: ToolInputDisplay::FileIo {
                operation: FileOperation::Read,
                path: "x".to_owned(),
                detail: None,
                content: None,
                before: None,
                after: None,
            },
            approval_rule: Some("Bash(echo *)".to_owned()),
            rule_subject: Some("echo ok".to_owned()),
            exclusive_tool: None,
            plan_policy: crate::PlanPolicy::NotInPlan,
            create_goal_review: false,
            sensitive_file: false,
            git_control: false,
            git_cwd_write: false,
        }
    }

    fn question_request() -> QuestionRequest {
        QuestionRequest {
            request_id: RequestId::generate(),
            agent_id: AgentId::main(),
            questions: vec![crate::Question {
                id: "color".to_owned(),
                prompt: "Choose a color".to_owned(),
                options: vec![crate::QuestionOption {
                    label: "blue".to_owned(),
                    description: None,
                }],
                multiple: false,
            }],
        }
    }

    #[tokio::test]
    async fn session_approval_is_durable_and_reused() {
        let root = temp_root("approval");
        let runtime = Runtime::new(&root);
        let port = Arc::new(ApprovalMock {
            requests: StdMutex::new(Vec::new()),
        });
        let mut create = options("s1");
        create.approval_port = Some(port.clone());
        let session = runtime.create_session(create).await.expect("create");
        let first = session
            .authorize_tool(&bash_request())
            .await
            .expect("authorize");
        assert_eq!(first.verdict, PermissionVerdict::Allow);
        assert_eq!(port.requests.lock().expect("requests").len(), 1);
        let second = session
            .authorize_tool(&bash_request())
            .await
            .expect("cached authorize");
        assert_eq!(second.verdict, PermissionVerdict::Allow);
        assert!(second.approval_response.is_none());
        assert_eq!(port.requests.lock().expect("requests").len(), 1);
        assert!(session
            .snapshot()
            .await
            .state
            .session_approval_rules
            .contains("Bash(echo *)"));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn auto_questions_return_null_while_manual_questions_require_a_port() {
        let root = temp_root("question-mode");
        let runtime = Runtime::new(&root);
        let manual = runtime
            .create_session(options("manual"))
            .await
            .expect("manual session");
        let error = manual
            .ask(question_request())
            .await
            .expect_err("manual question must not be invented");
        assert!(error.to_string().contains("interactive question port"));
        manual.close().await.expect("close manual");

        let mut auto_options = options("auto");
        auto_options.initial_permission_mode = PermissionMode::Auto;
        let auto = runtime
            .create_session(auto_options)
            .await
            .expect("auto session");
        assert!(auto
            .ask(question_request())
            .await
            .expect("auto null question")
            .answers
            .is_empty());
        auto.close().await.expect("close auto");
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
