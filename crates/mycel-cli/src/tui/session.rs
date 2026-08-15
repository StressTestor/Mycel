use crate::terminal::{InputEvent, KeyCode, KeyEvent, KeyKind};

use super::EditorState;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    Prompt,
    Shell,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionPhase {
    #[default]
    Idle,
    Busy,
    Compacting,
    Shell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionMode {
    Prompt,
    Shell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput {
    pub text: String,
    pub mode: SubmissionMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalAction {
    Submit(QueuedInput),
    Newline,
    Cancel,
    Clear,
    Queue(QueuedInput),
    Steer(Vec<String>),
    Detach,
    TogglePlan(bool),
    PasteMedia,
    ExitArmed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionReducer {
    pub editor: EditorState,
    pub input_mode: InputMode,
    pub phase: SessionPhase,
    pub queue: Vec<QueuedInput>,
    pub actions: Vec<LogicalAction>,
    pub plan: bool,
    pub history_draft_mode: Option<InputMode>,
}

impl SessionReducer {
    pub fn apply(&mut self, event: InputEvent) {
        match event {
            InputEvent::Text(text) => self.insert_text(&text, false),
            InputEvent::Paste(text) => self.insert_text(&text, true),
            InputEvent::Key(key) if key.kind != KeyKind::Release => self.apply_key(key),
            InputEvent::Key(_) | InputEvent::Unknown(_) => {}
        }
    }

    fn insert_text(&mut self, text: &str, paste: bool) {
        if self.input_mode == InputMode::Prompt && self.editor.text().is_empty() {
            if let Some(command) = text.strip_prefix('!') {
                self.input_mode = InputMode::Shell;
                if paste {
                    self.editor.insert_paste(command);
                } else {
                    self.editor.insert_typed(command);
                }
                return;
            }
        }
        if paste {
            self.editor.insert_paste(text);
        } else {
            self.editor.insert_typed(text);
        }
    }

    fn apply_key(&mut self, key: KeyEvent) {
        let modifiers = key.modifiers;
        if modifiers.control {
            match key.code {
                KeyCode::Char('c') => {
                    self.cancel_or_clear();
                    return;
                }
                KeyCode::Char('j') => {
                    self.editor.insert_newline();
                    self.actions.push(LogicalAction::Newline);
                    return;
                }
                KeyCode::Char('s') => {
                    self.steer();
                    return;
                }
                KeyCode::Char('v') => {
                    self.actions.push(LogicalAction::PasteMedia);
                    return;
                }
                KeyCode::Char('b')
                    if matches!(self.phase, SessionPhase::Busy | SessionPhase::Shell) =>
                {
                    self.actions.push(LogicalAction::Detach);
                    return;
                }
                KeyCode::Char('b') => {
                    self.editor.move_left();
                    return;
                }
                KeyCode::Char('w') => {
                    self.editor.kill_word_backward();
                    return;
                }
                KeyCode::Char('u') => {
                    self.editor.kill_to_line_start();
                    return;
                }
                KeyCode::Char('k') => {
                    self.editor.kill_to_line_end();
                    return;
                }
                KeyCode::Char('y') => {
                    self.editor.yank();
                    return;
                }
                KeyCode::Char('-' | '_') => {
                    self.editor.undo();
                    return;
                }
                _ => {}
            }
        }
        if modifiers.alt {
            match key.code {
                KeyCode::Char('d') => {
                    self.editor.kill_word_forward();
                    return;
                }
                KeyCode::Char('y') => {
                    self.editor.yank_pop();
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter if modifiers.shift => {
                self.editor.insert_newline();
                self.actions.push(LogicalAction::Newline);
            }
            KeyCode::Enter => self.submit_or_queue(),
            KeyCode::Escape => {
                if self.input_mode == InputMode::Shell && self.editor.text().is_empty() {
                    self.input_mode = InputMode::Prompt;
                } else if self.phase != SessionPhase::Idle {
                    self.phase = SessionPhase::Idle;
                    self.actions.push(LogicalAction::Cancel);
                }
            }
            KeyCode::Backspace => {
                if self.input_mode == InputMode::Shell && self.editor.text().is_empty() {
                    self.input_mode = InputMode::Prompt;
                } else {
                    self.editor.delete_backward();
                }
            }
            KeyCode::Delete => self.editor.delete_forward(),
            KeyCode::Left => self.editor.move_left(),
            KeyCode::Right => self.editor.move_right(),
            KeyCode::Home => self.editor.move_home(),
            KeyCode::End => self.editor.move_end(),
            KeyCode::Up => {
                if self.phase != SessionPhase::Idle
                    && self.editor.text().is_empty()
                    && self.recall_last_queued()
                {
                    return;
                }
                let entering = !self.editor.is_history_browsing();
                if self.editor.history_up(self.input_mode == InputMode::Shell) {
                    if entering {
                        self.history_draft_mode = Some(self.input_mode);
                    }
                    self.restore_history_mode();
                }
            }
            KeyCode::Down => {
                let was_browsing = self.editor.is_history_browsing();
                if self.editor.history_down() {
                    if was_browsing && !self.editor.is_history_browsing() {
                        if let Some(mode) = self.history_draft_mode.take() {
                            self.input_mode = mode;
                        }
                    } else {
                        self.restore_history_mode();
                    }
                }
            }
            KeyCode::Tab if modifiers.shift => {
                self.plan = !self.plan;
                self.actions.push(LogicalAction::TogglePlan(self.plan));
            }
            KeyCode::Char(character) if !modifiers.control && !modifiers.alt => {
                self.insert_text(&character.to_string(), false);
            }
            _ => {}
        }
    }

    fn submit_or_queue(&mut self) {
        if self.editor.text().is_empty() {
            return;
        }
        let mode = match self.input_mode {
            InputMode::Prompt => SubmissionMode::Prompt,
            InputMode::Shell => SubmissionMode::Shell,
        };
        let input = QueuedInput {
            text: self.editor.text().to_owned(),
            mode,
        };
        let history = match mode {
            SubmissionMode::Prompt => input.text.clone(),
            SubmissionMode::Shell => format!("!{}", input.text),
        };
        self.editor.add_history(history);
        self.editor.clear_after_submit();
        self.input_mode = InputMode::Prompt;
        if self.phase == SessionPhase::Idle {
            self.phase = match mode {
                SubmissionMode::Prompt => SessionPhase::Busy,
                SubmissionMode::Shell => SessionPhase::Shell,
            };
            self.actions.push(LogicalAction::Submit(input));
        } else {
            self.queue.push(input.clone());
            self.actions.push(LogicalAction::Queue(input));
        }
    }

    fn cancel_or_clear(&mut self) {
        if !self.editor.text().is_empty() {
            self.editor.clear();
            self.actions.push(LogicalAction::Clear);
        } else if self.phase != SessionPhase::Idle {
            self.phase = SessionPhase::Idle;
            self.actions.push(LogicalAction::Cancel);
        } else {
            self.actions.push(LogicalAction::ExitArmed);
        }
    }

    fn steer(&mut self) {
        if self.phase == SessionPhase::Idle {
            return;
        }
        let mut messages = Vec::new();
        self.queue.retain(|queued| {
            if queued.mode == SubmissionMode::Prompt {
                messages.push(queued.text.clone());
                false
            } else {
                true
            }
        });
        if self.input_mode == InputMode::Prompt && !self.editor.text().is_empty() {
            messages.push(self.editor.text().to_owned());
            self.editor.clear_after_submit();
        }
        if !messages.is_empty() {
            self.actions.push(LogicalAction::Steer(messages));
        }
    }

    fn recall_last_queued(&mut self) -> bool {
        let Some(input) = self.queue.pop() else {
            return false;
        };
        self.input_mode = match input.mode {
            SubmissionMode::Prompt => InputMode::Prompt,
            SubmissionMode::Shell => InputMode::Shell,
        };
        self.editor.replace_without_undo(input.text);
        true
    }

    fn restore_history_mode(&mut self) {
        if let Some(command) = self.editor.text().strip_prefix('!').map(str::to_owned) {
            self.input_mode = InputMode::Shell;
            self.editor.replace_history_text(command);
        } else {
            self.input_mode = InputMode::Prompt;
        }
    }
}
