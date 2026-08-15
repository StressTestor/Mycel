use std::io;

use super::unicode::truncate_to_width;

pub trait TerminalSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryTerminalSink {
    pub bytes: Vec<u8>,
}

impl TerminalSink for MemoryTerminalSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

/// Line-level differential renderer with injectable output. It emits only rows
/// whose logical content changed and clears rows removed by a shrink.
#[derive(Debug, Clone, Default)]
pub struct DifferentialRenderer {
    previous: Vec<String>,
}

impl DifferentialRenderer {
    pub fn render(
        &mut self,
        lines: &[String],
        width: usize,
        sink: &mut dyn TerminalSink,
    ) -> io::Result<()> {
        let normalized: Vec<String> = lines
            .iter()
            .map(|line| truncate_to_width(line, width, ""))
            .collect();
        if self.previous.is_empty() && !normalized.is_empty() {
            sink.write(b"\x1b[2J")?;
        }
        let row_count = self.previous.len().max(normalized.len());
        for row in 0..row_count {
            let previous = self.previous.get(row).map(String::as_str).unwrap_or("");
            let next = normalized.get(row).map(String::as_str).unwrap_or("");
            if previous == next {
                continue;
            }
            sink.write(format!("\x1b[{};1H\x1b[2K", row + 1).as_bytes())?;
            sink.write(next.as_bytes())?;
        }
        self.previous = normalized;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.previous.clear();
    }
}
