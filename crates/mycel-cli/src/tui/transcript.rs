const COALESCE_MS: u64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    User,
    Thinking,
    Assistant,
    Tool,
    Hook,
    Status,
    Subagent,
    BackgroundTask,
    Goal,
    Mcp,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFrameStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFrame {
    pub kind: FrameKind,
    pub text: String,
    pub streaming: bool,
    pub tool_id: Option<String>,
    pub tool_status: Option<ToolFrameStatus>,
    pub entity_id: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    UserMessage(String),
    Status(String),
    TurnStarted,
    ThinkingDelta(String),
    AssistantDelta(String),
    ToolStarted {
        id: String,
        name: String,
        preview: Option<String>,
    },
    ToolProgress {
        id: String,
        text: String,
    },
    ToolResult {
        id: String,
        output: String,
        failed: bool,
    },
    HookResult {
        name: String,
        content: String,
        blocked: bool,
    },
    Retrying {
        failed_attempt: u32,
        next_attempt: u32,
    },
    SubagentState {
        id: String,
        name: String,
        state: String,
        detail: Option<String>,
    },
    BackgroundTaskState {
        id: String,
        kind: String,
        state: String,
        description: String,
    },
    GoalState {
        status: String,
        objective: String,
        detail: Option<String>,
    },
    McpServerState {
        name: String,
        status: String,
        detail: Option<String>,
    },
    CompactionStarted {
        instruction: Option<String>,
    },
    CompactionCompleted {
        tokens_before: u64,
        tokens_after: u64,
        summary: String,
    },
    CompactionCancelled,
    CompactionBlocked {
        reason: Option<String>,
    },
    StepCompleted,
    TurnEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Thinking,
    Assistant,
}

#[derive(Debug, Default)]
pub struct TranscriptReducer {
    frames: Vec<TranscriptFrame>,
    pending_kind: Option<StreamKind>,
    pending_text: String,
    pending_since_ms: Option<u64>,
    active_stream: Option<(StreamKind, usize)>,
}

impl TranscriptReducer {
    pub fn frames(&self) -> &[TranscriptFrame] {
        &self.frames
    }

    pub fn push(&mut self, event: TranscriptEvent, now_ms: u64) {
        match event {
            TranscriptEvent::UserMessage(text) => {
                self.flush_pending();
                self.end_stream();
                self.frames.push(TranscriptFrame {
                    kind: FrameKind::User,
                    text,
                    streaming: false,
                    tool_id: None,
                    tool_status: None,
                    entity_id: None,
                    state: None,
                });
            }
            TranscriptEvent::Status(text) => {
                self.flush_pending();
                self.end_stream();
                self.frames.push(TranscriptFrame {
                    kind: FrameKind::Status,
                    text,
                    streaming: false,
                    tool_id: None,
                    tool_status: None,
                    entity_id: None,
                    state: None,
                });
            }
            TranscriptEvent::TurnStarted => self.end_stream(),
            TranscriptEvent::ThinkingDelta(delta) => {
                self.queue_delta(StreamKind::Thinking, delta, now_ms)
            }
            TranscriptEvent::AssistantDelta(delta) => {
                if self.pending_kind == Some(StreamKind::Thinking)
                    || self
                        .active_stream
                        .is_some_and(|(kind, _)| kind == StreamKind::Thinking)
                {
                    self.flush_pending();
                    self.end_stream();
                }
                self.queue_delta(StreamKind::Assistant, delta, now_ms);
            }
            TranscriptEvent::ToolStarted { id, name, preview } => {
                self.flush_pending();
                self.end_stream();
                let preview = preview
                    .map(|preview| format!(" {preview}"))
                    .unwrap_or_default();
                self.frames.push(TranscriptFrame {
                    kind: FrameKind::Tool,
                    text: format!("{name}{preview}"),
                    streaming: true,
                    tool_id: Some(id),
                    tool_status: Some(ToolFrameStatus::Running),
                    entity_id: None,
                    state: Some("running".to_owned()),
                });
            }
            TranscriptEvent::ToolProgress { id, text } => {
                if let Some(frame) = self.tool_frame_mut(&id) {
                    if !text.is_empty() {
                        if !frame.text.is_empty() {
                            frame.text.push('\n');
                        }
                        frame.text.push_str(&text);
                    }
                }
            }
            TranscriptEvent::ToolResult { id, output, failed } => {
                self.flush_pending();
                self.end_stream();
                if let Some(frame) = self.tool_frame_mut(&id) {
                    if !output.is_empty() {
                        if !frame.text.is_empty() {
                            frame.text.push('\n');
                        }
                        frame.text.push_str(&output);
                    }
                    frame.streaming = false;
                    frame.tool_status = Some(if failed {
                        ToolFrameStatus::Failed
                    } else {
                        ToolFrameStatus::Completed
                    });
                    frame.state = Some(if failed { "failed" } else { "completed" }.to_owned());
                }
            }
            TranscriptEvent::HookResult {
                name,
                content,
                blocked,
            } => {
                self.flush_pending();
                self.end_stream();
                let blocked_suffix = if blocked { " blocked" } else { "" };
                self.frames.push(TranscriptFrame {
                    kind: FrameKind::Hook,
                    text: format!("{name} hook{blocked_suffix}\n\n{}", content.trim()),
                    streaming: false,
                    tool_id: None,
                    tool_status: None,
                    entity_id: None,
                    state: Some(if blocked { "blocked" } else { "completed" }.to_owned()),
                });
            }
            TranscriptEvent::Retrying {
                failed_attempt,
                next_attempt,
            } => {
                self.discard_assistant_attempt();
                self.frames.push(TranscriptFrame {
                    kind: FrameKind::Status,
                    text: format!("retrying attempt {failed_attempt} as {next_attempt}"),
                    streaming: false,
                    tool_id: None,
                    tool_status: None,
                    entity_id: None,
                    state: Some("retrying".to_owned()),
                });
            }
            TranscriptEvent::SubagentState {
                id,
                name,
                state,
                detail,
            } => {
                self.flush_pending();
                self.end_stream();
                let text = detail.map_or(name.clone(), |detail| format!("{name}\n{detail}"));
                let active = matches!(
                    state.as_str(),
                    "spawned" | "started" | "suspended" | "backgrounded"
                );
                self.upsert_entity(FrameKind::Subagent, id, text, state, active);
            }
            TranscriptEvent::BackgroundTaskState {
                id,
                kind,
                state,
                description,
            } => {
                self.flush_pending();
                self.end_stream();
                let active = state == "running";
                self.upsert_entity(
                    FrameKind::BackgroundTask,
                    id,
                    format!("{kind}: {description}"),
                    state,
                    active,
                );
            }
            TranscriptEvent::GoalState {
                status,
                objective,
                detail,
            } => {
                self.flush_pending();
                self.end_stream();
                let text =
                    detail.map_or(objective.clone(), |detail| format!("{objective}\n{detail}"));
                self.frames.push(TranscriptFrame {
                    kind: FrameKind::Goal,
                    text,
                    streaming: false,
                    tool_id: None,
                    tool_status: None,
                    entity_id: None,
                    state: Some(status),
                });
            }
            TranscriptEvent::McpServerState {
                name,
                status,
                detail,
            } => {
                self.flush_pending();
                self.end_stream();
                let text = detail.map_or(name.clone(), |detail| format!("{name}\n{detail}"));
                let active = status == "pending";
                self.upsert_entity(FrameKind::Mcp, name, text, status, active);
            }
            TranscriptEvent::CompactionStarted { instruction } => {
                self.flush_pending();
                self.end_stream();
                self.upsert_entity(
                    FrameKind::Compaction,
                    "compaction".to_owned(),
                    instruction.unwrap_or_else(|| "Compacting context".to_owned()),
                    "running".to_owned(),
                    true,
                );
            }
            TranscriptEvent::CompactionCompleted {
                tokens_before,
                tokens_after,
                summary,
            } => {
                self.flush_pending();
                self.end_stream();
                self.upsert_entity(
                    FrameKind::Compaction,
                    "compaction".to_owned(),
                    format!("{tokens_before} → {tokens_after} tokens\n{summary}"),
                    "completed".to_owned(),
                    false,
                );
            }
            TranscriptEvent::CompactionCancelled => {
                self.flush_pending();
                self.end_stream();
                self.upsert_entity(
                    FrameKind::Compaction,
                    "compaction".to_owned(),
                    "Compaction cancelled".to_owned(),
                    "cancelled".to_owned(),
                    false,
                );
            }
            TranscriptEvent::CompactionBlocked { reason } => {
                self.flush_pending();
                self.end_stream();
                self.upsert_entity(
                    FrameKind::Compaction,
                    "compaction".to_owned(),
                    reason.unwrap_or_else(|| "Compaction blocked".to_owned()),
                    "blocked".to_owned(),
                    false,
                );
            }
            TranscriptEvent::StepCompleted | TranscriptEvent::TurnEnded => {
                self.flush_pending();
                self.end_stream();
            }
        }
    }

    pub fn tick(&mut self, now_ms: u64) {
        if self
            .pending_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) >= COALESCE_MS)
        {
            self.flush_pending();
        }
    }

    pub fn finish(&mut self) {
        self.flush_pending();
        self.end_stream();
    }

    fn queue_delta(&mut self, kind: StreamKind, delta: String, now_ms: u64) {
        if self.pending_kind.is_some_and(|pending| pending != kind) {
            self.flush_pending();
            self.end_stream();
        }
        self.pending_kind = Some(kind);
        self.pending_since_ms.get_or_insert(now_ms);
        self.pending_text.push_str(&delta);
    }

    fn flush_pending(&mut self) {
        let Some(kind) = self.pending_kind.take() else {
            return;
        };
        self.pending_since_ms = None;
        if self.pending_text.is_empty() {
            return;
        }
        if let Some((active_kind, index)) = self.active_stream {
            if active_kind == kind {
                self.frames[index]
                    .text
                    .push_str(&std::mem::take(&mut self.pending_text));
                return;
            }
        }
        let frame_kind = match kind {
            StreamKind::Thinking => FrameKind::Thinking,
            StreamKind::Assistant => FrameKind::Assistant,
        };
        let index = self.frames.len();
        self.frames.push(TranscriptFrame {
            kind: frame_kind,
            text: std::mem::take(&mut self.pending_text),
            streaming: true,
            tool_id: None,
            tool_status: None,
            entity_id: None,
            state: None,
        });
        self.active_stream = Some((kind, index));
    }

    fn end_stream(&mut self) {
        if let Some((_, index)) = self.active_stream.take() {
            if let Some(frame) = self.frames.get_mut(index) {
                frame.streaming = false;
            }
        }
    }

    fn discard_assistant_attempt(&mut self) {
        if self.pending_kind == Some(StreamKind::Assistant) {
            self.pending_kind = None;
            self.pending_text.clear();
            self.pending_since_ms = None;
        }
        if let Some((StreamKind::Assistant, index)) = self.active_stream.take() {
            if index + 1 == self.frames.len() {
                self.frames.pop();
            }
        }
    }

    fn tool_frame_mut(&mut self, id: &str) -> Option<&mut TranscriptFrame> {
        self.frames
            .iter_mut()
            .rev()
            .find(|frame| frame.tool_id.as_deref() == Some(id))
    }

    fn upsert_entity(
        &mut self,
        kind: FrameKind,
        id: String,
        text: String,
        state: String,
        active: bool,
    ) {
        if let Some(frame) = self
            .frames
            .iter_mut()
            .rev()
            .find(|frame| frame.kind == kind && frame.entity_id.as_deref() == Some(id.as_str()))
        {
            frame.text = text;
            frame.state = Some(state);
            frame.streaming = active;
            return;
        }
        self.frames.push(TranscriptFrame {
            kind,
            text,
            streaming: active,
            tool_id: None,
            tool_status: None,
            entity_id: Some(id),
            state: Some(state),
        });
    }
}
