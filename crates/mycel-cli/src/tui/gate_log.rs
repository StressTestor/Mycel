//! Bounded ring of gate decisions observed from the live agent-event stream.
//!
//! Feeds the substrate inspector (spec §5.3): Allow entries come from real
//! (non-synthetic) tool completions, Deny entries from blocked PreToolUse hook
//! results. Pure state fed by `observe`; no I/O, unit-tested by replaying the
//! same events the interactive loop projects.

use std::collections::VecDeque;

use mycel_agent_protocol::AgentEvent;

/// Ring capacity. Old decisions fall off; the inspector shows recent activity,
/// not an audit log (the substrate's audit.jsonl is the durable record).
pub const GATE_LOG_CAP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    Allow,
    Deny,
}

/// One observed decision. `detail` carries the deny reason verbatim (the
/// gate's `permissionDecisionReason`, which embeds `(source: antibody:<id>)`
/// for substrate-matched refusals — crates/mycel-gate/src/main.rs:205-214);
/// it is empty for allows, whose only payload is the tool completion itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDecision {
    pub at_ms: u64,
    pub verdict: GateVerdict,
    pub tool: String,
    pub target: String,
    pub detail: String,
}

/// A started-but-unfinished tool call, tracked so decisions can carry the tool
/// name and target that the decision events themselves do not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCall {
    id: String,
    tool: String,
    target: String,
}

#[derive(Debug, Default)]
pub struct GateLog {
    decisions: VecDeque<GateDecision>,
    pending: Vec<PendingCall>,
}

impl GateLog {
    /// Oldest-first observed decisions.
    pub fn decisions(&self) -> impl Iterator<Item = &GateDecision> {
        self.decisions.iter()
    }

    pub fn last(&self) -> Option<&GateDecision> {
        self.decisions.back()
    }

    /// Observe one live agent event. Returns true when a gate denial was
    /// recorded, so the caller can refresh the substrate summary (a denial is
    /// captured into the substrate as a sentinel event).
    ///
    /// Allow = a non-synthetic `ToolResult`: the gate let the call through and
    /// the tool ran (its own failure is still a gate allow). Synthetic results
    /// are calls that never executed — hook blocks, permission denials, batch
    /// skips (crates/mycel-agent-runtime/src/turn.rs:1276-1293) — never an
    /// allow, and their deny (if any) is recorded from the hook event instead.
    ///
    /// Deny = `HookResult { blocked }` from PreToolUse only. Post-tool hooks
    /// can also block (turn.rs:1403-1409), but the tool already ran; that is
    /// not a gate decision. The hook event carries no tool identity
    /// (turn.rs:1444-1456), so the deny is attributed to the pending call when
    /// exactly one is in flight; a parallel batch degrades to an empty
    /// tool/target rather than guessing.
    pub fn observe(&mut self, event: &AgentEvent, now_ms: u64) -> bool {
        match event {
            AgentEvent::ToolCallStarted {
                tool_call_id,
                name,
                args,
                ..
            } => {
                self.pending.push(PendingCall {
                    id: tool_call_id.clone(),
                    tool: name.clone(),
                    target: extract_target(args),
                });
                false
            }
            AgentEvent::ToolResult {
                tool_call_id,
                synthetic,
                ..
            } => {
                let call = self
                    .pending
                    .iter()
                    .position(|pending| pending.id == *tool_call_id)
                    .map(|index| self.pending.remove(index));
                if synthetic.unwrap_or(false) {
                    return false;
                }
                let Some(call) = call else {
                    // Started before this log existed (or lagged); nothing
                    // honest to attribute.
                    return false;
                };
                self.record(GateDecision {
                    at_ms: now_ms,
                    verdict: GateVerdict::Allow,
                    tool: call.tool,
                    target: call.target,
                    detail: String::new(),
                });
                false
            }
            AgentEvent::HookResult {
                hook_event,
                content,
                blocked,
                ..
            } => {
                if hook_event != "PreToolUse" || !blocked.unwrap_or(false) {
                    return false;
                }
                let (tool, target) = match self.pending.as_slice() {
                    [only] => (only.tool.clone(), only.target.clone()),
                    _ => (String::new(), String::new()),
                };
                self.record(GateDecision {
                    at_ms: now_ms,
                    verdict: GateVerdict::Deny,
                    tool,
                    target,
                    detail: content.clone(),
                });
                true
            }
            // A turn boundary clears in-flight calls so a cancelled or
            // dropped call can never mis-attribute a later decision.
            AgentEvent::TurnEnded { .. } => {
                self.pending.clear();
                false
            }
            _ => false,
        }
    }

    fn record(&mut self, decision: GateDecision) {
        if self.decisions.len() == GATE_LOG_CAP {
            self.decisions.pop_front();
        }
        self.decisions.push_back(decision);
    }
}

/// Best-effort target out of the provider tool arguments
/// (`AgentEvent::ToolCallStarted::args`,
/// crates/mycel-agent-protocol/src/event.rs:782-790): the first present
/// string among the common target keys. Absent or non-string targets render
/// as nothing rather than a JSON dump.
fn extract_target(args: &serde_json::Value) -> String {
    ["command", "file_path", "path", "pattern", "url"]
        .iter()
        .find_map(|key| args.get(key).and_then(serde_json::Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: &str, name: &str, args: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolCallStarted {
            turn_id: 1,
            tool_call_id: id.to_owned(),
            name: name.to_owned(),
            args,
            description: None,
            display: None,
        }
    }

    fn result(id: &str, is_error: bool, synthetic: bool) -> AgentEvent {
        AgentEvent::ToolResult {
            turn_id: 1,
            tool_call_id: id.to_owned(),
            output: serde_json::Value::Null,
            is_error: is_error.then_some(true),
            synthetic: synthetic.then_some(true),
        }
    }

    fn hook(hook_event: &str, content: &str, blocked: bool) -> AgentEvent {
        AgentEvent::HookResult {
            turn_id: Some(1),
            hook_event: hook_event.to_owned(),
            content: content.to_owned(),
            blocked: blocked.then_some(true),
        }
    }

    #[test]
    fn completions_record_allows_and_pre_hook_blocks_record_denies() {
        let mut log = GateLog::default();
        assert!(!log.observe(
            &started("call/1", "read", serde_json::json!({"path": "src/lib.rs"})),
            10,
        ));
        assert!(!log.observe(&result("call/1", false, false), 20));

        assert!(!log.observe(
            &started(
                "call/2",
                "write",
                serde_json::json!({"file_path": "~/.mycel/config.toml"}),
            ),
            30,
        ));
        let denied = log.observe(
            &hook(
                "PreToolUse",
                "Denied. (source: antibody:abcdef12-0000-0000-0000-000000000000)",
                true,
            ),
            40,
        );
        assert!(denied, "a pre-hook block is a recorded deny");
        // The synthetic result for the blocked call is not an allow.
        assert!(!log.observe(&result("call/2", true, true), 50));

        let decisions: Vec<_> = log.decisions().cloned().collect();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].verdict, GateVerdict::Allow);
        assert_eq!(decisions[0].tool, "read");
        assert_eq!(decisions[0].target, "src/lib.rs");
        assert_eq!(decisions[1].verdict, GateVerdict::Deny);
        assert_eq!(decisions[1].tool, "write");
        assert_eq!(decisions[1].target, "~/.mycel/config.toml");
        assert!(decisions[1].detail.contains("antibody:abcdef12"));
        assert_eq!(log.last(), Some(&decisions[1]));
    }

    #[test]
    fn post_hook_blocks_and_failed_tools_are_not_gate_denies() {
        let mut log = GateLog::default();
        log.observe(
            &started(
                "call/1",
                "shell",
                serde_json::json!({"command": "cargo test"}),
            ),
            10,
        );
        // The tool ran and failed on its own: still a gate allow.
        log.observe(&result("call/1", true, false), 20);
        // A post-tool hook block is not a gate decision.
        assert!(!log.observe(&hook("PostToolUseFailure", "post hook refused", true), 30));
        // A non-blocked pre-hook report is not a decision either.
        assert!(!log.observe(&hook("PreToolUse", "warn text", false), 40));

        let decisions: Vec<_> = log.decisions().cloned().collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].verdict, GateVerdict::Allow);
        assert_eq!(decisions[0].target, "cargo test");
    }

    #[test]
    fn ambiguous_parallel_batches_degrade_to_unattributed_denies() {
        let mut log = GateLog::default();
        log.observe(&started("call/1", "read", serde_json::json!({})), 10);
        log.observe(&started("call/2", "write", serde_json::json!({})), 11);
        assert!(log.observe(&hook("PreToolUse", "denied", true), 12));
        let deny = log.last().expect("deny recorded");
        assert_eq!(deny.tool, "", "two in-flight calls: no honest attribution");
        assert_eq!(deny.detail, "denied");
    }

    #[test]
    fn turn_end_clears_pending_so_stale_calls_never_attribute() {
        let mut log = GateLog::default();
        log.observe(
            &started(
                "call/1",
                "shell",
                serde_json::json!({"command": "sleep 99"}),
            ),
            10,
        );
        log.observe(
            &AgentEvent::TurnEnded {
                turn_id: 1,
                reason: mycel_agent_protocol::TurnEndReason::Cancelled,
                error: None,
                duration_ms: None,
            },
            20,
        );
        // A later completion for the dropped call has no pending entry.
        assert!(!log.observe(&result("call/1", false, false), 30));
        assert_eq!(log.decisions().count(), 0);
    }

    #[test]
    fn ring_caps_at_thirty_two_dropping_oldest() {
        let mut log = GateLog::default();
        for index in 0..(GATE_LOG_CAP + 4) {
            let id = format!("call/{index}");
            log.observe(
                &started(
                    &id,
                    "read",
                    serde_json::json!({"path": format!("f{index}")}),
                ),
                index as u64,
            );
            log.observe(&result(&id, false, false), index as u64);
        }
        assert_eq!(log.decisions().count(), GATE_LOG_CAP);
        assert_eq!(
            log.decisions().next().expect("oldest").target,
            "f4",
            "oldest entries fall off"
        );
    }
}
