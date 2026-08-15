use crate::terminal::{InputEvent, KeyCode};

use super::{is_key, printable_char};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub value: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPickerAction {
    Select(String),
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPickerReducer {
    pub options: Vec<PermissionOption>,
    pub selected: usize,
    pub actions: Vec<PermissionPickerAction>,
}

impl PermissionPickerReducer {
    pub fn new(options: Vec<PermissionOption>) -> Self {
        Self {
            options,
            selected: 0,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Escape) {
            self.actions.push(PermissionPickerAction::Cancel);
        } else if is_key(&event, KeyCode::Up) {
            self.selected = self.selected.saturating_sub(1);
        } else if is_key(&event, KeyCode::Down) {
            self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
        } else if is_key(&event, KeyCode::Enter) || printable_char(&event) == Some(' ') {
            if let Some(option) = self.options.get(self.selected) {
                self.actions
                    .push(PermissionPickerAction::Select(option.value.clone()));
            }
        }
    }
}
