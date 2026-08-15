use std::collections::BTreeSet;

use crate::terminal::{graphemes, InputEvent, KeyCode};

use super::{control_char, is_key, printable, printable_char};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionItem {
    pub question: String,
    pub header: Option<String>,
    pub multi_select: bool,
    pub other_label: Option<String>,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionAnswerMethod {
    Enter,
    NumberKey,
    Space,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionDialogAction {
    Answer {
        answers: Vec<Option<String>>,
        method: Option<QuestionAnswerMethod>,
    },
    ToggleToolOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionDialogView {
    pub current_tab: usize,
    pub submit_tab: bool,
    pub submit_action: usize,
    pub editing_other: bool,
    pub cursor: usize,
    pub answers: Vec<Option<String>>,
    pub unanswered: bool,
    pub selected_options: Vec<usize>,
    pub other_draft: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionResolvedAnswer {
    pub selected_labels: Vec<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionDialogReducer {
    pub questions: Vec<QuestionItem>,
    current_tab: usize,
    submit_action: usize,
    editing_other: bool,
    cursors: Vec<usize>,
    single: Vec<Option<usize>>,
    multi: Vec<BTreeSet<usize>>,
    other_drafts: Vec<String>,
    other_cursors: Vec<usize>,
    committed_other: Vec<Option<String>>,
    answers: Vec<Option<String>>,
    last_method: Option<QuestionAnswerMethod>,
    pub actions: Vec<QuestionDialogAction>,
}

impl QuestionDialogReducer {
    pub fn new(questions: Vec<QuestionItem>) -> Self {
        let count = questions.len();
        Self {
            questions,
            current_tab: 0,
            submit_action: 0,
            editing_other: false,
            cursors: vec![0; count],
            single: vec![None; count],
            multi: vec![BTreeSet::new(); count],
            other_drafts: vec![String::new(); count],
            other_cursors: vec![0; count],
            committed_other: vec![None; count],
            answers: vec![None; count],
            last_method: None,
            actions: Vec::new(),
        }
    }

    pub fn view(&self) -> QuestionDialogView {
        let selected_options = self.current_question().map_or_else(Vec::new, |index| {
            if self.questions[index].multi_select {
                self.multi[index].iter().copied().collect()
            } else {
                self.single[index].into_iter().collect()
            }
        });
        QuestionDialogView {
            current_tab: self.current_tab,
            submit_tab: self.is_submit_tab(),
            submit_action: self.submit_action,
            editing_other: self.editing_other && !self.is_submit_tab(),
            cursor: self
                .current_question()
                .map_or(0, |index| self.cursors[index]),
            answers: self.answers.clone(),
            unanswered: self.answers.iter().any(Option::is_none),
            selected_options,
            other_draft: self
                .current_question()
                .map(|index| self.other_drafts[index].clone()),
        }
    }

    pub fn resolved_answers(&self) -> Vec<Option<QuestionResolvedAnswer>> {
        self.questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                if question.multi_select {
                    let selected_labels = question
                        .options
                        .iter()
                        .enumerate()
                        .filter(|(option, _)| self.multi[index].contains(option))
                        .map(|(_, option)| option.label.clone())
                        .collect::<Vec<_>>();
                    let text = self.multi[index]
                        .contains(&question.options.len())
                        .then(|| self.committed_other[index].clone())
                        .flatten();
                    return (!selected_labels.is_empty() || text.is_some()).then_some(
                        QuestionResolvedAnswer {
                            selected_labels,
                            text,
                        },
                    );
                }
                self.single[index].map(|selection| {
                    if selection == question.options.len() {
                        QuestionResolvedAnswer {
                            selected_labels: Vec::new(),
                            text: self.committed_other[index].clone(),
                        }
                    } else {
                        QuestionResolvedAnswer {
                            selected_labels: question
                                .options
                                .get(selection)
                                .map(|option| vec![option.label.clone()])
                                .unwrap_or_default(),
                            text: None,
                        }
                    }
                })
            })
            .collect()
    }

    pub fn apply(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Escape) || control_char(&event, 'c') || control_char(&event, 'd')
        {
            self.actions.push(QuestionDialogAction::Answer {
                answers: Vec::new(),
                method: None,
            });
            return;
        }
        if control_char(&event, 'o') {
            self.actions.push(QuestionDialogAction::ToggleToolOutput);
            return;
        }
        if self.editing_other && !self.is_submit_tab() {
            self.apply_other(event);
        } else if self.is_submit_tab() {
            self.apply_submit(event);
        } else {
            self.apply_question(event);
        }
    }

    fn apply_question(&mut self, event: InputEvent) {
        let Some(question_index) = self.current_question() else {
            return;
        };
        let option_count = self.questions[question_index].options.len() + 1;
        if is_key(&event, KeyCode::Up) {
            self.move_cursor(-1);
        } else if is_key(&event, KeyCode::Down) {
            self.move_cursor(1);
        } else if is_key(&event, KeyCode::Left) {
            self.goto_tab_relative(-1);
        } else if is_key(&event, KeyCode::Right) || is_key(&event, KeyCode::Tab) {
            self.goto_tab_relative(1);
        } else if is_key(&event, KeyCode::Enter) {
            self.activate(self.cursors[question_index], QuestionAnswerMethod::Enter);
        } else if let Some(index) = printable_char(&event)
            .and_then(|character| character.to_digit(10))
            .and_then(|number| usize::try_from(number).ok())
            .and_then(|number| number.checked_sub(1))
            .filter(|index| *index < option_count && *index < 9)
        {
            self.cursors[question_index] = index;
            self.activate(index, QuestionAnswerMethod::NumberKey);
        } else if printable_char(&event) == Some(' ') && self.questions[question_index].multi_select
        {
            self.activate(self.cursors[question_index], QuestionAnswerMethod::Space);
        }
    }

    fn apply_other(&mut self, event: InputEvent) {
        let Some(question_index) = self.current_question() else {
            return;
        };
        if is_key(&event, KeyCode::Tab) {
            self.editing_other = false;
            self.goto_tab_relative(1);
        } else if is_key(&event, KeyCode::Up) {
            self.editing_other = false;
            self.move_cursor(-1);
        } else if is_key(&event, KeyCode::Down) {
            self.editing_other = false;
            self.move_cursor(1);
        } else if is_key(&event, KeyCode::Enter) {
            self.commit_other(QuestionAnswerMethod::Enter);
        } else if is_key(&event, KeyCode::Backspace) {
            let cursor = self.other_cursors[question_index];
            if cursor > 0 {
                let start = previous_boundary(&self.other_drafts[question_index], cursor);
                self.other_drafts[question_index].replace_range(start..cursor, "");
                self.other_cursors[question_index] = start;
            }
        } else if is_key(&event, KeyCode::Delete) {
            let cursor = self.other_cursors[question_index];
            let end = next_boundary(&self.other_drafts[question_index], cursor);
            self.other_drafts[question_index].replace_range(cursor..end, "");
        } else if is_key(&event, KeyCode::Left) {
            self.other_cursors[question_index] = previous_boundary(
                &self.other_drafts[question_index],
                self.other_cursors[question_index],
            );
        } else if is_key(&event, KeyCode::Right) {
            self.other_cursors[question_index] = next_boundary(
                &self.other_drafts[question_index],
                self.other_cursors[question_index],
            );
        } else if is_key(&event, KeyCode::Home) || control_char(&event, 'a') {
            self.other_cursors[question_index] = 0;
        } else if is_key(&event, KeyCode::End) || control_char(&event, 'e') {
            self.other_cursors[question_index] = self.other_drafts[question_index].len();
        } else if let Some(text) = printable(&event) {
            let cursor = self.other_cursors[question_index];
            self.other_drafts[question_index].insert_str(cursor, &text);
            self.other_cursors[question_index] += text.len();
        }
    }

    fn apply_submit(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Up) || is_key(&event, KeyCode::Down) {
            self.submit_action = (self.submit_action + 1) % 2;
        } else if is_key(&event, KeyCode::Left) {
            self.goto_tab_relative(-1);
        } else if is_key(&event, KeyCode::Right) || is_key(&event, KeyCode::Tab) {
            self.goto_tab_relative(1);
        } else if is_key(&event, KeyCode::Enter) {
            self.execute_submit(self.submit_action, QuestionAnswerMethod::Enter);
        } else if printable_char(&event) == Some('1') {
            self.submit_action = 0;
            self.execute_submit(0, QuestionAnswerMethod::NumberKey);
        } else if printable_char(&event) == Some('2') {
            self.submit_action = 1;
            self.execute_submit(1, QuestionAnswerMethod::NumberKey);
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let Some(question_index) = self.current_question() else {
            return;
        };
        let count = self.questions[question_index].options.len() + 1;
        if count == 0 {
            return;
        }
        self.cursors[question_index] =
            (self.cursors[question_index] as isize + delta).rem_euclid(count as isize) as usize;
    }

    fn goto_tab_relative(&mut self, delta: isize) {
        let count = self.questions.len() + 1;
        if count == 0 {
            return;
        }
        self.current_tab = (self.current_tab as isize + delta).rem_euclid(count as isize) as usize;
        self.editing_other = false;
        if self.is_submit_tab() {
            self.submit_action = 0;
        }
    }

    fn activate(&mut self, option: usize, method: QuestionAnswerMethod) {
        let Some(question_index) = self.current_question() else {
            return;
        };
        self.cursors[question_index] = option;
        self.editing_other = false;
        if option == self.questions[question_index].options.len() {
            self.editing_other = true;
            self.other_cursors[question_index] = self.other_drafts[question_index].len();
            return;
        }
        if self.questions[question_index].multi_select {
            if !self.multi[question_index].insert(option) {
                self.multi[question_index].remove(&option);
            }
            self.last_method = Some(method);
            self.update_answer(question_index);
        } else {
            self.single[question_index] = Some(option);
            self.committed_other[question_index] = None;
            self.last_method = Some(method);
            self.update_answer(question_index);
            self.advance_after_single(question_index);
        }
    }

    fn commit_other(&mut self, method: QuestionAnswerMethod) {
        let Some(question_index) = self.current_question() else {
            return;
        };
        let value = self.other_drafts[question_index].trim().to_owned();
        if value.is_empty() {
            return;
        }
        self.other_drafts[question_index] = value.clone();
        self.committed_other[question_index] = Some(value);
        let other = self.questions[question_index].options.len();
        if self.questions[question_index].multi_select {
            self.multi[question_index].insert(other);
        } else {
            self.single[question_index] = Some(other);
        }
        self.last_method = Some(method);
        self.update_answer(question_index);
        self.editing_other = false;
        if !self.questions[question_index].multi_select {
            self.advance_after_single(question_index);
        }
    }

    fn update_answer(&mut self, question_index: usize) {
        let question = &self.questions[question_index];
        if question.multi_select {
            let mut labels = Vec::new();
            for (index, option) in question.options.iter().enumerate() {
                if self.multi[question_index].contains(&index) && !option.label.is_empty() {
                    labels.push(option.label.clone());
                }
            }
            let other = question.options.len();
            if self.multi[question_index].contains(&other) {
                if let Some(value) = &self.committed_other[question_index] {
                    labels.push(value.clone());
                }
            }
            self.answers[question_index] = (!labels.is_empty()).then(|| labels.join(", "));
            return;
        }
        self.answers[question_index] = self.single[question_index].and_then(|selection| {
            if selection == question.options.len() {
                self.committed_other[question_index].clone()
            } else {
                question
                    .options
                    .get(selection)
                    .map(|option| option.label.clone())
                    .filter(|label| !label.is_empty())
            }
        });
    }

    fn advance_after_single(&mut self, question_index: usize) {
        self.current_tab = (question_index + 1..self.questions.len())
            .find(|index| self.answers[*index].is_none())
            .unwrap_or(self.questions.len());
        if self.is_submit_tab() {
            self.submit_action = 0;
        }
    }

    fn execute_submit(&mut self, action: usize, method: QuestionAnswerMethod) {
        if action == 1 {
            self.actions.push(QuestionDialogAction::Answer {
                answers: Vec::new(),
                method: None,
            });
        } else {
            self.actions.push(QuestionDialogAction::Answer {
                answers: self.answers.clone(),
                method: self.last_method.or(Some(method)),
            });
        }
    }

    fn current_question(&self) -> Option<usize> {
        (self.current_tab < self.questions.len()).then_some(self.current_tab)
    }

    fn is_submit_tab(&self) -> bool {
        self.current_tab == self.questions.len()
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
