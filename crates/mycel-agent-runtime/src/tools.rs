use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

use mycel_agent_protocol::{ExecutableToolResult, ToolDefinition, ToolInputDisplay, ToolUpdate};
use serde_json::Value;

use crate::{
    AgentId, CancellationToken, ExclusiveTool, PlanPolicy, SessionId, ToolAccess, ToolCallId,
};

pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ExecutableToolResult, ToolError>> + Send + 'a>>;

pub trait ToolUpdateSink: Send + Sync {
    fn emit(&self, update: ToolUpdate);
}

#[derive(Clone)]
pub struct ToolPrepareContext {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub turn_id: u64,
    pub tool_call_id: ToolCallId,
}

#[derive(Clone)]
pub struct ToolInvocation {
    pub context: ToolPrepareContext,
    pub arguments: Value,
    pub cancellation: CancellationToken,
    pub updates: Arc<dyn ToolUpdateSink>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolExecutionSpec {
    pub accesses: Vec<ToolAccess>,
    pub display: ToolInputDisplay,
    pub description: Option<String>,
    pub stop_batch_after_this: bool,
    pub action: String,
    pub approval_rule: Option<String>,
    pub rule_subject: Option<String>,
    pub exclusive_tool: Option<ExclusiveTool>,
    pub plan_policy: PlanPolicy,
    pub create_goal_review: bool,
    pub sensitive_file: bool,
    pub git_control: bool,
    pub git_cwd_write: bool,
}

impl ToolExecutionSpec {
    pub fn new(display: ToolInputDisplay, action: impl Into<String>) -> Self {
        Self {
            accesses: Vec::new(),
            display,
            description: None,
            stop_batch_after_this: false,
            action: action.into(),
            approval_rule: None,
            rule_subject: None,
            exclusive_tool: None,
            plan_policy: PlanPolicy::NotInPlan,
            create_goal_review: false,
            sensitive_file: false,
            git_control: false,
            git_cwd_write: false,
        }
    }
}

pub trait ExecutableTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        validate_json_schema(&self.definition().parameters, arguments)
    }

    fn prepare(
        &self,
        arguments: &Value,
        context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError>;

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    state: Arc<RwLock<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    revision: u64,
    tools: BTreeMap<String, RegisteredTool>,
}

#[derive(Clone)]
struct RegisteredTool {
    definition: ToolDefinition,
    executable: Arc<dyn ExecutableTool>,
}

#[derive(Clone)]
pub struct ToolSnapshot {
    pub revision: u64,
    definitions: Vec<ToolDefinition>,
    tools: BTreeMap<String, Arc<dyn ExecutableTool>>,
}

impl ToolSnapshot {
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        self.tools.get(name).cloned()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tool: Arc<dyn ExecutableTool>) -> Result<(), ToolRegistryError> {
        let definition = tool.definition();
        definition
            .validate()
            .map_err(|error| ToolRegistryError::InvalidDefinition(error.to_string()))?;
        let mut state = write_lock(&self.state);
        if state.tools.contains_key(&definition.name) {
            return Err(ToolRegistryError::Duplicate(definition.name));
        }
        state.revision = state.revision.wrapping_add(1);
        state.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                executable: tool,
            },
        );
        Ok(())
    }

    pub fn replace(&self, tool: Arc<dyn ExecutableTool>) -> Result<(), ToolRegistryError> {
        let definition = tool.definition();
        definition
            .validate()
            .map_err(|error| ToolRegistryError::InvalidDefinition(error.to_string()))?;
        let mut state = write_lock(&self.state);
        state.revision = state.revision.wrapping_add(1);
        state.tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                executable: tool,
            },
        );
        Ok(())
    }

    pub fn unregister(&self, name: &str) -> bool {
        let mut state = write_lock(&self.state);
        let removed = state.tools.remove(name).is_some();
        if removed {
            state.revision = state.revision.wrapping_add(1);
        }
        removed
    }

    /// Remove a tool only if the registry still contains this exact
    /// executable. Session-scoped compositions use this during teardown so a
    /// later replacement with the same public name is never removed by the
    /// previous owner.
    pub(crate) fn unregister_if_same(&self, tool: &Arc<dyn ExecutableTool>) -> bool {
        let name = tool.definition().name;
        let mut state = write_lock(&self.state);
        let same = state
            .tools
            .get(&name)
            .is_some_and(|registered| Arc::ptr_eq(&registered.executable, tool));
        if same {
            state.tools.remove(&name);
            state.revision = state.revision.wrapping_add(1);
        }
        same
    }

    /// Atomically replaces a caller-owned set of tools.
    ///
    /// Every replacement is validated before the registry is mutated. Names
    /// in `remove_names` may be reused by the replacement set; every other
    /// existing name remains first-wins. This is the update seam used by MCP
    /// refreshes so a model step can never observe a half-removed server.
    pub fn replace_batch(
        &self,
        remove_names: &BTreeSet<String>,
        replacements: Vec<Arc<dyn ExecutableTool>>,
    ) -> Result<(), ToolRegistryError> {
        let mut prepared = BTreeMap::new();
        for tool in replacements {
            let definition = tool.definition();
            definition
                .validate()
                .map_err(|error| ToolRegistryError::InvalidDefinition(error.to_string()))?;
            if prepared
                .insert(
                    definition.name.clone(),
                    RegisteredTool {
                        definition: definition.clone(),
                        executable: tool,
                    },
                )
                .is_some()
            {
                return Err(ToolRegistryError::Duplicate(definition.name));
            }
        }

        let mut state = write_lock(&self.state);
        if let Some(collision) = prepared
            .keys()
            .find(|name| state.tools.contains_key(*name) && !remove_names.contains(*name))
        {
            return Err(ToolRegistryError::Duplicate(collision.clone()));
        }
        let changed = remove_names
            .iter()
            .any(|name| state.tools.contains_key(name))
            || !prepared.is_empty();
        for name in remove_names {
            state.tools.remove(name);
        }
        state.tools.extend(prepared);
        if changed {
            state.revision = state.revision.wrapping_add(1);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> ToolSnapshot {
        let state = read_lock(&self.state);
        ToolSnapshot {
            revision: state.revision,
            definitions: state
                .tools
                .values()
                .map(|registered| registered.definition.clone())
                .collect(),
            tools: state
                .tools
                .iter()
                .map(|(name, registered)| (name.clone(), Arc::clone(&registered.executable)))
                .collect(),
        }
    }
}

/// Deliberately small JSON Schema evaluator for tool argument schemas. It
/// covers the structural vocabulary used by built-in and MCP tools; tools with
/// richer schemas can override `validate_arguments` with a compiled validator.
pub fn validate_json_schema(schema: &Value, value: &Value) -> Result<(), ToolError> {
    validate_schema_at(schema, value, "$")
}

fn validate_schema_at(schema: &Value, value: &Value, path: &str) -> Result<(), ToolError> {
    let object = schema.as_object().ok_or_else(|| ToolError::InvalidSchema {
        path: path.to_owned(),
        message: "schema must be an object".to_owned(),
    })?;

    if let Some(constant) = object.get("const") {
        if constant != value {
            return Err(argument_error(path, "value does not match const"));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return Err(argument_error(path, "value is not in enum"));
        }
    }
    if let Some(any_of) = object.get("anyOf").and_then(Value::as_array) {
        if !any_of
            .iter()
            .any(|candidate| validate_schema_at(candidate, value, path).is_ok())
        {
            return Err(argument_error(path, "value matches no anyOf branch"));
        }
    }
    if let Some(one_of) = object.get("oneOf").and_then(Value::as_array) {
        let matches = one_of
            .iter()
            .filter(|candidate| validate_schema_at(candidate, value, path).is_ok())
            .count();
        if matches != 1 {
            return Err(argument_error(
                path,
                "value must match exactly one oneOf branch",
            ));
        }
    }

    if let Some(expected) = object.get("type") {
        let matches = match expected {
            Value::String(expected) => type_matches(expected, value),
            Value::Array(expected) => expected
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| type_matches(expected, value)),
            _ => false,
        };
        if !matches {
            return Err(argument_error(path, "value has the wrong type"));
        }
    }

    if let Some(value) = value.as_object() {
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let required: BTreeSet<&str> = object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for required in required {
            if !value.contains_key(required) {
                return Err(argument_error(
                    path,
                    &format!("required property {required:?} is missing"),
                ));
            }
        }
        for (name, child) in value {
            if let Some(child_schema) = properties.get(name) {
                validate_schema_at(child_schema, child, &format!("{path}.{name}"))?;
            } else if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(argument_error(
                    path,
                    &format!("additional property {name:?} is not allowed"),
                ));
            } else if let Some(child_schema) = object
                .get("additionalProperties")
                .filter(|schema| schema.is_object())
            {
                validate_schema_at(child_schema, child, &format!("{path}.{name}"))?;
            }
        }
    }

    if let Some(value) = value.as_array() {
        if let Some(minimum) = object.get("minItems").and_then(Value::as_u64) {
            if value.len() < usize::try_from(minimum).unwrap_or(usize::MAX) {
                return Err(argument_error(path, "array is shorter than minItems"));
            }
        }
        if let Some(maximum) = object.get("maxItems").and_then(Value::as_u64) {
            if value.len() > usize::try_from(maximum).unwrap_or(usize::MAX) {
                return Err(argument_error(path, "array is longer than maxItems"));
            }
        }
        if let Some(items) = object.get("items") {
            for (index, child) in value.iter().enumerate() {
                validate_schema_at(items, child, &format!("{path}[{index}]"))?;
            }
        }
    }

    if let Some(value) = value.as_str() {
        if let Some(minimum) = object.get("minLength").and_then(Value::as_u64) {
            if value.chars().count() < usize::try_from(minimum).unwrap_or(usize::MAX) {
                return Err(argument_error(path, "string is shorter than minLength"));
            }
        }
        if let Some(maximum) = object.get("maxLength").and_then(Value::as_u64) {
            if value.chars().count() > usize::try_from(maximum).unwrap_or(usize::MAX) {
                return Err(argument_error(path, "string is longer than maxLength"));
            }
        }
    }

    if let Some(value) = value.as_f64() {
        if let Some(minimum) = object.get("minimum").and_then(Value::as_f64) {
            if value < minimum {
                return Err(argument_error(path, "number is smaller than minimum"));
            }
        }
        if let Some(maximum) = object.get("maximum").and_then(Value::as_f64) {
            if value > maximum {
                return Err(argument_error(path, "number is larger than maximum"));
            }
        }
        if let Some(minimum) = object.get("exclusiveMinimum").and_then(Value::as_f64) {
            if value <= minimum {
                return Err(argument_error(
                    path,
                    "number is not larger than exclusiveMinimum",
                ));
            }
        }
        if let Some(maximum) = object.get("exclusiveMaximum").and_then(Value::as_f64) {
            if value >= maximum {
                return Err(argument_error(
                    path,
                    "number is not smaller than exclusiveMaximum",
                ));
            }
        }
    }

    Ok(())
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn argument_error(path: &str, message: &str) -> ToolError {
    ToolError::InvalidArguments {
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolRegistryError {
    #[error("invalid tool definition: {0}")]
    InvalidDefinition(String),
    #[error("tool {0:?} is already registered")]
    Duplicate(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("invalid tool schema at {path}: {message}")]
    InvalidSchema { path: String, message: String },
    #[error("invalid tool arguments at {path}: {message}")]
    InvalidArguments { path: String, message: String },
    #[error("tool preparation failed: {0}")]
    Prepare(String),
    #[error("tool execution failed: {0}")]
    Execute(String),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_validation_checks_required_types_and_additional_properties() {
        let schema = json!({
            "type":"object",
            "properties":{"path":{"type":"string","minLength":1}},
            "required":["path"],
            "additionalProperties":false
        });
        validate_json_schema(&schema, &json!({"path":"a"})).expect("valid");
        assert!(validate_json_schema(&schema, &json!({})).is_err());
        assert!(validate_json_schema(&schema, &json!({"path":"a","extra":true})).is_err());

        let bounded = json!({"type":"integer","minimum":-2,"maximum":2});
        validate_json_schema(&bounded, &json!(-2)).expect("inclusive minimum");
        validate_json_schema(&bounded, &json!(2)).expect("inclusive maximum");
        assert!(validate_json_schema(&bounded, &json!(-3)).is_err());
        assert!(validate_json_schema(&bounded, &json!(3)).is_err());
    }
}
