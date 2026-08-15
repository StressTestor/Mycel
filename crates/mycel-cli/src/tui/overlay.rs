use crate::terminal::{grapheme_width, graphemes};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub lines: Vec<String>,
    pub captures_focus: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusStack {
    stack: Vec<String>,
}

impl FocusStack {
    pub fn push(&mut self, id: impl Into<String>) {
        let id = id.into();
        self.stack.retain(|existing| existing != &id);
        self.stack.push(id);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let length = self.stack.len();
        self.stack.retain(|existing| existing != id);
        self.stack.len() != length
    }

    pub fn current(&self) -> Option<&str> {
        self.stack.last().map(String::as_str)
    }

    pub fn clear(&mut self) {
        self.stack.clear();
    }
}

pub fn compose_overlay(
    base: &[String],
    viewport_width: usize,
    viewport_height: usize,
    overlay: &Overlay,
) -> Vec<String> {
    let mut rows: Vec<Vec<Cell>> = (0..viewport_height)
        .map(|row| {
            base.get(row).map_or_else(
                || vec![Cell::default(); viewport_width],
                |line| cells(line, viewport_width),
            )
        })
        .collect();

    let max_rows = overlay.height.min(overlay.lines.len());
    for source_row in 0..max_rows {
        let target_row = overlay.y + source_row;
        if target_row >= viewport_height {
            break;
        }
        let source = cells(&overlay.lines[source_row], overlay.width);
        for (source_column, cell) in source.into_iter().enumerate() {
            let target_column = overlay.x + source_column;
            if target_column >= viewport_width || source_column >= overlay.width {
                break;
            }
            clear_wide_overlap(&mut rows[target_row], target_column);
            rows[target_row][target_column] = cell;
        }
    }

    rows.into_iter().map(render_cells).collect()
}

#[derive(Debug, Clone, Default)]
struct Cell {
    text: String,
    continuation: bool,
}

fn cells(line: &str, width: usize) -> Vec<Cell> {
    let mut cells = vec![Cell::default(); width];
    let mut column = 0usize;
    for cluster in graphemes(line) {
        let cluster_width = grapheme_width(cluster);
        if cluster_width == 0 {
            if column > 0 {
                cells[column - 1].text.push_str(cluster);
            }
            continue;
        }
        if column + cluster_width > width {
            break;
        }
        cells[column].text = cluster.to_owned();
        if cluster_width == 2 {
            cells[column + 1].continuation = true;
        }
        column += cluster_width;
    }
    cells
}

fn clear_wide_overlap(row: &mut [Cell], column: usize) {
    if row[column].continuation && column > 0 {
        row[column - 1] = Cell::default();
    }
    if !row[column].text.is_empty() && column + 1 < row.len() && row[column + 1].continuation {
        row[column + 1] = Cell::default();
    }
    row[column] = Cell::default();
}

fn render_cells(cells: Vec<Cell>) -> String {
    let mut output = String::new();
    for cell in cells {
        if cell.continuation {
            continue;
        }
        if cell.text.is_empty() {
            output.push(' ');
        } else {
            output.push_str(&cell.text);
        }
    }
    output.trim_end().to_owned()
}
