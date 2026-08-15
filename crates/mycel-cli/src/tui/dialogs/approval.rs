use crate::terminal::{graphemes, InputEvent, KeyCode};

use super::{control_char, is_key, printable_char};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    ApprovedForSession,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalChoice {
    pub label: String,
    pub decision: ApprovalDecision,
    pub selected_label: Option<String>,
    pub requires_feedback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDialogAction {
    Respond {
        decision: ApprovalDecision,
        feedback: Option<String>,
        selected_label: Option<String>,
    },
    OpenPreview,
    ToggleToolOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDialogReducer {
    pub choices: Vec<ApprovalChoice>,
    pub selected: usize,
    pub feedback_mode: bool,
    pub feedback: String,
    feedback_cursor: usize,
    pub preview_available: bool,
    pub actions: Vec<ApprovalDialogAction>,
}

impl ApprovalDialogReducer {
    pub fn new(choices: Vec<ApprovalChoice>, preview_available: bool) -> Self {
        Self {
            choices,
            selected: 0,
            feedback_mode: false,
            feedback: String::new(),
            feedback_cursor: 0,
            preview_available,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Escape) || control_char(&event, 'c') || control_char(&event, 'd')
        {
            self.actions.push(ApprovalDialogAction::Respond {
                decision: ApprovalDecision::Rejected,
                feedback: None,
                selected_label: None,
            });
            return;
        }
        if control_char(&event, 'e') {
            if self.preview_available {
                self.actions.push(ApprovalDialogAction::OpenPreview);
            }
            return;
        }
        if control_char(&event, 'o') {
            self.actions.push(ApprovalDialogAction::ToggleToolOutput);
            return;
        }
        if self.feedback_mode {
            self.apply_feedback(event);
            return;
        }
        if self.choices.is_empty() {
            return;
        }
        if is_key(&event, KeyCode::Up) {
            self.selected = (self.selected + self.choices.len() - 1) % self.choices.len();
            return;
        }
        if is_key(&event, KeyCode::Down) {
            self.selected = (self.selected + 1) % self.choices.len();
            return;
        }
        if is_key(&event, KeyCode::Enter) {
            self.select(self.selected);
            return;
        }
        if let Some(index) = printable_char(&event)
            .and_then(|character| character.to_digit(10))
            .and_then(|number| usize::try_from(number).ok())
            .and_then(|number| number.checked_sub(1))
            .filter(|index| *index < self.choices.len())
        {
            self.select(index);
        }
    }

    fn apply_feedback(&mut self, event: InputEvent) {
        if self.choices.is_empty() {
            return;
        }
        if is_key(&event, KeyCode::Up) {
            self.feedback_mode = false;
            self.selected = (self.selected + self.choices.len() - 1) % self.choices.len();
            return;
        }
        if is_key(&event, KeyCode::Down) {
            self.feedback_mode = false;
            self.selected = (self.selected + 1) % self.choices.len();
            return;
        }
        if is_key(&event, KeyCode::Enter) {
            self.submit(self.selected, self.feedback.clone());
            return;
        }
        if is_key(&event, KeyCode::Backspace) {
            if self.feedback_cursor > 0 {
                let start = previous_boundary(&self.feedback, self.feedback_cursor);
                self.feedback.replace_range(start..self.feedback_cursor, "");
                self.feedback_cursor = start;
            }
            return;
        }
        if is_key(&event, KeyCode::Delete) {
            let end = next_boundary(&self.feedback, self.feedback_cursor);
            self.feedback.replace_range(self.feedback_cursor..end, "");
            return;
        }
        if is_key(&event, KeyCode::Left) {
            self.feedback_cursor = previous_boundary(&self.feedback, self.feedback_cursor);
            return;
        }
        if is_key(&event, KeyCode::Right) {
            self.feedback_cursor = next_boundary(&self.feedback, self.feedback_cursor);
            return;
        }
        if is_key(&event, KeyCode::Home) || control_char(&event, 'a') {
            self.feedback_cursor = 0;
            return;
        }
        if is_key(&event, KeyCode::End) || control_char(&event, 'e') {
            self.feedback_cursor = self.feedback.len();
            return;
        }
        if let Some(text) = super::printable(&event) {
            self.feedback.insert_str(self.feedback_cursor, &text);
            self.feedback_cursor += text.len();
        }
    }

    fn select(&mut self, index: usize) {
        let Some(choice) = self.choices.get(index) else {
            return;
        };
        self.selected = index;
        if choice.requires_feedback {
            self.feedback_mode = true;
            self.feedback_cursor = self.feedback.len();
            return;
        }
        self.submit(index, String::new());
    }

    fn submit(&mut self, index: usize, feedback: String) {
        let Some(choice) = self.choices.get(index) else {
            return;
        };
        self.actions.push(ApprovalDialogAction::Respond {
            decision: choice.decision,
            feedback: (!feedback.is_empty()).then_some(feedback),
            selected_label: choice.selected_label.clone(),
        });
    }
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    let mut previous = 0usize;
    let mut offset = 0usize;
    for cluster in graphemes(text) {
        if offset >= cursor {
            break;
        }
        previous = offset;
        offset += cluster.len();
    }
    previous
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    let mut offset = 0usize;
    for cluster in graphemes(text) {
        offset += cluster.len();
        if offset > cursor {
            return offset;
        }
    }
    text.len()
}
