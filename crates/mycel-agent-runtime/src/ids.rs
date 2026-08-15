use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn generated(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}-{now:032x}-{:08x}-{sequence:016x}",
        std::process::id()
    )
}

fn validate_component(kind: &'static str, value: &str, path_safe: bool) -> Result<(), IdError> {
    if value.is_empty() {
        return Err(IdError::Empty(kind));
    }
    if value.len() > 160 {
        return Err(IdError::TooLong(kind));
    }
    if value.chars().any(char::is_control)
        || (path_safe
            && (matches!(value, "." | "..")
                || value
                    .chars()
                    .any(|character| matches!(character, '/' | '\\'))))
    {
        return Err(IdError::Unsafe {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

macro_rules! string_id {
    ($name:ident, $kind:literal, $prefix:literal, $path_safe:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                validate_component($kind, &value, $path_safe)?;
                Ok(Self(value))
            }

            pub fn generate() -> Self {
                Self(generated($prefix))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

string_id!(SessionId, "session id", "session", true);
string_id!(AgentId, "agent id", "agent", true);
string_id!(RequestId, "request id", "request", true);
// Provider tool-call IDs are opaque protocol identifiers, not path
// components. Preserve them byte-for-byte (including `/`) so durable and live
// records remain paired with the provider request.
string_id!(ToolCallId, "tool call id", "tool", false);

impl AgentId {
    pub fn main() -> Self {
        Self("main".to_owned())
    }

    pub fn is_main(&self) -> bool {
        self.0 == "main"
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("{0} exceeds 160 bytes")]
    TooLong(&'static str),
    #[error("unsafe {kind} {value:?}")]
    Unsafe { kind: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_distinct_and_valid() {
        let first = RequestId::generate();
        let second = RequestId::generate();
        assert_ne!(first, second);
        RequestId::new(first.as_str()).expect("generated id must validate");
    }

    #[test]
    fn path_components_are_rejected() {
        assert!(matches!(
            SessionId::new("../escape"),
            Err(IdError::Unsafe { .. })
        ));
        assert!(matches!(SessionId::new(".."), Err(IdError::Unsafe { .. })));
    }

    #[test]
    fn opaque_tool_call_ids_are_not_treated_as_paths() {
        let id = ToolCallId::new("provider/call\\segment").expect("opaque provider id");
        assert_eq!(id.as_str(), "provider/call\\segment");
    }
}
