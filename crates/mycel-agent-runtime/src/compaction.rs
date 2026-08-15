use std::{collections::BTreeMap, sync::Arc, time::Duration};

use mycel_agent_protocol::{
    CompactionResult, CompactionTrigger, ContentPart, FinishReason, Message, ProviderError,
    ProviderErrorKind, ProviderRequest, Role, ThinkingEffort,
};
use serde_json::json;

use crate::{
    context::is_real_user_input, CancellationToken, SessionError, SessionHandle, TodoItem,
    TodoStatus, ToolHookEvent, ToolRegistry, TurnProvider,
};

const SUMMARY_PREFIX: &str = "The conversation so far has been compacted to free up context. What follows is your own working summary of this task — use it to continue your train of thought rather than starting over. Treat it as notes, not proof: where it says a step was done, tests passed, or a fix worked, verify that yourself before relying on it. Any user messages earlier in this context are preserved verbatim from the compacted conversation; where a system-reminder note among them marks an omitted middle section, the user messages it replaced are covered by this summary.";
const BASE_INSTRUCTION: &str = r#"You are about to run out of context. Write a first-person handoff note to
yourself so you can seamlessly continue this task after the earlier
conversation is cleared.

--- This message is a direct task, not part of the above conversation ---

Write the note as your own continuing train of thought — first person, present
tense, the way you would reason through the next move. Do not write a
third-party report about someone else's work, and do not impose rigid section
headings; let the shape follow the task. Write the note in the same language the
conversation has been using — do not switch to English just because these
instructions happen to be in English.

Make the note self-sufficient: the next turn will see only your most recent user
messages and this note — every assistant message, tool call, and tool result
above will be gone. In your own words, preserve what you genuinely need to
continue:

- What the latest request is actually asking for: your reading of its intent and
  any ambiguity you have already resolved — not a re-transcription, since what
  fits is kept verbatim in your most recent messages.
- The instructions and constraints currently in force, plus decisions already
  settled and questions still open.
- What has actually been done, with exact commands, paths, results, signatures,
  and failures needed to continue without rediscovery.
- What you still do not know and must inspect rather than assume.
- The forward plan, including the exact next action, remaining sequence, known
  edge cases, and any required final-answer format.

Your TODO list is re-attached automatically below this note from its live
source, so do not transcribe it. Be honest about uncertainty. Keep the note
concise and proportional to the task. Respond with text only."#;

#[derive(Clone, Debug)]
pub struct CompactionEngineConfig {
    pub max_attempts: u8,
    pub max_overflow_shrinks: u8,
    pub retry_delay: Duration,
    pub max_instruction_bytes: usize,
}

impl Default for CompactionEngineConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            max_overflow_shrinks: 3,
            retry_delay: Duration::from_millis(250),
            max_instruction_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompactionRequest {
    pub trigger: CompactionTrigger,
    pub instruction: Option<String>,
    pub system_prompt: String,
    pub thinking_effort: Option<ThinkingEffort>,
    pub max_completion_tokens: Option<u64>,
    pub turn_id: Option<u64>,
}

impl CompactionRequest {
    pub fn manual(system_prompt: impl Into<String>, instruction: Option<String>) -> Self {
        Self {
            trigger: CompactionTrigger::Manual,
            instruction,
            system_prompt: system_prompt.into(),
            thinking_effort: None,
            max_completion_tokens: None,
            turn_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutoCompactionConfig {
    pub max_context_tokens: u64,
    pub trigger_ratio: f64,
    pub reserved_context_tokens: u64,
}

impl AutoCompactionConfig {
    pub fn validate(&self) -> Result<(), CompactionError> {
        if self.max_context_tokens == 0 {
            return Err(CompactionError::InvalidConfig(
                "auto compaction requires a positive context window".to_owned(),
            ));
        }
        if !(0.5..=0.99).contains(&self.trigger_ratio) {
            return Err(CompactionError::InvalidConfig(
                "auto compaction trigger_ratio must be between 0.5 and 0.99".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn should_compact(&self, used_tokens: u64) -> bool {
        let ratio_threshold = (self.max_context_tokens as f64 * self.trigger_ratio) as u64;
        let reserved_threshold = (self.reserved_context_tokens > 0
            && self.reserved_context_tokens < self.max_context_tokens)
            .then(|| {
                self.max_context_tokens
                    .saturating_sub(self.reserved_context_tokens)
            });
        used_tokens >= ratio_threshold
            || reserved_threshold.is_some_and(|threshold| used_tokens >= threshold)
    }
}

#[derive(Clone)]
pub struct CompactionEngine {
    provider: Arc<dyn TurnProvider>,
    tools: ToolRegistry,
    config: CompactionEngineConfig,
}

impl CompactionEngine {
    pub fn standard(provider: Arc<dyn TurnProvider>, tools: ToolRegistry) -> Self {
        Self {
            provider,
            tools,
            config: CompactionEngineConfig::default(),
        }
    }

    pub fn new(
        provider: Arc<dyn TurnProvider>,
        tools: ToolRegistry,
        config: CompactionEngineConfig,
    ) -> Result<Self, CompactionError> {
        if config.max_attempts == 0 {
            return Err(CompactionError::InvalidConfig(
                "max_attempts must be positive".to_owned(),
            ));
        }
        if config.max_overflow_shrinks == 0 || config.max_overflow_shrinks > 3 {
            return Err(CompactionError::InvalidConfig(
                "max_overflow_shrinks must be between 1 and 3".to_owned(),
            ));
        }
        if config.max_instruction_bytes == 0 {
            return Err(CompactionError::InvalidConfig(
                "max_instruction_bytes must be positive".to_owned(),
            ));
        }
        Ok(Self {
            provider,
            tools,
            config,
        })
    }

    pub async fn compact_manual(
        &self,
        session: &SessionHandle,
        mut request: CompactionRequest,
        cancellation: CancellationToken,
    ) -> Result<CompactionResult, CompactionError> {
        request.trigger = CompactionTrigger::Manual;
        let _turn = session
            .try_acquire_turn()
            .ok_or(CompactionError::TurnActive)?;
        if session.snapshot().await.state.context.history().is_empty() {
            return Err(CompactionError::EmptyHistory);
        }
        self.compact_locked(session, request, cancellation).await
    }

    pub(crate) async fn compact_active(
        &self,
        session: &SessionHandle,
        mut request: CompactionRequest,
        cancellation: CancellationToken,
    ) -> Result<CompactionResult, CompactionError> {
        request.trigger = CompactionTrigger::Auto;
        self.compact_locked(session, request, cancellation).await
    }

    async fn compact_locked(
        &self,
        session: &SessionHandle,
        request: CompactionRequest,
        cancellation: CancellationToken,
    ) -> Result<CompactionResult, CompactionError> {
        let instruction = request
            .instruction
            .as_deref()
            .map(str::trim)
            .filter(|instruction| !instruction.is_empty())
            .map(str::to_owned);
        if instruction
            .as_ref()
            .is_some_and(|instruction| instruction.len() > self.config.max_instruction_bytes)
        {
            return Err(CompactionError::InstructionTooLarge(
                self.config.max_instruction_bytes,
            ));
        }
        session
            .begin_compaction(request.trigger, instruction.clone())
            .await?;
        if request.trigger == CompactionTrigger::Auto {
            session.publish_compaction_blocked(request.turn_id);
        }
        let outcome = self
            .compaction_round(session, &request, instruction, &cancellation)
            .await;
        match outcome {
            Ok((result, context_summary)) => {
                if let Err(error) = session.apply_compaction(&result, &context_summary).await {
                    return Err(
                        cancel_after_failure(session, CompactionError::Session(error)).await,
                    );
                }
                if let Err(error) = session.complete_compaction(result.clone()).await {
                    return Err(
                        cancel_after_failure(session, CompactionError::Session(error)).await,
                    );
                }
                let trigger = trigger_name(request.trigger);
                let _ = session
                    .run_lifecycle_hook(
                        ToolHookEvent::PostCompact,
                        trigger,
                        BTreeMap::from([
                            ("trigger".to_owned(), json!(trigger)),
                            ("estimatedTokenCount".to_owned(), json!(result.tokens_after)),
                        ]),
                        &CancellationToken::new(),
                    )
                    .await;
                Ok(result)
            }
            Err(error) => {
                let cancellation_error = cancellation.is_cancelled()
                    || session.cancellation().is_cancelled()
                    || matches!(error, CompactionError::Cancelled);
                let cleanup = session.cancel_compaction().await;
                if let Err(cleanup) = cleanup {
                    return Err(CompactionError::Cleanup {
                        primary: error.to_string(),
                        cleanup: cleanup.to_string(),
                    });
                }
                if cancellation_error {
                    Err(CompactionError::Cancelled)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn compaction_round(
        &self,
        session: &SessionHandle,
        request: &CompactionRequest,
        custom_instruction: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<(CompactionResult, String), CompactionError> {
        let snapshot = session.snapshot().await;
        let original_entries = snapshot.state.context.history().to_vec();
        let original_history = snapshot.state.context.provider_history();
        let projection_before = snapshot.state.context.compaction_projection("");
        let trigger = trigger_name(request.trigger);
        let pre = session
            .run_lifecycle_hook(
                ToolHookEvent::PreCompact,
                trigger,
                BTreeMap::from([
                    ("trigger".to_owned(), json!(trigger)),
                    (
                        "tokenCount".to_owned(),
                        json!(projection_before.tokens_before),
                    ),
                ]),
                cancellation,
            )
            .await;
        if let Some(blocked) = pre.blocked {
            return Err(CompactionError::HookBlocked(blocked.reason));
        }
        ensure_not_cancelled(session, cancellation)?;

        let mut history = original_history.clone();
        let mut dropped_count = 0u64;
        let mut overflow_shrinks = 0u8;
        let mut empty_shrinks = 0u8;
        let mut transient_attempts = 0u8;
        let mut media_stripped = false;
        let summary = loop {
            ensure_not_cancelled(session, cancellation)?;
            let mut provider_history = history.clone();
            provider_history.push(Message::user(build_instruction(
                custom_instruction.as_deref(),
            )));
            let provider_request = ProviderRequest {
                provider: self.provider.name().to_owned(),
                model: self.provider.model().to_owned(),
                system_prompt: request.system_prompt.clone(),
                tools: self.tools.snapshot().definitions().to_vec(),
                history: provider_history,
                thinking_effort: request.thinking_effort.clone(),
                max_completion_tokens: request.max_completion_tokens,
                response_format: None,
                metadata: BTreeMap::from([
                    ("kind".to_owned(), json!("compaction")),
                    ("droppedCount".to_owned(), json!(dropped_count)),
                ]),
            };
            provider_request
                .validate()
                .map_err(|error| CompactionError::InvalidRequest(error.to_string()))?;
            let session_cancellation = session.cancellation();
            let future = self
                .provider
                .complete(provider_request, cancellation.clone());
            tokio::pin!(future);
            let response = tokio::select! {
                _ = cancellation.cancelled() => return Err(CompactionError::Cancelled),
                _ = session_cancellation.cancelled() => {
                    cancellation.cancel();
                    return Err(CompactionError::Cancelled);
                }
                response = &mut future => response,
            };
            match response {
                Ok(response) => match extract_summary(&response) {
                    Ok(summary) => {
                        if let Some(usage) = response.usage {
                            session.record_usage(self.provider.model(), usage).await?;
                        }
                        break summary;
                    }
                    Err(error @ (CompactionError::EmptySummary | CompactionError::Truncated))
                        if history.len() > 1
                            && empty_shrinks.saturating_add(1) < self.config.max_attempts =>
                    {
                        empty_shrinks = empty_shrinks.saturating_add(1);
                        let before = history.len();
                        history = drop_oldest_and_leading_tools(&history);
                        dropped_count = dropped_count.saturating_add(
                            u64::try_from(before.saturating_sub(history.len())).unwrap_or(u64::MAX),
                        );
                        transient_attempts = 0;
                        let _ = error;
                    }
                    Err(error) => return Err(error),
                },
                Err(error) if is_context_overflow(&error) => {
                    if !media_stripped {
                        let stripped = strip_media(&history);
                        media_stripped = true;
                        if stripped != history {
                            history = stripped;
                            transient_attempts = 0;
                            continue;
                        }
                    }
                    if history.len() <= 1 || overflow_shrinks >= self.config.max_overflow_shrinks {
                        return Err(CompactionError::Provider(error));
                    }
                    overflow_shrinks = overflow_shrinks.saturating_add(1);
                    let before = history.len();
                    history = overflow_projection(&history, overflow_shrinks);
                    dropped_count = dropped_count.saturating_add(
                        u64::try_from(before.saturating_sub(history.len())).unwrap_or(u64::MAX),
                    );
                    transient_attempts = 0;
                }
                Err(error)
                    if error.kind == ProviderErrorKind::Cancelled
                        || cancellation.is_cancelled()
                        || session.cancellation().is_cancelled() =>
                {
                    return Err(CompactionError::Cancelled);
                }
                Err(error)
                    if error.retryable
                        && transient_attempts.saturating_add(1) < self.config.max_attempts =>
                {
                    transient_attempts = transient_attempts.saturating_add(1);
                    let delay = error
                        .retry_after_ms
                        .map(Duration::from_millis)
                        .unwrap_or(self.config.retry_delay);
                    let session_cancellation = session.cancellation();
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(CompactionError::Cancelled),
                        _ = session_cancellation.cancelled() => {
                            cancellation.cancel();
                            return Err(CompactionError::Cancelled);
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => return Err(CompactionError::Provider(error)),
            }
        };

        let current = session.snapshot().await;
        let current_history = current.state.context.history();
        if current_history.len() < original_entries.len()
            || current_history[..original_entries.len()] != original_entries
            || current_history[original_entries.len()..]
                .iter()
                .any(|entry| !is_real_user_input(entry))
        {
            return Err(CompactionError::ContextChanged);
        }
        let raw_summary = append_todos(summary.trim(), current.state.tool_store.get("todos"))?;
        let context_summary = format!("{SUMMARY_PREFIX}\n{raw_summary}");
        let projection = current
            .state
            .context
            .compaction_projection(&context_summary);
        let result = CompactionResult {
            summary: raw_summary,
            compacted_count: u64::try_from(original_history.len()).unwrap_or(u64::MAX),
            tokens_before: projection.tokens_before,
            tokens_after: projection.tokens_after,
            kept_user_message_count: Some(
                u64::try_from(projection.kept_user_message_count).unwrap_or(u64::MAX),
            ),
            kept_head_user_message_count: projection
                .kept_head_user_message_count
                .map(|count| u64::try_from(count).unwrap_or(u64::MAX)),
            dropped_count: (dropped_count > 0).then_some(dropped_count),
        };
        Ok((result, context_summary))
    }
}

fn trigger_name(trigger: CompactionTrigger) -> &'static str {
    match trigger {
        CompactionTrigger::Manual => "manual",
        CompactionTrigger::Auto => "auto",
    }
}

async fn cancel_after_failure(
    session: &SessionHandle,
    primary: CompactionError,
) -> CompactionError {
    match session.cancel_compaction().await {
        Ok(()) => primary,
        Err(cleanup) => CompactionError::Cleanup {
            primary: primary.to_string(),
            cleanup: cleanup.to_string(),
        },
    }
}

fn append_todos(
    summary: &str,
    value: Option<&serde_json::Value>,
) -> Result<String, CompactionError> {
    let Some(value) = value else {
        return Ok(summary.to_owned());
    };
    let todos: Vec<TodoItem> = serde_json::from_value(value.clone())
        .map_err(|error| CompactionError::InvalidTodoState(error.to_string()))?;
    if todos.is_empty() {
        return Ok(summary.to_owned());
    }
    let mut rendered = format!("{}\n\n## TODO List", summary.trim());
    for todo in todos {
        let status = match todo.status {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Done => "done",
        };
        rendered.push_str(&format!("\n  [{status}] {}", todo.title));
    }
    Ok(rendered)
}

fn build_instruction(custom: Option<&str>) -> String {
    custom.map_or_else(
        || BASE_INSTRUCTION.to_owned(),
        |custom| format!("{BASE_INSTRUCTION}\n\nOptional user instruction:\n{custom}"),
    )
}

fn extract_summary(
    response: &mycel_agent_protocol::GenerateResult,
) -> Result<String, CompactionError> {
    if response.finish_reason == Some(FinishReason::Truncated) {
        return Err(CompactionError::Truncated);
    }
    let summary = response
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if summary.trim().is_empty() {
        return Err(CompactionError::EmptySummary);
    }
    Ok(summary)
}

fn ensure_not_cancelled(
    session: &SessionHandle,
    cancellation: &CancellationToken,
) -> Result<(), CompactionError> {
    if cancellation.is_cancelled() || session.cancellation().is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    Ok(())
}

fn is_context_overflow(error: &ProviderError) -> bool {
    error.status_code == Some(413)
        || (error.kind == ProviderErrorKind::InvalidRequest
            && error.message.to_ascii_lowercase().contains("context"))
}

fn strip_media(history: &[Message]) -> Vec<Message> {
    history
        .iter()
        .cloned()
        .map(|mut message| {
            message.content = message
                .content
                .into_iter()
                .map(|part| match part {
                    ContentPart::ImageUrl { .. } => ContentPart::text("[image]"),
                    ContentPart::AudioUrl { .. } => ContentPart::text("[audio]"),
                    ContentPart::VideoUrl { .. } => ContentPart::text("[video]"),
                    part => part,
                })
                .collect();
            message
        })
        .collect()
}

fn overflow_projection(history: &[Message], tier: u8) -> Vec<Message> {
    let numerator = match tier {
        1 => 70usize,
        2 => 50usize,
        _ => 35usize,
    };
    let keep = history.len().saturating_mul(numerator).div_ceil(100).max(1);
    drop_leading_tools(history[history.len().saturating_sub(keep)..].to_vec())
}

fn drop_oldest_and_leading_tools(history: &[Message]) -> Vec<Message> {
    if history.len() <= 1 {
        return history.to_vec();
    }
    drop_leading_tools(history[1..].to_vec())
}

fn drop_leading_tools(mut history: Vec<Message>) -> Vec<Message> {
    let first_non_tool = history
        .iter()
        .position(|message| message.role != Role::Tool)
        .unwrap_or(history.len());
    history.drain(..first_non_tool);
    history
}

#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("invalid compaction configuration: {0}")]
    InvalidConfig(String),
    #[error("No messages to compact in current history.")]
    EmptyHistory,
    #[error("Cannot compact while a turn is active. Wait for it to finish, then retry.")]
    TurnActive,
    #[error("compaction instruction exceeds the {0}-byte limit")]
    InstructionTooLarge(usize),
    #[error("PreCompact hook blocked compaction: {0}")]
    HookBlocked(String),
    #[error("compaction request was invalid: {0}")]
    InvalidRequest(String),
    #[error("compaction response was truncated before producing a complete summary")]
    Truncated,
    #[error("compaction response did not contain a non-empty summary")]
    EmptySummary,
    #[error("session context changed incompatibly while compaction was running")]
    ContextChanged,
    #[error("canonical todo state is malformed: {0}")]
    InvalidTodoState(String),
    #[error("compaction was cancelled")]
    Cancelled,
    #[error("compaction provider failed: {0}")]
    Provider(ProviderError),
    #[error("compaction failed: {primary}; additionally cleanup failed: {cleanup}")]
    Cleanup { primary: String, cleanup: String },
    #[error(transparent)]
    Session(#[from] SessionError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{
            atomic::{AtomicU64, Ordering},
            Mutex,
        },
    };

    use mycel_agent_protocol::{
        GenerateResult, OptionalNullable, PromptOrigin, ProviderErrorKind, TokenUsage,
    };

    use crate::{
        CompactionState, ContextEntry, Runtime, SessionId, SessionOptions, TurnProviderFuture,
    };

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mycel-compaction-{name}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[derive(Default)]
    struct ScriptedProvider {
        responses: Mutex<VecDeque<Result<GenerateResult, ProviderError>>>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    impl ScriptedProvider {
        fn push(&self, response: Result<GenerateResult, ProviderError>) {
            self.responses
                .lock()
                .expect("responses")
                .push_back(response);
        }
    }

    impl TurnProvider for ScriptedProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn model(&self) -> &str {
            "model"
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
                .expect("scripted response");
            Box::pin(async move { response })
        }
    }

    fn response(text: &str) -> GenerateResult {
        GenerateResult {
            id: Some("response".to_owned()),
            message: Message::assistant(vec![ContentPart::text(text)], Vec::new()),
            usage: Some(TokenUsage {
                input_other: 10,
                output: 2,
                ..TokenUsage::default()
            }),
            finish_reason: Some(FinishReason::Completed),
            raw_finish_reason: None,
            trace_id: OptionalNullable::Missing,
        }
    }

    async fn session(root: &PathBuf) -> (Runtime, SessionHandle) {
        let runtime = Runtime::new(root);
        let session = runtime
            .create_session(SessionOptions::new(
                SessionId::new("session").expect("session id"),
            ))
            .await
            .expect("create session");
        (runtime, session)
    }

    #[tokio::test]
    async fn manual_compaction_is_durable_evented_and_replayable() {
        let root = temp_root("manual");
        let (runtime, session) = session(&root).await;
        session
            .append_user_message("original task", PromptOrigin::User)
            .await
            .expect("user");
        session
            .append_context(ContextEntry {
                message: Message::assistant(vec![ContentPart::text("earlier answer")], Vec::new()),
                origin: None,
                is_error: false,
                tool_call_displays: BTreeMap::new(),
                note: None,
            })
            .await
            .expect("assistant");
        let provider = Arc::new(ScriptedProvider::default());
        provider.push(Ok(response("continue with the native port")));
        let engine = CompactionEngine::new(
            provider.clone(),
            ToolRegistry::new(),
            CompactionEngineConfig::default(),
        )
        .expect("engine");
        let mut events = session.subscribe();
        let result = engine
            .compact_manual(
                &session,
                CompactionRequest::manual("system", Some("preserve exact gates".to_owned())),
                CancellationToken::new(),
            )
            .await
            .expect("compact");
        assert_eq!(result.summary, "continue with the native port");
        assert_eq!(
            events.recv().await.expect("started").event,
            mycel_agent_protocol::AgentEvent::CompactionStarted {
                trigger: CompactionTrigger::Manual,
                instruction: Some("preserve exact gates".to_owned()),
            }
        );
        assert!(matches!(
            events.recv().await.expect("completed").event,
            mycel_agent_protocol::AgentEvent::CompactionCompleted { .. }
        ));
        let snapshot = session.snapshot().await;
        assert_eq!(snapshot.state.compaction, CompactionState::Completed);
        assert_eq!(snapshot.state.context.history().len(), 2);
        assert_eq!(
            snapshot.state.context.history()[0].message.text(""),
            "original task"
        );
        assert!(snapshot.state.context.history()[1]
            .message
            .text("")
            .starts_with(SUMMARY_PREFIX));
        {
            let requests = provider.requests.lock().expect("requests");
            assert_eq!(requests.len(), 1);
            assert!(requests[0]
                .history
                .last()
                .expect("instruction")
                .text("")
                .contains("preserve exact gates"));
        }

        session.close().await.expect("close");
        let resumed = runtime
            .resume_session(SessionOptions::new(
                SessionId::new("session").expect("session id"),
            ))
            .await
            .expect("resume");
        assert_eq!(
            resumed.snapshot().await.state.context.history(),
            snapshot.state.context.history()
        );
        resumed.close().await.expect("close resumed");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn manual_compaction_rejects_an_active_turn_without_a_provider_call() {
        let root = temp_root("busy");
        let (_runtime, session) = session(&root).await;
        session
            .append_user_message("task", PromptOrigin::User)
            .await
            .expect("user");
        let provider = Arc::new(ScriptedProvider::default());
        let engine = CompactionEngine::new(
            provider.clone(),
            ToolRegistry::new(),
            CompactionEngineConfig::default(),
        )
        .expect("engine");
        let _turn = session.acquire_turn().await;
        assert!(matches!(
            engine
                .compact_manual(
                    &session,
                    CompactionRequest::manual("system", None),
                    CancellationToken::new(),
                )
                .await,
            Err(CompactionError::TurnActive)
        ));
        assert!(provider.requests.lock().expect("requests").is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn overflow_strips_media_then_retries_without_losing_the_real_history() {
        let root = temp_root("media");
        let (_runtime, session) = session(&root).await;
        session
            .append_context(ContextEntry {
                message: Message {
                    role: Role::User,
                    name: None,
                    content: vec![ContentPart::ImageUrl {
                        image_url: mycel_agent_protocol::MediaUrl {
                            url: "data:image/png;base64,AAAA".to_owned(),
                            id: None,
                        },
                    }],
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    partial: false,
                    tools: Vec::new(),
                },
                origin: Some(PromptOrigin::User),
                is_error: false,
                tool_call_displays: BTreeMap::new(),
                note: None,
            })
            .await
            .expect("media user");
        let provider = Arc::new(ScriptedProvider::default());
        let mut overflow =
            ProviderError::new(ProviderErrorKind::InvalidRequest, "context too large");
        overflow.status_code = Some(413);
        provider.push(Err(overflow));
        provider.push(Ok(response("media was discussed")));
        let engine = CompactionEngine::new(
            provider.clone(),
            ToolRegistry::new(),
            CompactionEngineConfig::default(),
        )
        .expect("engine");
        engine
            .compact_manual(
                &session,
                CompactionRequest::manual("system", None),
                CancellationToken::new(),
            )
            .await
            .expect("compact");
        {
            let requests = provider.requests.lock().expect("requests");
            assert_eq!(requests.len(), 2);
            assert!(matches!(
                requests[0].history[0].content[0],
                ContentPart::ImageUrl { .. }
            ));
            assert_eq!(requests[1].history[0].text(""), "[image]");
        }
        assert!(matches!(
            session.snapshot().await.state.context.history()[0]
                .message
                .content[0],
            ContentPart::ImageUrl { .. }
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
