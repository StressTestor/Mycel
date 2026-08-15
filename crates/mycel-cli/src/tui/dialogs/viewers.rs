use crate::terminal::{InputEvent, KeyCode};

use super::{control_char, is_key, printable_char};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerKind {
    Help,
    Output,
    ApprovalPreview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerAction {
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollViewerReducer {
    pub kind: ViewerKind,
    pub line_count: usize,
    pub visible_rows: usize,
    pub scroll_top: usize,
    pub actions: Vec<ViewerAction>,
}

impl ScrollViewerReducer {
    pub fn new(kind: ViewerKind, line_count: usize, visible_rows: usize) -> Self {
        Self {
            kind,
            line_count,
            visible_rows: visible_rows.max(1),
            scroll_top: 0,
            actions: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) {
        let character = printable_char(&event);
        let close = is_key(&event, KeyCode::Escape)
            || matches!(character, Some('q' | 'Q'))
            || (self.kind == ViewerKind::Help && is_key(&event, KeyCode::Enter))
            || (self.kind == ViewerKind::ApprovalPreview && control_char(&event, 'e'));
        if close {
            self.actions.push(ViewerAction::Close);
            return;
        }

        if is_key(&event, KeyCode::Up) || character == Some('k') {
            self.scroll_by(-1);
        } else if is_key(&event, KeyCode::Down) || character == Some('j') {
            self.scroll_by(1);
        } else if self.kind == ViewerKind::Help && is_key(&event, KeyCode::PageUp) {
            self.scroll_by(-10);
        } else if self.kind == ViewerKind::Help && is_key(&event, KeyCode::PageDown) {
            self.scroll_by(10);
        } else if self.kind != ViewerKind::Help
            && (is_key(&event, KeyCode::PageUp)
                || control_char(&event, 'b')
                || (self.kind == ViewerKind::Output && control_char(&event, 'u'))
                || character == Some(' '))
        {
            self.scroll_by(-self.page_delta());
        } else if self.kind != ViewerKind::Help
            && (is_key(&event, KeyCode::PageDown)
                || control_char(&event, 'f')
                || (self.kind == ViewerKind::Output && control_char(&event, 'd')))
        {
            self.scroll_by(self.page_delta());
        } else if self.kind != ViewerKind::Help
            && (is_key(&event, KeyCode::Home) || character == Some('g'))
        {
            self.scroll_top = 0;
        } else if self.kind != ViewerKind::Help
            && (is_key(&event, KeyCode::End) || character == Some('G'))
        {
            self.scroll_top = self.max_scroll();
        }
    }

    pub fn resize(&mut self, visible_rows: usize) {
        self.visible_rows = visible_rows.max(1);
        self.scroll_top = self.scroll_top.min(self.max_scroll());
    }

    pub fn replace_lines(&mut self, line_count: usize, follow_tail: bool) {
        let was_at_bottom = self.scroll_top == self.max_scroll();
        self.line_count = line_count;
        self.scroll_top = if follow_tail && was_at_bottom {
            self.max_scroll()
        } else {
            self.scroll_top.min(self.max_scroll())
        };
    }

    fn page_delta(&self) -> isize {
        self.visible_rows.saturating_sub(1).max(1) as isize
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll_top =
            (self.scroll_top as isize + delta).clamp(0, self.max_scroll() as isize) as usize;
    }

    fn max_scroll(&self) -> usize {
        self.line_count.saturating_sub(self.visible_rows)
    }
}
