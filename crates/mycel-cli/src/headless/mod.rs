mod reducer;
mod render;

use std::{error::Error, fmt};

pub use reducer::{HeadlessEvent, HeadlessEventReducer, HeadlessRecord, RetryMetadata, ToolCall};
pub use render::{RenderedOutput, StreamJsonRenderer, TextRenderer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessError {
    message: String,
}

impl HeadlessError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HeadlessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HeadlessError {}

pub trait HeadlessEventSink {
    fn emit(&mut self, event: HeadlessEvent) -> Result<(), HeadlessError>;
}

pub trait HeadlessRenderer {
    fn render(&mut self, record: HeadlessRecord) -> Result<RenderedOutput, HeadlessError>;
    fn finish(&mut self) -> Result<RenderedOutput, HeadlessError> {
        Ok(RenderedOutput::default())
    }
}

pub struct HeadlessPipeline {
    reducer: HeadlessEventReducer,
    renderer: Box<dyn HeadlessRenderer>,
    output: RenderedOutput,
    finished: bool,
}

impl HeadlessPipeline {
    pub fn new(renderer: Box<dyn HeadlessRenderer>) -> Self {
        Self {
            reducer: HeadlessEventReducer::default(),
            renderer,
            output: RenderedOutput::default(),
            finished: false,
        }
    }

    pub fn emit_resume_hint(&mut self, session_id: &str) -> Result<(), HeadlessError> {
        self.render_records(vec![HeadlessRecord::ResumeHint {
            session_id: session_id.to_owned(),
        }])
    }

    pub fn finish(mut self) -> Result<RenderedOutput, HeadlessError> {
        if !self.finished {
            let records = self.reducer.finish();
            self.render_records(records)?;
            let tail = self.renderer.finish()?;
            self.output.append(tail);
            self.finished = true;
        }
        Ok(self.output)
    }

    fn render_records(&mut self, records: Vec<HeadlessRecord>) -> Result<(), HeadlessError> {
        for record in records {
            let chunk = self.renderer.render(record)?;
            self.output.append(chunk);
        }
        Ok(())
    }
}

impl HeadlessEventSink for HeadlessPipeline {
    fn emit(&mut self, event: HeadlessEvent) -> Result<(), HeadlessError> {
        if self.finished {
            return Err(HeadlessError::new(
                "cannot emit after headless pipeline finish",
            ));
        }
        let records = self.reducer.push(event);
        self.render_records(records)
    }
}
