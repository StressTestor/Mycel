//! Production model-provider and credential adapters for Mycel.
//!
//! The crate owns vendor wire formats, HTTP streaming, retry classification,
//! and the two subscription-auth compatibility layers retained by Mycel.  It
//! deliberately does not own runtime/session policy.

pub mod auth;
pub mod capabilities;
pub mod discovery;
pub mod error;
pub mod google_auth;
pub mod http;
pub mod providers;
mod random;
pub mod registry;

pub use auth::*;
pub use capabilities::*;
pub use discovery::*;
pub use error::*;
pub use google_auth::*;
pub use http::*;
pub use providers::*;
pub use registry::*;
