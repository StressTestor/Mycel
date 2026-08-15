use std::collections::VecDeque;

use crate::terminal::graphemes;

const UNDO_LIMIT: usize = 256;
const KILL_RING_LIMIT: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    WordInsert,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorState {
    text: String,
    cursor: usize,
    undo: Vec<Snapshot>,
    kill_ring: VecDeque<String>,
    kill_ring_index: usize,
    history: Vec<HistoryEntry>,
    history_index: Option<usize>,
    history_draft: Option<Snapshot>,
    history_shell_only: Option<bool>,
    last_edit: Option<EditKind>,
    last_yank: Option<(usize, usize)>,
}

impl EditorState {
    pub fn with_history(history: Vec<HistoryEntry>) -> Self {
        Self {
            history,
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn kill_ring(&self) -> &VecDeque<String> {
        &self.kill_ring
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.push_snapshot();
        self.text = text.into();
        self.cursor = self.text.len();
        self.end_navigation();
        self.last_edit = Some(EditKind::Other);
    }

    pub fn replace_without_undo(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.end_navigation();
        self.last_edit = None;
    }

    pub fn replace_history_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.break_edit_chain();
    }

    pub fn clear(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.push_snapshot();
        self.text.clear();
        self.cursor = 0;
        self.end_navigation();
        self.last_edit = Some(EditKind::Other);
    }

    pub fn clear_after_submit(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.undo.clear();
        self.end_navigation();
        self.last_edit = None;
        self.last_yank = None;
    }

    pub fn insert_typed(&mut self, text: &str) {
        self.end_navigation();
        for cluster in graphemes(text) {
            let word = cluster.chars().all(is_word_character);
            if !word || self.last_edit != Some(EditKind::WordInsert) {
                self.push_snapshot();
            }
            self.text.insert_str(self.cursor, cluster);
            self.cursor += cluster.len();
            self.last_edit = Some(if word {
                EditKind::WordInsert
            } else {
                EditKind::Other
            });
            self.last_yank = None;
        }
    }

    pub fn insert_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_snapshot();
        self.end_navigation();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.last_edit = Some(EditKind::Other);
        self.last_yank = None;
    }

    pub fn insert_newline(&mut self) {
        self.insert_paste("\n");
    }

    pub fn move_left(&mut self) {
        self.cursor = previous_boundary(&self.text, self.cursor);
        self.break_edit_chain();
    }

    pub fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
        self.break_edit_chain();
    }

    pub fn move_home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.break_edit_chain();
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        self.break_edit_chain();
    }

    pub fn move_vertical(&mut self, delta: isize) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = self.cursor - line_start;
        let target_start = if delta < 0 {
            if line_start == 0 {
                return;
            }
            self.text[..line_start - 1]
                .rfind('\n')
                .map_or(0, |index| index + 1)
        } else {
            let Some(next) = self.text[self.cursor..].find('\n') else {
                return;
            };
            self.cursor + next + 1
        };
        let target_end = self.text[target_start..]
            .find('\n')
            .map_or(self.text.len(), |index| target_start + index);
        let mut target = target_start;
        let mut used = 0usize;
        for cluster in graphemes(&self.text[target_start..target_end]) {
            if used + cluster.len() > column {
                break;
            }
            used += cluster.len();
            target += cluster.len();
        }
        self.cursor = target;
        self.break_edit_chain();
    }

    pub fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_snapshot();
        let start = previous_boundary(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.last_edit = Some(EditKind::Other);
        self.last_yank = None;
    }

    pub fn delete_forward(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        self.push_snapshot();
        let end = next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..end, "");
        self.last_edit = Some(EditKind::Other);
        self.last_yank = None;
    }

    pub fn kill_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = word_start_backward(&self.text, self.cursor);
        self.kill_range(start, self.cursor);
    }

    pub fn kill_word_forward(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        let end = word_end_forward(&self.text, self.cursor);
        self.kill_range(self.cursor, end);
    }

    pub fn kill_to_line_start(&mut self) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        if start == self.cursor {
            return;
        }
        self.kill_range(start, self.cursor);
    }

    pub fn kill_to_line_end(&mut self) {
        let mut end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        if end == self.cursor && end < self.text.len() {
            end += 1;
        }
        if end == self.cursor {
            return;
        }
        self.kill_range(self.cursor, end);
    }

    pub fn yank(&mut self) {
        let Some(value) = self.kill_ring.front().cloned() else {
            return;
        };
        self.push_snapshot();
        let start = self.cursor;
        self.text.insert_str(self.cursor, &value);
        self.cursor += value.len();
        self.last_yank = Some((start, self.cursor));
        self.kill_ring_index = 0;
        self.last_edit = Some(EditKind::Other);
    }

    pub fn yank_pop(&mut self) {
        let Some((start, end)) = self.last_yank else {
            return;
        };
        if self.kill_ring.len() < 2 {
            return;
        }
        self.kill_ring_index = (self.kill_ring_index + 1) % self.kill_ring.len();
        let replacement = self.kill_ring[self.kill_ring_index].clone();
        self.text.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        self.last_yank = Some((start, self.cursor));
    }

    pub fn undo(&mut self) {
        let Some(snapshot) = self.undo.pop() else {
            return;
        };
        self.text = snapshot.text;
        self.cursor = snapshot.cursor;
        self.end_navigation();
        self.last_edit = None;
        self.last_yank = None;
    }

    pub fn add_history(&mut self, text: String) {
        if text.is_empty() || self.history.last().is_some_and(|entry| entry.text == text) {
            return;
        }
        self.history.push(HistoryEntry { text });
        if self.history.len() > 100 {
            self.history.remove(0);
        }
    }

    pub fn history_up(&mut self, shell_only: bool) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let filter = self.history_shell_only.unwrap_or(shell_only);
        let mut candidate = self.history_index.unwrap_or(self.history.len());
        let index = loop {
            if candidate == 0 {
                return false;
            }
            candidate -= 1;
            if !filter || self.history[candidate].text.starts_with('!') {
                break candidate;
            }
        };
        if self.history_index.is_none() {
            self.history_draft = Some(Snapshot {
                text: self.text.clone(),
                cursor: self.cursor,
            });
            self.history_shell_only = Some(shell_only);
        }
        self.history_index = Some(index);
        self.text = self.history[index].text.clone();
        self.cursor = 0;
        self.break_edit_chain();
        true
    }

    pub fn history_down(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        let filter = self.history_shell_only.unwrap_or(false);
        let next = (index + 1..self.history.len())
            .find(|candidate| !filter || self.history[*candidate].text.starts_with('!'));
        if let Some(next) = next {
            self.history_index = Some(next);
            self.text = self.history[next].text.clone();
            self.cursor = self.text.len();
        } else {
            self.history_index = None;
            self.history_shell_only = None;
            if let Some(draft) = self.history_draft.take() {
                self.text = draft.text;
                self.cursor = draft.cursor;
            }
        }
        self.break_edit_chain();
        true
    }

    pub const fn is_history_browsing(&self) -> bool {
        self.history_index.is_some()
    }

    fn kill_range(&mut self, start: usize, end: usize) {
        self.push_snapshot();
        let killed = self.text[start..end].to_owned();
        self.text.replace_range(start..end, "");
        self.cursor = start;
        if !killed.is_empty() {
            self.kill_ring.push_front(killed);
            self.kill_ring.truncate(KILL_RING_LIMIT);
        }
        self.kill_ring_index = 0;
        self.last_edit = Some(EditKind::Other);
        self.last_yank = None;
    }

    fn push_snapshot(&mut self) {
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    fn end_navigation(&mut self) {
        self.history_index = None;
        self.history_draft = None;
        self.history_shell_only = None;
    }

    fn break_edit_chain(&mut self) {
        self.last_edit = None;
        self.last_yank = None;
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

fn word_start_backward(text: &str, cursor: usize) -> usize {
    let prefix = &text[..cursor];
    let clusters: Vec<&str> = graphemes(prefix).collect();
    let mut index = clusters.len();
    while index > 0 && clusters[index - 1].chars().all(char::is_whitespace) {
        index -= 1;
    }
    if index == 0 {
        return 0;
    }
    let word = clusters[index - 1].chars().all(is_word_character);
    while index > 0
        && !clusters[index - 1].chars().all(char::is_whitespace)
        && clusters[index - 1].chars().all(is_word_character) == word
    {
        index -= 1;
    }
    clusters[..index].iter().map(|cluster| cluster.len()).sum()
}

fn word_end_forward(text: &str, cursor: usize) -> usize {
    let suffix = &text[cursor..];
    let clusters: Vec<&str> = graphemes(suffix).collect();
    let mut index = 0usize;
    while index < clusters.len() && clusters[index].chars().all(char::is_whitespace) {
        index += 1;
    }
    if index == clusters.len() {
        return text.len();
    }
    let word = clusters[index].chars().all(is_word_character);
    while index < clusters.len()
        && !clusters[index].chars().all(char::is_whitespace)
        && clusters[index].chars().all(is_word_character) == word
    {
        index += 1;
    }
    cursor
        + clusters[..index]
            .iter()
            .map(|cluster| cluster.len())
            .sum::<usize>()
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}
