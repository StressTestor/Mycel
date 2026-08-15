use crate::{
    terminal::{InputEvent, KeyCode},
    tui::EditorState,
};

use super::{control_char, is_key, pressed_key, printable, printable_char};

const PROVIDER_PAGE_SIZE: usize = 8;
const STOP_CONFIRM_TIMEOUT_MS: u64 = 5_000;
const MAX_GOAL_OBJECTIVE_LENGTH: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    pub label: String,
    pub provider_ids: Vec<String>,
    pub add_action: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderManagerAction {
    Add,
    Delete(Vec<String>),
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderManagerReducer {
    pub rows: Vec<ProviderRow>,
    pub selected: usize,
    pub confirm: Option<Vec<String>>,
    pub actions: Vec<ProviderManagerAction>,
}

impl ProviderManagerReducer {
    pub fn new(rows: Vec<ProviderRow>, active_provider: Option<&str>) -> Self {
        let selected = active_provider
            .and_then(|id| {
                rows.iter().position(|row| {
                    !row.add_action && row.provider_ids.iter().any(|item| item == id)
                })
            })
            .unwrap_or(0);
        Self {
            rows,
            selected,
            confirm: None,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if self.confirm.is_some() {
            if is_key(&event, KeyCode::Escape) || matches!(printable_char(&event), Some('n' | 'N'))
            {
                self.confirm = None;
            } else if matches!(printable_char(&event), Some('y' | 'Y')) {
                if let Some(ids) = self.confirm.take() {
                    self.actions.push(ProviderManagerAction::Delete(ids));
                }
            }
            return;
        }
        if is_key(&event, KeyCode::Escape) {
            self.actions.push(ProviderManagerAction::Close);
        } else if is_key(&event, KeyCode::Up) {
            self.selected = self.selected.saturating_sub(1);
        } else if is_key(&event, KeyCode::Down) {
            self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
        } else if is_key(&event, KeyCode::Left) || is_key(&event, KeyCode::PageUp) {
            self.selected = self.selected.saturating_sub(PROVIDER_PAGE_SIZE);
        } else if is_key(&event, KeyCode::Right) || is_key(&event, KeyCode::PageDown) {
            self.selected =
                (self.selected + PROVIDER_PAGE_SIZE).min(self.rows.len().saturating_sub(1));
        } else if is_key(&event, KeyCode::Enter) {
            if self
                .rows
                .get(self.selected)
                .is_some_and(|row| row.add_action)
            {
                self.actions.push(ProviderManagerAction::Add);
            }
        } else if matches!(printable_char(&event), Some('d' | 'D')) {
            if let Some(row) = self.rows.get(self.selected).filter(|row| !row.add_action) {
                self.confirm = Some(row.provider_ids.clone());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
    Lost,
}

impl TaskStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub detached: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TasksFilter {
    All,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TasksBrowserAction {
    Select(String),
    ToggleFilter(TasksFilter),
    Refresh,
    Close,
    Stop(String),
    StopIgnored(String),
    OpenOutput(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasksBrowserView {
    pub filter: TasksFilter,
    pub visible: Vec<String>,
    pub selected: Option<String>,
    pub pending_stop: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasksBrowserReducer {
    pub tasks: Vec<TaskRow>,
    pub filter: TasksFilter,
    selected: usize,
    pending_stop: Option<(String, u64)>,
    pub actions: Vec<TasksBrowserAction>,
}

impl TasksBrowserReducer {
    pub fn new(tasks: Vec<TaskRow>, filter: TasksFilter, selected_id: Option<&str>) -> Self {
        let mut reducer = Self {
            tasks,
            filter,
            selected: 0,
            pending_stop: None,
            actions: Vec::new(),
        };
        if let Some(id) = selected_id {
            reducer.selected = reducer
                .visible()
                .iter()
                .position(|task| task.id == id)
                .unwrap_or(0);
        }
        reducer
    }

    pub fn view(&self) -> TasksBrowserView {
        let visible = self.visible();
        TasksBrowserView {
            filter: self.filter,
            selected: visible.get(self.selected).map(|task| task.id.clone()),
            visible: visible.iter().map(|task| task.id.clone()).collect(),
            pending_stop: self.pending_stop.as_ref().map(|(id, _)| id.clone()),
        }
    }

    pub fn replace_tasks(&mut self, tasks: Vec<TaskRow>) {
        let selected_id = self
            .visible()
            .get(self.selected)
            .map(|task| task.id.clone());
        self.tasks = tasks;
        let visible = self.visible();
        self.selected = selected_id
            .as_ref()
            .and_then(|id| visible.iter().position(|task| &task.id == id))
            .unwrap_or_else(|| self.selected.min(visible.len().saturating_sub(1)));
        if self.pending_stop.as_ref().is_some_and(|(id, _)| {
            self.tasks
                .iter()
                .find(|task| &task.id == id)
                .is_none_or(|task| task.status.is_terminal())
        }) {
            self.pending_stop = None;
        }
    }

    pub fn tick(&mut self, now_ms: u64) {
        if self
            .pending_stop
            .as_ref()
            .is_some_and(|(_, deadline)| now_ms >= *deadline)
        {
            self.pending_stop = None;
        }
    }

    pub fn apply(&mut self, event: InputEvent, now_ms: u64) {
        self.tick(now_ms);
        if let Some((task_id, _)) = self.pending_stop.clone() {
            self.pending_stop = None;
            if matches!(printable_char(&event), Some('y' | 'Y')) {
                self.actions.push(TasksBrowserAction::Stop(task_id));
            }
            return;
        }
        if is_key(&event, KeyCode::Escape) || matches!(printable_char(&event), Some('q' | 'Q')) {
            self.actions.push(TasksBrowserAction::Close);
        } else if is_key(&event, KeyCode::Up) || printable_char(&event) == Some('k') {
            self.move_selection(false);
        } else if is_key(&event, KeyCode::Down) || printable_char(&event) == Some('j') {
            self.move_selection(true);
        } else if is_key(&event, KeyCode::Tab) || printable_char(&event) == Some('\t') {
            self.filter = match self.filter {
                TasksFilter::All => TasksFilter::Active,
                TasksFilter::Active => TasksFilter::All,
            };
            self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            self.actions
                .push(TasksBrowserAction::ToggleFilter(self.filter));
        } else if matches!(printable_char(&event), Some('r' | 'R')) {
            self.actions.push(TasksBrowserAction::Refresh);
        } else if matches!(printable_char(&event), Some('s' | 'S')) {
            if let Some(task) = self.visible().get(self.selected) {
                if task.status.is_terminal() {
                    self.actions
                        .push(TasksBrowserAction::StopIgnored(task.id.clone()));
                } else {
                    self.pending_stop = Some((task.id.clone(), now_ms + STOP_CONFIRM_TIMEOUT_MS));
                }
            }
        } else if is_key(&event, KeyCode::Enter)
            || matches!(printable_char(&event), Some('o' | 'O'))
        {
            if let Some(task) = self.visible().get(self.selected) {
                self.actions
                    .push(TasksBrowserAction::OpenOutput(task.id.clone()));
            }
        }
    }

    fn move_selection(&mut self, down: bool) {
        let visible_ids: Vec<String> = self.visible().iter().map(|task| task.id.clone()).collect();
        if visible_ids.is_empty() {
            return;
        }
        self.selected = if down {
            (self.selected + 1).min(visible_ids.len() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
        self.actions.push(TasksBrowserAction::Select(
            visible_ids[self.selected].clone(),
        ));
    }

    fn visible(&self) -> Vec<&TaskRow> {
        let mut visible: Vec<&TaskRow> = self
            .tasks
            .iter()
            .filter(|task| task.detached != Some(false))
            .filter(|task| self.filter == TasksFilter::All || !task.status.is_terminal())
            .collect();
        visible.sort_by(|left, right| {
            let terminal = left.status.is_terminal().cmp(&right.status.is_terminal());
            if terminal != std::cmp::Ordering::Equal {
                return terminal;
            }
            if left.status.is_terminal() {
                right
                    .ended_at
                    .unwrap_or(right.started_at)
                    .cmp(&left.ended_at.unwrap_or(left.started_at))
            } else {
                left.started_at.cmp(&right.started_at)
            }
        });
        visible
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalQueueItem {
    pub id: String,
    pub objective: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalMoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalQueueAction {
    Move {
        id: String,
        direction: GoalMoveDirection,
    },
    Edit(String),
    Delete(String),
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalQueueReducer {
    pub goals: Vec<GoalQueueItem>,
    pub selected: usize,
    pub moving_goal: Option<String>,
    pub busy: bool,
    pub actions: Vec<GoalQueueAction>,
}

impl GoalQueueReducer {
    pub fn new(goals: Vec<GoalQueueItem>, selected_id: Option<&str>) -> Self {
        let selected = selected_id
            .and_then(|id| goals.iter().position(|goal| goal.id == id))
            .unwrap_or(0);
        Self {
            goals,
            selected,
            moving_goal: None,
            busy: false,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if self.busy {
            return;
        }
        if is_key(&event, KeyCode::Escape) {
            self.actions.push(GoalQueueAction::Cancel);
            return;
        }
        let selected = self.goals.get(self.selected).map(|goal| goal.id.clone());
        if printable_char(&event) == Some(' ') {
            self.moving_goal = if self.moving_goal == selected {
                None
            } else {
                selected
            };
        } else if matches!(printable_char(&event), Some('e' | 'E')) {
            if let Some(id) = selected {
                self.actions.push(GoalQueueAction::Edit(id));
            }
        } else if matches!(printable_char(&event), Some('d' | 'D')) {
            if let Some(id) = selected {
                self.actions.push(GoalQueueAction::Delete(id));
            }
        } else if let Some(id) = self.moving_goal.clone() {
            if is_key(&event, KeyCode::Up) {
                self.actions.push(GoalQueueAction::Move {
                    id,
                    direction: GoalMoveDirection::Up,
                });
            } else if is_key(&event, KeyCode::Down) {
                self.actions.push(GoalQueueAction::Move {
                    id,
                    direction: GoalMoveDirection::Down,
                });
            }
        } else if is_key(&event, KeyCode::Up) {
            self.selected = self.selected.saturating_sub(1);
        } else if is_key(&event, KeyCode::Down) {
            self.selected = (self.selected + 1).min(self.goals.len().saturating_sub(1));
        }
    }

    pub fn apply_snapshot(&mut self, goals: Vec<GoalQueueItem>, selected_id: Option<&str>) {
        self.goals = goals;
        self.selected = selected_id
            .and_then(|id| self.goals.iter().position(|goal| goal.id == id))
            .unwrap_or_else(|| self.selected.min(self.goals.len().saturating_sub(1)));
        if self
            .moving_goal
            .as_ref()
            .is_some_and(|id| !self.goals.iter().any(|goal| &goal.id == id))
        {
            self.moving_goal = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalEditAction {
    Save { id: String, objective: String },
    Cancel(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalEditReducer {
    pub id: String,
    pub editor: EditorState,
    pub error: Option<String>,
    pub done: bool,
    pub actions: Vec<GoalEditAction>,
}

impl GoalEditReducer {
    pub fn new(id: String, objective: String) -> Self {
        let mut editor = EditorState::default();
        editor.replace_without_undo(objective);
        Self {
            id,
            editor,
            error: None,
            done: false,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if self.done {
            return;
        }
        if is_key(&event, KeyCode::Escape) || control_char(&event, 'c') || control_char(&event, 'd')
        {
            self.done = true;
            self.actions.push(GoalEditAction::Cancel(self.id.clone()));
            return;
        }
        self.error = None;
        if control_char(&event, 'j')
            || pressed_key(&event).is_some_and(|key| {
                key.code == KeyCode::Enter && key.modifiers.shift && !key.modifiers.control
            })
        {
            self.editor.insert_newline();
        } else if is_key(&event, KeyCode::Enter) {
            self.submit();
        } else if is_key(&event, KeyCode::Backspace) {
            self.editor.delete_backward();
        } else if is_key(&event, KeyCode::Delete) {
            self.editor.delete_forward();
        } else if is_key(&event, KeyCode::Left) {
            self.editor.move_left();
        } else if is_key(&event, KeyCode::Right) {
            self.editor.move_right();
        } else if is_key(&event, KeyCode::Up) {
            self.editor.move_vertical(-1);
        } else if is_key(&event, KeyCode::Down) {
            self.editor.move_vertical(1);
        } else if is_key(&event, KeyCode::Home) || control_char(&event, 'a') {
            self.editor.move_home();
        } else if is_key(&event, KeyCode::End) || control_char(&event, 'e') {
            self.editor.move_end();
        } else if let InputEvent::Paste(text) = event {
            self.editor.insert_paste(&sanitize_goal_paste(&text));
        } else if let Some(text) = printable(&event) {
            self.editor.insert_typed(&text);
        }
    }

    fn submit(&mut self) {
        let objective = self.editor.text().trim().to_owned();
        if objective.is_empty() {
            self.error = Some("Goal objective cannot be empty.".to_owned());
        } else if objective.encode_utf16().count() > MAX_GOAL_OBJECTIVE_LENGTH {
            self.error = Some(format!(
                "Goal objective cannot exceed {MAX_GOAL_OBJECTIVE_LENGTH} characters."
            ));
        } else {
            self.done = true;
            self.actions.push(GoalEditAction::Save {
                id: self.id.clone(),
                objective,
            });
        }
    }
}

fn sanitize_goal_paste(text: &str) -> String {
    let bytes = text.replace("\r\n", "\n").replace('\r', "\n").into_bytes();
    let mut output = String::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            continue;
        }
        let rest = std::str::from_utf8(&bytes[index..]).unwrap_or_default();
        let Some(character) = rest.chars().next() else {
            break;
        };
        if character == '\n' || !character.is_control() {
            output.push(character);
        }
        index += character.len_utf8();
    }
    output
}
