use std::collections::{BTreeMap, BTreeSet};

use mycel_agent_protocol::{
    ContentPart, ExecutableToolOutput, ExecutableToolResult, LoopContentPart, LoopEvent, Message,
    PluginCommandTrigger, PromptOrigin, Role, SkillTrigger, ToolCall, ToolCallKind,
    ToolInputDisplay,
};
use serde::{Deserialize, Serialize};

const INTERRUPTED_TOOL_OUTPUT: &str =
    "Tool execution was interrupted before its result was recorded. Do not assume the tool completed successfully.";
const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;
const COMPACT_USER_MESSAGE_HEAD_TOKENS: u64 = 2_000;
const MEDIA_TOKEN_ESTIMATE: u64 = 2_000;

/// Provider-neutral message plus runtime-only provenance and rendering data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEntry {
    #[serde(flatten)]
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PromptOrigin>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_call_displays: BTreeMap<String, ToolInputDisplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ContextEntry {
    pub fn user(text: impl Into<String>, origin: PromptOrigin) -> Self {
        Self {
            message: Message::user(text),
            origin: Some(origin),
            is_error: false,
            tool_call_displays: BTreeMap::new(),
            note: None,
        }
    }

    fn tool(tool_call_id: impl Into<String>, result: &ExecutableToolResult) -> Self {
        let content = match &result.output {
            ExecutableToolOutput::Text(text) => vec![ContentPart::text(text.clone())],
            ExecutableToolOutput::Parts(parts) => parts.clone(),
        };
        Self {
            message: Message {
                role: Role::Tool,
                name: None,
                content,
                tool_calls: Vec::new(),
                tool_call_id: Some(tool_call_id.into()),
                partial: false,
                tools: Vec::new(),
            },
            origin: None,
            is_error: result.is_error,
            tool_call_displays: BTreeMap::new(),
            note: result.note.clone(),
        }
    }
}

/// Canonical context with unresolved tool exchanges permitted only at the
/// tail. Messages arriving while a tool exchange is open are deferred until
/// every result in that exchange is present.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalContext {
    history: Vec<ContextEntry>,
    token_count: u64,
    token_count_covered_message_count: usize,
    #[serde(skip)]
    open_steps: BTreeMap<String, usize>,
    #[serde(skip)]
    pending_tool_result_ids: BTreeSet<String>,
    #[serde(skip)]
    deferred_messages: Vec<ContextEntry>,
}

impl CanonicalContext {
    pub fn history(&self) -> &[ContextEntry] {
        &self.history
    }

    pub fn provider_history(&self) -> Vec<Message> {
        self.history
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    pub fn token_count(&self) -> u64 {
        self.token_count
    }

    pub fn token_count_with_pending_estimate(&self) -> u64 {
        self.token_count
            .saturating_add(estimate_tokens_for_messages(
                &self.history[self
                    .token_count_covered_message_count
                    .min(self.history.len())..],
            ))
    }

    /// Deterministically derive the live-context metadata persisted by a new
    /// compaction record. Replay runs the same selection algorithm from the
    /// pre-compaction history, so these counts are enough to reproduce the
    /// exact head/elision/tail fold after restart.
    pub fn compaction_projection(&self, context_summary: &str) -> CompactionProjection {
        let user_messages = self
            .history
            .iter()
            .filter(|entry| is_real_user_input(entry))
            .cloned()
            .collect::<Vec<_>>();
        let selection = select_compaction_user_messages(&user_messages);
        let mut kept = selection.head.clone();
        if selection.elided {
            kept.push(ContextEntry::user(
                compaction_elision(selection.omitted_tokens),
                PromptOrigin::Injection {
                    variant: "compaction_elision".to_owned(),
                },
            ));
        }
        kept.extend(selection.tail.clone());
        CompactionProjection {
            tokens_before: estimate_tokens_for_messages(&self.history),
            tokens_after: estimate_text_tokens(context_summary)
                .saturating_add(estimate_tokens_for_messages(&kept)),
            kept_user_message_count: selection.head.len().saturating_add(selection.tail.len()),
            kept_head_user_message_count: selection.elided.then_some(selection.head.len()),
        }
    }

    pub fn pending_tool_result_ids(&self) -> &BTreeSet<String> {
        &self.pending_tool_result_ids
    }

    pub fn deferred_message_count(&self) -> usize {
        self.deferred_messages.len()
    }

    pub fn append_message(&mut self, entry: ContextEntry) -> Result<AppendOutcome, ContextError> {
        self.validate_message(&entry)?;
        if !self.pending_tool_result_ids.is_empty() {
            self.deferred_messages.push(entry);
            return Ok(AppendOutcome::Deferred);
        }
        if entry.message.role == Role::Tool {
            return Ok(AppendOutcome::DroppedOrphanToolResult);
        }
        self.push_entry(entry)?;
        Ok(AppendOutcome::Appended)
    }

    pub fn append_loop_event(&mut self, event: &LoopEvent) -> Result<(), ContextError> {
        self.validate_loop_event(event)?;
        match event {
            LoopEvent::StepBegin { uuid, .. } => {
                self.close_all_pending(INTERRUPTED_TOOL_OUTPUT)?;
                let entry = ContextEntry {
                    message: Message::assistant(Vec::new(), Vec::new()),
                    origin: None,
                    is_error: false,
                    tool_call_displays: BTreeMap::new(),
                    note: None,
                };
                self.history.push(entry);
                self.open_steps
                    .insert(uuid.clone(), self.history.len().saturating_sub(1));
            }
            LoopEvent::StepEnd { uuid, usage, .. } => {
                let open_index = self.open_steps.remove(uuid);
                if let Some(usage) = usage {
                    let covered = open_index
                        .map(|index| index.saturating_add(1))
                        .unwrap_or(self.history.len())
                        .min(self.history.len());
                    if usage.grand_total() == 0 {
                        let start = self.token_count_covered_message_count.min(covered);
                        self.token_count =
                            self.token_count
                                .saturating_add(estimate_tokens_for_messages(
                                    &self.history[start..covered],
                                ));
                    } else {
                        self.token_count = usage.grand_total();
                    }
                    self.token_count_covered_message_count = covered;
                }
                self.flush_deferred()?;
            }
            LoopEvent::ContentPart {
                step_uuid, part, ..
            } => {
                let entry = self.open_step_mut(step_uuid)?;
                entry.message.content.push(match part {
                    LoopContentPart::Text { text } => ContentPart::Text { text: text.clone() },
                    LoopContentPart::Think { think, encrypted } => ContentPart::Think {
                        think: think.clone(),
                        encrypted: encrypted.clone(),
                    },
                });
            }
            LoopEvent::ToolCall {
                step_uuid,
                tool_call_id,
                name,
                args,
                display,
                extras,
                ..
            } => {
                if self.pending_tool_result_ids.contains(tool_call_id) {
                    return Err(ContextError::DuplicateToolCallId(tool_call_id.clone()));
                }
                let entry = self.open_step_mut(step_uuid)?;
                entry.message.tool_calls.push(ToolCall {
                    kind: ToolCallKind::Function,
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments: Some(
                        serde_json::to_string(args)
                            .map_err(|error| ContextError::Arguments(error.to_string()))?,
                    ),
                    extras: extras.clone(),
                });
                if let Some(display) = display {
                    entry
                        .tool_call_displays
                        .insert(tool_call_id.clone(), display.clone());
                }
                self.pending_tool_result_ids.insert(tool_call_id.clone());
            }
            LoopEvent::ToolResult {
                tool_call_id,
                result,
                ..
            } => {
                self.append_tool_result(tool_call_id, result)?;
            }
            _ => unreachable!("live-only events rejected by validate_loop_event"),
        }
        debug_assert!(self.validate_tail_invariant().is_ok());
        Ok(())
    }

    pub fn finish_resume(&mut self) -> Result<Vec<LoopEvent>, ContextError> {
        self.open_steps.clear();
        let mut events = Vec::new();
        while let Some(tool_call_id) = self.pending_tool_result_ids.iter().next().cloned() {
            let result = ExecutableToolResult {
                output: ExecutableToolOutput::Text(INTERRUPTED_TOOL_OUTPUT.to_owned()),
                is_error: true,
                stop_turn: false,
                message: None,
                note: None,
                truncated: false,
            };
            self.append_tool_result(&tool_call_id, &result)?;
            events.push(LoopEvent::ToolResult {
                parent_uuid: tool_call_id.clone(),
                tool_call_id,
                result,
            });
        }
        Ok(events)
    }

    pub fn validate_message(&self, entry: &ContextEntry) -> Result<(), ContextError> {
        entry
            .message
            .validate()
            .map_err(|error| ContextError::InvalidMessage(error.to_string()))
    }

    pub fn validate_loop_event(&self, event: &LoopEvent) -> Result<(), ContextError> {
        if !event.is_recorded() {
            return Err(ContextError::LiveOnlyLoopEvent);
        }
        match event {
            LoopEvent::ContentPart { step_uuid, .. } => {
                if !self.open_steps.contains_key(step_uuid) {
                    return Err(ContextError::UnknownStep(step_uuid.clone()));
                }
            }
            LoopEvent::ToolCall {
                step_uuid,
                tool_call_id,
                ..
            } => {
                if !self.open_steps.contains_key(step_uuid) {
                    return Err(ContextError::UnknownStep(step_uuid.clone()));
                }
                if self.pending_tool_result_ids.contains(tool_call_id) {
                    return Err(ContextError::DuplicateToolCallId(tool_call_id.clone()));
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn update_token_count(&mut self, token_count: u64) {
        self.token_count = token_count;
        self.token_count_covered_message_count = self.history.len();
    }

    pub fn undo(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let mut removed_users = 0usize;
        let mut index = self.history.len();
        while index > 0 {
            index -= 1;
            let entry = &self.history[index];
            if matches!(entry.origin, Some(PromptOrigin::Injection { .. })) {
                continue;
            }
            if matches!(entry.origin, Some(PromptOrigin::CompactionSummary)) {
                break;
            }
            let removed = self.history.remove(index);
            if index < self.token_count_covered_message_count {
                self.token_count_covered_message_count -= 1;
                self.token_count = self
                    .token_count
                    .saturating_sub(estimate_tokens_for_messages(std::slice::from_ref(&removed)));
            }
            if is_real_user_input(&removed) {
                removed_users += 1;
                if removed_users >= count {
                    break;
                }
            }
        }
        self.reset_transient_state();
    }

    pub fn apply_compaction(&mut self, input: CompactionRecord) -> Result<(), ContextError> {
        let summary = ContextEntry::user(
            input.context_summary.clone(),
            PromptOrigin::CompactionSummary,
        );
        if input.kept_user_message_count.is_none() && input.compacted_count < self.history.len() {
            let mut history = vec![summary];
            history.extend(self.history.iter().skip(input.compacted_count).cloned());
            self.history = history;
        } else {
            let user_messages: Vec<ContextEntry> = self
                .history
                .iter()
                .filter(|entry| is_real_user_input(entry))
                .cloned()
                .collect();
            let selection = if input.kept_head_user_message_count.is_some() {
                select_compaction_user_messages(&user_messages)
            } else {
                CompactionUserSelection {
                    head: Vec::new(),
                    tail: select_recent_user_messages(&user_messages),
                    elided: false,
                    omitted_tokens: 0,
                }
            };
            let mut history = Vec::with_capacity(
                selection
                    .head
                    .len()
                    .saturating_add(selection.tail.len())
                    .saturating_add(2),
            );
            history.extend(selection.head);
            if selection.elided {
                history.push(ContextEntry::user(
                    compaction_elision(selection.omitted_tokens),
                    PromptOrigin::Injection {
                        variant: "compaction_elision".to_owned(),
                    },
                ));
            }
            history.extend(selection.tail);
            history.push(summary);
            self.history = history;
        }
        self.token_count = input.tokens_after;
        self.token_count_covered_message_count = self.history.len();
        self.reset_transient_state();
        self.validate_tail_invariant()
    }

    pub fn validate_tail_invariant(&self) -> Result<(), ContextError> {
        let mut pending = BTreeSet::new();
        for (index, entry) in self.history.iter().enumerate() {
            if !pending.is_empty() {
                if entry.message.role != Role::Tool {
                    return Err(ContextError::UnresolvedExchangeBefore {
                        index,
                        pending: pending.into_iter().collect(),
                    });
                }
                let id = entry
                    .message
                    .tool_call_id
                    .as_ref()
                    .ok_or(ContextError::ToolResultWithoutId)?;
                if !pending.remove(id) {
                    return Err(ContextError::OrphanToolResult(id.clone()));
                }
                continue;
            }
            if entry.message.role == Role::Tool {
                return Err(ContextError::OrphanToolResult(
                    entry.message.tool_call_id.clone().unwrap_or_default(),
                ));
            }
            for call in &entry.message.tool_calls {
                if !pending.insert(call.id.clone()) {
                    return Err(ContextError::DuplicateToolCallId(call.id.clone()));
                }
            }
        }
        if pending != self.pending_tool_result_ids {
            return Err(ContextError::PendingSetMismatch {
                computed: pending.into_iter().collect(),
                tracked: self.pending_tool_result_ids.iter().cloned().collect(),
            });
        }
        Ok(())
    }

    fn open_step_mut(&mut self, step_uuid: &str) -> Result<&mut ContextEntry, ContextError> {
        let index = self
            .open_steps
            .get(step_uuid)
            .copied()
            .ok_or_else(|| ContextError::UnknownStep(step_uuid.to_owned()))?;
        self.history
            .get_mut(index)
            .ok_or_else(|| ContextError::UnknownStep(step_uuid.to_owned()))
    }

    fn append_tool_result(
        &mut self,
        tool_call_id: &str,
        result: &ExecutableToolResult,
    ) -> Result<AppendOutcome, ContextError> {
        if !self.pending_tool_result_ids.remove(tool_call_id) {
            return Ok(AppendOutcome::DroppedOrphanToolResult);
        }
        self.history.push(ContextEntry::tool(tool_call_id, result));
        self.flush_deferred()?;
        Ok(AppendOutcome::Appended)
    }

    fn close_all_pending(&mut self, output: &str) -> Result<(), ContextError> {
        while let Some(tool_call_id) = self.pending_tool_result_ids.iter().next().cloned() {
            let result = ExecutableToolResult {
                output: ExecutableToolOutput::Text(output.to_owned()),
                is_error: true,
                stop_turn: false,
                message: None,
                note: None,
                truncated: false,
            };
            self.append_tool_result(&tool_call_id, &result)?;
        }
        Ok(())
    }

    fn flush_deferred(&mut self) -> Result<(), ContextError> {
        while self.pending_tool_result_ids.is_empty() && !self.deferred_messages.is_empty() {
            let entry = self.deferred_messages.remove(0);
            if entry.message.role == Role::Tool {
                continue;
            }
            self.push_entry(entry)?;
        }
        Ok(())
    }

    fn push_entry(&mut self, entry: ContextEntry) -> Result<(), ContextError> {
        for call in &entry.message.tool_calls {
            if self.pending_tool_result_ids.contains(&call.id) {
                return Err(ContextError::DuplicateToolCallId(call.id.clone()));
            }
        }
        self.pending_tool_result_ids
            .extend(entry.message.tool_calls.iter().map(|call| call.id.clone()));
        self.history.push(entry);
        Ok(())
    }

    fn reset_transient_state(&mut self) {
        self.open_steps.clear();
        self.pending_tool_result_ids.clear();
        self.deferred_messages.clear();
        // Undo can remove only the result side of a trailing exchange. Rebuild
        // the exact missing-id set from the canonical tail.
        let mut pending = BTreeSet::new();
        for entry in &self.history {
            if entry.message.role == Role::Tool {
                if let Some(id) = &entry.message.tool_call_id {
                    pending.remove(id);
                }
            } else if pending.is_empty() {
                pending.extend(entry.message.tool_calls.iter().map(|call| call.id.clone()));
            }
        }
        self.pending_tool_result_ids = pending;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionRecord {
    pub context_summary: String,
    pub compacted_count: usize,
    pub tokens_after: u64,
    pub kept_user_message_count: Option<usize>,
    pub kept_head_user_message_count: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionProjection {
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub kept_user_message_count: usize,
    pub kept_head_user_message_count: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    Deferred,
    DroppedOrphanToolResult,
}

pub(crate) fn is_real_user_input(entry: &ContextEntry) -> bool {
    entry.message.role == Role::User
        && matches!(
            entry.origin,
            Some(PromptOrigin::User)
                | Some(PromptOrigin::SkillActivation {
                    trigger: SkillTrigger::UserSlash,
                    ..
                })
                | Some(PromptOrigin::PluginCommand {
                    trigger: PluginCommandTrigger::UserSlash,
                    ..
                })
        )
}

#[derive(Debug)]
struct CompactionUserSelection {
    head: Vec<ContextEntry>,
    tail: Vec<ContextEntry>,
    elided: bool,
    omitted_tokens: u64,
}

fn select_recent_user_messages(messages: &[ContextEntry]) -> Vec<ContextEntry> {
    let mut selected = Vec::new();
    let mut remaining = COMPACT_USER_MESSAGE_MAX_TOKENS;
    for message in messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = estimate_tokens_for_message(message);
        if tokens <= remaining {
            selected.push(message.clone());
            remaining = remaining.saturating_sub(tokens);
        } else {
            selected.push(truncate_user_message(message, remaining, false));
            break;
        }
    }
    selected.reverse();
    selected
}

fn select_compaction_user_messages(messages: &[ContextEntry]) -> CompactionUserSelection {
    let total_tokens = estimate_tokens_for_messages(messages);
    if total_tokens <= COMPACT_USER_MESSAGE_MAX_TOKENS {
        return CompactionUserSelection {
            head: Vec::new(),
            tail: messages.to_vec(),
            elided: false,
            omitted_tokens: 0,
        };
    }

    let tail_budget =
        COMPACT_USER_MESSAGE_MAX_TOKENS.saturating_sub(COMPACT_USER_MESSAGE_HEAD_TOKENS);
    let mut tail = Vec::new();
    let mut tail_remaining = tail_budget;
    let mut head_end = messages.len();
    let mut tail_boundary_prefix = None;
    for (index, message) in messages.iter().enumerate().rev() {
        if tail_remaining == 0 {
            break;
        }
        let tokens = estimate_tokens_for_message(message);
        if tokens <= tail_remaining {
            tail.push(message.clone());
            tail_remaining = tail_remaining.saturating_sub(tokens);
            head_end = index;
            continue;
        }
        let text = message_text(message);
        let suffix = truncate_text_tokens(&text, tail_remaining, true);
        tail.push(replace_message_text(message, suffix.clone()));
        head_end = index;
        let prefix_len = text.len().saturating_sub(suffix.len());
        if prefix_len > 0 {
            tail_boundary_prefix =
                Some(replace_message_text(message, text[..prefix_len].to_owned()));
        }
        break;
    }
    tail.reverse();

    let mut head_candidates = messages[..head_end].to_vec();
    if let Some(prefix) = tail_boundary_prefix {
        head_candidates.push(prefix);
    }
    let mut head = Vec::new();
    let mut head_remaining = COMPACT_USER_MESSAGE_HEAD_TOKENS;
    for message in &head_candidates {
        if head_remaining == 0 {
            break;
        }
        let tokens = estimate_tokens_for_message(message);
        if tokens <= head_remaining {
            head.push(message.clone());
            head_remaining = head_remaining.saturating_sub(tokens);
        } else {
            head.push(truncate_user_message(message, head_remaining, false));
            break;
        }
    }

    let kept_tokens =
        estimate_tokens_for_messages(&head).saturating_add(estimate_tokens_for_messages(&tail));
    CompactionUserSelection {
        head,
        tail,
        elided: true,
        omitted_tokens: total_tokens.saturating_sub(kept_tokens),
    }
}

fn truncate_user_message(message: &ContextEntry, max_tokens: u64, from_end: bool) -> ContextEntry {
    replace_message_text(
        message,
        truncate_text_tokens(&message_text(message), max_tokens, from_end),
    )
}

fn replace_message_text(message: &ContextEntry, text: String) -> ContextEntry {
    let mut message = message.clone();
    message.message.content = vec![ContentPart::text(text)];
    message.message.tool_calls.clear();
    message.message.tools.clear();
    message
}

fn message_text(message: &ContextEntry) -> String {
    message
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn truncate_text_tokens(text: &str, max_tokens: u64, from_end: bool) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    if from_end {
        let mut ascii = 0u64;
        let mut non_ascii = 0u64;
        let mut start = text.len();
        for (index, character) in text.char_indices().rev() {
            if character.is_ascii() {
                ascii = ascii.saturating_add(1);
            } else {
                non_ascii = non_ascii.saturating_add(1);
            }
            if ascii.div_ceil(4).saturating_add(non_ascii) > max_tokens {
                break;
            }
            start = index;
        }
        return text[start..].to_owned();
    }

    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    let mut end = 0usize;
    for (index, character) in text.char_indices() {
        if character.is_ascii() {
            ascii = ascii.saturating_add(1);
        } else {
            non_ascii = non_ascii.saturating_add(1);
        }
        if ascii.div_ceil(4).saturating_add(non_ascii) > max_tokens {
            break;
        }
        end = index.saturating_add(character.len_utf8());
    }
    text[..end].to_owned()
}

fn estimate_tokens_for_messages(entries: &[ContextEntry]) -> u64 {
    entries.iter().fold(0u64, |total, entry| {
        total.saturating_add(estimate_tokens_for_message(entry))
    })
}

fn estimate_tokens_for_message(entry: &ContextEntry) -> u64 {
    let mut total = estimate_text_tokens(match entry.message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    });
    for part in &entry.message.content {
        total = total.saturating_add(match part {
            ContentPart::Text { text } => estimate_text_tokens(text),
            ContentPart::Think { think, .. } => estimate_text_tokens(think),
            ContentPart::ImageUrl { .. }
            | ContentPart::AudioUrl { .. }
            | ContentPart::VideoUrl { .. } => MEDIA_TOKEN_ESTIMATE,
        });
    }
    for call in &entry.message.tool_calls {
        total = total.saturating_add(estimate_text_tokens(&call.name));
        total = total.saturating_add(estimate_text_tokens(
            call.arguments.as_deref().unwrap_or_default(),
        ));
    }
    for tool in &entry.message.tools {
        total = total
            .saturating_add(estimate_text_tokens(&tool.name))
            .saturating_add(estimate_text_tokens(&tool.description))
            .saturating_add(estimate_text_tokens(
                &serde_json::to_string(&tool.parameters).unwrap_or_default(),
            ));
    }
    total
}

fn estimate_text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for character in text.chars() {
        if character.is_ascii() {
            ascii = ascii.saturating_add(1);
        } else {
            non_ascii = non_ascii.saturating_add(1);
        }
    }
    ascii.div_ceil(4).saturating_add(non_ascii)
}

fn compaction_elision(omitted_tokens: u64) -> String {
    format!(
        "<system-reminder>\nSome of this conversation's user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and roughly {omitted_tokens} tokens in between were dropped. The omitted content is covered by the compaction summary at the end of the conversation.\n</system-reminder>"
    )
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("invalid context message: {0}")]
    InvalidMessage(String),
    #[error("recorded content references unknown step {0:?}")]
    UnknownStep(String),
    #[error("duplicate tool call id {0:?}")]
    DuplicateToolCallId(String),
    #[error("orphan tool result {0:?}")]
    OrphanToolResult(String),
    #[error("tool result is missing its call id")]
    ToolResultWithoutId,
    #[error("live-only loop event cannot be applied to canonical context")]
    LiveOnlyLoopEvent,
    #[error("tool arguments could not be serialized: {0}")]
    Arguments(String),
    #[error("unresolved tool exchange before history index {index}: {pending:?}")]
    UnresolvedExchangeBefore { index: usize, pending: Vec<String> },
    #[error(
        "tracked pending calls differ from history: computed={computed:?}, tracked={tracked:?}"
    )]
    PendingSetMismatch {
        computed: Vec<String>,
        tracked: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn begin(step: &str) -> LoopEvent {
        LoopEvent::StepBegin {
            uuid: step.to_owned(),
            turn_id: "1".to_owned(),
            step: 1,
        }
    }

    fn call(step: &str, id: &str) -> LoopEvent {
        LoopEvent::ToolCall {
            uuid: format!("call-{id}"),
            turn_id: "1".to_owned(),
            step: 1,
            step_uuid: step.to_owned(),
            tool_call_id: id.to_owned(),
            name: "Read".to_owned(),
            args: json!({"path":"a"}),
            description: None,
            display: None,
            extras: BTreeMap::new(),
        }
    }

    fn result(id: &str) -> LoopEvent {
        LoopEvent::ToolResult {
            parent_uuid: format!("call-{id}"),
            tool_call_id: id.to_owned(),
            result: ExecutableToolResult {
                output: ExecutableToolOutput::Text("ok".to_owned()),
                is_error: false,
                stop_turn: false,
                message: None,
                note: None,
                truncated: false,
            },
        }
    }

    #[test]
    fn defers_later_messages_until_every_tool_result_arrives() {
        let mut context = CanonicalContext::default();
        context.append_loop_event(&begin("s1")).expect("begin");
        context.append_loop_event(&call("s1", "a")).expect("a");
        context.append_loop_event(&call("s1", "b")).expect("b");
        assert_eq!(
            context
                .append_message(ContextEntry::user("later", PromptOrigin::User))
                .expect("append"),
            AppendOutcome::Deferred
        );
        context.append_loop_event(&result("b")).expect("b result");
        assert_eq!(context.history().len(), 2);
        context.append_loop_event(&result("a")).expect("a result");
        assert_eq!(context.history().len(), 4);
        assert_eq!(context.history()[3].message.text(""), "later");
        context.validate_tail_invariant().expect("valid tail");
    }

    #[test]
    fn step_boundary_closes_mid_history_gap_in_place() {
        let mut context = CanonicalContext::default();
        context.append_loop_event(&begin("s1")).expect("begin");
        context.append_loop_event(&call("s1", "a")).expect("call");
        context.append_loop_event(&begin("s2")).expect("next begin");
        assert_eq!(context.history().len(), 3);
        assert!(context.history()[1].is_error);
        assert!(context.pending_tool_result_ids().is_empty());
        context.validate_tail_invariant().expect("valid tail");
    }

    #[test]
    fn resume_returns_durable_closure_events_for_trailing_calls() {
        let mut context = CanonicalContext::default();
        context.append_loop_event(&begin("s1")).expect("begin");
        context.append_loop_event(&call("s1", "a")).expect("call");
        let closure = context.finish_resume().expect("finish");
        assert_eq!(closure.len(), 1);
        assert!(context.pending_tool_result_ids().is_empty());
        context.validate_tail_invariant().expect("valid tail");
    }

    #[test]
    fn resume_closes_tool_calls_revealed_by_deferred_history() {
        let mut context = CanonicalContext::default();
        context.append_loop_event(&begin("s1")).expect("begin");
        context.append_loop_event(&call("s1", "a")).expect("call a");
        let deferred_assistant = ContextEntry {
            message: Message::assistant(
                Vec::new(),
                vec![ToolCall {
                    kind: ToolCallKind::Function,
                    id: "b".to_owned(),
                    name: "Read".to_owned(),
                    arguments: Some("{}".to_owned()),
                    extras: BTreeMap::new(),
                }],
            ),
            origin: None,
            is_error: false,
            tool_call_displays: BTreeMap::new(),
            note: None,
        };
        assert_eq!(
            context
                .append_message(deferred_assistant)
                .expect("defer assistant"),
            AppendOutcome::Deferred
        );
        let closure = context.finish_resume().expect("finish");
        assert_eq!(closure.len(), 2);
        assert!(context.pending_tool_result_ids().is_empty());
        context.validate_tail_invariant().expect("valid tail");
    }

    #[test]
    fn compaction_keeps_only_user_initiated_input() {
        let mut context = CanonicalContext::default();
        context
            .append_message(ContextEntry::user("plain", PromptOrigin::User))
            .expect("plain user");
        context
            .append_message(ContextEntry::user(
                "model skill",
                PromptOrigin::SkillActivation {
                    activation_id: "activation".to_owned(),
                    skill_name: "inspect".to_owned(),
                    skill_args: None,
                    trigger: SkillTrigger::ModelTool,
                    skill_type: None,
                    skill_path: None,
                    skill_source: None,
                },
            ))
            .expect("model skill");
        context
            .append_message(ContextEntry::user(
                "shell output",
                PromptOrigin::ShellCommand {
                    phase: mycel_agent_protocol::ShellCommandPhase::Output,
                    is_error: None,
                },
            ))
            .expect("shell output");

        context
            .apply_compaction(CompactionRecord {
                context_summary: "summary".to_owned(),
                compacted_count: 3,
                tokens_after: 4,
                kept_user_message_count: Some(1),
                kept_head_user_message_count: None,
            })
            .expect("compact");

        assert_eq!(context.history().len(), 2);
        assert_eq!(context.history()[0].message.text(""), "plain");
        assert_eq!(
            context.history()[1].origin,
            Some(PromptOrigin::CompactionSummary)
        );
    }

    #[test]
    fn split_compaction_replays_oldest_and_newest_text_without_splitting_unicode() {
        let mut context = CanonicalContext::default();
        let oversized = format!("oldest-start-{}-newest-end-🦀", "x".repeat(100_000));
        context
            .append_message(ContextEntry::user(oversized, PromptOrigin::User))
            .expect("oversized user");

        context
            .apply_compaction(CompactionRecord {
                context_summary: "summary".to_owned(),
                compacted_count: 1,
                tokens_after: 20_001,
                kept_user_message_count: Some(2),
                kept_head_user_message_count: Some(1),
            })
            .expect("compact");

        assert_eq!(context.history().len(), 4);
        assert!(context.history()[0]
            .message
            .text("")
            .starts_with("oldest-start-"));
        assert!(matches!(
            context.history()[1].origin,
            Some(PromptOrigin::Injection { ref variant }) if variant == "compaction_elision"
        ));
        assert!(context.history()[2]
            .message
            .text("")
            .ends_with("newest-end-🦀"));
        assert_eq!(
            context.history()[3].origin,
            Some(PromptOrigin::CompactionSummary)
        );
    }
}
