use std::path::{Component, Path};

use crate::terminal::{graphemes, InputEvent, KeyCode};

use super::{control_char, fuzzy_score, is_key, printable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChoice {
    pub id: String,
    pub title: Option<String>,
    pub work_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    Cwd,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerAction {
    Select(String),
    CrossCwd {
        session_id: String,
        command: String,
    },
    ToggleScope {
        scope: SessionScope,
        selected_id: String,
    },
    Cancel,
    Interrupt,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPickerView {
    pub scope: SessionScope,
    pub query: String,
    pub selected: Option<String>,
    pub filtered: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPickerReducer {
    pub sessions: Vec<SessionChoice>,
    pub current_session_id: String,
    pub current_work_dir: String,
    pub scope: SessionScope,
    pub page_size: usize,
    query: String,
    cursor: usize,
    pub actions: Vec<SessionPickerAction>,
}

impl SessionPickerReducer {
    pub fn new(
        sessions: Vec<SessionChoice>,
        current_session_id: String,
        current_work_dir: String,
        scope: SessionScope,
        initial_selected: Option<&str>,
        page_size: usize,
    ) -> Self {
        let cursor = initial_selected
            .and_then(|id| sessions.iter().position(|session| session.id == id))
            .unwrap_or(0);
        Self {
            sessions,
            current_session_id,
            current_work_dir,
            scope,
            page_size: page_size.max(1),
            query: String::new(),
            cursor,
            actions: Vec::new(),
        }
    }

    pub fn view(&self) -> SessionPickerView {
        let filtered = self.filtered_indices();
        SessionPickerView {
            scope: self.scope,
            query: self.query.clone(),
            selected: filtered
                .get(self.cursor.min(filtered.len().saturating_sub(1)))
                .map(|index| self.sessions[*index].id.clone()),
            filtered: filtered
                .iter()
                .map(|index| self.sessions[*index].id.clone())
                .collect(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if control_char(&event, 'c') {
            self.actions.push(SessionPickerAction::Interrupt);
            return;
        }
        if control_char(&event, 'd') {
            self.actions.push(SessionPickerAction::Exit);
            return;
        }
        if control_char(&event, 'a') {
            let selected_id = self
                .selected()
                .map(|session| session.id.clone())
                .unwrap_or_else(|| self.current_session_id.clone());
            self.scope = match self.scope {
                SessionScope::Cwd => SessionScope::All,
                SessionScope::All => SessionScope::Cwd,
            };
            self.query.clear();
            self.cursor = self
                .filtered_indices()
                .iter()
                .position(|index| self.sessions[*index].id == selected_id)
                .unwrap_or(0);
            self.actions.push(SessionPickerAction::ToggleScope {
                scope: self.scope,
                selected_id,
            });
            return;
        }
        if is_key(&event, KeyCode::Escape) {
            if self.query.is_empty() {
                self.actions.push(SessionPickerAction::Cancel);
            } else {
                self.query.clear();
                self.cursor = 0;
            }
            return;
        }
        if is_key(&event, KeyCode::Enter) {
            if let Some(session) = self.selected() {
                if same_path(&session.work_dir, &self.current_work_dir) {
                    self.actions
                        .push(SessionPickerAction::Select(session.id.clone()));
                } else {
                    self.actions.push(SessionPickerAction::CrossCwd {
                        session_id: session.id.clone(),
                        command: format!(
                            "cd {} && mycel --resume {}",
                            quote_posix(&session.work_dir),
                            quote_posix(&session.id)
                        ),
                    });
                }
            }
            return;
        }
        let length = self.filtered_indices().len();
        if is_key(&event, KeyCode::Up) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if is_key(&event, KeyCode::Down) {
            self.cursor = (self.cursor + 1).min(length.saturating_sub(1));
        } else if is_key(&event, KeyCode::PageUp) {
            self.cursor = self.cursor.saturating_sub(self.page_size);
        } else if is_key(&event, KeyCode::PageDown) {
            self.cursor = (self.cursor + self.page_size).min(length.saturating_sub(1));
        } else if is_key(&event, KeyCode::Backspace) {
            self.query = graphemes(&self.query)
                .collect::<Vec<_>>()
                .split_last()
                .map_or_else(String::new, |(_, rest)| rest.concat());
            self.cursor = 0;
        } else if let Some(text) = printable(&event) {
            if text.chars().all(|character| !character.is_control()) {
                self.query.push_str(&text);
                self.cursor = 0;
            }
        }
    }

    fn selected(&self) -> Option<&SessionChoice> {
        let filtered = self.filtered_indices();
        filtered
            .get(self.cursor.min(filtered.len().saturating_sub(1)))
            .and_then(|index| self.sessions.get(*index))
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let mut matches: Vec<(usize, i64)> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                if self.scope != SessionScope::All
                    && !same_path(&session.work_dir, &self.current_work_dir)
                {
                    return None;
                }
                fuzzy_score(
                    &self.query,
                    session
                        .title
                        .as_deref()
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or(&session.id),
                )
                .map(|score| (index, score))
            })
            .collect();
        matches.sort_by_key(|(_, score)| *score);
        matches.into_iter().map(|(index, _)| index).collect()
    }
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn same_path(left: &str, right: &str) -> bool {
    normalize_path(left) == normalize_path(right)
}

fn normalize_path(value: &str) -> Vec<String> {
    let mut normalized = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Prefix(prefix) => {
                normalized.push(prefix.as_os_str().to_string_lossy().into_owned());
            }
            Component::RootDir => normalized.push("/".to_owned()),
            Component::Normal(part) => normalized.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if normalized.last().is_some_and(|part| part != "/") {
                    normalized.pop();
                }
            }
            Component::CurDir => {}
        }
    }
    normalized
}
