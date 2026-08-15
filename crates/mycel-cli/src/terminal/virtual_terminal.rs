use super::unicode::{grapheme_width, graphemes};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Default)]
struct Cell {
    text: String,
    continuation: bool,
}

/// Deterministic ANSI viewport used by parity goldens. It implements the
/// cursor/erase subset emitted by [`super::DifferentialRenderer`].
#[derive(Debug, Clone)]
pub struct VirtualTerminal {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
    cursor: Cursor,
    saved_cursor: Cursor,
    wrap_pending: bool,
    pending: Vec<u8>,
}

impl VirtualTerminal {
    pub fn new(width: usize, height: usize) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            wrap_pending: false,
            pending: Vec::new(),
        }
    }

    pub const fn width(&self) -> usize {
        self.width
    }

    pub const fn height(&self) -> usize {
        self.height
    }

    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut index = 0usize;
        while index < input.len() {
            match input[index] {
                0x1b => {
                    let consumed = self.consume_escape(&input[index..]);
                    if consumed == 0 {
                        break;
                    }
                    index += consumed;
                }
                b'\r' => {
                    self.cursor.column = 0;
                    self.wrap_pending = false;
                    index += 1;
                }
                b'\n' => {
                    self.line_feed();
                    self.wrap_pending = false;
                    index += 1;
                }
                0x08 => {
                    self.cursor.column = self.cursor.column.saturating_sub(1);
                    self.wrap_pending = false;
                    index += 1;
                }
                b'\t' => {
                    self.cursor.column = ((self.cursor.column / 8) + 1) * 8;
                    if self.cursor.column >= self.width {
                        self.cursor.column = self.width - 1;
                    }
                    self.wrap_pending = false;
                    index += 1;
                }
                0x00..=0x1f | 0x7f => index += 1,
                _ => {
                    let end = input[index..]
                        .iter()
                        .position(|byte| *byte == 0x1b || byte.is_ascii_control())
                        .map_or(input.len(), |offset| index + offset);
                    match std::str::from_utf8(&input[index..end]) {
                        Ok(text) => {
                            self.write_text(text);
                            index = end;
                        }
                        Err(error) if error.valid_up_to() > 0 => {
                            let valid_end = index + error.valid_up_to();
                            let text = std::str::from_utf8(&input[index..valid_end])
                                .expect("valid UTF-8 prefix");
                            self.write_text(text);
                            index = valid_end;
                        }
                        Err(error) if error.error_len().is_none() => break,
                        Err(error) => {
                            index += error.error_len().expect("invalid UTF-8 length");
                        }
                    }
                }
            }
        }
        self.pending.extend_from_slice(&input[index..]);
    }

    pub fn lines(&self) -> Vec<String> {
        (0..self.height)
            .map(|row| {
                let mut line = String::new();
                for column in 0..self.width {
                    let cell = &self.cells[self.offset(row, column)];
                    if cell.continuation {
                        continue;
                    }
                    if cell.text.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(&cell.text);
                    }
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    fn write_text(&mut self, text: &str) {
        for cluster in graphemes(text) {
            self.write_grapheme(cluster);
        }
    }

    fn write_grapheme(&mut self, cluster: &str) {
        let width = grapheme_width(cluster);
        if width == 0 || cluster.starts_with('\u{200d}') {
            self.append_to_previous(cluster);
            return;
        }
        if self.wrap_pending
            || (self.width > 1 && width == 2 && self.cursor.column + 1 >= self.width)
        {
            self.cursor.column = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        self.clear_overlapping(self.cursor.row, self.cursor.column);
        let index = self.offset(self.cursor.row, self.cursor.column);
        self.cells[index] = Cell {
            text: cluster.to_owned(),
            continuation: false,
        };
        if width == 2 && self.cursor.column + 1 < self.width {
            let continuation = self.offset(self.cursor.row, self.cursor.column + 1);
            self.cells[continuation] = Cell {
                text: String::new(),
                continuation: true,
            };
        }
        if self.cursor.column + width >= self.width {
            self.cursor.column = self.width - 1;
            self.wrap_pending = true;
        } else {
            self.cursor.column += width;
        }
    }

    fn append_to_previous(&mut self, cluster: &str) {
        if !self.wrap_pending && self.cursor.column == 0 {
            return;
        }
        let mut column = if self.wrap_pending {
            self.cursor.column
        } else {
            self.cursor.column - 1
        };
        if self.cells[self.offset(self.cursor.row, column)].continuation && column > 0 {
            column -= 1;
        }
        let index = self.offset(self.cursor.row, column);
        if !self.cells[index].text.is_empty() {
            self.cells[index].text.push_str(cluster);
        }
    }

    fn consume_escape(&mut self, bytes: &[u8]) -> usize {
        if bytes.len() < 2 {
            return 0;
        }
        match bytes[1] {
            b'[' => {
                let Some(end) = bytes[2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .map(|offset| offset + 2)
                else {
                    return 0;
                };
                self.apply_csi(&bytes[2..end], bytes[end]);
                end + 1
            }
            b']' => string_sequence_length(bytes, true),
            b'P' | b'_' | b'^' => string_sequence_length(bytes, false),
            b'7' => {
                self.saved_cursor = self.cursor;
                2
            }
            b'8' => {
                self.cursor = self.saved_cursor;
                self.clamp_cursor();
                self.wrap_pending = false;
                2
            }
            _ => 2,
        }
    }

    fn apply_csi(&mut self, parameters: &[u8], final_byte: u8) {
        self.wrap_pending = false;
        let parameters = std::str::from_utf8(parameters).unwrap_or_default();
        let values: Vec<usize> = parameters
            .trim_start_matches('?')
            .split(';')
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        let value = |index: usize, default: usize| {
            values
                .get(index)
                .copied()
                .filter(|value| *value != 0)
                .unwrap_or(default)
        };
        match final_byte {
            b'A' => self.cursor.row = self.cursor.row.saturating_sub(value(0, 1)),
            b'B' => self.cursor.row = (self.cursor.row + value(0, 1)).min(self.height - 1),
            b'C' => self.cursor.column = (self.cursor.column + value(0, 1)).min(self.width - 1),
            b'D' => self.cursor.column = self.cursor.column.saturating_sub(value(0, 1)),
            b'G' => self.cursor.column = value(0, 1).saturating_sub(1).min(self.width - 1),
            b'd' => self.cursor.row = value(0, 1).saturating_sub(1).min(self.height - 1),
            b'H' | b'f' => {
                self.cursor.row = value(0, 1).saturating_sub(1).min(self.height - 1);
                self.cursor.column = value(1, 1).saturating_sub(1).min(self.width - 1);
            }
            b'J' => self.erase_display(values.first().copied().unwrap_or(0)),
            b'K' => self.erase_line(values.first().copied().unwrap_or(0)),
            b's' => self.saved_cursor = self.cursor,
            b'u' => {
                self.cursor = self.saved_cursor;
                self.clamp_cursor();
            }
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 | 3 => self.cells.fill(Cell::default()),
            1 => {
                let end = self.offset(self.cursor.row, self.cursor.column);
                self.cells[..=end].fill(Cell::default());
            }
            _ => {
                let start = self.offset(self.cursor.row, self.cursor.column);
                self.cells[start..].fill(Cell::default());
            }
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let row_start = self.offset(self.cursor.row, 0);
        let row_end = row_start + self.width;
        match mode {
            1 => self.cells[row_start..=row_start + self.cursor.column].fill(Cell::default()),
            2 => self.cells[row_start..row_end].fill(Cell::default()),
            _ => self.cells[row_start + self.cursor.column..row_end].fill(Cell::default()),
        }
    }

    fn clear_overlapping(&mut self, row: usize, column: usize) {
        let index = self.offset(row, column);
        if self.cells[index].continuation && column > 0 {
            let previous = self.offset(row, column - 1);
            self.cells[previous] = Cell::default();
        }
        if !self.cells[index].text.is_empty() && column + 1 < self.width {
            let next = self.offset(row, column + 1);
            if self.cells[next].continuation {
                self.cells[next] = Cell::default();
            }
        }
        self.cells[index] = Cell::default();
    }

    fn line_feed(&mut self) {
        if self.cursor.row + 1 < self.height {
            self.cursor.row += 1;
            return;
        }
        self.cells.rotate_left(self.width);
        let start = self.cells.len() - self.width;
        self.cells[start..].fill(Cell::default());
        self.cursor.row = self.height - 1;
    }

    fn clamp_cursor(&mut self) {
        self.cursor.row = self.cursor.row.min(self.height - 1);
        self.cursor.column = self.cursor.column.min(self.width - 1);
    }

    const fn offset(&self, row: usize, column: usize) -> usize {
        row * self.width + column
    }
}

fn string_sequence_length(bytes: &[u8], allow_bel: bool) -> usize {
    let mut index = 2usize;
    while index < bytes.len() {
        if allow_bel && bytes[index] == 0x07 {
            return index + 1;
        }
        if bytes[index] == 0x1b && bytes.get(index + 1).copied() == Some(b'\\') {
            return index + 2;
        }
        index += 1;
    }
    0
}
