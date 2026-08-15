use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Provider-neutral model tool definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deferred: bool,
}

impl ToolDefinition {
    pub fn validate(&self) -> Result<(), ToolDefinitionError> {
        if self.name.trim().is_empty() {
            return Err(ToolDefinitionError::EmptyName);
        }
        if !self.parameters.is_object() {
            return Err(ToolDefinitionError::ParametersNotObject);
        }
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolDefinitionError {
    #[error("tool name must not be empty")]
    EmptyName,
    #[error("tool parameters must be a JSON object")]
    ParametersNotObject,
}
