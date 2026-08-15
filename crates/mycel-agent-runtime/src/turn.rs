use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use mycel_agent_protocol::{
    AgentEvent, CompactionTrigger, ContentPart, ExecutableToolOutput, ExecutableToolResult,
    FinishReason, GenerateResult, InterruptionReason, LoopContentPart, LoopEvent,
    LoopStepStopReason, Message, PromptOrigin, ProviderError, ProviderErrorKind, ProviderRequest,
    ProviderStreamEvent, Role, StreamAssembler, StreamIndex, StreamPart, ThinkingEffort,
    TokenUsage, ToolCall, ToolCallKind, ToolUpdate, TurnEndReason,
};
use serde_json::{json, Value};

use crate::{
    Authorization, AutoCompactionConfig, CancellationToken, CompactionEngine, CompactionError,
    CompactionRequest, HookRunReport, HookRunner, LifecycleHookInput, PermissionVerdict, RequestId,
    ScheduleError, SessionError, SessionHandle, ToolCallId, ToolError, ToolExecutionSpec,
    ToolHookEvent, ToolHookInput, ToolInvocation, ToolPrepareContext, ToolRegistry, ToolScheduler,
    ToolSnapshot, ToolUpdateSink,
};

/// Forward-compatible durable record emitted before a retry live event.
pub const TURN_STEP_RETRYING_RECORD_TYPE: &str = "runtime.turn.step_retrying";
/// Forward-compatible durable record emitted before an interruption live event.
pub const TURN_INTERRUPTED_RECORD_TYPE: &str = "runtime.turn.interrupted";
/// Forward-compatible durable record emitted before a terminal live event.
pub const TURN_TERMINAL_RECORD_TYPE: &str = "runtime.turn.terminal";

pub type TurnProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<GenerateResult, ProviderError>> + Send + 'a>>;
pub type TurnProviderStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ProviderError>> + Send + 'a>>;

/// Synchronous event sink borrowed by a provider for one request attempt.
/// Returning an error stops the transport and fails the attempt closed.
pub trait TurnProviderStreamSink: Send {
    fn push(&mut self, event: ProviderStreamEvent) -> Result<(), ProviderError>;
}

/// Local provider seam. Existing aggregate providers only implement
/// [`TurnProvider::complete`]; the default streaming method converts their
/// result into canonical protocol events. Native streaming adapters override
/// [`TurnProvider::stream`] and push events as the transport yields them.
pub trait TurnProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn complete<'a>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken,
    ) -> TurnProviderFuture<'a>;

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken,
        sink: &'a mut dyn TurnProviderStreamSink,
    ) -> TurnProviderStreamFuture<'a> {
        Box::pin(async move {
            let response = self.complete(request, cancellation).await?;
            for event in response.into_stream_events() {
                sink.push(event)?;
            }
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
pub struct TurnEngineConfig {
    pub max_steps: u32,
    pub max_retries_per_step: u32,
    pub retry_delay: Duration,
    pub max_context_overflow_fallbacks: u8,
    pub tool_cancellation_grace: Duration,
    pub auto_compaction: Option<AutoCompactionConfig>,
}

impl Default for TurnEngineConfig {
    fn default() -> Self {
        Self {
            max_steps: 64,
            max_retries_per_step: 2,
            retry_delay: Duration::from_millis(250),
            max_context_overflow_fallbacks: 2,
            tool_cancellation_grace: Duration::from_secs(2),
            auto_compaction: None,
        }
    }
}

pub struct TurnInput {
    pub content: Vec<ContentPart>,
    pub origin: PromptOrigin,
    pub system_prompt: String,
    pub thinking_effort: Option<ThinkingEffort>,
    pub max_completion_tokens: Option<u64>,
    pub metadata: BTreeMap<String, Value>,
}

impl TurnInput {
    pub fn user(text: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            content: vec![ContentPart::text(text)],
            origin: PromptOrigin::User,
            system_prompt: system_prompt.into(),
            thinking_effort: None,
            max_completion_tokens: None,
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcomeReason {
    Completed,
    MaxTokens,
    Filtered,
    Paused,
    ToolStopped,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutcome {
    pub turn_id: u64,
    pub attempted_steps: u32,
    pub usage: TokenUsage,
    pub reason: TurnOutcomeReason,
}

pub struct TurnEngine {
    provider: Arc<dyn TurnProvider>,
    tools: ToolRegistry,
    hooks: HookRunner,
    scheduler: ToolScheduler,
    config: TurnEngineConfig,
    compaction: Option<CompactionEngine>,
}

impl TurnEngine {
    pub fn new(
        provider: Arc<dyn TurnProvider>,
        tools: ToolRegistry,
        hooks: HookRunner,
        scheduler: ToolScheduler,
        config: TurnEngineConfig,
    ) -> Result<Self, TurnError> {
        if config.max_steps == 0 {
            return Err(TurnError::InvalidConfig(
                "max_steps must be positive".to_owned(),
            ));
        }
        if config.max_context_overflow_fallbacks > 3 {
            return Err(TurnError::InvalidConfig(
                "max_context_overflow_fallbacks must be at most 3".to_owned(),
            ));
        }
        if let Some(compaction) = &config.auto_compaction {
            compaction
                .validate()
                .map_err(|error| TurnError::InvalidConfig(error.to_string()))?;
        }
        let compaction = config
            .auto_compaction
            .as_ref()
            .map(|_| CompactionEngine::standard(Arc::clone(&provider), tools.clone()));
        Ok(Self {
            provider,
            tools,
            hooks,
            scheduler,
            config,
            compaction,
        })
    }

    /// Snapshot of the tools this engine can advertise on its next step.
    /// Native child hosts use this to reject factory capability escalation
    /// before any provider request is dispatched.
    pub fn tool_definitions(&self) -> Vec<mycel_agent_protocol::ToolDefinition> {
        self.tools.snapshot().definitions().to_vec()
    }

    /// Execute one host-initiated tool through the same validation, hook,
    /// permission, scheduler, cancellation, and durable loop-record path used
    /// by provider-issued tool calls. Interactive slash commands use this seam
    /// instead of calling an executable directly and bypassing governance.
    pub async fn invoke_host_tool(
        &self,
        session: &SessionHandle,
        prompt: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<ExecutableToolResult, TurnError> {
        let prompt = prompt.into();
        let tool_name = tool_name.into();
        let _turn_guard = session.acquire_turn().await;
        let linked_cancellation = LinkedCancellation::new(cancellation, session.cancellation());
        let cancellation = linked_cancellation.token();
        let turn_id = session
            .begin_turn(vec![ContentPart::text(prompt)], PromptOrigin::User)
            .await?;
        let step = 1;
        let step_uuid = RequestId::generate().into_string();
        session
            .append_loop_event(LoopEvent::StepBegin {
                uuid: step_uuid.clone(),
                turn_id: turn_id.to_string(),
                step,
            })
            .await?;

        let provider_call_id = RequestId::generate().into_string();
        let call = ToolCall {
            kind: ToolCallKind::Function,
            id: provider_call_id,
            name: tool_name,
            arguments: Some(arguments.to_string()),
            extras: BTreeMap::new(),
        };
        let snapshot = self.tools.snapshot();
        let prepared = self.prepare_calls(session, turn_id, &[call], &snapshot);
        let call = prepared
            .first()
            .expect("one direct tool call must prepare exactly one entry");
        let spec = call.prepared.as_ref().ok().map(|(_, spec)| spec);
        session
            .append_loop_event(LoopEvent::ToolCall {
                uuid: call.call_uuid.clone(),
                turn_id: turn_id.to_string(),
                step,
                step_uuid: step_uuid.clone(),
                tool_call_id: call.provider_call_id.clone(),
                name: call.name.clone(),
                args: call.arguments.clone(),
                description: spec.and_then(|spec| spec.description.clone()),
                display: spec.map(|spec| spec.display.clone()),
                extras: call.extras.clone(),
            })
            .await?;
        session
            .append_loop_event(LoopEvent::StepEnd {
                uuid: step_uuid,
                turn_id: turn_id.to_string(),
                step,
                usage: None,
                finish_reason: Some(LoopStepStopReason::ToolUse),
                llm_first_token_latency_ms: None,
                llm_stream_duration_ms: None,
                llm_request_build_ms: None,
                llm_server_first_token_ms: None,
                llm_server_decode_ms: None,
                llm_client_consume_ms: None,
                provider_finish_reason: Some(FinishReason::ToolCalls),
                raw_finish_reason: None,
                message_id: None,
            })
            .await?;

        let mut completed = self
            .execute_batch(session, turn_id, prepared, &cancellation)
            .await;
        let completed = completed
            .pop()
            .expect("one direct tool call must complete exactly once");
        session
            .append_loop_event(LoopEvent::ToolResult {
                parent_uuid: completed.call_uuid,
                tool_call_id: completed.tool_call_id.clone(),
                result: completed.result.clone(),
            })
            .await?;
        session.publish_live(AgentEvent::ToolResult {
            turn_id,
            tool_call_id: completed.tool_call_id,
            output: serde_json::to_value(&completed.result.output).unwrap_or(Value::Null),
            is_error: completed.result.is_error.then_some(true),
            synthetic: completed.synthetic.then_some(true),
        });
        let terminal_reason = if cancellation.is_cancelled() {
            session.record_turn_cancel(turn_id, "aborted").await?;
            TurnEndReason::Cancelled
        } else {
            TurnEndReason::Completed
        };
        self.record_terminal(session, turn_id, terminal_reason, TokenUsage::default())
            .await?;
        Ok(completed.result)
    }

    pub async fn run_turn(
        &self,
        session: &SessionHandle,
        input: TurnInput,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        if matches!(&input.origin, PromptOrigin::User) {
            let matcher_value = input
                .content
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join(" ");
            let mut hook_input = LifecycleHookInput::new(
                ToolHookEvent::UserPromptSubmit,
                session.id().clone(),
                session.main_agent_id().clone(),
            )
            .with_matcher_value(matcher_value);
            hook_input.insert_field(
                "prompt",
                serde_json::to_value(&input.content).unwrap_or(Value::Null),
            );
            let report = self.hooks.run_lifecycle(&hook_input, &cancellation).await;
            if let Some(blocked) = report.blocked {
                return Err(TurnError::HookBlocked(blocked.reason));
            }
        }

        let result = self.run_turn_inner(session, input, cancellation).await;
        let event = match &result {
            Ok(outcome) if outcome.reason == TurnOutcomeReason::Aborted => {
                Some((ToolHookEvent::Interrupt, "cancelled", None))
            }
            Err(error) => Some((
                ToolHookEvent::StopFailure,
                turn_error_name(error),
                Some(error.to_string()),
            )),
            _ => None,
        };
        if let Some((event, matcher, error_message)) = event {
            let mut hook_input = LifecycleHookInput::new(
                event,
                session.id().clone(),
                session.main_agent_id().clone(),
            )
            .with_matcher_value(matcher);
            if let Some(error_message) = error_message {
                hook_input.insert_field("error_type", json!(matcher));
                hook_input.insert_field("error_message", json!(error_message));
            } else {
                hook_input.insert_field("reason", json!(matcher));
            }
            self.hooks
                .run_lifecycle(&hook_input, &CancellationToken::new())
                .await;
        }
        result
    }

    async fn run_turn_inner(
        &self,
        session: &SessionHandle,
        input: TurnInput,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        let _turn_guard = session.acquire_turn().await;
        let linked_cancellation = LinkedCancellation::new(cancellation, session.cancellation());
        let cancellation = linked_cancellation.token();
        let turn_id = session
            .begin_turn(input.content, input.origin.clone())
            .await?;
        let mut usage = TokenUsage::default();
        let mut attempted_steps = 0u32;
        let mut stop_hook_continuation_used = false;
        let mut last_compacted_token_count = None;

        loop {
            if cancellation.is_cancelled() {
                return self
                    .finish_aborted(session, turn_id, attempted_steps, None, usage)
                    .await;
            }
            session.flush_pending_steers().await?;
            if let (Some(policy), Some(compaction)) =
                (&self.config.auto_compaction, &self.compaction)
            {
                let used_tokens = session
                    .snapshot()
                    .await
                    .state
                    .context
                    .token_count_with_pending_estimate();
                let already_at_floor =
                    last_compacted_token_count.is_some_and(|last| used_tokens <= last);
                if !already_at_floor && policy.should_compact(used_tokens) {
                    let request = CompactionRequest {
                        trigger: CompactionTrigger::Auto,
                        instruction: None,
                        system_prompt: input.system_prompt.clone(),
                        thinking_effort: input.thinking_effort.clone(),
                        max_completion_tokens: input.max_completion_tokens,
                        turn_id: Some(turn_id),
                    };
                    match compaction
                        .compact_active(session, request, cancellation.clone())
                        .await
                    {
                        Ok(_) => {
                            last_compacted_token_count = Some(
                                session
                                    .snapshot()
                                    .await
                                    .state
                                    .context
                                    .token_count_with_pending_estimate(),
                            );
                        }
                        Err(CompactionError::Cancelled) => {
                            return self
                                .finish_aborted(session, turn_id, attempted_steps, None, usage)
                                .await;
                        }
                        Err(error) => {
                            self.record_terminal(session, turn_id, TurnEndReason::Failed, usage)
                                .await?;
                            return Err(TurnError::Compaction(error));
                        }
                    }
                }
            }
            if attempted_steps >= self.config.max_steps {
                self.record_interruption(
                    session,
                    turn_id,
                    attempted_steps,
                    None,
                    InterruptionReason::MaxSteps,
                    "maximum steps exceeded",
                )
                .await?;
                self.record_terminal(session, turn_id, TurnEndReason::Failed, usage)
                    .await?;
                return Err(TurnError::MaxStepsExceeded(self.config.max_steps));
            }

            attempted_steps += 1;
            let step = u64::from(attempted_steps);
            // Tool availability and canonical history are rebuilt at every
            // step, after all prior tool results have been reduced.
            let tool_snapshot = self.tools.snapshot();
            let history = session.snapshot().await.state.context.provider_history();
            let step_uuid = RequestId::generate().into_string();
            let request = ProviderRequest {
                provider: self.provider.name().to_owned(),
                model: self.provider.model().to_owned(),
                system_prompt: input.system_prompt.clone(),
                tools: tool_snapshot.definitions().to_vec(),
                history,
                thinking_effort: input.thinking_effort.clone(),
                max_completion_tokens: input.max_completion_tokens,
                response_format: None,
                metadata: input.metadata.clone(),
            };
            if let Err(error) = request.validate() {
                self.record_terminal(session, turn_id, TurnEndReason::Failed, usage)
                    .await?;
                return Err(TurnError::InvalidRequest(error.to_string()));
            }
            session
                .append_loop_event(LoopEvent::StepBegin {
                    uuid: step_uuid.clone(),
                    turn_id: turn_id.to_string(),
                    step,
                })
                .await?;

            let response = match self
                .complete_with_fallbacks(session, turn_id, step, &step_uuid, request, &cancellation)
                .await
            {
                Ok(response) => response,
                Err(StepFailure::Cancelled) => {
                    self.close_failed_step(session, turn_id, step, &step_uuid)
                        .await?;
                    return self
                        .finish_aborted(session, turn_id, attempted_steps, Some(step), usage)
                        .await;
                }
                Err(StepFailure::Provider(error)) => {
                    self.close_failed_step(session, turn_id, step, &step_uuid)
                        .await?;
                    self.record_interruption(
                        session,
                        turn_id,
                        attempted_steps,
                        Some(step),
                        InterruptionReason::Error,
                        &error.message,
                    )
                    .await?;
                    self.record_terminal(session, turn_id, TurnEndReason::Failed, usage)
                        .await?;
                    return Err(TurnError::Provider(error));
                }
                Err(StepFailure::Compaction(error)) => {
                    self.close_failed_step(session, turn_id, step, &step_uuid)
                        .await?;
                    self.record_interruption(
                        session,
                        turn_id,
                        attempted_steps,
                        Some(step),
                        InterruptionReason::Error,
                        &error.to_string(),
                    )
                    .await?;
                    self.record_terminal(session, turn_id, TurnEndReason::Failed, usage)
                        .await?;
                    return Err(TurnError::Compaction(error));
                }
                Err(StepFailure::Session(error)) => {
                    self.close_failed_step(session, turn_id, step, &step_uuid)
                        .await?;
                    return Err(TurnError::Session(error));
                }
            };

            // Spend must survive any validation, hook, permission, execution,
            // or cancellation failure that follows the provider response.
            if let Some(step_usage) = response.usage {
                session
                    .record_usage(self.provider.model(), step_usage)
                    .await?;
                usage = usage.saturating_add(step_usage);
            }

            if let Err(error) = validate_provider_response(&response) {
                self.close_failed_step(session, turn_id, step, &step_uuid)
                    .await?;
                self.record_interruption(
                    session,
                    turn_id,
                    attempted_steps,
                    Some(step),
                    InterruptionReason::Error,
                    &error.to_string(),
                )
                .await?;
                self.record_terminal(session, turn_id, TurnEndReason::Failed, usage)
                    .await?;
                return Err(error);
            }
            let prepared = self.prepare_calls(
                session,
                turn_id,
                &response.message.tool_calls,
                &tool_snapshot,
            );
            self.record_response(session, turn_id, step, &step_uuid, &response, &prepared)
                .await?;

            if prepared.is_empty() {
                if !stop_hook_continuation_used {
                    let mut hook_input = LifecycleHookInput::new(
                        ToolHookEvent::Stop,
                        session.id().clone(),
                        session.main_agent_id().clone(),
                    )
                    .with_turn_id(turn_id);
                    hook_input.insert_field("stop_hook_active", json!(false));
                    let report = self.hooks.run_lifecycle(&hook_input, &cancellation).await;
                    if cancellation.is_cancelled() {
                        return self
                            .finish_aborted(session, turn_id, attempted_steps, None, usage)
                            .await;
                    }
                    if let Some(blocked) = report.blocked {
                        stop_hook_continuation_used = true;
                        session
                            .append_user_message(
                                blocked.reason,
                                PromptOrigin::SystemTrigger {
                                    name: "stop_hook".to_owned(),
                                },
                            )
                            .await?;
                        continue;
                    }
                }
                let reason = terminal_reason(response.finish_reason);
                if !self
                    .record_terminal_with_steers(
                        session,
                        turn_id,
                        TurnEndReason::Completed,
                        usage,
                        true,
                    )
                    .await?
                {
                    continue;
                }
                return Ok(TurnOutcome {
                    turn_id,
                    attempted_steps,
                    usage,
                    reason,
                });
            }

            let results = self
                .execute_batch(session, turn_id, prepared, &cancellation)
                .await;
            let mut stop_turn = false;
            for completed in results {
                stop_turn |= completed.result.stop_turn;
                session
                    .append_loop_event(LoopEvent::ToolResult {
                        parent_uuid: completed.call_uuid,
                        tool_call_id: completed.tool_call_id.clone(),
                        result: completed.result.clone(),
                    })
                    .await?;
                session.publish_live(AgentEvent::ToolResult {
                    turn_id,
                    tool_call_id: completed.tool_call_id,
                    output: serde_json::to_value(&completed.result.output).unwrap_or(Value::Null),
                    is_error: completed.result.is_error.then_some(true),
                    synthetic: completed.synthetic.then_some(true),
                });
            }
            if stop_turn {
                self.record_terminal(session, turn_id, TurnEndReason::Completed, usage)
                    .await?;
                return Ok(TurnOutcome {
                    turn_id,
                    attempted_steps,
                    usage,
                    reason: TurnOutcomeReason::ToolStopped,
                });
            }
        }
    }

    async fn complete_with_fallbacks(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        step: u64,
        step_uuid: &str,
        mut request: ProviderRequest,
        cancellation: &CancellationToken,
    ) -> Result<GenerateResult, StepFailure> {
        let mut retry = 0u32;
        let mut overflow_fallback = 0u8;
        let mut failed_attempt = 0u64;
        loop {
            if cancellation.is_cancelled() {
                return Err(StepFailure::Cancelled);
            }
            let mut sink = TurnStreamCollector::new(session, turn_id);
            let result = {
                let future = self
                    .provider
                    .stream(request.clone(), cancellation.clone(), &mut sink);
                tokio::pin!(future);
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(StepFailure::Cancelled),
                    result = &mut future => result,
                }
            };
            let result = match result {
                Ok(()) => sink.finish(),
                Err(error) => Err(error),
            };
            match result {
                Ok(response) => return Ok(response),
                Err(error) if error.kind == ProviderErrorKind::Cancelled => {
                    return Err(StepFailure::Cancelled);
                }
                Err(error)
                    if is_context_overflow(&error)
                        && overflow_fallback < self.config.max_context_overflow_fallbacks =>
                {
                    overflow_fallback += 1;
                    failed_attempt += 1;
                    if let Some(compaction) = &self.compaction {
                        let compaction_request = CompactionRequest {
                            trigger: CompactionTrigger::Auto,
                            instruction: None,
                            system_prompt: request.system_prompt.clone(),
                            thinking_effort: request.thinking_effort.clone(),
                            max_completion_tokens: request.max_completion_tokens,
                            turn_id: Some(turn_id),
                        };
                        match compaction
                            .compact_active(session, compaction_request, cancellation.clone())
                            .await
                        {
                            Ok(_) => {
                                request.history =
                                    session.snapshot().await.state.context.provider_history();
                            }
                            Err(CompactionError::Cancelled) => {
                                return Err(StepFailure::Cancelled);
                            }
                            Err(error) => return Err(StepFailure::Compaction(error)),
                        }
                    } else {
                        request.history = overflow_projection(&request.history, overflow_fallback);
                    }
                    self.record_retry(session, turn_id, step, step_uuid, failed_attempt, 0, &error)
                        .await
                        .map_err(StepFailure::Session)?;
                }
                Err(error)
                    if !is_context_overflow(&error)
                        && error.retryable
                        && retry < self.config.max_retries_per_step =>
                {
                    retry += 1;
                    failed_attempt += 1;
                    self.record_retry(
                        session,
                        turn_id,
                        step,
                        step_uuid,
                        failed_attempt,
                        self.config
                            .retry_delay
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX),
                        &error,
                    )
                    .await
                    .map_err(StepFailure::Session)?;
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(StepFailure::Cancelled),
                        _ = tokio::time::sleep(self.config.retry_delay) => {}
                    }
                }
                Err(error) => return Err(StepFailure::Provider(error)),
            }
        }
    }

    fn prepare_calls(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        calls: &[ToolCall],
        snapshot: &ToolSnapshot,
    ) -> Vec<PreparedCall> {
        calls
            .iter()
            .map(|call| {
                let call_uuid = RequestId::generate().into_string();
                let tool_call_id =
                    ToolCallId::new(call.id.clone()).unwrap_or_else(|_| ToolCallId::generate());
                let arguments = parse_arguments(call.arguments.as_deref());
                let (arguments, prepared) = match arguments {
                    Ok(arguments) => match snapshot.get(&call.name) {
                        None => (arguments, Err(format!("unknown tool {:?}", call.name))),
                        Some(tool) => {
                            let result = tool
                                .validate_arguments(&arguments)
                                .and_then(|()| {
                                    tool.prepare(
                                        &arguments,
                                        &ToolPrepareContext {
                                            session_id: session.id().clone(),
                                            agent_id: session.main_agent_id().clone(),
                                            turn_id,
                                            tool_call_id: tool_call_id.clone(),
                                        },
                                    )
                                })
                                .map(|spec| (tool, spec))
                                .map_err(|error| error.to_string());
                            (arguments, result)
                        }
                    },
                    Err(error) => (json!({}), Err(error)),
                };
                PreparedCall {
                    call_uuid,
                    provider_call_id: call.id.clone(),
                    runtime_call_id: tool_call_id,
                    name: call.name.clone(),
                    arguments,
                    extras: call.extras.clone(),
                    prepared,
                }
            })
            .collect()
    }

    async fn record_response(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        step: u64,
        step_uuid: &str,
        response: &GenerateResult,
        prepared: &[PreparedCall],
    ) -> Result<(), TurnError> {
        for part in &response.message.content {
            let part = match part {
                ContentPart::Text { text } => LoopContentPart::Text { text: text.clone() },
                ContentPart::Think { think, encrypted } => LoopContentPart::Think {
                    think: think.clone(),
                    encrypted: encrypted.clone(),
                },
                _ => return Err(TurnError::UnsupportedProviderMedia),
            };
            session
                .append_streamed_loop_event(LoopEvent::ContentPart {
                    uuid: RequestId::generate().into_string(),
                    turn_id: turn_id.to_string(),
                    step,
                    step_uuid: step_uuid.to_owned(),
                    part,
                })
                .await?;
        }
        for call in prepared {
            let spec = call.prepared.as_ref().ok().map(|(_, spec)| spec);
            session
                .append_loop_event(LoopEvent::ToolCall {
                    uuid: call.call_uuid.clone(),
                    turn_id: turn_id.to_string(),
                    step,
                    step_uuid: step_uuid.to_owned(),
                    tool_call_id: call.provider_call_id.clone(),
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                    description: spec.and_then(|spec| spec.description.clone()),
                    display: spec.map(|spec| spec.display.clone()),
                    extras: call.extras.clone(),
                })
                .await?;
        }
        session
            .append_loop_event(LoopEvent::StepEnd {
                uuid: step_uuid.to_owned(),
                turn_id: turn_id.to_string(),
                step,
                usage: response.usage,
                finish_reason: Some(step_stop_reason(
                    response.finish_reason,
                    !prepared.is_empty(),
                )),
                llm_first_token_latency_ms: None,
                llm_stream_duration_ms: None,
                llm_request_build_ms: None,
                llm_server_first_token_ms: None,
                llm_server_decode_ms: None,
                llm_client_consume_ms: None,
                provider_finish_reason: response.finish_reason,
                raw_finish_reason: response.raw_finish_reason.clone(),
                message_id: response.id.clone(),
            })
            .await?;
        Ok(())
    }

    async fn execute_batch(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        prepared: Vec<PreparedCall>,
        cancellation: &CancellationToken,
    ) -> Vec<CompletedCall> {
        let stop_after = prepared.iter().position(|call| {
            call.prepared
                .as_ref()
                .is_ok_and(|(_, spec)| spec.stop_batch_after_this)
        });
        let mut running = Vec::with_capacity(prepared.len());
        for (index, call) in prepared.into_iter().enumerate() {
            if stop_after.is_some_and(|stop| index > stop) {
                running.push(PendingCall::Immediate(CompletedCall::synthetic(
                    call,
                    "tool call skipped because an earlier call stopped the batch",
                )));
                continue;
            }
            let (tool, spec) = match call.prepared.clone() {
                Ok(prepared) => prepared,
                Err(error) => {
                    running.push(PendingCall::Immediate(CompletedCall::synthetic(
                        call, &error,
                    )));
                    continue;
                }
            };
            let session = session.clone();
            let hooks = self.hooks.clone();
            let scheduler = self.scheduler.clone();
            let cancellation = cancellation.clone();
            let grace = self.config.tool_cancellation_grace;
            let call_uuid = call.call_uuid.clone();
            let tool_call_id = call.provider_call_id.clone();
            running.push(PendingCall::Running {
                call_uuid,
                tool_call_id,
                task: tokio::spawn(async move {
                    execute_call(
                        session,
                        hooks,
                        scheduler,
                        tool,
                        spec,
                        call,
                        turn_id,
                        cancellation,
                        grace,
                    )
                    .await
                }),
            });
        }

        let mut completed = Vec::with_capacity(running.len());
        // Tasks are concurrent, but terminal records are emitted only after
        // this provider-order join.
        for call in running {
            completed.push(match call {
                PendingCall::Immediate(call) => call,
                PendingCall::Running {
                    call_uuid,
                    tool_call_id,
                    task,
                } => match task.await {
                    Ok(call) => call,
                    Err(error) => CompletedCall {
                        call_uuid,
                        tool_call_id,
                        result: error_result(format!("tool task failed: {error}")),
                        synthetic: true,
                    },
                },
            });
        }
        completed
    }

    async fn close_failed_step(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        step: u64,
        step_uuid: &str,
    ) -> Result<(), SessionError> {
        session
            .append_loop_event(LoopEvent::StepEnd {
                uuid: step_uuid.to_owned(),
                turn_id: turn_id.to_string(),
                step,
                usage: None,
                finish_reason: Some(LoopStepStopReason::Unknown),
                llm_first_token_latency_ms: None,
                llm_stream_duration_ms: None,
                llm_request_build_ms: None,
                llm_server_first_token_ms: None,
                llm_server_decode_ms: None,
                llm_client_consume_ms: None,
                provider_finish_reason: None,
                raw_finish_reason: None,
                message_id: None,
            })
            .await
    }

    async fn finish_aborted(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        attempted_steps: u32,
        active_step: Option<u64>,
        usage: TokenUsage,
    ) -> Result<TurnOutcome, TurnError> {
        session.record_turn_cancel(turn_id, "aborted").await?;
        self.record_interruption(
            session,
            turn_id,
            attempted_steps,
            active_step,
            InterruptionReason::Aborted,
            "turn aborted",
        )
        .await?;
        self.record_terminal(session, turn_id, TurnEndReason::Cancelled, usage)
            .await?;
        Ok(TurnOutcome {
            turn_id,
            attempted_steps,
            usage,
            reason: TurnOutcomeReason::Aborted,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_retry(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        step: u64,
        step_uuid: &str,
        failed_attempt: u64,
        delay_ms: u64,
        error: &ProviderError,
    ) -> Result<(), SessionError> {
        let next_attempt = failed_attempt.saturating_add(1);
        let max_attempts = u64::from(self.config.max_retries_per_step)
            .saturating_add(u64::from(self.config.max_context_overflow_fallbacks))
            .saturating_add(1);
        let event = AgentEvent::TurnStepRetrying {
            turn_id,
            step,
            step_id: Some(step_uuid.to_owned()),
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
            error_name: format!("{:?}", error.kind),
            error_message: error.message.clone(),
            status_code: error.status_code,
        };
        session
            .append_observation(
                TURN_STEP_RETRYING_RECORD_TYPE,
                json!({
                    "turnId": turn_id,
                    "step": step,
                    "stepId": step_uuid,
                    "failedAttempt": failed_attempt,
                    "nextAttempt": next_attempt,
                    "maxAttempts": max_attempts,
                    "delayMs": delay_ms,
                    "errorName": format!("{:?}", error.kind),
                    "errorMessage": error.message,
                    "statusCode": error.status_code,
                }),
                event,
            )
            .await
    }

    async fn record_interruption(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        attempted_steps: u32,
        active_step: Option<u64>,
        reason: InterruptionReason,
        message: &str,
    ) -> Result<(), SessionError> {
        let step = active_step.unwrap_or(u64::from(attempted_steps));
        let reason_text = format!("{reason:?}").to_lowercase();
        let event = AgentEvent::TurnStepInterrupted {
            turn_id,
            step,
            step_id: None,
            reason: reason_text.clone(),
            message: Some(message.to_owned()),
        };
        session
            .append_observation(
                TURN_INTERRUPTED_RECORD_TYPE,
                json!({
                    "turnId": turn_id,
                    "attemptedSteps": attempted_steps,
                    "activeStep": active_step,
                    "step": step,
                    "reason": reason_text,
                    "message": message,
                }),
                event,
            )
            .await
    }

    async fn record_terminal(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        reason: TurnEndReason,
        usage: TokenUsage,
    ) -> Result<(), SessionError> {
        self.record_terminal_with_steers(session, turn_id, reason, usage, false)
            .await
            .map(|_| ())
    }

    async fn record_terminal_with_steers(
        &self,
        session: &SessionHandle,
        turn_id: u64,
        reason: TurnEndReason,
        usage: TokenUsage,
        continue_for_steer: bool,
    ) -> Result<bool, SessionError> {
        session
            .append_terminal_observation(
                TURN_TERMINAL_RECORD_TYPE,
                json!({
                    "turnId": turn_id,
                    "reason": reason,
                    "usage": usage,
                }),
                AgentEvent::TurnEnded {
                    turn_id,
                    reason,
                    error: None,
                    duration_ms: None,
                },
                continue_for_steer,
            )
            .await
    }
}

struct TurnStreamCollector<'a> {
    session: &'a SessionHandle,
    turn_id: u64,
    assembler: StreamAssembler,
    response_ended: bool,
    tool_ids: BTreeSet<String>,
    tool_indexes: BTreeMap<StreamIndex, String>,
    last_tool_id: Option<String>,
}

impl<'a> TurnStreamCollector<'a> {
    fn new(session: &'a SessionHandle, turn_id: u64) -> Self {
        Self {
            session,
            turn_id,
            assembler: StreamAssembler::default(),
            response_ended: false,
            tool_ids: BTreeSet::new(),
            tool_indexes: BTreeMap::new(),
            last_tool_id: None,
        }
    }

    fn finish(self) -> Result<GenerateResult, ProviderError> {
        if !self.response_ended {
            return Err(malformed_stream_error(
                "provider stream ended without response_end",
            ));
        }
        self.assembler
            .finish()
            .map_err(|error| malformed_stream_error(&error.to_string()))
    }

    fn validate_part(&mut self, part: &StreamPart) -> Result<(), ProviderError> {
        if let StreamPart::Function {
            id,
            name,
            stream_index,
            ..
        } = part
        {
            if id.trim().is_empty() || name.trim().is_empty() {
                return Err(malformed_stream_error(
                    "streamed tool calls require non-empty ids and names",
                ));
            }
            if !self.tool_ids.insert(id.clone()) {
                return Err(malformed_stream_error(
                    "provider stream contains a duplicate tool call id",
                ));
            }
            if let Some(index) = stream_index {
                if self
                    .tool_indexes
                    .insert(index.clone(), id.clone())
                    .is_some()
                {
                    return Err(malformed_stream_error(
                        "provider stream contains a duplicate tool call index",
                    ));
                }
            }
            self.last_tool_id = Some(id.clone());
        }
        Ok(())
    }

    fn publish_part(&self, part: &StreamPart) {
        let event = match part {
            StreamPart::Text { text } => Some(AgentEvent::AssistantDelta {
                turn_id: self.turn_id,
                delta: text.clone(),
            }),
            StreamPart::Think { think, .. } => Some(AgentEvent::ThinkingDelta {
                turn_id: self.turn_id,
                delta: think.clone(),
            }),
            StreamPart::Function {
                id,
                name,
                arguments,
                ..
            } => Some(AgentEvent::ToolCallDelta {
                turn_id: self.turn_id,
                tool_call_id: id.clone(),
                name: Some(name.clone()),
                arguments_part: arguments.clone(),
            }),
            StreamPart::ToolCallPart {
                arguments_part,
                index,
            } => match index {
                Some(index) => self.tool_indexes.get(index),
                None => self.last_tool_id.as_ref(),
            }
            .map(|id| AgentEvent::ToolCallDelta {
                turn_id: self.turn_id,
                tool_call_id: id.clone(),
                name: None,
                arguments_part: arguments_part.clone(),
            }),
            StreamPart::ImageUrl { .. }
            | StreamPart::AudioUrl { .. }
            | StreamPart::VideoUrl { .. } => None,
        };
        if let Some(event) = event {
            self.session.publish_live(event);
        }
    }
}

impl TurnProviderStreamSink for TurnStreamCollector<'_> {
    fn push(&mut self, event: ProviderStreamEvent) -> Result<(), ProviderError> {
        if let ProviderStreamEvent::Part { part } = &event {
            self.validate_part(part)?;
        }
        self.assembler
            .push(event.clone())
            .map_err(|error| malformed_stream_error(&error.to_string()))?;
        match &event {
            ProviderStreamEvent::Part { part } => self.publish_part(part),
            ProviderStreamEvent::ResponseEnd => self.response_ended = true,
            ProviderStreamEvent::ResponseStart { .. }
            | ProviderStreamEvent::Usage { .. }
            | ProviderStreamEvent::Finish { .. } => {}
        }
        Ok(())
    }
}

fn malformed_stream_error(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedResponse, message)
}

#[derive(Clone)]
struct PreparedCall {
    call_uuid: String,
    provider_call_id: String,
    runtime_call_id: ToolCallId,
    name: String,
    arguments: Value,
    extras: BTreeMap<String, Value>,
    prepared: Result<(Arc<dyn crate::ExecutableTool>, ToolExecutionSpec), String>,
}

enum PendingCall {
    Immediate(CompletedCall),
    Running {
        call_uuid: String,
        tool_call_id: String,
        task: tokio::task::JoinHandle<CompletedCall>,
    },
}

struct CompletedCall {
    call_uuid: String,
    tool_call_id: String,
    result: ExecutableToolResult,
    synthetic: bool,
}

impl CompletedCall {
    fn synthetic(call: PreparedCall, message: &str) -> Self {
        Self {
            call_uuid: call.call_uuid,
            tool_call_id: call.provider_call_id,
            result: error_result(message.to_owned()),
            synthetic: true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_call(
    session: SessionHandle,
    hooks: HookRunner,
    scheduler: ToolScheduler,
    tool: Arc<dyn crate::ExecutableTool>,
    spec: ToolExecutionSpec,
    call: PreparedCall,
    turn_id: u64,
    cancellation: CancellationToken,
    cancellation_grace: Duration,
) -> CompletedCall {
    let pre_input = hook_input(&session, turn_id, &call, ToolHookEvent::PreToolUse, None);
    let pre = hooks
        .run(ToolHookEvent::PreToolUse, &pre_input, &cancellation)
        .await;
    emit_hook_report(&session, turn_id, ToolHookEvent::PreToolUse, &pre);
    if let Some(block) = pre.blocked {
        return CompletedCall::synthetic(call, &block.reason);
    }

    let authorization = session
        .authorize_tool(&crate::ToolPermissionRequest {
            turn_id,
            tool_call_id: call.runtime_call_id.clone(),
            tool_name: call.name.clone(),
            action: spec.action.clone(),
            display: spec.display.clone(),
            approval_rule: spec.approval_rule.clone(),
            rule_subject: spec.rule_subject.clone(),
            exclusive_tool: spec.exclusive_tool,
            plan_policy: spec.plan_policy.clone(),
            create_goal_review: spec.create_goal_review,
            sensitive_file: spec.sensitive_file,
            git_control: spec.git_control,
            git_cwd_write: spec.git_cwd_write,
        })
        .await;
    match authorization {
        Ok(Authorization {
            verdict: PermissionVerdict::Allow,
            ..
        }) => {}
        Ok(authorization) => {
            return CompletedCall::synthetic(
                call,
                authorization
                    .reason
                    .as_deref()
                    .unwrap_or("tool permission denied"),
            );
        }
        Err(error) => {
            return CompletedCall::synthetic(call, &format!("authorization failed: {error}"));
        }
    }

    let permit = match scheduler
        .acquire(spec.accesses.clone(), &cancellation)
        .await
    {
        Ok(permit) => permit,
        Err(ScheduleError::Cancelled) => {
            return CompletedCall::synthetic(call, "tool execution cancelled before start");
        }
    };
    let updates: Arc<dyn ToolUpdateSink> = Arc::new(SessionUpdateSink {
        session: session.clone(),
        turn_id,
        tool_call_id: call.provider_call_id.clone(),
    });
    let invocation = ToolInvocation {
        context: ToolPrepareContext {
            session_id: session.id().clone(),
            agent_id: session.main_agent_id().clone(),
            turn_id,
            tool_call_id: call.runtime_call_id.clone(),
        },
        arguments: call.arguments.clone(),
        cancellation: cancellation.clone(),
        updates,
    };
    let mut task = tokio::spawn(async move { tool.execute(invocation).await });
    let result = tokio::select! {
        result = &mut task => join_tool(result),
        _ = cancellation.cancelled() => {
            match tokio::time::timeout(cancellation_grace, &mut task).await {
                Ok(result) => join_tool(result),
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    Err(ToolError::Execute("tool did not stop within cancellation grace".to_owned()))
                }
            }
        }
    };
    drop(permit);
    let mut result = result.unwrap_or_else(|error| error_result(error.to_string()));

    let hook_event = if result.is_error {
        ToolHookEvent::PostToolUseFailure
    } else {
        ToolHookEvent::PostToolUse
    };
    let post_input = hook_input(&session, turn_id, &call, hook_event, Some(result.clone()));
    let post = hooks.run(hook_event, &post_input, &cancellation).await;
    emit_hook_report(&session, turn_id, hook_event, &post);
    if let Some(block) = post.blocked {
        result.is_error = true;
        result.note = Some(match result.note.take() {
            Some(note) => format!("{note}\nPost-tool hook failed: {}", block.reason),
            None => format!("Post-tool hook failed: {}", block.reason),
        });
    }
    CompletedCall {
        call_uuid: call.call_uuid,
        tool_call_id: call.provider_call_id,
        result,
        synthetic: false,
    }
}

fn hook_input(
    session: &SessionHandle,
    turn_id: u64,
    call: &PreparedCall,
    event: ToolHookEvent,
    result: Option<ExecutableToolResult>,
) -> ToolHookInput {
    ToolHookInput {
        hook_event_name: event,
        session_id: session.id().clone(),
        agent_id: session.main_agent_id().clone(),
        turn_id,
        tool_call_id: call.runtime_call_id.clone(),
        tool_name: call.name.clone(),
        arguments: call.arguments.clone(),
        content: serde_json::to_string(&call.arguments).unwrap_or_default(),
        result,
    }
}

fn emit_hook_report(
    session: &SessionHandle,
    turn_id: u64,
    event: ToolHookEvent,
    report: &HookRunReport,
) {
    for execution in &report.executions {
        let content = execution
            .message
            .clone()
            .or_else(|| execution.blocked.as_ref().map(|block| block.reason.clone()))
            .unwrap_or_default();
        session.publish_live(AgentEvent::HookResult {
            turn_id: Some(turn_id),
            hook_event: format!("{event:?}"),
            content,
            blocked: execution.blocked.as_ref().map(|_| true),
        });
    }
}

struct SessionUpdateSink {
    session: SessionHandle,
    turn_id: u64,
    tool_call_id: String,
}

impl ToolUpdateSink for SessionUpdateSink {
    fn emit(&self, update: ToolUpdate) {
        self.session.publish_live(AgentEvent::ToolProgress {
            turn_id: self.turn_id,
            tool_call_id: self.tool_call_id.clone(),
            update,
        });
    }
}

fn join_tool(
    result: Result<Result<ExecutableToolResult, ToolError>, tokio::task::JoinError>,
) -> Result<ExecutableToolResult, ToolError> {
    result.map_err(|error| ToolError::Execute(format!("tool task failed: {error}")))?
}

fn parse_arguments(arguments: Option<&str>) -> Result<Value, String> {
    let arguments = arguments.unwrap_or("{}");
    let value: Value = serde_json::from_str(arguments)
        .map_err(|error| format!("malformed tool arguments: {error}"))?;
    if !value.is_object() {
        return Err("tool arguments must be a JSON object".to_owned());
    }
    Ok(value)
}

fn validate_provider_response(response: &GenerateResult) -> Result<(), TurnError> {
    response
        .message
        .validate()
        .map_err(|error| TurnError::MalformedResponse(error.to_string()))?;
    if response.message.content.iter().any(|part| {
        matches!(
            part,
            ContentPart::ImageUrl { .. }
                | ContentPart::AudioUrl { .. }
                | ContentPart::VideoUrl { .. }
        )
    }) {
        return Err(TurnError::UnsupportedProviderMedia);
    }
    Ok(())
}

fn error_result(message: String) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(message),
        is_error: true,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

fn step_stop_reason(reason: Option<FinishReason>, has_tools: bool) -> LoopStepStopReason {
    if has_tools || reason == Some(FinishReason::ToolCalls) {
        return LoopStepStopReason::ToolUse;
    }
    match reason {
        Some(FinishReason::Completed) | None => LoopStepStopReason::EndTurn,
        Some(FinishReason::Truncated) => LoopStepStopReason::MaxTokens,
        Some(FinishReason::Filtered) => LoopStepStopReason::Filtered,
        Some(FinishReason::Paused) => LoopStepStopReason::Paused,
        Some(FinishReason::Other) => LoopStepStopReason::Unknown,
        Some(FinishReason::ToolCalls) => LoopStepStopReason::ToolUse,
    }
}

fn terminal_reason(reason: Option<FinishReason>) -> TurnOutcomeReason {
    match reason {
        Some(FinishReason::Truncated) => TurnOutcomeReason::MaxTokens,
        Some(FinishReason::Filtered) => TurnOutcomeReason::Filtered,
        Some(FinishReason::Paused) => TurnOutcomeReason::Paused,
        _ => TurnOutcomeReason::Completed,
    }
}

fn turn_error_name(error: &TurnError) -> &'static str {
    match error {
        TurnError::InvalidConfig(_) => "InvalidConfig",
        TurnError::InvalidRequest(_) => "InvalidRequest",
        TurnError::MalformedResponse(_) => "MalformedResponse",
        TurnError::UnsupportedProviderMedia => "UnsupportedProviderMedia",
        TurnError::MaxStepsExceeded(_) => "MaxStepsExceeded",
        TurnError::HookBlocked(_) => "HookBlocked",
        TurnError::Compaction(_) => "CompactionError",
        TurnError::Provider(_) => "ProviderError",
        TurnError::Session(_) => "SessionError",
    }
}

fn is_context_overflow(error: &ProviderError) -> bool {
    error.status_code == Some(413)
        || (error.kind == ProviderErrorKind::InvalidRequest
            && error.message.to_ascii_lowercase().contains("context"))
}

fn overflow_projection(history: &[Message], tier: u8) -> Vec<Message> {
    if tier == 1 {
        return history
            .iter()
            .cloned()
            .map(|mut message| {
                message.content = message
                    .content
                    .into_iter()
                    .map(|part| match part {
                        ContentPart::ImageUrl { .. }
                        | ContentPart::AudioUrl { .. }
                        | ContentPart::VideoUrl { .. } => {
                            ContentPart::text("[media omitted during context fallback]")
                        }
                        part => part,
                    })
                    .collect();
                message
            })
            .collect();
    }
    let divisor = 1usize << usize::from(tier.saturating_sub(1));
    let keep = history.len().div_ceil(divisor).max(1);
    let mut start = history.len().saturating_sub(keep);
    while start < history.len() && history[start].role == Role::Tool {
        start += 1;
    }
    history[start..].to_vec()
}

struct LinkedCancellation {
    token: CancellationToken,
    watchers: Vec<tokio::task::JoinHandle<()>>,
}

impl LinkedCancellation {
    fn new(caller: CancellationToken, session: CancellationToken) -> Self {
        let token = CancellationToken::new();
        if caller.is_cancelled() || session.is_cancelled() {
            token.cancel();
        }
        let mut watchers = Vec::with_capacity(2);
        for source in [caller, session] {
            let token_for_source = token.clone();
            let token_for_exit = token.clone();
            watchers.push(tokio::spawn(async move {
                tokio::select! {
                        _ = source.cancelled() => { token_for_source.cancel(); }
                        _ = token_for_exit.cancelled() => {}
                }
            }));
        }
        Self { token, watchers }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for LinkedCancellation {
    fn drop(&mut self) {
        self.token.cancel();
        for watcher in &self.watchers {
            watcher.abort();
        }
    }
}

enum StepFailure {
    Cancelled,
    Provider(ProviderError),
    Compaction(CompactionError),
    Session(SessionError),
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("invalid turn engine configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid provider request: {0}")]
    InvalidRequest(String),
    #[error("malformed provider response: {0}")]
    MalformedResponse(String),
    #[error("provider output media is not representable by retained loop records")]
    UnsupportedProviderMedia,
    #[error("turn exceeded {0} steps")]
    MaxStepsExceeded(u32),
    #[error("turn blocked by UserPromptSubmit hook: {0}")]
    HookBlocked(String),
    #[error(transparent)]
    Compaction(#[from] CompactionError),
    #[error(transparent)]
    Provider(ProviderError),
    #[error(transparent)]
    Session(#[from] SessionError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Mutex,
        },
    };

    use mycel_agent_protocol::{
        FileOperation, OptionalNullable, PermissionDecision, PermissionRule, PermissionScope,
        ToolCallKind, ToolDefinition, ToolInputDisplay,
    };

    use crate::{read_record_file, Runtime, SessionId, SessionOptions, ToolFuture};

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mycel-turn-{name}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct FakeProvider {
        responses: Mutex<VecDeque<Result<GenerateResult, ProviderError>>>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<Result<GenerateResult, ProviderError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl TurnProvider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        fn complete<'a>(
            &'a self,
            request: ProviderRequest,
            _cancellation: CancellationToken,
        ) -> TurnProviderFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("fake response");
            Box::pin(async move { response })
        }
    }

    enum StreamAction {
        Event(ProviderStreamEvent),
        Error(ProviderError),
        Signal(Arc<tokio::sync::Notify>),
        Wait(Arc<tokio::sync::Notify>),
        WaitForCancellation,
    }

    struct ScriptedStreamProvider {
        attempts: Mutex<VecDeque<Vec<StreamAction>>>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    impl ScriptedStreamProvider {
        fn new(attempts: Vec<Vec<StreamAction>>) -> Self {
            Self {
                attempts: Mutex::new(attempts.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl TurnProvider for ScriptedStreamProvider {
        fn name(&self) -> &str {
            "streaming-fake"
        }

        fn model(&self) -> &str {
            "streaming-model"
        }

        fn complete<'a>(
            &'a self,
            _request: ProviderRequest,
            _cancellation: CancellationToken,
        ) -> TurnProviderFuture<'a> {
            Box::pin(async {
                Err(ProviderError::new(
                    ProviderErrorKind::Other,
                    "aggregate path must not be used",
                ))
            })
        }

        fn stream<'a>(
            &'a self,
            request: ProviderRequest,
            cancellation: CancellationToken,
            sink: &'a mut dyn TurnProviderStreamSink,
        ) -> TurnProviderStreamFuture<'a> {
            self.requests.lock().expect("stream requests").push(request);
            let actions = self
                .attempts
                .lock()
                .expect("stream attempts")
                .pop_front()
                .expect("scripted stream attempt");
            Box::pin(async move {
                for action in actions {
                    match action {
                        StreamAction::Event(event) => sink.push(event)?,
                        StreamAction::Error(error) => return Err(error),
                        StreamAction::Signal(signal) => signal.notify_one(),
                        StreamAction::Wait(signal) => signal.notified().await,
                        StreamAction::WaitForCancellation => {
                            cancellation.cancelled().await;
                            return Err(ProviderError::new(
                                ProviderErrorKind::Cancelled,
                                "stream cancelled",
                            ));
                        }
                    }
                }
                Ok(())
            })
        }
    }

    fn stream_start(id: &str) -> StreamAction {
        StreamAction::Event(ProviderStreamEvent::ResponseStart {
            id: Some(id.to_owned()),
            trace_id: OptionalNullable::Missing,
        })
    }

    fn stream_text(text: &str) -> StreamAction {
        StreamAction::Event(ProviderStreamEvent::Part {
            part: StreamPart::Text {
                text: text.to_owned(),
            },
        })
    }

    fn stream_finish(reason: FinishReason) -> StreamAction {
        StreamAction::Event(ProviderStreamEvent::Finish {
            reason: Some(reason),
            raw_reason: None,
        })
    }

    fn stream_end() -> StreamAction {
        StreamAction::Event(ProviderStreamEvent::ResponseEnd)
    }

    fn completed_stream(id: &str, text: &str) -> Vec<StreamAction> {
        vec![
            stream_start(id),
            stream_text(text),
            stream_finish(FinishReason::Completed),
            stream_end(),
        ]
    }

    struct FakeTool {
        name: String,
        output: String,
        delay: Duration,
        stop_batch: bool,
        executions: AtomicUsize,
        saw_usage: Option<Arc<AtomicBool>>,
        record_path: Option<std::path::PathBuf>,
    }

    impl FakeTool {
        fn new(name: &str, output: &str) -> Self {
            Self {
                name: name.to_owned(),
                output: output.to_owned(),
                delay: Duration::ZERO,
                stop_batch: false,
                executions: AtomicUsize::new(0),
                saw_usage: None,
                record_path: None,
            }
        }
    }

    impl crate::ExecutableTool for FakeTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name.clone(),
                description: self.name.clone(),
                parameters: json!({
                    "type":"object",
                    "properties":{"value":{"type":"string"}},
                    "required":["value"],
                    "additionalProperties":false
                }),
                deferred: false,
            }
        }

        fn prepare(
            &self,
            _arguments: &Value,
            _context: &ToolPrepareContext,
        ) -> Result<ToolExecutionSpec, ToolError> {
            let mut spec = ToolExecutionSpec::new(
                ToolInputDisplay::FileIo {
                    operation: FileOperation::Read,
                    path: self.name.clone(),
                    detail: None,
                    content: None,
                    before: None,
                    after: None,
                },
                "test tool",
            );
            spec.accesses = vec![crate::ToolAccess::None];
            spec.stop_batch_after_this = self.stop_batch;
            Ok(spec)
        }

        fn execute<'a>(&'a self, _invocation: ToolInvocation) -> ToolFuture<'a> {
            self.executions.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if let (Some(observed), Some(path)) = (&self.saw_usage, &self.record_path) {
                    let records = read_record_file(path)
                        .await
                        .map_err(|error| ToolError::Execute(error.to_string()))?;
                    observed.store(
                        records.records.iter().any(|record| {
                            record.kind() == Some(mycel_agent_protocol::RecordKind::UsageRecord)
                        }),
                        Ordering::Release,
                    );
                }
                tokio::time::sleep(self.delay).await;
                Ok(ExecutableToolResult {
                    output: ExecutableToolOutput::Text(self.output.clone()),
                    is_error: false,
                    stop_turn: false,
                    message: None,
                    note: None,
                    truncated: false,
                })
            })
        }
    }

    struct RegisterTool {
        registry: ToolRegistry,
        next: Mutex<Option<Arc<dyn crate::ExecutableTool>>>,
    }

    impl crate::ExecutableTool for RegisterTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "Register".to_owned(),
                description: "register the next tool".to_owned(),
                parameters: json!({
                    "type":"object",
                    "properties":{"value":{"type":"string"}},
                    "required":["value"],
                    "additionalProperties":false
                }),
                deferred: false,
            }
        }

        fn prepare(
            &self,
            _arguments: &Value,
            _context: &ToolPrepareContext,
        ) -> Result<ToolExecutionSpec, ToolError> {
            Ok(ToolExecutionSpec::new(
                ToolInputDisplay::FileIo {
                    operation: FileOperation::Read,
                    path: "registry".to_owned(),
                    detail: None,
                    content: None,
                    before: None,
                    after: None,
                },
                "register test tool",
            ))
        }

        fn execute<'a>(&'a self, _invocation: ToolInvocation) -> ToolFuture<'a> {
            Box::pin(async move {
                let next =
                    self.next.lock().expect("next tool").take().ok_or_else(|| {
                        ToolError::Execute("next tool already registered".to_owned())
                    })?;
                self.registry
                    .register(next)
                    .map_err(|error| ToolError::Execute(error.to_string()))?;
                Ok(ExecutableToolResult {
                    output: ExecutableToolOutput::Text("registered".to_owned()),
                    is_error: false,
                    stop_turn: false,
                    message: None,
                    note: None,
                    truncated: false,
                })
            })
        }
    }

    fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            kind: ToolCallKind::Function,
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: Some(arguments.to_owned()),
            extras: BTreeMap::new(),
        }
    }

    fn response(content: &str, calls: Vec<ToolCall>, usage: u64) -> GenerateResult {
        GenerateResult {
            id: Some(RequestId::generate().into_string()),
            message: Message::assistant(vec![ContentPart::text(content)], calls),
            usage: Some(TokenUsage {
                input_other: usage,
                output: 1,
                input_cache_read: 0,
                input_cache_creation: 0,
            }),
            finish_reason: Some(if content == "done" {
                FinishReason::Completed
            } else {
                FinishReason::ToolCalls
            }),
            raw_finish_reason: None,
            trace_id: OptionalNullable::Missing,
        }
    }

    async fn session(root: &std::path::Path) -> SessionHandle {
        let mut options = SessionOptions::new(SessionId::new("s1").expect("id"));
        options.permission_rules.push(PermissionRule {
            decision: PermissionDecision::Allow,
            scope: PermissionScope::TurnOverride,
            pattern: "*".to_owned(),
            reason: Some("turn-engine fixture tools are deterministic".to_owned()),
        });
        Runtime::new(root)
            .create_session(options)
            .await
            .expect("session")
    }

    fn engine(
        provider: Arc<dyn TurnProvider>,
        tools: ToolRegistry,
        hooks: HookRunner,
    ) -> TurnEngine {
        TurnEngine::new(
            provider,
            tools,
            hooks,
            ToolScheduler::new(),
            TurnEngineConfig {
                retry_delay: Duration::ZERO,
                ..TurnEngineConfig::default()
            },
        )
        .expect("engine")
    }

    fn durable_assistant_text(records: &[mycel_agent_protocol::AgentRecord]) -> Vec<String> {
        records
            .iter()
            .filter(|record| {
                record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                    && record.payload["event"]["type"] == "content.part"
                    && record.payload["event"]["part"]["type"] == "text"
            })
            .filter_map(|record| record.payload["event"]["part"]["text"].as_str())
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn threshold_auto_compaction_folds_context_before_the_provider_step() {
        let root = temp_root("auto-compaction");
        let session = session(&root).await;
        session
            .append_user_message("x".repeat(400), PromptOrigin::User)
            .await
            .expect("seed oversized history");
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response("handoff summary", Vec::new(), 1)),
            Ok(response("answer after compaction", Vec::new(), 1)),
        ]));
        let engine = TurnEngine::new(
            provider.clone(),
            ToolRegistry::new(),
            HookRunner::new(),
            ToolScheduler::new(),
            TurnEngineConfig {
                retry_delay: Duration::ZERO,
                auto_compaction: Some(AutoCompactionConfig {
                    max_context_tokens: 100,
                    trigger_ratio: 0.5,
                    reserved_context_tokens: 0,
                }),
                ..TurnEngineConfig::default()
            },
        )
        .expect("engine");

        engine
            .run_turn(
                &session,
                TurnInput::user("new prompt", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");

        {
            let requests = provider.requests.lock().expect("requests");
            assert_eq!(requests.len(), 2);
            assert!(requests[0]
                .history
                .last()
                .expect("compaction instruction")
                .text("")
                .contains("first-person handoff note"));
            assert!(requests[1]
                .history
                .iter()
                .any(|message| message.text("").contains("handoff summary")));
        }
        assert_eq!(
            session.snapshot().await.state.compaction,
            crate::CompactionState::Completed
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn host_tool_invocation_uses_governed_durable_execution_without_provider() {
        let root = temp_root("host-tool");
        let session = session(&root).await;
        let tools = ToolRegistry::new();
        let tool = Arc::new(FakeTool::new("Agent", "delegated"));
        tools.register(tool.clone()).expect("register tool");
        let provider = Arc::new(FakeProvider::new(Vec::new()));
        let engine = engine(provider.clone(), tools, HookRunner::new());

        let result = engine
            .invoke_host_tool(
                &session,
                "/delegate inspect the patch",
                "Agent",
                json!({"value":"inspect the patch"}),
                CancellationToken::new(),
            )
            .await
            .expect("host invocation");

        assert_eq!(
            result.output,
            ExecutableToolOutput::Text("delegated".to_owned())
        );
        assert!(!result.is_error);
        assert_eq!(tool.executions.load(Ordering::Relaxed), 1);
        assert!(provider.requests.lock().expect("requests").is_empty());
        let records = read_record_file(session.record_path())
            .await
            .expect("durable records");
        let kinds = records
            .records
            .iter()
            .filter_map(|record| record.kind())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&mycel_agent_protocol::RecordKind::TurnPrompt));
        assert!(records.records.iter().any(|record| {
            record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                && record.payload["event"]["type"] == "tool.call"
                && record.payload["event"]["name"] == "Agent"
        }));
        assert!(records.records.iter().any(|record| {
            record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                && record.payload["event"]["type"] == "tool.result"
        }));
        assert!(records
            .records
            .iter()
            .any(|record| { record.record_type == TURN_TERMINAL_RECORD_TYPE }));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn streamed_deltas_arrive_before_final_assembly_and_are_not_replayed() {
        let root = temp_root("stream-timing");
        let session = session(&root).await;
        let mut events = session.subscribe();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ScriptedStreamProvider::new(vec![vec![
            stream_start("timing"),
            stream_text("early"),
            StreamAction::Signal(entered.clone()),
            StreamAction::Wait(release.clone()),
            stream_text(" late"),
            stream_finish(FinishReason::Completed),
            stream_end(),
        ]]));
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            engine(provider, ToolRegistry::new(), HookRunner::new())
                .run_turn(
                    &run_session,
                    TurnInput::user("go", "system"),
                    CancellationToken::new(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("first delta emitted");

        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("live delta timeout")
                .expect("live delta");
            if matches!(
                event.event,
                AgentEvent::AssistantDelta { ref delta, .. } if delta == "early"
            ) {
                break;
            }
        }
        let before = read_record_file(session.record_path())
            .await
            .expect("records before completion");
        assert!(durable_assistant_text(&before.records).is_empty());

        release.notify_one();
        task.await.expect("join").expect("turn");
        let mut later_deltas = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("terminal event timeout")
                .expect("terminal event");
            match event.event {
                AgentEvent::AssistantDelta { delta, .. } => later_deltas.push(delta),
                AgentEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }
        assert_eq!(later_deltas, [" late"]);
        let records = read_record_file(session.record_path())
            .await
            .expect("final records");
        assert_eq!(durable_assistant_text(&records.records), ["early late"]);
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn accepted_steer_is_durable_and_forces_a_follow_up_step_before_terminal() {
        let root = temp_root("steer-boundary");
        let session = session(&root).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ScriptedStreamProvider::new(vec![
            vec![
                stream_start("first"),
                StreamAction::Signal(entered.clone()),
                StreamAction::Wait(release.clone()),
                stream_text("first answer"),
                stream_finish(FinishReason::Completed),
                stream_end(),
            ],
            completed_stream("second", "redirected answer"),
        ]));
        let run_engine = Arc::new(engine(
            provider.clone(),
            ToolRegistry::new(),
            HookRunner::new(),
        ));
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            run_engine
                .run_turn(
                    &run_session,
                    TurnInput::user("initial", "system"),
                    CancellationToken::new(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("first request entered");
        session
            .steer(vec![ContentPart::text("redirect")], PromptOrigin::User)
            .await
            .expect("steer active turn");
        let before = read_record_file(session.record_path())
            .await
            .expect("records before release");
        assert_eq!(
            before
                .records
                .last()
                .and_then(mycel_agent_protocol::AgentRecord::kind),
            Some(mycel_agent_protocol::RecordKind::TurnSteer)
        );

        release.notify_one();
        let outcome = task.await.expect("join").expect("turn");
        assert_eq!(outcome.attempted_steps, 2);
        {
            let requests = provider.requests.lock().expect("stream requests");
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1]
                    .history
                    .iter()
                    .map(|message| message.text(""))
                    .collect::<Vec<_>>(),
                ["initial", "first answer", "redirect"]
            );
        }
        let records = read_record_file(session.record_path())
            .await
            .expect("final records");
        assert_eq!(
            records
                .records
                .iter()
                .filter(|record| record.record_type == TURN_TERMINAL_RECORD_TYPE)
                .count(),
            1
        );
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn streamed_assistant_thinking_and_tool_deltas_preserve_arrival_order() {
        let root = temp_root("stream-order");
        let session = session(&root).await;
        let mut events = session.subscribe();
        let provider = Arc::new(ScriptedStreamProvider::new(vec![
            vec![
                stream_start("tools"),
                stream_text("answer"),
                StreamAction::Event(ProviderStreamEvent::Part {
                    part: StreamPart::Think {
                        think: "reason".to_owned(),
                        encrypted: None,
                    },
                }),
                StreamAction::Event(ProviderStreamEvent::Part {
                    part: StreamPart::Function {
                        id: "call-1".to_owned(),
                        name: "One".to_owned(),
                        arguments: Some("{".to_owned()),
                        extras: BTreeMap::new(),
                        stream_index: Some(StreamIndex::Number(0)),
                    },
                }),
                StreamAction::Event(ProviderStreamEvent::Part {
                    part: StreamPart::ToolCallPart {
                        arguments_part: Some(r#""value":"x"}"#.to_owned()),
                        index: Some(StreamIndex::Number(0)),
                    },
                }),
                stream_finish(FinishReason::ToolCalls),
                stream_end(),
            ],
            completed_stream("done", "done"),
        ]));
        let tools = ToolRegistry::new();
        tools
            .register(Arc::new(FakeTool::new("One", "one")))
            .expect("tool");
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            engine(provider, tools, HookRunner::new())
                .run_turn(
                    &run_session,
                    TurnInput::user("go", "system"),
                    CancellationToken::new(),
                )
                .await
        });

        let mut order = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("event timeout")
                .expect("event");
            match event.event {
                AgentEvent::AssistantDelta { delta, .. } => {
                    order.push(format!("assistant:{delta}"));
                }
                AgentEvent::ThinkingDelta { delta, .. } => {
                    order.push(format!("thinking:{delta}"));
                }
                AgentEvent::ToolCallDelta {
                    name,
                    arguments_part,
                    ..
                } => order.push(format!(
                    "tool-delta:{}:{}",
                    name.as_deref().unwrap_or(""),
                    arguments_part.as_deref().unwrap_or("")
                )),
                AgentEvent::ToolCallStarted { tool_call_id, .. } => {
                    order.push(format!("tool-started:{tool_call_id}"));
                }
                AgentEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }
        task.await.expect("join").expect("turn");
        assert_eq!(
            &order[..5],
            [
                "assistant:answer",
                "thinking:reason",
                "tool-delta:One:{",
                "tool-delta::\"value\":\"x\"}",
                "tool-started:call-1",
            ]
        );
        assert_eq!(order.last().map(String::as_str), Some("assistant:done"));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn retry_discards_failed_stream_partials_from_durable_context() {
        let root = temp_root("stream-retry");
        let session = session(&root).await;
        let mut events = session.subscribe();
        let provider = Arc::new(ScriptedStreamProvider::new(vec![
            vec![
                stream_start("failed"),
                stream_text("discard"),
                StreamAction::Error(ProviderError::new(
                    ProviderErrorKind::Connection,
                    "temporary disconnect",
                )),
            ],
            completed_stream("success", "keep"),
        ]));
        engine(provider, ToolRegistry::new(), HookRunner::new())
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("retried turn");

        let mut deltas = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("event timeout")
                .expect("event");
            match event.event {
                AgentEvent::AssistantDelta { delta, .. } => deltas.push(delta),
                AgentEvent::TurnEnded { .. } => break,
                _ => {}
            }
        }
        assert_eq!(deltas, ["discard", "keep"]);
        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        assert_eq!(durable_assistant_text(&records.records), ["keep"]);
        assert!(records
            .records
            .iter()
            .any(|record| record.record_type == TURN_STEP_RETRYING_RECORD_TYPE));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn cancellation_discards_stream_partials_and_closes_the_step() {
        let root = temp_root("stream-cancel");
        let session = session(&root).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let provider = Arc::new(ScriptedStreamProvider::new(vec![vec![
            stream_start("cancel"),
            stream_text("partial"),
            StreamAction::Signal(entered.clone()),
            StreamAction::WaitForCancellation,
        ]]));
        let cancellation = CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            engine(provider, ToolRegistry::new(), HookRunner::new())
                .run_turn(
                    &run_session,
                    TurnInput::user("go", "system"),
                    run_cancellation,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("stream entered");
        cancellation.cancel();
        let outcome = task.await.expect("join").expect("cancelled outcome");
        assert_eq!(outcome.reason, TurnOutcomeReason::Aborted);
        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        assert!(durable_assistant_text(&records.records).is_empty());
        assert!(records.records.iter().any(|record| {
            record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                && record.payload["event"]["type"] == "step.end"
        }));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn malformed_stream_without_response_end_fails_closed() {
        let root = temp_root("stream-malformed");
        let session = session(&root).await;
        let provider = Arc::new(ScriptedStreamProvider::new(vec![vec![
            stream_start("malformed"),
            stream_text("never durable"),
        ]]));
        let error = engine(provider, ToolRegistry::new(), HookRunner::new())
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect_err("malformed stream");
        assert!(matches!(
            error,
            TurnError::Provider(ProviderError {
                kind: ProviderErrorKind::MalformedResponse,
                ..
            })
        ));
        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        assert!(durable_assistant_text(&records.records).is_empty());
        assert!(records
            .records
            .iter()
            .any(|record| record.record_type == TURN_TERMINAL_RECORD_TYPE));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn usage_precedes_tools_and_results_remain_in_provider_order() {
        let root = temp_root("ordering");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response(
                "tools",
                vec![
                    tool_call("a", "Slow", r#"{"value":"a"}"#),
                    tool_call("b", "Fast", r#"{"value":"b"}"#),
                ],
                10,
            )),
            Ok(response("done", vec![], 4)),
        ]));
        let tools = ToolRegistry::new();
        let observed_usage = Arc::new(AtomicBool::new(false));
        let mut slow = FakeTool::new("Slow", "slow");
        slow.delay = Duration::from_millis(30);
        slow.saw_usage = Some(Arc::clone(&observed_usage));
        slow.record_path = Some(session.record_path().to_path_buf());
        tools.register(Arc::new(slow)).expect("slow");
        tools
            .register(Arc::new(FakeTool::new("Fast", "fast")))
            .expect("fast");
        let outcome = engine(provider, tools, HookRunner::new())
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");
        assert_eq!(outcome.attempted_steps, 2);
        assert!(observed_usage.load(Ordering::Acquire));

        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        let result_ids: Vec<String> = records
            .records
            .iter()
            .filter(|record| {
                record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                    && record.payload["event"]["type"] == "tool.result"
            })
            .filter_map(|record| {
                record.payload["event"]["toolCallId"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(result_ids, ["a", "b"]);
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn invalid_arguments_never_reach_hooks_or_tools() {
        let root = temp_root("validation");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response(
                "bad",
                vec![tool_call("bad", "Read", r#"{"value":1}"#)],
                1,
            )),
            Ok(response("done", vec![], 1)),
        ]));
        let tool = Arc::new(FakeTool::new("Read", "unreachable"));
        let tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("tool");
        let hooks = HookRunner::new();
        let marker = root.join("hook-ran");
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::PreToolUse,
                matcher: crate::HookMatcher::Any,
                command: format!("touch {}", marker.display()),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Closed,
            })
            .expect("hook");
        engine(provider, tools, hooks)
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");
        assert_eq!(tool.executions.load(Ordering::Relaxed), 0);
        assert!(!tokio::fs::try_exists(marker).await.expect("exists"));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn user_prompt_hook_blocks_before_provider_dispatch() {
        let root = temp_root("prompt-hook");
        tokio::fs::create_dir_all(&root).await.expect("mkdir");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![Ok(response("done", vec![], 1))]));
        let hooks = HookRunner::new();
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::UserPromptSubmit,
                matcher: crate::HookMatcher::tool_name_regex("^go$").expect("matcher"),
                command: "printf 'prompt denied' >&2; exit 2".to_owned(),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Open,
            })
            .expect("hook");
        let result = engine(provider.clone(), ToolRegistry::new(), hooks)
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(TurnError::HookBlocked(reason)) if reason == "prompt denied"));
        assert!(provider.requests.lock().expect("requests").is_empty());
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn stop_hook_gets_exactly_one_continuation() {
        let root = temp_root("stop-hook");
        tokio::fs::create_dir_all(&root).await.expect("mkdir");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response("done", vec![], 1)),
            Ok(response("done", vec![], 1)),
        ]));
        let hooks = HookRunner::new();
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::Stop,
                matcher: crate::HookMatcher::Any,
                command: "printf 'continue once' >&2; exit 2".to_owned(),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Open,
            })
            .expect("hook");
        let outcome = engine(provider.clone(), ToolRegistry::new(), hooks)
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");
        assert_eq!(outcome.attempted_steps, 2);
        assert_eq!(provider.requests.lock().expect("requests").len(), 2);
        assert!(session
            .snapshot()
            .await
            .state
            .context
            .history()
            .iter()
            .any(|entry| entry
                .message
                .content
                .iter()
                .any(|part| { part.as_text().is_some_and(|text| text == "continue once") })));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_and_post_hooks_wrap_tool_execution() {
        let root = temp_root("hook-order");
        tokio::fs::create_dir_all(&root).await.expect("mkdir");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response(
                "tool",
                vec![tool_call("call", "One", r#"{"value":"x"}"#)],
                1,
            )),
            Ok(response("done", vec![], 1)),
        ]));
        let tools = ToolRegistry::new();
        tools
            .register(Arc::new(FakeTool::new("One", "one")))
            .expect("one");
        let hooks = HookRunner::new();
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::PreToolUse,
                matcher: crate::HookMatcher::Any,
                command: "printf pre >> hook-order".to_owned(),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Closed,
            })
            .expect("pre hook");
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::PostToolUse,
                matcher: crate::HookMatcher::Any,
                command: "printf post >> hook-order".to_owned(),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Closed,
            })
            .expect("post hook");
        engine(provider, tools, hooks)
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");
        assert_eq!(
            tokio::fs::read_to_string(root.join("hook-order"))
                .await
                .expect("hook marker"),
            "prepost"
        );
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn stop_batch_produces_a_paired_skipped_result() {
        let root = temp_root("stop-batch");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response(
                "tools",
                vec![
                    tool_call("stop", "Stop", r#"{"value":"a"}"#),
                    tool_call("skipped", "Later", r#"{"value":"b"}"#),
                ],
                1,
            )),
            Ok(response("done", vec![], 1)),
        ]));
        let tools = ToolRegistry::new();
        let mut stop = FakeTool::new("Stop", "stopped batch");
        stop.stop_batch = true;
        let later = Arc::new(FakeTool::new("Later", "must not execute"));
        tools.register(Arc::new(stop)).expect("stop");
        tools.register(later.clone()).expect("later");

        engine(provider, tools, HookRunner::new())
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");
        assert_eq!(later.executions.load(Ordering::Relaxed), 0);
        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        let results: Vec<&Value> = records
            .records
            .iter()
            .filter(|record| {
                record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                    && record.payload["event"]["type"] == "tool.result"
            })
            .map(|record| &record.payload["event"])
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["toolCallId"], "stop");
        assert_eq!(results[1]["toolCallId"], "skipped");
        assert_eq!(results[1]["result"]["isError"], true);
        assert!(results[1]["result"]["output"]
            .as_str()
            .expect("text output")
            .contains("stopped the batch"));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn available_tools_are_rebuilt_after_every_step() {
        let root = temp_root("tool-rebuild");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response(
                "register",
                vec![tool_call("register", "Register", r#"{"value":"late"}"#)],
                1,
            )),
            Ok(response(
                "use-late",
                vec![tool_call("late", "Late", r#"{"value":"now"}"#)],
                1,
            )),
            Ok(response("done", vec![], 1)),
        ]));
        let tools = ToolRegistry::new();
        let late = Arc::new(FakeTool::new("Late", "late output"));
        tools
            .register(Arc::new(RegisterTool {
                registry: tools.clone(),
                next: Mutex::new(Some(late.clone())),
            }))
            .expect("register tool");

        engine(provider.clone(), tools, HookRunner::new())
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await
            .expect("turn");
        assert_eq!(late.executions.load(Ordering::Relaxed), 1);
        {
            let requests = provider.requests.lock().expect("requests");
            assert_eq!(requests.len(), 3);
            assert!(requests[0].tools.iter().all(|tool| tool.name != "Late"));
            assert!(requests[1].tools.iter().any(|tool| tool.name == "Late"));
        }
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fake_tool_live_events_observe_durable_results_and_terminal_state() {
        let root = temp_root("durable-before-live");
        let session = session(&root).await;
        let mut events = session.subscribe();
        let provider = Arc::new(FakeProvider::new(vec![
            Ok(response(
                "tool",
                vec![tool_call("call/opaque", "One", r#"{"value":"x"}"#)],
                1,
            )),
            Ok(response("done", vec![], 1)),
        ]));
        let tools = ToolRegistry::new();
        tools
            .register(Arc::new(FakeTool::new("One", "one")))
            .expect("one");
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            engine(provider, tools, HookRunner::new())
                .run_turn(
                    &run_session,
                    TurnInput::user("go", "system"),
                    CancellationToken::new(),
                )
                .await
        });

        let mut saw_result = false;
        let mut saw_terminal = false;
        while !saw_result || !saw_terminal {
            let event = events.recv().await.expect("event");
            match event.event {
                AgentEvent::ToolResult { tool_call_id, .. } if tool_call_id == "call/opaque" => {
                    let records = read_record_file(session.record_path())
                        .await
                        .expect("records at tool event");
                    saw_result = records.records.iter().any(|record| {
                        record.kind()
                            == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                            && record.payload["event"]["type"] == "tool.result"
                            && record.payload["event"]["toolCallId"] == "call/opaque"
                    });
                }
                AgentEvent::TurnEnded { .. } => {
                    let records = read_record_file(session.record_path())
                        .await
                        .expect("records at terminal event");
                    saw_terminal = records
                        .records
                        .iter()
                        .any(|record| record.record_type == TURN_TERMINAL_RECORD_TYPE);
                }
                _ => {}
            }
        }
        task.await.expect("join").expect("turn");
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn context_overflow_fallback_is_bounded_and_failures_are_durable() {
        let root = temp_root("overflow");
        let session = session(&root).await;
        let stop_failure_marker = root.join("stop-failure-hook");
        let hooks = HookRunner::new();
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::StopFailure,
                matcher: crate::HookMatcher::tool_name_regex("ProviderError").expect("matcher"),
                command: format!("touch {}", stop_failure_marker.display()),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Open,
            })
            .expect("hook");
        let overflow = || {
            let mut error =
                ProviderError::new(ProviderErrorKind::InvalidRequest, "context window exceeded");
            error.status_code = Some(413);
            Err(error)
        };
        let provider = Arc::new(FakeProvider::new(vec![overflow(), overflow(), overflow()]));
        let result = engine(provider.clone(), ToolRegistry::new(), hooks)
            .run_turn(
                &session,
                TurnInput::user("go", "system"),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(TurnError::Provider(_))));
        assert_eq!(provider.requests.lock().expect("requests").len(), 3);
        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        assert_eq!(
            records
                .records
                .iter()
                .filter(|record| record.record_type == TURN_STEP_RETRYING_RECORD_TYPE)
                .count(),
            2
        );
        assert!(records
            .records
            .iter()
            .any(|record| record.record_type == TURN_INTERRUPTED_RECORD_TYPE));
        assert!(records
            .records
            .iter()
            .any(|record| record.record_type == TURN_TERMINAL_RECORD_TYPE));
        assert!(tokio::fs::try_exists(stop_failure_marker)
            .await
            .expect("marker"));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn uncooperative_tools_are_aborted_after_the_cancellation_grace() {
        let root = temp_root("cancellation-grace");
        let session = session(&root).await;
        let provider = Arc::new(FakeProvider::new(vec![Ok(response(
            "tool",
            vec![tool_call("slow", "Slow", r#"{"value":"x"}"#)],
            1,
        ))]));
        let tools = ToolRegistry::new();
        let mut slow = FakeTool::new("Slow", "too late");
        slow.delay = Duration::from_secs(60);
        let slow = Arc::new(slow);
        tools.register(slow.clone()).expect("slow");
        let interrupt_marker = root.join("interrupt-hook");
        let hooks = HookRunner::new();
        hooks
            .register(crate::HookRegistration {
                event: ToolHookEvent::Interrupt,
                matcher: crate::HookMatcher::Any,
                command: format!("touch {}", interrupt_marker.display()),
                cwd: root.clone(),
                timeout: None,
                fail_mode: crate::CommandHookFailMode::Open,
            })
            .expect("hook");
        let engine = TurnEngine::new(
            provider,
            tools,
            hooks,
            ToolScheduler::new(),
            TurnEngineConfig {
                retry_delay: Duration::ZERO,
                tool_cancellation_grace: Duration::from_millis(5),
                ..TurnEngineConfig::default()
            },
        )
        .expect("engine");
        let cancellation = CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let run_session = session.clone();
        let task = tokio::spawn(async move {
            engine
                .run_turn(
                    &run_session,
                    TurnInput::user("go", "system"),
                    run_cancellation,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while slow.executions.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tool must start");
        cancellation.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("grace must bound cancellation")
            .expect("join")
            .expect("aborted outcome");
        assert_eq!(outcome.reason, TurnOutcomeReason::Aborted);
        let records = read_record_file(session.record_path())
            .await
            .expect("records");
        assert!(records
            .records
            .iter()
            .any(|record| { record.kind() == Some(mycel_agent_protocol::RecordKind::TurnCancel) }));
        assert!(records.records.iter().any(|record| {
            record.kind() == Some(mycel_agent_protocol::RecordKind::ContextAppendLoopEvent)
                && record.payload["event"]["type"] == "tool.result"
                && record.payload["event"]["toolCallId"] == "slow"
        }));
        assert!(tokio::fs::try_exists(interrupt_marker)
            .await
            .expect("marker"));
        session.close().await.expect("close");
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
