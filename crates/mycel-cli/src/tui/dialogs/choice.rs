use std::collections::BTreeMap;

use crate::terminal::{graphemes, InputEvent, KeyCode};

use super::{alt_char, fuzzy_score, is_key, printable, printable_char};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceScope {
    Persistent,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceDialogAction {
    Select { value: String, scope: ChoiceScope },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchableChoiceView {
    pub query: String,
    pub selected: Option<String>,
    pub filtered: Vec<String>,
    pub page: usize,
    pub page_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchableChoiceReducer {
    pub options: Vec<ChoiceOption>,
    pub searchable: bool,
    pub session_only_enabled: bool,
    pub page_size: usize,
    query: String,
    cursor: usize,
    pub actions: Vec<ChoiceDialogAction>,
}

impl SearchableChoiceReducer {
    pub fn new(
        options: Vec<ChoiceOption>,
        current_value: Option<&str>,
        searchable: bool,
        session_only_enabled: bool,
        page_size: usize,
    ) -> Self {
        let cursor = current_value
            .and_then(|value| options.iter().position(|option| option.value == value))
            .unwrap_or(0);
        Self {
            options,
            searchable,
            session_only_enabled,
            page_size: page_size.max(1),
            query: String::new(),
            cursor,
            actions: Vec::new(),
        }
    }

    pub fn view(&self) -> SearchableChoiceView {
        let filtered = self.filtered_indices();
        let selected = filtered
            .get(self.cursor.min(filtered.len().saturating_sub(1)))
            .map(|index| self.options[*index].value.clone());
        let page_count = filtered.len().max(1).div_ceil(self.page_size);
        SearchableChoiceView {
            query: self.query.clone(),
            selected,
            filtered: filtered
                .iter()
                .map(|index| self.options[*index].value.clone())
                .collect(),
            page: self.cursor / self.page_size,
            page_count,
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Escape) {
            if self.query.is_empty() {
                self.actions.push(ChoiceDialogAction::Cancel);
            } else {
                self.query.clear();
                self.cursor = 0;
            }
            return;
        }
        if alt_char(&event, 's') && self.session_only_enabled {
            self.select(ChoiceScope::Session);
            return;
        }
        if is_key(&event, KeyCode::Left) {
            self.page_by(false);
            return;
        }
        if is_key(&event, KeyCode::Right) {
            self.page_by(true);
            return;
        }
        if is_key(&event, KeyCode::Enter) {
            self.select(ChoiceScope::Persistent);
            return;
        }
        if printable_char(&event) == Some(' ') && !self.searchable {
            self.select(ChoiceScope::Persistent);
            return;
        }
        self.apply_shared_list_key(event);
    }

    fn apply_shared_list_key(&mut self, event: InputEvent) -> bool {
        let length = self.filtered_indices().len();
        if is_key(&event, KeyCode::Up) {
            self.cursor = self.cursor.saturating_sub(1);
            return true;
        }
        if is_key(&event, KeyCode::Down) {
            self.cursor = (self.cursor + 1).min(length.saturating_sub(1));
            return true;
        }
        if is_key(&event, KeyCode::PageUp) {
            self.cursor = self.cursor.saturating_sub(self.page_size);
            return true;
        }
        if is_key(&event, KeyCode::PageDown) {
            self.cursor = (self.cursor + self.page_size).min(length.saturating_sub(1));
            return true;
        }
        if !self.searchable {
            return false;
        }
        if is_key(&event, KeyCode::Backspace) {
            self.query = graphemes(&self.query)
                .collect::<Vec<_>>()
                .split_last()
                .map_or_else(String::new, |(_, rest)| rest.concat());
            self.cursor = 0;
            return true;
        }
        if let Some(text) = printable(&event) {
            if text.chars().all(|character| !character.is_control()) {
                self.query.push_str(&text);
                self.cursor = 0;
                return true;
            }
        }
        false
    }

    fn page_by(&mut self, forward: bool) {
        let length = self.filtered_indices().len();
        self.cursor = if forward {
            (self.cursor + self.page_size).min(length.saturating_sub(1))
        } else {
            self.cursor.saturating_sub(self.page_size)
        };
    }

    fn select(&mut self, scope: ChoiceScope) {
        let Some(index) = self.filtered_indices().get(self.cursor).copied() else {
            return;
        };
        self.actions.push(ChoiceDialogAction::Select {
            value: self.options[index].value.clone(),
            scope,
        });
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let mut matches: Vec<(usize, i64)> = self
            .options
            .iter()
            .enumerate()
            .filter_map(|(index, option)| {
                fuzzy_score(
                    &self.query,
                    &format!(
                        "{} {}",
                        option.label,
                        option.description.as_deref().unwrap_or("")
                    ),
                )
                .map(|score| (index, score))
            })
            .collect();
        matches.sort_by_key(|(_, score)| *score);
        matches.into_iter().map(|(index, _)| index).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub alias: String,
    pub name: String,
    pub provider: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub thinking_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDialogAction {
    Select {
        alias: String,
        effort: String,
        scope: ChoiceScope,
    },
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectorView {
    pub query: String,
    pub selected: Option<String>,
    pub effort: Option<String>,
    pub filtered: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectorReducer {
    pub models: Vec<ModelChoice>,
    pub searchable: bool,
    pub session_only_enabled: bool,
    pub page_size: usize,
    pub current_model: String,
    pub current_effort: String,
    query: String,
    cursor: usize,
    overrides: BTreeMap<String, String>,
    pub actions: Vec<ModelDialogAction>,
}

impl ModelSelectorReducer {
    pub fn new(
        models: Vec<ModelChoice>,
        current_model: String,
        current_effort: String,
        searchable: bool,
        session_only_enabled: bool,
        page_size: usize,
    ) -> Self {
        let cursor = models
            .iter()
            .position(|model| model.alias == current_model)
            .unwrap_or(0);
        Self {
            models,
            searchable,
            session_only_enabled,
            page_size: page_size.max(1),
            current_model,
            current_effort,
            query: String::new(),
            cursor,
            overrides: BTreeMap::new(),
            actions: Vec::new(),
        }
    }

    pub fn view(&self) -> ModelSelectorView {
        let filtered = self.filtered_indices();
        let selected = filtered.get(self.cursor).copied();
        ModelSelectorView {
            query: self.query.clone(),
            selected: selected.map(|index| self.models[index].alias.clone()),
            effort: selected.map(|index| self.effective_effort(index)),
            filtered: filtered
                .iter()
                .map(|index| self.models[*index].alias.clone())
                .collect(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Escape) {
            if self.query.is_empty() {
                self.actions.push(ModelDialogAction::Cancel);
            } else {
                self.query.clear();
                self.cursor = 0;
            }
            return;
        }
        if self.apply_list_key(&event) {
            return;
        }
        if is_key(&event, KeyCode::Left) || is_key(&event, KeyCode::Right) {
            self.move_effort(is_key(&event, KeyCode::Right));
            return;
        }
        if is_key(&event, KeyCode::Enter) {
            self.commit(ChoiceScope::Persistent);
        } else if alt_char(&event, 's') && self.session_only_enabled {
            self.commit(ChoiceScope::Session);
        }
    }

    fn apply_list_key(&mut self, event: &InputEvent) -> bool {
        let length = self.filtered_indices().len();
        if is_key(event, KeyCode::Up) {
            self.cursor = self.cursor.saturating_sub(1);
            return true;
        }
        if is_key(event, KeyCode::Down) {
            self.cursor = (self.cursor + 1).min(length.saturating_sub(1));
            return true;
        }
        if is_key(event, KeyCode::PageUp) {
            self.cursor = self.cursor.saturating_sub(self.page_size);
            return true;
        }
        if is_key(event, KeyCode::PageDown) {
            self.cursor = (self.cursor + self.page_size).min(length.saturating_sub(1));
            return true;
        }
        if !self.searchable {
            return false;
        }
        if is_key(event, KeyCode::Backspace) {
            self.query.pop();
            self.cursor = 0;
            return true;
        }
        if let Some(text) = printable(event) {
            if text.chars().all(|character| !character.is_control()) {
                self.query.push_str(&text);
                self.cursor = 0;
                return true;
            }
        }
        false
    }

    fn move_effort(&mut self, right: bool) {
        let Some(model_index) = self.filtered_indices().get(self.cursor).copied() else {
            return;
        };
        let segments = self.segments(model_index);
        if segments.len() <= 1 {
            return;
        }
        let current = self.effective_effort(model_index);
        let index = segments
            .iter()
            .position(|effort| effort == &current)
            .unwrap_or(0);
        let next = if segments.len() == 2 {
            usize::from(index == 0)
        } else if right {
            (index + 1).min(segments.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        if next != index {
            self.overrides.insert(
                self.models[model_index].alias.clone(),
                segments[next].clone(),
            );
        }
    }

    fn commit(&mut self, scope: ChoiceScope) {
        let Some(index) = self.filtered_indices().get(self.cursor).copied() else {
            return;
        };
        self.actions.push(ModelDialogAction::Select {
            alias: self.models[index].alias.clone(),
            effort: self.effective_effort(index),
            scope,
        });
    }

    fn effective_effort(&self, index: usize) -> String {
        let model = &self.models[index];
        let segments = self.segments(index);
        let draft = self
            .overrides
            .get(&model.alias)
            .cloned()
            .or_else(|| (model.alias == self.current_model).then(|| self.current_effort.clone()))
            .or_else(|| model.default_effort.clone())
            .unwrap_or_else(|| segments[segments.len() / 2].clone());
        if segments.contains(&draft) {
            draft
        } else {
            segments[0].clone()
        }
    }

    fn segments(&self, index: usize) -> Vec<String> {
        let model = &self.models[index];
        if !model.efforts.is_empty() {
            model.efforts.clone()
        } else if model.thinking_supported {
            vec!["off".to_owned(), "on".to_owned()]
        } else {
            vec!["off".to_owned()]
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let mut matches: Vec<(usize, i64)> = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| {
                fuzzy_score(
                    &self.query,
                    &format!("{} {} {}", model.alias, model.name, model.provider),
                )
                .map(|score| (index, score))
            })
            .collect();
        matches.sort_by_key(|(_, score)| *score);
        matches.into_iter().map(|(index, _)| index).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortSelectorReducer {
    pub efforts: Vec<String>,
    pub active: usize,
    pub session_only_enabled: bool,
    pub actions: Vec<ChoiceDialogAction>,
}

impl EffortSelectorReducer {
    pub fn new(efforts: Vec<String>, current: &str, session_only_enabled: bool) -> Self {
        let active = efforts
            .iter()
            .position(|effort| effort == current)
            .unwrap_or(0);
        Self {
            efforts,
            active,
            session_only_enabled,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        if is_key(&event, KeyCode::Escape) {
            self.actions.push(ChoiceDialogAction::Cancel);
        } else if is_key(&event, KeyCode::Left) {
            self.active = self.active.saturating_sub(1);
        } else if is_key(&event, KeyCode::Right) {
            self.active = (self.active + 1).min(self.efforts.len().saturating_sub(1));
        } else if is_key(&event, KeyCode::Enter) {
            self.select(ChoiceScope::Persistent);
        } else if alt_char(&event, 's') && self.session_only_enabled {
            self.select(ChoiceScope::Session);
        }
    }

    fn select(&mut self, scope: ChoiceScope) {
        if let Some(value) = self.efforts.get(self.active) {
            self.actions.push(ChoiceDialogAction::Select {
                value: value.clone(),
                scope,
            });
        }
    }
}
