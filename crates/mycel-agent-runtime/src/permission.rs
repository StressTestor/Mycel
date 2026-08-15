use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

use mycel_agent_protocol::{
    ApprovalDecision, ApprovalRequest, ApprovalResponse, ApprovalScope, PermissionDecision,
    PermissionMode, PermissionRule, ToolInputDisplay,
};

use crate::{AgentId, RequestId, ToolCallId};

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ApprovalPort: Send + Sync {
    fn request_approval<'a>(
        &'a self,
        request: ApprovalRequest,
    ) -> PortFuture<'a, Result<ApprovalResponse, PortError>>;
}

pub trait QuestionPort: Send + Sync {
    fn ask<'a>(
        &'a self,
        request: QuestionRequest,
    ) -> PortFuture<'a, Result<QuestionResponse, PortError>>;
}

/// Hook seam used by the future command-hook engine. A denial here is always
/// evaluated before user rules and automatic modes.
pub trait PreToolPermissionPort: Send + Sync {
    fn before_tool<'a>(
        &'a self,
        request: &'a ToolPermissionRequest,
    ) -> PortFuture<'a, Result<PreToolDecision, PortError>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionRequest {
    pub request_id: RequestId,
    pub agent_id: AgentId,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    pub multiple: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionResponse {
    pub answers: Vec<QuestionAnswer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub selected_labels: Vec<String>,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreToolDecision {
    Continue,
    Deny { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExclusiveTool {
    AgentSwarm,
    Workflow,
}

impl ExclusiveTool {
    fn tool_name(self) -> &'static str {
        match self {
            Self::AgentSwarm => "AgentSwarm",
            Self::Workflow => "Workflow",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PlanPolicy {
    #[default]
    NotInPlan,
    Denied {
        reason: String,
    },
    Allowed,
    ExitReview,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolPermissionRequest {
    pub turn_id: u64,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub action: String,
    pub display: ToolInputDisplay,
    /// Canonical rule persisted when a session approval is granted.
    pub approval_rule: Option<String>,
    /// Tool-specific subject matched by `Tool(subject)` rules.
    pub rule_subject: Option<String>,
    pub exclusive_tool: Option<ExclusiveTool>,
    pub plan_policy: PlanPolicy,
    pub create_goal_review: bool,
    pub sensitive_file: bool,
    pub git_control: bool,
    pub git_cwd_write: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionEvaluation {
    pub verdict: PermissionVerdict,
    pub matched_by: PermissionMatch,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionMatch {
    PreToolHook,
    SwarmExclusive,
    WorkflowExclusive,
    AutoQuestionGuard,
    PlanGuard,
    UserDeny,
    AutoMode,
    SessionApproval,
    UserAsk,
    UserAllow,
    ExitPlanReview,
    CreateGoalReview,
    PlanOperation,
    SensitiveFile,
    GitControl,
    YoloMode,
    ExclusiveTool,
    DefaultSafe,
    GitCwdWrite,
    Fallback,
    ApprovalResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorization {
    pub verdict: PermissionVerdict,
    pub matched_by: PermissionMatch,
    pub reason: Option<String>,
    pub approval_response: Option<ApprovalResponse>,
    pub remember_session_rule: Option<String>,
}

#[derive(Clone)]
pub struct PermissionEngine {
    state: Arc<RwLock<PermissionState>>,
    pre_tool: Option<Arc<dyn PreToolPermissionPort>>,
}

#[derive(Clone, Debug)]
struct PermissionState {
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
    session_approvals: Vec<String>,
}

impl PermissionEngine {
    pub fn new(
        mode: PermissionMode,
        rules: Vec<PermissionRule>,
        session_approvals: impl IntoIterator<Item = String>,
        pre_tool: Option<Arc<dyn PreToolPermissionPort>>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(PermissionState {
                mode,
                rules,
                session_approvals: session_approvals.into_iter().collect(),
            })),
            pre_tool,
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.read_state().mode
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        self.write_state().mode = mode;
    }

    pub fn set_rules(&self, rules: Vec<PermissionRule>) {
        self.write_state().rules = rules;
    }

    pub fn remember_session_approval(&self, rule: String) {
        let mut state = self.write_state();
        if !state.session_approvals.contains(&rule) {
            state.session_approvals.push(rule);
        }
    }

    pub fn session_approvals(&self) -> Vec<String> {
        self.read_state().session_approvals.clone()
    }

    pub fn evaluate(&self, request: &ToolPermissionRequest) -> PermissionEvaluation {
        let state = self.read_state().clone();

        if let Some(exclusive) = request.exclusive_tool {
            if request.tool_name != exclusive.tool_name() {
                return evaluation(
                    PermissionVerdict::Deny,
                    match exclusive {
                        ExclusiveTool::AgentSwarm => PermissionMatch::SwarmExclusive,
                        ExclusiveTool::Workflow => PermissionMatch::WorkflowExclusive,
                    },
                    Some(format!(
                        "{} is the only tool permitted in this response",
                        exclusive.tool_name()
                    )),
                );
            }
        }

        if state.mode == PermissionMode::Auto && request.tool_name == "AskUserQuestion" {
            return evaluation(
                PermissionVerdict::Deny,
                PermissionMatch::AutoQuestionGuard,
                Some("AskUserQuestion is disabled in auto mode".to_owned()),
            );
        }

        if let PlanPolicy::Denied { reason } = &request.plan_policy {
            return evaluation(
                PermissionVerdict::Deny,
                PermissionMatch::PlanGuard,
                Some(reason.clone()),
            );
        }

        if let Some(rule) = first_matching_rule(&state.rules, PermissionDecision::Deny, request) {
            return evaluation(
                PermissionVerdict::Deny,
                PermissionMatch::UserDeny,
                rule.reason.clone(),
            );
        }

        if state.mode == PermissionMode::Auto {
            return evaluation(PermissionVerdict::Allow, PermissionMatch::AutoMode, None);
        }

        if state
            .session_approvals
            .iter()
            .any(|pattern| pattern_matches(pattern, request))
        {
            return evaluation(
                PermissionVerdict::Allow,
                PermissionMatch::SessionApproval,
                None,
            );
        }

        if let Some(rule) = first_matching_rule(&state.rules, PermissionDecision::Ask, request) {
            return evaluation(
                PermissionVerdict::Ask,
                PermissionMatch::UserAsk,
                rule.reason.clone(),
            );
        }

        if let Some(rule) = first_matching_rule(&state.rules, PermissionDecision::Allow, request) {
            return evaluation(
                PermissionVerdict::Allow,
                PermissionMatch::UserAllow,
                rule.reason.clone(),
            );
        }

        if request.plan_policy == PlanPolicy::ExitReview {
            return evaluation(
                PermissionVerdict::Ask,
                PermissionMatch::ExitPlanReview,
                Some("review the plan before leaving plan mode".to_owned()),
            );
        }

        if request.create_goal_review {
            return evaluation(
                PermissionVerdict::Ask,
                PermissionMatch::CreateGoalReview,
                Some("review the goal before starting autonomous work".to_owned()),
            );
        }

        if request.plan_policy == PlanPolicy::Allowed {
            return evaluation(
                PermissionVerdict::Allow,
                PermissionMatch::PlanOperation,
                None,
            );
        }

        if request.sensitive_file {
            return evaluation(
                PermissionVerdict::Ask,
                PermissionMatch::SensitiveFile,
                Some("the operation accesses a sensitive file".to_owned()),
            );
        }

        if request.git_control {
            return evaluation(
                PermissionVerdict::Ask,
                PermissionMatch::GitControl,
                Some("the operation changes Git control data".to_owned()),
            );
        }

        if state.mode == PermissionMode::Yolo {
            return evaluation(PermissionVerdict::Allow, PermissionMatch::YoloMode, None);
        }

        if request
            .exclusive_tool
            .is_some_and(|exclusive| request.tool_name == exclusive.tool_name())
        {
            return evaluation(
                PermissionVerdict::Allow,
                PermissionMatch::ExclusiveTool,
                None,
            );
        }

        if is_default_safe_tool(&request.tool_name) {
            return evaluation(PermissionVerdict::Allow, PermissionMatch::DefaultSafe, None);
        }

        if request.git_cwd_write && matches!(request.tool_name.as_str(), "Write" | "Edit") {
            return evaluation(PermissionVerdict::Allow, PermissionMatch::GitCwdWrite, None);
        }

        evaluation(PermissionVerdict::Ask, PermissionMatch::Fallback, None)
    }

    pub async fn authorize(
        &self,
        request: &ToolPermissionRequest,
        approval_port: Option<&dyn ApprovalPort>,
    ) -> Result<Authorization, PortError> {
        if let Some(pre_tool) = &self.pre_tool {
            if let PreToolDecision::Deny { reason } = pre_tool.before_tool(request).await? {
                return Ok(Authorization {
                    verdict: PermissionVerdict::Deny,
                    matched_by: PermissionMatch::PreToolHook,
                    reason: Some(reason),
                    approval_response: None,
                    remember_session_rule: None,
                });
            }
        }

        let evaluation = self.evaluate(request);
        if evaluation.verdict != PermissionVerdict::Ask {
            return Ok(Authorization {
                verdict: evaluation.verdict,
                matched_by: evaluation.matched_by,
                reason: evaluation.reason,
                approval_response: None,
                remember_session_rule: None,
            });
        }

        let port = approval_port.ok_or_else(|| {
            PortError::new(format!(
                "tool {} requires approval, but no interactive approval port is configured",
                request.tool_name
            ))
        })?;
        let response = port
            .request_approval(ApprovalRequest {
                tool_call_id: request.tool_call_id.as_str().to_owned(),
                tool_name: request.tool_name.clone(),
                action: request.action.clone(),
                display: request.display.clone(),
            })
            .await?;
        let approved = response.decision == ApprovalDecision::Approved;
        let remember_session_rule = if approved && response.scope == Some(ApprovalScope::Session) {
            request.approval_rule.clone()
        } else {
            None
        };
        Ok(Authorization {
            verdict: if approved {
                PermissionVerdict::Allow
            } else {
                PermissionVerdict::Deny
            },
            matched_by: PermissionMatch::ApprovalResponse,
            reason: response.feedback.clone().or(evaluation.reason),
            approval_response: Some(response),
            remember_session_rule,
        })
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, PermissionState> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, PermissionState> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn evaluation(
    verdict: PermissionVerdict,
    matched_by: PermissionMatch,
    reason: Option<String>,
) -> PermissionEvaluation {
    PermissionEvaluation {
        verdict,
        matched_by,
        reason,
    }
}

fn first_matching_rule<'a>(
    rules: &'a [PermissionRule],
    decision: PermissionDecision,
    request: &ToolPermissionRequest,
) -> Option<&'a PermissionRule> {
    rules
        .iter()
        .find(|rule| rule.decision == decision && pattern_matches(&rule.pattern, request))
}

fn pattern_matches(pattern: &str, request: &ToolPermissionRequest) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    let (tool_pattern, subject_pattern) = if let Some(open) = pattern.find('(') {
        if !pattern.ends_with(')')
            || pattern[open + 1..pattern.len() - 1]
                .chars()
                .any(|character| matches!(character, '(' | ')'))
        {
            return false;
        }
        let tool = &pattern[..open];
        if tool.is_empty() || tool.contains(')') {
            return false;
        }
        let subject = &pattern[open + 1..pattern.len() - 1];
        (tool, (!subject.is_empty()).then_some(subject))
    } else {
        if pattern.contains(')') {
            return false;
        }
        (pattern, None)
    };
    if !glob_matches(tool_pattern, &request.tool_name) {
        return false;
    }
    subject_pattern.is_none_or(|subject| {
        request
            .rule_subject
            .as_deref()
            .is_some_and(|value| glob_matches(subject, value))
    })
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        match token {
            '*' => {
                current[0] = previous[0];
                for index in 1..=value.len() {
                    current[index] = previous[index] || current[index - 1];
                }
            }
            '?' => {
                current[1..].copy_from_slice(&previous[..value.len()]);
            }
            literal => {
                for index in 1..=value.len() {
                    current[index] = previous[index - 1] && value[index - 1] == literal;
                }
            }
        }
        previous = current;
    }
    previous[value.len()]
}

fn is_default_safe_tool(name: &str) -> bool {
    matches!(
        name,
        "Read"
            | "Grep"
            | "Glob"
            | "ReadMediaFile"
            | "SetTodoList"
            | "TodoList"
            | "TaskList"
            | "TaskOutput"
            | "CronList"
            | "WebSearch"
            | "FetchURL"
            | "Agent"
            | "AskUserQuestion"
            | "Skill"
            | "GetGoal"
            | "SetGoalBudget"
            | "UpdateGoal"
            | "select_tools"
    )
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PortError {
    pub message: String,
}

impl PortError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mycel_agent_protocol::{FileOperation, PermissionScope};

    use super::*;

    fn request(name: &str) -> ToolPermissionRequest {
        ToolPermissionRequest {
            turn_id: 1,
            tool_call_id: ToolCallId::new("call-1").expect("id"),
            tool_name: name.to_owned(),
            action: "run command".to_owned(),
            display: ToolInputDisplay::FileIo {
                operation: FileOperation::Read,
                path: "README.md".to_owned(),
                detail: None,
                content: None,
                before: None,
                after: None,
            },
            approval_rule: Some(name.to_owned()),
            rule_subject: None,
            exclusive_tool: None,
            plan_policy: PlanPolicy::NotInPlan,
            create_goal_review: false,
            sensitive_file: false,
            git_control: false,
            git_cwd_write: false,
        }
    }

    #[test]
    fn deny_rule_beats_yolo_and_argument_rules_require_a_subject() {
        let engine = PermissionEngine::new(
            PermissionMode::Yolo,
            vec![PermissionRule {
                decision: PermissionDecision::Deny,
                scope: PermissionScope::User,
                pattern: "Bash(rm *)".to_owned(),
                reason: Some("blocked".to_owned()),
            }],
            [],
            None,
        );
        let mut command = request("Bash");
        assert_eq!(
            engine.evaluate(&command).matched_by,
            PermissionMatch::YoloMode
        );
        command.rule_subject = Some("rm file".to_owned());
        let result = engine.evaluate(&command);
        assert_eq!(result.verdict, PermissionVerdict::Deny);
        assert_eq!(result.matched_by, PermissionMatch::UserDeny);
    }

    #[test]
    fn auto_question_guard_precedes_auto_approval() {
        let engine = PermissionEngine::new(PermissionMode::Auto, vec![], [], None);
        let result = engine.evaluate(&request("AskUserQuestion"));
        assert_eq!(result.verdict, PermissionVerdict::Deny);
        assert_eq!(result.matched_by, PermissionMatch::AutoQuestionGuard);
    }

    #[test]
    fn session_approval_precedes_a_user_ask_rule() {
        let engine = PermissionEngine::new(
            PermissionMode::Manual,
            vec![PermissionRule {
                decision: PermissionDecision::Ask,
                scope: PermissionScope::User,
                pattern: "Bash".to_owned(),
                reason: None,
            }],
            ["Bash".to_owned()],
            None,
        );
        assert_eq!(
            engine.evaluate(&request("Bash")).matched_by,
            PermissionMatch::SessionApproval
        );
    }

    struct ApprovalMock {
        calls: AtomicUsize,
    }

    impl ApprovalPort for ApprovalMock {
        fn request_approval<'a>(
            &'a self,
            _request: ApprovalRequest,
        ) -> PortFuture<'a, Result<ApprovalResponse, PortError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
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

    #[tokio::test]
    async fn authorization_returns_the_canonical_session_rule_to_persist() {
        let engine = PermissionEngine::new(PermissionMode::Manual, vec![], [], None);
        let port = ApprovalMock {
            calls: AtomicUsize::new(0),
        };
        let authorization = engine
            .authorize(&request("Bash"), Some(&port))
            .await
            .expect("authorization");
        assert_eq!(authorization.verdict, PermissionVerdict::Allow);
        assert_eq!(authorization.remember_session_rule.as_deref(), Some("Bash"));
        assert_eq!(port.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn authorization_without_an_interactive_port_fails_loudly() {
        let engine = PermissionEngine::new(PermissionMode::Manual, vec![], [], None);
        let error = engine
            .authorize(&request("Bash"), None)
            .await
            .expect_err("manual approval cannot be invented");
        assert!(error.message.contains("requires approval"));
        assert!(error.message.contains("no interactive approval port"));
    }
}
