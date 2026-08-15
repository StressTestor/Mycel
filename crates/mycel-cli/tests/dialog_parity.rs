use mycel_cli::{
    terminal::{InputDecoder, InputEvent},
    tui::{
        ApprovalChoice, ApprovalDecision, ApprovalDialogAction, ApprovalDialogReducer,
        ChoiceDialogAction, ChoiceOption, ChoiceScope, EffortSelectorReducer, GoalEditAction,
        GoalEditReducer, GoalMoveDirection, GoalQueueAction, GoalQueueItem, GoalQueueReducer,
        ModelChoice, ModelDialogAction, ModelSelectorReducer, PermissionOption,
        PermissionPickerAction, PermissionPickerReducer, ProviderManagerAction,
        ProviderManagerReducer, ProviderRow, QuestionAnswerMethod, QuestionDialogAction,
        QuestionDialogReducer, QuestionItem, QuestionOption, ScrollViewerReducer, SessionChoice,
        SessionPickerAction, SessionPickerReducer, SessionScope, TaskRow, TaskStatus,
        TasksBrowserAction, TasksBrowserReducer, TasksFilter, ViewerAction, ViewerKind,
    },
};
use serde_json::{json, Value};

#[test]
fn dialog_inputs_match_view_model_parity_corpus() {
    let cases: Value =
        serde_json::from_str(include_str!("../fixtures/parity/dialog-view-cases.json"))
            .expect("dialog fixture JSON");
    for case in cases.as_array().expect("case array") {
        let name = string(case, "name");
        let actual = match string(case, "kind") {
            "approval" => run_approval(case),
            "question" => run_question(case),
            "choice" => run_choice(case),
            "model" => run_model(case),
            "effort" => run_effort(case),
            "session" => run_session(case),
            "provider" => run_provider(case),
            "tasks" => run_tasks(case),
            "goal_queue" => run_goal_queue(case),
            "goal_edit" => run_goal_edit(case),
            "viewer" => run_viewer(case),
            "permission" => run_permission(case),
            other => panic!("unknown dialog kind: {other}"),
        };
        assert_eq!(actual, case["expected"], "dialog parity case: {name}");
    }
}

fn run_approval(case: &Value) -> Value {
    let initial = &case["initial"];
    let choices = match string(initial, "preset") {
        "plan" => vec![
            approval_choice(
                "Approve",
                ApprovalDecision::Approved,
                Some("Approve"),
                false,
            ),
            approval_choice("Reject", ApprovalDecision::Rejected, Some("Reject"), false),
            approval_choice("Revise", ApprovalDecision::Rejected, Some("Revise"), true),
        ],
        _ => vec![
            approval_choice("Approve once", ApprovalDecision::Approved, None, false),
            approval_choice(
                "Approve for this session",
                ApprovalDecision::ApprovedForSession,
                None,
                false,
            ),
            approval_choice("Reject", ApprovalDecision::Rejected, None, false),
            approval_choice(
                "Reject with feedback",
                ApprovalDecision::Rejected,
                None,
                true,
            ),
        ],
    };
    let mut reducer = ApprovalDialogReducer::new(
        choices,
        initial
            .get("preview")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "selected": reducer.selected,
        "feedback_mode": reducer.feedback_mode,
        "feedback": reducer.feedback,
        "actions": reducer.actions.iter().map(approval_action).collect::<Vec<_>>(),
    })
}

fn approval_choice(
    label: &str,
    decision: ApprovalDecision,
    selected_label: Option<&str>,
    requires_feedback: bool,
) -> ApprovalChoice {
    ApprovalChoice {
        label: label.to_owned(),
        decision,
        selected_label: selected_label.map(str::to_owned),
        requires_feedback,
    }
}

fn approval_action(action: &ApprovalDialogAction) -> Value {
    match action {
        ApprovalDialogAction::Respond {
            decision,
            feedback,
            selected_label,
        } => {
            let mut value = json!({
                "kind": "respond",
                "decision": approval_decision(*decision),
            });
            if let Some(feedback) = feedback {
                value["feedback"] = json!(feedback);
            }
            if let Some(label) = selected_label {
                value["selected_label"] = json!(label);
            }
            value
        }
        ApprovalDialogAction::OpenPreview => json!({ "kind": "open_preview" }),
        ApprovalDialogAction::ToggleToolOutput => json!({ "kind": "toggle_output" }),
    }
}

fn approval_decision(value: ApprovalDecision) -> &'static str {
    match value {
        ApprovalDecision::Approved => "approved",
        ApprovalDecision::ApprovedForSession => "approved_for_session",
        ApprovalDecision::Rejected => "rejected",
        ApprovalDecision::Cancelled => "cancelled",
    }
}

fn run_question(case: &Value) -> Value {
    let questions = case["initial"]["questions"]
        .as_array()
        .expect("questions")
        .iter()
        .map(|value| QuestionItem {
            question: string(value, "question").to_owned(),
            header: value
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_owned),
            multi_select: value.get("multi").and_then(Value::as_bool).unwrap_or(false),
            other_label: value
                .get("other_label")
                .and_then(Value::as_str)
                .map(str::to_owned),
            options: value["options"]
                .as_array()
                .expect("options")
                .iter()
                .map(|option| QuestionOption {
                    label: option.as_str().expect("option label").to_owned(),
                    description: None,
                })
                .collect(),
        })
        .collect();
    let mut reducer = QuestionDialogReducer::new(questions);
    apply_inputs(case, |event| reducer.apply(event));
    let view = reducer.view();
    json!({
        "current_tab": view.current_tab,
        "submit_tab": view.submit_tab,
        "submit_action": view.submit_action,
        "editing_other": view.editing_other,
        "cursor": view.cursor,
        "answers": view.answers,
        "unanswered": view.unanswered,
        "actions": reducer.actions.iter().map(question_action).collect::<Vec<_>>(),
    })
}

fn question_action(action: &QuestionDialogAction) -> Value {
    match action {
        QuestionDialogAction::ToggleToolOutput => json!({ "kind": "toggle_output" }),
        QuestionDialogAction::Answer { answers, method } => {
            let mut value = json!({ "kind": "answer", "answers": answers });
            if let Some(method) = method {
                value["method"] = json!(answer_method(*method));
            }
            value
        }
    }
}

fn answer_method(value: QuestionAnswerMethod) -> &'static str {
    match value {
        QuestionAnswerMethod::Enter => "enter",
        QuestionAnswerMethod::NumberKey => "number_key",
        QuestionAnswerMethod::Space => "space",
    }
}

fn run_choice(case: &Value) -> Value {
    let initial = &case["initial"];
    let options = string_array(initial, "options")
        .into_iter()
        .map(|value| ChoiceOption {
            value: value.clone(),
            label: value,
            description: None,
        })
        .collect();
    let mut reducer = mycel_cli::tui::SearchableChoiceReducer::new(
        options,
        initial.get("current").and_then(Value::as_str),
        initial["searchable"].as_bool().unwrap_or(false),
        initial["session_only"].as_bool().unwrap_or(false),
        8,
    );
    apply_inputs(case, |event| reducer.apply(event));
    let view = reducer.view();
    json!({
        "query": view.query,
        "selected": view.selected,
        "filtered": view.filtered,
        "page": view.page,
        "page_count": view.page_count,
        "actions": reducer.actions.iter().map(choice_action).collect::<Vec<_>>(),
    })
}

fn choice_action(action: &ChoiceDialogAction) -> Value {
    match action {
        ChoiceDialogAction::Cancel => json!({ "kind": "cancel" }),
        ChoiceDialogAction::Select { value, scope } => json!({
            "kind": "select",
            "value": value,
            "scope": choice_scope(*scope),
        }),
    }
}

fn choice_scope(value: ChoiceScope) -> &'static str {
    match value {
        ChoiceScope::Persistent => "persistent",
        ChoiceScope::Session => "session",
    }
}

fn run_model(case: &Value) -> Value {
    let initial = &case["initial"];
    let models = initial["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|value| ModelChoice {
            alias: string(value, "alias").to_owned(),
            name: string(value, "alias").to_owned(),
            provider: "test".to_owned(),
            efforts: string_array(value, "efforts"),
            default_effort: None,
            thinking_supported: true,
        })
        .collect();
    let mut reducer = ModelSelectorReducer::new(
        models,
        string(initial, "current").to_owned(),
        string(initial, "current_effort").to_owned(),
        true,
        true,
        8,
    );
    apply_inputs(case, |event| reducer.apply(event));
    let view = reducer.view();
    json!({
        "query": view.query,
        "selected": view.selected,
        "effort": view.effort,
        "filtered": view.filtered,
        "actions": reducer.actions.iter().map(model_action).collect::<Vec<_>>(),
    })
}

fn model_action(action: &ModelDialogAction) -> Value {
    match action {
        ModelDialogAction::Cancel => json!({ "kind": "cancel" }),
        ModelDialogAction::Select {
            alias,
            effort,
            scope,
        } => json!({
            "kind": "select",
            "alias": alias,
            "effort": effort,
            "scope": choice_scope(*scope),
        }),
    }
}

fn run_effort(case: &Value) -> Value {
    let initial = &case["initial"];
    let mut reducer = EffortSelectorReducer::new(
        string_array(initial, "efforts"),
        string(initial, "current"),
        initial["session_only"].as_bool().unwrap_or(false),
    );
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "active": reducer.active,
        "actions": reducer.actions.iter().map(choice_action).collect::<Vec<_>>(),
    })
}

fn run_session(case: &Value) -> Value {
    let initial = &case["initial"];
    let sessions = initial["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|value| SessionChoice {
            id: string(value, "id").to_owned(),
            title: value
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            work_dir: string(value, "work_dir").to_owned(),
        })
        .collect();
    let mut reducer = SessionPickerReducer::new(
        sessions,
        string(initial, "current_id").to_owned(),
        string(initial, "cwd").to_owned(),
        parse_session_scope(string(initial, "scope")),
        None,
        50,
    );
    apply_inputs(case, |event| reducer.apply(event));
    let view = reducer.view();
    json!({
        "scope": session_scope(view.scope),
        "query": view.query,
        "selected": view.selected,
        "filtered": view.filtered,
        "actions": reducer.actions.iter().map(session_action).collect::<Vec<_>>(),
    })
}

fn session_action(action: &SessionPickerAction) -> Value {
    match action {
        SessionPickerAction::Select(id) => json!({ "kind": "select", "id": id }),
        SessionPickerAction::CrossCwd {
            session_id,
            command,
        } => json!({ "kind": "cross_cwd", "session_id": session_id, "command": command }),
        SessionPickerAction::ToggleScope { scope, selected_id } => json!({
            "kind": "toggle_scope",
            "scope": session_scope(*scope),
            "selected_id": selected_id,
        }),
        SessionPickerAction::Cancel => json!({ "kind": "cancel" }),
        SessionPickerAction::Interrupt => json!({ "kind": "interrupt" }),
        SessionPickerAction::Exit => json!({ "kind": "exit" }),
    }
}

fn parse_session_scope(value: &str) -> SessionScope {
    if value == "all" {
        SessionScope::All
    } else {
        SessionScope::Cwd
    }
}

fn session_scope(value: SessionScope) -> &'static str {
    match value {
        SessionScope::Cwd => "cwd",
        SessionScope::All => "all",
    }
}

fn run_provider(case: &Value) -> Value {
    let initial = &case["initial"];
    let rows = initial["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|value| ProviderRow {
            id: string(value, "id").to_owned(),
            label: string(value, "id").to_owned(),
            provider_ids: string_array(value, "provider_ids"),
            add_action: value.get("add").and_then(Value::as_bool).unwrap_or(false),
        })
        .collect();
    let mut reducer =
        ProviderManagerReducer::new(rows, initial.get("active").and_then(Value::as_str));
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "selected": reducer.selected,
        "confirm": reducer.confirm,
        "actions": reducer.actions.iter().map(provider_action).collect::<Vec<_>>(),
    })
}

fn provider_action(action: &ProviderManagerAction) -> Value {
    match action {
        ProviderManagerAction::Add => json!({ "kind": "add" }),
        ProviderManagerAction::Delete(ids) => {
            json!({ "kind": "delete", "provider_ids": ids })
        }
        ProviderManagerAction::Close => json!({ "kind": "close" }),
    }
}

fn run_tasks(case: &Value) -> Value {
    let initial = &case["initial"];
    let tasks = initial["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .map(|value| TaskRow {
            id: string(value, "id").to_owned(),
            status: parse_task_status(string(value, "status")),
            started_at: value["started"].as_u64().expect("started"),
            ended_at: value.get("ended").and_then(Value::as_u64),
            detached: value.get("detached").and_then(Value::as_bool),
        })
        .collect();
    let mut reducer =
        TasksBrowserReducer::new(tasks, parse_tasks_filter(string(initial, "filter")), None);
    apply_inputs(case, |event| reducer.apply(event, 0));
    let view = reducer.view();
    json!({
        "filter": tasks_filter(view.filter),
        "visible": view.visible,
        "selected": view.selected,
        "pending_stop": view.pending_stop,
        "actions": reducer.actions.iter().map(tasks_action).collect::<Vec<_>>(),
    })
}

fn parse_task_status(value: &str) -> TaskStatus {
    match value {
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        "timed_out" => TaskStatus::TimedOut,
        "killed" => TaskStatus::Killed,
        "lost" => TaskStatus::Lost,
        _ => TaskStatus::Running,
    }
}

fn parse_tasks_filter(value: &str) -> TasksFilter {
    if value == "active" {
        TasksFilter::Active
    } else {
        TasksFilter::All
    }
}

fn tasks_filter(value: TasksFilter) -> &'static str {
    match value {
        TasksFilter::All => "all",
        TasksFilter::Active => "active",
    }
}

fn tasks_action(action: &TasksBrowserAction) -> Value {
    match action {
        TasksBrowserAction::Select(id) => json!({ "kind": "select", "id": id }),
        TasksBrowserAction::ToggleFilter(filter) => {
            json!({ "kind": "toggle_filter", "filter": tasks_filter(*filter) })
        }
        TasksBrowserAction::Refresh => json!({ "kind": "refresh" }),
        TasksBrowserAction::Close => json!({ "kind": "close" }),
        TasksBrowserAction::Stop(id) => json!({ "kind": "stop", "id": id }),
        TasksBrowserAction::StopIgnored(id) => {
            json!({ "kind": "stop_ignored", "id": id })
        }
        TasksBrowserAction::OpenOutput(id) => json!({ "kind": "open_output", "id": id }),
    }
}

fn run_goal_queue(case: &Value) -> Value {
    let goals = string_array(&case["initial"], "goals")
        .into_iter()
        .map(|id| GoalQueueItem {
            objective: format!("objective {id}"),
            id,
        })
        .collect();
    let mut reducer = GoalQueueReducer::new(goals, None);
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "selected": reducer.selected,
        "moving_goal": reducer.moving_goal,
        "actions": reducer.actions.iter().map(goal_action).collect::<Vec<_>>(),
    })
}

fn goal_action(action: &GoalQueueAction) -> Value {
    match action {
        GoalQueueAction::Move { id, direction } => json!({
            "kind": "move",
            "id": id,
            "direction": match direction {
                GoalMoveDirection::Up => "up",
                GoalMoveDirection::Down => "down",
            },
        }),
        GoalQueueAction::Edit(id) => json!({ "kind": "edit", "id": id }),
        GoalQueueAction::Delete(id) => json!({ "kind": "delete", "id": id }),
        GoalQueueAction::Cancel => json!({ "kind": "cancel" }),
    }
}

fn run_goal_edit(case: &Value) -> Value {
    let initial = &case["initial"];
    let mut reducer = GoalEditReducer::new(
        string(initial, "id").to_owned(),
        string(initial, "objective").to_owned(),
    );
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "text": reducer.editor.text(),
        "error": reducer.error,
        "done": reducer.done,
        "actions": reducer.actions.iter().map(goal_edit_action).collect::<Vec<_>>(),
    })
}

fn goal_edit_action(action: &GoalEditAction) -> Value {
    match action {
        GoalEditAction::Save { id, objective } => {
            json!({ "kind": "save", "id": id, "objective": objective })
        }
        GoalEditAction::Cancel(id) => json!({ "kind": "cancel", "id": id }),
    }
}

fn run_viewer(case: &Value) -> Value {
    let initial = &case["initial"];
    let kind = match string(initial, "viewer") {
        "help" => ViewerKind::Help,
        "preview" => ViewerKind::ApprovalPreview,
        _ => ViewerKind::Output,
    };
    let mut reducer = ScrollViewerReducer::new(
        kind,
        initial["lines"].as_u64().expect("lines") as usize,
        initial["visible"].as_u64().expect("visible") as usize,
    );
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "scroll_top": reducer.scroll_top,
        "actions": reducer.actions.iter().map(viewer_action).collect::<Vec<_>>(),
    })
}

fn viewer_action(action: &ViewerAction) -> Value {
    match action {
        ViewerAction::Close => json!({ "kind": "close" }),
    }
}

fn run_permission(case: &Value) -> Value {
    let options = string_array(&case["initial"], "options")
        .into_iter()
        .map(|value| PermissionOption {
            label: value.clone(),
            value,
            description: String::new(),
        })
        .collect();
    let mut reducer = PermissionPickerReducer::new(options);
    apply_inputs(case, |event| reducer.apply(event));
    json!({
        "selected": reducer.selected,
        "actions": reducer.actions.iter().map(permission_action).collect::<Vec<_>>(),
    })
}

fn permission_action(action: &PermissionPickerAction) -> Value {
    match action {
        PermissionPickerAction::Select(value) => json!({ "kind": "select", "value": value }),
        PermissionPickerAction::Cancel => json!({ "kind": "cancel" }),
    }
}

fn apply_inputs(case: &Value, mut apply: impl FnMut(InputEvent)) {
    let mut decoder = InputDecoder::default();
    for chunk in case["chunks"].as_array().expect("chunks") {
        for event in decoder.feed(chunk.as_str().expect("chunk string").as_bytes()) {
            apply(event);
        }
        for event in decoder.flush() {
            apply(event);
        }
    }
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .expect("string array")
        .iter()
        .map(|item| item.as_str().expect("string").to_owned())
        .collect()
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing {key}"))
}
