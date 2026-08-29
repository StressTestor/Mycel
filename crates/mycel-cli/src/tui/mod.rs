//! Logical TUI state reducers. Rendering and process terminal ownership stay
//! outside this module, so raw-input and event traces are deterministic.

pub mod components;
mod dialogs;
mod editor;
mod gate_log;
mod overlay;
mod session;
pub mod theme;
mod transcript;

pub use dialogs::*;
pub use editor::{EditorState, HistoryEntry};
pub use gate_log::{GateDecision, GateLog, GateVerdict};
pub use overlay::{compose_overlay, FocusStack, Overlay};
pub use session::{
    InputMode, LogicalAction, QueuedInput, SessionPhase, SessionReducer, SubmissionMode,
};
pub use transcript::{
    FrameKind, ToolFrameStatus, TranscriptEvent, TranscriptFrame, TranscriptReducer,
};
