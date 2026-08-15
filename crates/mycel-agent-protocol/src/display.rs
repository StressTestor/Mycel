use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Read,
    Write,
    Edit,
    Glob,
    Grep,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayListItem {
    pub title: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolInputDisplay {
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<CommandLanguage>,
    },
    FileIo {
        operation: FileOperation,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
    },
    Diff {
        path: String,
        before: String,
        after: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hunks: Option<u64>,
    },
    Search {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    UrlFetch {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    AgentCall {
        agent_name: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<bool>,
    },
    SkillCall {
        skill_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    TodoList {
        items: Vec<DisplayListItem>,
    },
    Task {
        task_id: String,
        status: String,
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_kind: Option<String>,
    },
    TaskStop {
        task_id: String,
        task_description: String,
    },
    PlanReview {
        plan: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<Vec<PlanReviewOption>>,
    },
    GoalStart {
        objective: String,
        #[serde(
            rename = "completionCriterion",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        completion_criterion: Option<String>,
        mode: GoalStartMode,
    },
    Generic {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStartMode {
    Manual,
    Yolo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandLanguage {
    Bash,
}
