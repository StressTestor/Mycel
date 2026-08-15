use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderType {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "kimi")]
    Kimi,
    #[serde(rename = "google-genai")]
    GoogleGenAi,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "vertexai")]
    VertexAi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStorage {
    Codex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthReference {
    pub storage: CredentialStorage,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_host: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntryConfig {
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthReference>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub max_context_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ModelProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub support_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ModelOverrides>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProtocol {
    Anthropic,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Manual,
    Yolo,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionScope {
    TurnOverride,
    SessionRuntime,
    Project,
    #[default]
    User,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub decision: PermissionDecision,
    #[serde(default)]
    pub scope: PermissionScope,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PermissionRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopControl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps_per_turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries_per_step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ralph_iterations: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_trigger_ratio: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrintBackgroundMode {
    Exit,
    Drain,
    Steer,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_running_tasks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_on_exit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_auto_background_on_timeout: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_task_timeout_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_grace_period_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_wait_ceiling_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_background_mode: Option<PrintBackgroundMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub print_max_turns: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_edge_px: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_byte_budget: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HookConfig {
    pub event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "fail_mode")]
    pub fail_mode: Option<HookFailMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionResult,
    UserPromptSubmit,
    Stop,
    StopFailure,
    Interrupt,
    SessionStart,
    SessionEnd,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Notification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailMode {
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(flatten)]
        common: McpCommonConfig,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(
            rename = "bearerTokenEnvVar",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        bearer_token_env_var: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<McpAuth>,
        #[serde(flatten)]
        common: McpCommonConfig,
    },
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Stdio {
                command, common, ..
            } => {
                if command.trim().is_empty() {
                    return Err(ConfigError::EmptyMcpCommand);
                }
                common.validate()
            }
            Self::Http {
                url,
                bearer_token_env_var,
                common,
                ..
            } => {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(ConfigError::InvalidMcpUrl(url.clone()));
                }
                if bearer_token_env_var
                    .as_ref()
                    .is_some_and(|name| name.trim().is_empty())
                {
                    return Err(ConfigError::EmptyMcpBearerTokenEnv);
                }
                common.validate()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    Oauth,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCommonConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_tools: Vec<String>,
}

impl McpCommonConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.startup_timeout_ms == Some(0) {
            return Err(ConfigError::InvalidMcpStartupTimeout);
        }
        if self.tool_timeout_ms == Some(0) {
            return Err(ConfigError::InvalidMcpToolTimeout);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MycelConfig {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_all_available_skills: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_skill_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_control: Option<LoopControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub experimental: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub raw: BTreeMap<String, Value>,
}

impl MycelConfig {
    /// Validates the normalized persisted shape without requiring a provider
    /// to be currently available. Runtime resolution is a separate step so a
    /// partially configured file remains inspectable and repairable.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, provider) in &self.providers {
            if name.trim().is_empty() {
                return Err(ConfigError::EmptyProviderName);
            }
            if provider
                .oauth
                .as_ref()
                .is_some_and(|oauth| oauth.key.trim().is_empty())
            {
                return Err(ConfigError::EmptyOAuthKey(name.clone()));
            }
        }
        for (alias, model) in &self.models {
            if model.max_context_size == 0 {
                return Err(ConfigError::InvalidContextSize(alias.clone()));
            }
            if model.max_output_size == Some(0) {
                return Err(ConfigError::InvalidOutputSize(alias.clone()));
            }
        }
        for rule in self
            .permission
            .iter()
            .flat_map(|permission| &permission.rules)
        {
            if rule.pattern.trim().is_empty() {
                return Err(ConfigError::EmptyPermissionPattern);
            }
        }
        if self
            .loop_control
            .as_ref()
            .and_then(|control| control.compaction_trigger_ratio)
            .is_some_and(|ratio| !(0.5..=0.99).contains(&ratio))
        {
            return Err(ConfigError::InvalidCompactionRatio);
        }
        if self
            .loop_control
            .as_ref()
            .and_then(|control| control.max_ralph_iterations)
            .is_some_and(|iterations| iterations < -1)
        {
            return Err(ConfigError::InvalidRalphIterations);
        }
        for hook in &self.hooks {
            if hook.command.trim().is_empty() {
                return Err(ConfigError::EmptyHookCommand);
            }
            if hook
                .timeout
                .is_some_and(|timeout| !(1..=600).contains(&timeout))
            {
                return Err(ConfigError::InvalidHookTimeout);
            }
        }
        if self
            .background
            .as_ref()
            .and_then(|background| background.max_running_tasks)
            == Some(0)
        {
            return Err(ConfigError::InvalidBackgroundTaskLimit);
        }
        if self
            .background
            .as_ref()
            .and_then(|background| background.print_wait_ceiling_s)
            == Some(0)
        {
            return Err(ConfigError::InvalidPrintWaitCeiling);
        }
        if self
            .background
            .as_ref()
            .and_then(|background| background.print_max_turns)
            == Some(0)
        {
            return Err(ConfigError::InvalidPrintMaxTurns);
        }
        if self
            .image
            .as_ref()
            .is_some_and(|image| image.max_edge_px == Some(0) || image.read_byte_budget == Some(0))
        {
            return Err(ConfigError::InvalidImageLimit);
        }
        Ok(())
    }

    pub fn validate_runtime(&self) -> Result<(), ConfigError> {
        self.validate()?;
        for (alias, model) in &self.models {
            if !self.providers.contains_key(&model.provider) {
                return Err(ConfigError::UnknownProvider {
                    alias: alias.clone(),
                    provider: model.provider.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn merged(&self, patch: MycelConfigPatch) -> Result<Self, ConfigError> {
        let mut base = serde_json::to_value(self)
            .map_err(|error| ConfigError::PatchSerialization(error.to_string()))?;
        let patch = serde_json::to_value(patch)
            .map_err(|error| ConfigError::PatchSerialization(error.to_string()))?;
        merge_json_objects(&mut base, patch);
        let merged: Self = serde_json::from_value(base)
            .map_err(|error| ConfigError::InvalidPatch(error.to_string()))?;
        merged.validate()?;
        Ok(merged)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MycelConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<BTreeMap<String, ProviderEntryPatch>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<BTreeMap<String, ModelConfigPatch>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionConfigPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<HookConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_all_available_skills: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_skill_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_control: Option<LoopControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<BTreeMap<String, bool>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderEntryPatch {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<ProviderType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<BTreeMap<String, Value>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ModelProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ModelOverrides>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRule>>,
}

fn merge_json_objects(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                match target.get_mut(&key) {
                    Some(current) => merge_json_objects(current, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfigFile {
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl McpConfigFile {
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, server) in &self.mcp_servers {
            if name.trim().is_empty() {
                return Err(ConfigError::EmptyMcpServerName);
            }
            server.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("provider name must not be empty")]
    EmptyProviderName,
    #[error("provider {0:?} has an empty OAuth key")]
    EmptyOAuthKey(String),
    #[error("model {0:?} must define a positive max_context_size")]
    InvalidContextSize(String),
    #[error("model {0:?} must define a positive max_output_size")]
    InvalidOutputSize(String),
    #[error("model {alias:?} references unknown provider {provider:?}")]
    UnknownProvider { alias: String, provider: String },
    #[error("permission rule pattern must not be empty")]
    EmptyPermissionPattern,
    #[error("compaction trigger ratio must be between 0.5 and 0.99")]
    InvalidCompactionRatio,
    #[error("max_ralph_iterations must be -1 or greater")]
    InvalidRalphIterations,
    #[error("hook command must not be empty")]
    EmptyHookCommand,
    #[error("hook timeout must be between 1 and 600 seconds")]
    InvalidHookTimeout,
    #[error("background max_running_tasks must be positive")]
    InvalidBackgroundTaskLimit,
    #[error("background print_wait_ceiling_s must be positive")]
    InvalidPrintWaitCeiling,
    #[error("background print_max_turns must be positive")]
    InvalidPrintMaxTurns,
    #[error("image limits must be positive")]
    InvalidImageLimit,
    #[error("invalid config patch: {0}")]
    InvalidPatch(String),
    #[error("failed to serialize config patch: {0}")]
    PatchSerialization(String),
    #[error("MCP server name must not be empty")]
    EmptyMcpServerName,
    #[error("MCP stdio command must not be empty")]
    EmptyMcpCommand,
    #[error("invalid MCP URL {0:?}")]
    InvalidMcpUrl(String),
    #[error("MCP bearer token environment variable must not be empty")]
    EmptyMcpBearerTokenEnv,
    #[error("MCP startup timeout must be positive")]
    InvalidMcpStartupTimeout,
    #[error("MCP tool timeout must be positive")]
    InvalidMcpToolTimeout,
}
