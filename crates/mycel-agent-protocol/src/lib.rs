//! Provider-neutral contracts shared by Mycel's Rust agent runtime.
//!
//! This crate deliberately contains no network, filesystem, terminal, or
//! process implementation. It is the stable serialization boundary used by
//! providers, the runtime, session persistence, and the terminal client.

pub mod capability;
pub mod config;
pub mod display;
pub mod event;
pub mod loop_event;
pub mod message;
pub mod nullable;
pub mod permission;
pub mod provider;
pub mod record;
pub mod session;
pub mod tool;

pub use capability::*;
pub use config::*;
pub use display::*;
pub use event::*;
pub use loop_event::*;
pub use message::*;
pub use nullable::*;
pub use permission::*;
pub use provider::*;
pub use record::*;
pub use session::*;
pub use tool::*;
