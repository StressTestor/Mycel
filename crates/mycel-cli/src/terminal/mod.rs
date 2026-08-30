//! Terminal primitives and the injectable production TTY ownership boundary.

pub mod compose;
mod driver;
mod input;
mod render;
pub mod style;
mod unicode;
mod virtual_terminal;

pub use driver::{
    BackendEvent, KeyboardProtocol, ProcessTerminalBackend, TerminalBackend, TerminalDriver,
    TerminalEvent, TerminalSession, TerminalSignal, TerminalSize, DISABLE_BRACKETED_PASTE,
    DISABLE_MODIFY_OTHER_KEYS, ENABLE_BRACKETED_PASTE, ENABLE_MODIFY_OTHER_KEYS,
    ENTER_ALTERNATE_SCREEN, KITTY_KEYBOARD_QUERY, LEAVE_ALTERNATE_SCREEN, POP_KITTY_KEYBOARD,
};
pub use input::{InputDecoder, InputEvent, KeyCode, KeyEvent, KeyKind, Modifiers};
pub use render::{DifferentialRenderer, MemoryTerminalSink, TerminalSink};
pub use unicode::{grapheme_width, graphemes, truncate_to_width, visible_width, wrap_text};
pub use virtual_terminal::{Cursor, VirtualTerminal};
