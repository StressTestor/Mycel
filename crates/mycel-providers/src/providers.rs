use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use mycel_agent_protocol::{
    ChatProvider, ContentPart, FinishReason, Message, OptionalNullable, ProviderError,
    ProviderErrorKind, ProviderEventStream, ProviderFuture, ProviderRequest, ProviderRequestAuth,
    ProviderStreamEvent, ResponseFormat, Role, StreamIndex, StreamPart, TokenUsage, ToolDefinition,
};
use serde_json::{json, Map, Value};

use crate::{
    error::{invalid_request, malformed_error},
    http::{collect_body, decode_sse, send_with_retry, HttpRequest, HttpTransport, RetryPolicy},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
struct ProviderCore {
    name: String,
    model: String,
    base_url: String,
    custom_headers: BTreeMap<String, String>,
    transport: Arc<dyn HttpTransport>,
    retry: RetryPolicy,
}

impl ProviderCore {
    fn json_request(
        &self,
        path: &str,
        body: Value,
        auth: &ProviderRequestAuth,
        auth_style: AuthStyle,
    ) -> Result<HttpRequest, ProviderError> {
        let api_key = auth
            .api_key
            .as_ref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "provider API key is missing",
                )
            })?;
        let mut headers = self.custom_headers.clone();
        insert_header(&mut headers, "accept", "text/event-stream".into());
        insert_header(&mut headers, "content-type", "application/json".into());
        match auth_style {
            AuthStyle::Bearer => {
                insert_header(
                    &mut headers,
                    "authorization",
                    format!("Bearer {}", api_key.expose()),
                );
            }
            AuthStyle::Anthropic => {
                insert_header(&mut headers, "x-api-key", api_key.expose().to_owned());
                if !headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("anthropic-version"))
                {
                    headers.insert("anthropic-version".into(), "2023-06-01".into());
                }
            }
            AuthStyle::GoogleApiKey => {
                insert_header(&mut headers, "x-goog-api-key", api_key.expose().to_owned());
            }
        }
        merge_secret_headers(&mut headers, auth);
        Ok(HttpRequest {
            method: "POST".into(),
            url: format!("{}{}", self.base_url.trim_end_matches('/'), path),
            headers,
            body: serde_json::to_vec(&body).map_err(|error| {
                invalid_request(format!("could not encode provider request: {error}"))
            })?,
            timeout: REQUEST_TIMEOUT,
        })
    }
}

#[derive(Clone, Copy)]
enum AuthStyle {
    Bearer,
    Anthropic,
    GoogleApiKey,
}

fn merge_secret_headers(headers: &mut BTreeMap<String, String>, auth: &ProviderRequestAuth) {
    for (name, value) in &auth.headers {
        insert_header(headers, name, value.expose().to_owned());
    }
}

fn insert_header(headers: &mut BTreeMap<String, String>, name: &str, value: String) {
    headers.retain(|key, _| !key.eq_ignore_ascii_case(name));
    headers.insert(name.to_owned(), value);
}

#[derive(Clone)]
pub struct OpenAiChatProvider {
    core: ProviderCore,
}

impl OpenAiChatProvider {
    pub fn new(
        model: impl Into<String>,
        base_url: Option<String>,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let model = model.into();
        Self {
            core: ProviderCore {
                name: "openai".into(),
                model,
                base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
                custom_headers,
                transport,
                retry: RetryPolicy::default(),
            },
        }
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.core.retry = retry;
        self
    }
}

impl ChatProvider for OpenAiChatProvider {
    fn name(&self) -> &str {
        &self.core.name
    }

    fn model(&self) -> &str {
        &self.core.model
    }

    fn stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
        auth: &'a ProviderRequestAuth,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            validate_request(request, &self.core.model)?;
            let body = openai_chat_body(request, false)?;
            let http =
                self.core
                    .json_request("/chat/completions", body, auth, AuthStyle::Bearer)?;
            let response = send_with_retry(&self.core.transport, http, self.core.retry).await?;
            Ok(openai_chat_stream(
                response.body,
                response.headers.get("x-request-id").cloned(),
            ))
        })
    }
}

#[derive(Clone)]
pub struct KimiProvider {
    core: ProviderCore,
}

impl KimiProvider {
    pub fn new(
        model: impl Into<String>,
        base_url: Option<String>,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            core: ProviderCore {
                name: "kimi".into(),
                model: model.into(),
                base_url: base_url.unwrap_or_else(|| "https://api.moonshot.ai/v1".into()),
                custom_headers,
                transport,
                retry: RetryPolicy::default(),
            },
        }
    }

    pub fn managed(
        model: impl Into<String>,
        base_url: Option<String>,
        identity_headers: &BTreeMap<String, String>,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let mut headers = identity_headers.clone();
        headers.extend(custom_headers);
        Self::new(
            model,
            Some(base_url.unwrap_or_else(|| "https://api.kimi.com/coding/v1".into())),
            headers,
            transport,
        )
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.core.retry = retry;
        self
    }

    pub async fn upload_video(
        &self,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
        auth: &ProviderRequestAuth,
    ) -> Result<String, ProviderError> {
        if !matches!(
            mime_type,
            "video/mp4"
                | "video/mpeg"
                | "video/quicktime"
                | "video/webm"
                | "video/x-matroska"
                | "video/x-msvideo"
                | "video/x-flv"
                | "video/3gpp"
        ) {
            return Err(invalid_request(format!(
                "unsupported Kimi video type {mime_type}"
            )));
        }
        let key = auth
            .api_key
            .as_ref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "provider API key is missing",
                )
            })?;
        let boundary = format!(
            "mycel-{}",
            hex::encode(crate::random::secure_random::<12>()?)
        );
        let mut body = Vec::new();
        append_multipart_field(&mut body, &boundary, "purpose", "video");
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: {mime_type}\r\n\r\n", escape_header_value(filename)).as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let mut headers = self.core.custom_headers.clone();
        insert_header(
            &mut headers,
            "authorization",
            format!("Bearer {}", key.expose()),
        );
        insert_header(
            &mut headers,
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        );
        insert_header(&mut headers, "accept", "application/json".into());
        merge_secret_headers(&mut headers, auth);
        let request = HttpRequest {
            method: "POST".into(),
            url: format!("{}/files", self.core.base_url.trim_end_matches('/')),
            headers,
            body,
            timeout: REQUEST_TIMEOUT,
        };
        let response = send_with_retry(&self.core.transport, request, self.core.retry).await?;
        let bytes = collect_body(response.body, 1_048_576).await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| malformed_error(format!("invalid Kimi file response: {error}")))?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| malformed_error("Kimi file response is missing id"))?;
        Ok(format!("ms://{id}"))
    }
}

impl ChatProvider for KimiProvider {
    fn name(&self) -> &str {
        &self.core.name
    }

    fn model(&self) -> &str {
        &self.core.model
    }

    fn stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
        auth: &'a ProviderRequestAuth,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            validate_request(request, &self.core.model)?;
            let body = openai_chat_body(request, true)?;
            let http =
                self.core
                    .json_request("/chat/completions", body, auth, AuthStyle::Bearer)?;
            let response = send_with_retry(&self.core.transport, http, self.core.retry).await?;
            let trace = response
                .headers
                .get("x-trace-id")
                .or_else(|| response.headers.get("x-request-id"))
                .cloned();
            Ok(openai_chat_stream(response.body, trace))
        })
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesProvider {
    core: ProviderCore,
    codex_subscription: bool,
}

impl OpenAiResponsesProvider {
    pub fn new(
        model: impl Into<String>,
        base_url: Option<String>,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, ProviderError> {
        let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".into());
        let codex_subscription = is_codex_base_url(&base_url);
        if base_url.contains("chatgpt.com") && !codex_subscription {
            return Err(invalid_request(
                "Codex subscription base URL must be exactly https://chatgpt.com/backend-api/codex",
            ));
        }
        Ok(Self {
            core: ProviderCore {
                name: "openai_responses".into(),
                model: model.into(),
                base_url,
                custom_headers,
                transport,
                retry: RetryPolicy::default(),
            },
            codex_subscription,
        })
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.core.retry = retry;
        self
    }
}

impl ChatProvider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        &self.core.name
    }

    fn model(&self) -> &str {
        &self.core.model
    }

    fn stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
        auth: &'a ProviderRequestAuth,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            validate_request(request, &self.core.model)?;
            let body = openai_responses_body(request, self.codex_subscription)?;
            let http = self
                .core
                .json_request("/responses", body, auth, AuthStyle::Bearer)?;
            let response = send_with_retry(&self.core.transport, http, self.core.retry).await?;
            Ok(openai_responses_stream(response.body))
        })
    }
}

#[derive(Clone)]
pub struct AnthropicProvider {
    core: ProviderCore,
    beta_api: bool,
    beta_features: Vec<String>,
    adaptive_thinking: Option<bool>,
    kimi_thinking: bool,
}

impl AnthropicProvider {
    pub fn new(
        model: impl Into<String>,
        base_url: Option<String>,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            core: ProviderCore {
                name: "anthropic".into(),
                model: model.into(),
                base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
                custom_headers,
                transport,
                retry: RetryPolicy::default(),
            },
            beta_api: false,
            beta_features: vec!["interleaved-thinking-2025-05-14".into()],
            adaptive_thinking: None,
            kimi_thinking: false,
        }
    }

    pub fn kimi_protocol(
        model: impl Into<String>,
        base_url: String,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let base_url = base_url
            .trim_end_matches('/')
            .strip_suffix("/v1")
            .unwrap_or(base_url.trim_end_matches('/'))
            .to_owned();
        Self::new(model, Some(base_url), custom_headers, transport)
            .with_kimi_thinking(true)
            .with_beta_api(true)
    }

    pub fn with_beta_api(mut self, enabled: bool) -> Self {
        self.beta_api = enabled;
        self
    }

    pub fn with_beta_features(mut self, features: Vec<String>) -> Self {
        self.beta_features = features;
        self
    }

    pub fn with_adaptive_thinking(mut self, enabled: Option<bool>) -> Self {
        self.adaptive_thinking = enabled;
        self
    }

    pub fn with_kimi_thinking(mut self, enabled: bool) -> Self {
        self.kimi_thinking = enabled;
        self
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.core.retry = retry;
        self
    }
}

impl ChatProvider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.core.name
    }

    fn model(&self) -> &str {
        &self.core.model
    }

    fn stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
        auth: &'a ProviderRequestAuth,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            validate_request(request, &self.core.model)?;
            let adaptive = if self.kimi_thinking {
                true
            } else {
                self.adaptive_thinking
                    .unwrap_or_else(|| anthropic_adaptive_model(&request.model))
            };
            let betas = if adaptive {
                self.beta_features
                    .iter()
                    .filter(|feature| feature.as_str() != "interleaved-thinking-2025-05-14")
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                self.beta_features.clone()
            };
            let body =
                anthropic_body(request, self.beta_api, &betas, adaptive, self.kimi_thinking)?;
            let path = if self.beta_api {
                "/v1/messages?beta=true"
            } else {
                "/v1/messages"
            };
            let mut http = self
                .core
                .json_request(path, body, auth, AuthStyle::Anthropic)?;
            if !self.beta_api && !betas.is_empty() {
                insert_header(&mut http.headers, "anthropic-beta", betas.join(","));
            }
            let response = send_with_retry(&self.core.transport, http, self.core.retry).await?;
            Ok(anthropic_stream(response.body))
        })
    }
}

#[derive(Clone, Debug)]
pub enum GoogleEndpoint {
    Gemini,
    VertexApiKey,
    VertexServiceAccount { project: String, location: String },
}

#[derive(Clone)]
pub struct GoogleProvider {
    core: ProviderCore,
    endpoint: GoogleEndpoint,
}

impl GoogleProvider {
    pub fn new(
        model: impl Into<String>,
        endpoint: GoogleEndpoint,
        base_url: Option<String>,
        custom_headers: BTreeMap<String, String>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        let default_base = match &endpoint {
            GoogleEndpoint::Gemini => "https://generativelanguage.googleapis.com",
            GoogleEndpoint::VertexApiKey => "https://aiplatform.googleapis.com",
            GoogleEndpoint::VertexServiceAccount { location, .. } if location == "us" => {
                "https://aiplatform.us.rep.googleapis.com"
            }
            GoogleEndpoint::VertexServiceAccount { .. } => "",
        };
        Self {
            core: ProviderCore {
                name: match endpoint {
                    GoogleEndpoint::Gemini => "google-genai".into(),
                    _ => "vertexai".into(),
                },
                model: model.into(),
                base_url: base_url.unwrap_or_else(|| default_base.into()),
                custom_headers,
                transport,
                retry: RetryPolicy::default(),
            },
            endpoint,
        }
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.core.retry = retry;
        self
    }
}

impl ChatProvider for GoogleProvider {
    fn name(&self) -> &str {
        &self.core.name
    }

    fn model(&self) -> &str {
        &self.core.model
    }

    fn stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
        auth: &'a ProviderRequestAuth,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            validate_request(request, &self.core.model)?;
            let body = google_body(request)?;
            let (base, path, style) = google_url(&self.core, &self.endpoint);
            let mut core = self.core.clone();
            core.base_url = base;
            let http = core.json_request(&path, body, auth, style)?;
            let response = send_with_retry(&core.transport, http, core.retry).await?;
            Ok(google_stream(response.body))
        })
    }
}

fn validate_request(request: &ProviderRequest, expected_model: &str) -> Result<(), ProviderError> {
    request
        .validate()
        .map_err(|error| invalid_request(error.to_string()))?;
    if request.model != expected_model {
        return Err(invalid_request(format!(
            "provider model {expected_model:?} does not match request model {:?}",
            request.model
        )));
    }
    Ok(())
}

fn openai_chat_body(request: &ProviderRequest, kimi: bool) -> Result<Value, ProviderError> {
    let mut id_map = ToolIdMap::default();
    let mut messages = Vec::new();
    if !request.system_prompt.is_empty() {
        messages.push(json!({"role":"system","content":request.system_prompt}));
    }
    for message in &request.history {
        messages.extend(openai_chat_message(message, kimi, &mut id_map)?);
    }
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), Value::Bool(true)),
        ("stream_options".into(), json!({"include_usage":true})),
    ]);
    let tools = request
        .wire_tools()
        .map(|tool| openai_tool(tool, kimi))
        .collect::<Vec<_>>();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tokens) = request.max_completion_tokens {
        let key = if kimi || reasoning_model(&request.model) || request.model.starts_with("gpt-5") {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body.insert(key.into(), Value::from(tokens.min(131_072)));
    }
    if let Some(format) = &request.response_format {
        body.insert("response_format".into(), openai_response_format(format));
    }
    if let Some(effort) = &request.thinking_effort {
        if kimi {
            let thinking = match effort.as_str() {
                "off" => json!({"type":"disabled"}),
                "on" => json!({"type":"enabled"}),
                concrete => json!({"type":"enabled","effort":concrete}),
            };
            body.insert("thinking".into(), thinking);
        } else if !matches!(effort.as_str(), "off" | "on") {
            body.insert(
                "reasoning_effort".into(),
                Value::String(effort.as_str().into()),
            );
        }
    }
    copy_generation_metadata(&mut body, &request.metadata);
    Ok(Value::Object(body))
}

fn openai_chat_message(
    message: &Message,
    kimi: bool,
    ids: &mut ToolIdMap,
) -> Result<Vec<Value>, ProviderError> {
    if message.is_tool_declaration_only() && kimi {
        return Ok(vec![json!({
            "role":"system",
            "content":"",
            "tools": message.tools.iter().map(|tool| openai_tool(tool, true)).collect::<Vec<_>>()
        })]);
    }
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut object = Map::from_iter([("role".into(), Value::String(role.into()))]);
    let (content, reasoning, media) = openai_content(&message.content, message.role, kimi);
    if !(message.role == Role::Assistant
        && !message.tool_calls.is_empty()
        && content == Value::String(String::new()))
    {
        object.insert("content".into(), content);
    }
    if let Some(reasoning) = reasoning {
        object.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if let Some(name) = &message.name {
        object.insert("name".into(), Value::String(name.clone()));
    }
    if message.role == Role::Tool {
        let original = message.tool_call_id.as_deref().unwrap_or_default();
        object.insert("tool_call_id".into(), Value::String(ids.resolve(original)));
    }
    if !message.tool_calls.is_empty() {
        let calls = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "type":"function",
                    "id":ids.register(&call.id),
                    "function":{"name":call.name,"arguments":call.arguments.clone().unwrap_or_else(|| "{}".into())}
                })
            })
            .collect();
        object.insert("tool_calls".into(), Value::Array(calls));
    }
    let mut output = vec![Value::Object(object)];
    if message.role == Role::Tool && !media.is_empty() {
        output.push(json!({"role":"user","content":media}));
    }
    Ok(output)
}

fn openai_content(
    parts: &[ContentPart],
    role: Role,
    kimi: bool,
) -> (Value, Option<String>, Vec<Value>) {
    let mut content = Vec::new();
    let mut reasoning = String::new();
    let mut tool_media = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => content.push(json!({"type":"text","text":text})),
            ContentPart::Think { think, .. } => reasoning.push_str(think),
            ContentPart::ImageUrl { image_url } => {
                let value = json!({"type":"image_url","image_url":{"url":image_url.url}});
                if role == Role::Tool && !kimi {
                    tool_media.push(value)
                } else {
                    content.push(value)
                }
            }
            ContentPart::AudioUrl { audio_url } => {
                content.push(json!({"type":"text","text":format!("[audio: {}]", audio_url.url)}))
            }
            ContentPart::VideoUrl { video_url } => {
                let value = if kimi {
                    json!({"type":"video_url","video_url":{"url":video_url.url}})
                } else {
                    json!({"type":"text","text":format!("[video: {}]", video_url.url)})
                };
                if role == Role::Tool && !kimi {
                    tool_media.push(value)
                } else {
                    content.push(value)
                }
            }
        }
    }
    let value =
        if content.len() == 1 && content[0].get("type") == Some(&Value::String("text".into())) {
            content[0].get("text").cloned().unwrap_or_default()
        } else if content.is_empty() {
            Value::String(String::new())
        } else {
            Value::Array(content)
        };
    (
        value,
        (!reasoning.is_empty()).then_some(reasoning),
        if tool_media.is_empty() {
            tool_media
        } else {
            let mut prefixed =
                vec![json!({"type":"text","text":"Attached media from tool result:"})];
            prefixed.extend(tool_media);
            prefixed
        },
    )
}

fn openai_tool(tool: &ToolDefinition, kimi: bool) -> Value {
    if kimi && tool.name.starts_with('$') {
        json!({"type":"builtin_function","function":{"name":tool.name}})
    } else {
        let parameters = if kimi {
            normalize_kimi_schema(&tool.parameters)
        } else {
            tool.parameters.clone()
        };
        json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":parameters}})
    }
}

/// Normalize JSON Schema constructs accepted by Mycel tools into the stricter
/// Kimi function-schema dialect. Local references are expanded unless doing so
/// would recurse through a cycle, in which case the reference is retained.
pub fn normalize_kimi_schema(schema: &Value) -> Value {
    fn normalize(value: &Value, root: &Value, active_refs: &mut HashSet<String>) -> Value {
        if let Some(object) = value.as_object() {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                if reference.starts_with("#/") && !active_refs.contains(reference) {
                    if let Some(target) = root.pointer(&reference[1..]) {
                        active_refs.insert(reference.to_owned());
                        let mut expanded = normalize(target, root, active_refs);
                        active_refs.remove(reference);
                        if let Some(expanded_object) = expanded.as_object_mut() {
                            for (key, sibling) in object {
                                if key != "$ref" {
                                    expanded_object
                                        .insert(key.clone(), normalize(sibling, root, active_refs));
                                }
                            }
                            infer_schema_type(expanded_object);
                            return expanded;
                        }
                    }
                }
            }
            let mut output = object
                .iter()
                .map(|(key, child)| (key.clone(), normalize(child, root, active_refs)))
                .collect::<Map<_, _>>();
            infer_schema_type(&mut output);
            Value::Object(output)
        } else if let Some(array) = value.as_array() {
            Value::Array(
                array
                    .iter()
                    .map(|child| normalize(child, root, active_refs))
                    .collect(),
            )
        } else {
            value.clone()
        }
    }

    normalize(schema, schema, &mut HashSet::new())
}

fn infer_schema_type(schema: &mut Map<String, Value>) {
    let literal_type = schema.get("const").and_then(json_type).or_else(|| {
        schema
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(json_type)
    });
    if let (Some(existing), Some(literal_type)) =
        (schema.get("type").and_then(Value::as_str), literal_type)
    {
        if existing != literal_type && !(existing == "number" && literal_type == "integer") {
            schema.insert("type".into(), Value::String(literal_type.into()));
        }
        return;
    }
    if schema.contains_key("type") {
        return;
    }
    let inferred =
        if schema.contains_key("properties") || schema.contains_key("additionalProperties") {
            Some("object")
        } else if schema.contains_key("items") {
            Some("array")
        } else {
            literal_type
        };
    if let Some(inferred) = inferred {
        schema.insert("type".into(), Value::String(inferred.into()));
    }
}

fn json_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::Null => Some("null"),
        Value::Bool(_) => Some("boolean"),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some("integer"),
        Value::Number(_) => Some("number"),
        Value::String(_) => Some("string"),
        Value::Array(_) => Some("array"),
        Value::Object(_) => Some("object"),
    }
}

fn openai_response_format(format: &ResponseFormat) -> Value {
    match format {
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema { json_schema } => json!({
            "type":"json_schema",
            "json_schema":{
                "name":json_schema.name,
                "schema":json_schema.schema,
                "strict":json_schema.strict,
                "description":json_schema.description
            }
        }),
    }
}

fn openai_responses_body(request: &ProviderRequest, codex: bool) -> Result<Value, ProviderError> {
    let mut ids = ToolIdMap::default();
    let mut input = Vec::new();
    for message in &request.history {
        input.extend(responses_items(message, &request.model, &mut ids)?);
    }
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("input".into(), Value::Array(input)),
        ("store".into(), Value::Bool(false)),
        ("stream".into(), Value::Bool(true)),
    ]);
    if !request.system_prompt.is_empty() {
        body.insert(
            "instructions".into(),
            Value::String(request.system_prompt.clone()),
        );
    }
    let tools = request
        .wire_tools()
        .map(|tool| {
            json!({
                "type":"function","name":tool.name,"description":tool.description,
                "parameters":tool.parameters,"strict":false
            })
        })
        .collect::<Vec<_>>();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(format) = &request.response_format {
        body.insert("text".into(), json!({"format":responses_format(format)}));
    }
    if let Some(effort) = &request.thinking_effort {
        if effort.as_str() != "off" {
            let effort = if effort.as_str() == "on" {
                "medium"
            } else {
                effort.as_str()
            };
            body.insert(
                "reasoning".into(),
                json!({"effort":effort,"summary":"auto"}),
            );
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    if !codex {
        if let Some(tokens) = request.max_completion_tokens {
            body.insert("max_output_tokens".into(), Value::from(tokens));
        }
    }
    copy_generation_metadata(&mut body, &request.metadata);
    Ok(Value::Object(body))
}

fn responses_items(
    message: &Message,
    model: &str,
    ids: &mut ToolIdMap,
) -> Result<Vec<Value>, ProviderError> {
    let role = match message.role {
        Role::System if responses_developer_role(model) => "developer",
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => {
            return Ok(vec![json!({
                "type":"function_call_output",
                "call_id":ids.resolve(message.tool_call_id.as_deref().unwrap_or_default()),
                "output":message.text("\n")
            })]);
        }
    };
    let mut output = Vec::new();
    let mut content = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => content.push(json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":text})),
            ContentPart::ImageUrl { image_url } => content.push(json!({"type":"input_image","image_url":image_url.url})),
            ContentPart::AudioUrl { audio_url } => content.push(responses_audio(&audio_url.url)),
            ContentPart::VideoUrl { video_url } => content.push(json!({"type":"input_text","text":format!("[video: {}]", video_url.url)})),
            ContentPart::Think { think, encrypted } => output.push(json!({
                "type":"reasoning",
                "summary":if think.is_empty() { Vec::<Value>::new() } else { vec![json!({"type":"summary_text","text":think})] },
                "encrypted_content":encrypted
            })),
        }
    }
    if !content.is_empty() {
        output.push(json!({"role":role,"content":content}));
    }
    for call in &message.tool_calls {
        output.push(json!({
            "type":"function_call","call_id":ids.register(&call.id),"name":call.name,
            "arguments":call.arguments.clone().unwrap_or_else(|| "{}".into())
        }));
    }
    Ok(output)
}

fn responses_audio(url: &str) -> Value {
    if let Some((mime, data)) = parse_data_uri(url) {
        if matches!(
            mime,
            "audio/mp3" | "audio/mpeg" | "audio/wav" | "audio/x-wav"
        ) {
            let extension = if mime.contains("wav") { "wav" } else { "mp3" };
            return json!({"type":"input_file","file_data":data,"filename":format!("audio.{extension}")});
        }
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        json!({"type":"input_file","file_url":url})
    } else {
        json!({"type":"input_text","text":format!("[audio: {url}]")})
    }
}

fn responses_format(format: &ResponseFormat) -> Value {
    match format {
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema { json_schema } => json!({
            "type":"json_schema","name":json_schema.name,"schema":json_schema.schema,
            "strict":json_schema.strict,"description":json_schema.description
        }),
    }
}

fn anthropic_body(
    request: &ProviderRequest,
    beta_api: bool,
    betas: &[String],
    adaptive_thinking: bool,
    kimi_thinking: bool,
) -> Result<Value, ProviderError> {
    if matches!(request.response_format, Some(ResponseFormat::JsonObject)) {
        return Err(invalid_request(
            "Anthropic requires a JSON schema response format",
        ));
    }
    let mut messages: Vec<Value> = Vec::new();
    for message in &request.history {
        let (role, blocks) = anthropic_message(message, &request.model)?;
        if blocks.is_empty() {
            continue;
        }
        if let Some(last) = messages
            .last_mut()
            .filter(|value| value.get("role") == Some(&Value::String(role.into())))
        {
            last.get_mut("content")
                .and_then(Value::as_array_mut)
                .expect("constructed array")
                .extend(blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    inject_last_cache_control(&mut messages);
    let max_tokens = anthropic_output_limit(&request.model, request.max_completion_tokens);
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("messages".into(), Value::Array(messages)),
        ("max_tokens".into(), Value::from(max_tokens)),
        ("stream".into(), Value::Bool(true)),
    ]);
    if !request.system_prompt.is_empty() {
        body.insert("system".into(), json!([{"type":"text","text":request.system_prompt,"cache_control":{"type":"ephemeral"}}]));
    }
    let mut tools = request
        .wire_tools()
        .map(|tool| {
            json!({
                "name":tool.name,"description":tool.description,"input_schema":tool.parameters
            })
        })
        .collect::<Vec<_>>();
    if let Some(last) = tools.last_mut().and_then(Value::as_object_mut) {
        last.insert("cache_control".into(), json!({"type":"ephemeral"}));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    let mut output_config = Map::new();
    if let Some(effort) = request.thinking_effort.as_ref() {
        let effort_name = effort.as_str();
        let thinking = if effort_name == "off" {
            json!({"type":"disabled"})
        } else if kimi_thinking {
            if effort_name != "on" {
                output_config.insert("effort".into(), Value::String(effort_name.into()));
            }
            json!({"type":"enabled"})
        } else if adaptive_thinking {
            if effort_name != "on" {
                output_config.insert("effort".into(), Value::String(effort_name.into()));
            }
            json!({"type":"adaptive","display":"summarized"})
        } else {
            json!({"type":"enabled","budget_tokens":anthropic_thinking_budget(effort_name)})
        };
        body.insert("thinking".into(), thinking);
    }
    if let Some(ResponseFormat::JsonSchema { json_schema }) = &request.response_format {
        output_config.insert(
            "format".into(),
            json!({"type":"json_schema","schema":json_schema.schema}),
        );
    }
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    }
    if beta_api && !betas.is_empty() {
        body.insert("betas".into(), json!(betas));
    }
    copy_generation_metadata(&mut body, &request.metadata);
    Ok(Value::Object(body))
}

fn anthropic_message(
    message: &Message,
    model: &str,
) -> Result<(&'static str, Vec<Value>), ProviderError> {
    if message.role == Role::System {
        return Ok((
            "user",
            vec![json!({"type":"text","text":format!("<system>{}</system>", message.text("\n"))})],
        ));
    }
    if message.role == Role::Tool {
        let content = message
            .content
            .iter()
            .map(|part| anthropic_content(part, model))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        return Ok((
            "user",
            vec![json!({
                "type":"tool_result","tool_use_id":message.tool_call_id.as_deref().unwrap_or_default(),"content":content
            })],
        ));
    }
    let mut blocks = message
        .content
        .iter()
        .map(|part| anthropic_content(part, model))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    for call in &message.tool_calls {
        let arguments = call.arguments.as_deref().unwrap_or("{}");
        let input: Value = serde_json::from_str(arguments).map_err(|error| {
            invalid_request(format!(
                "tool arguments for {} are invalid JSON: {error}",
                call.name
            ))
        })?;
        if !input.is_object() {
            return Err(invalid_request(
                "Anthropic tool arguments must be a JSON object",
            ));
        }
        blocks.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
    }
    Ok((
        if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        },
        blocks,
    ))
}

fn anthropic_content(part: &ContentPart, model: &str) -> Result<Option<Value>, ProviderError> {
    Ok(Some(match part {
        ContentPart::Text { text } => json!({"type":"text","text":text}),
        ContentPart::Think {
            think,
            encrypted: Some(encrypted),
        } if think.is_empty() => json!({"type":"redacted_thinking","data":encrypted}),
        ContentPart::Think {
            think,
            encrypted: Some(encrypted),
        } => json!({"type":"thinking","thinking":think,"signature":encrypted}),
        ContentPart::Think {
            think,
            encrypted: None,
        } => {
            if preserve_unsigned_anthropic_thinking(model) {
                json!({"type":"thinking","thinking":think})
            } else {
                return Ok(None);
            }
        }
        ContentPart::ImageUrl { image_url } => anthropic_media("image", &image_url.url)?,
        ContentPart::VideoUrl { video_url } => anthropic_media("video", &video_url.url)?,
        ContentPart::AudioUrl { .. } => {
            json!({"type":"text","text":"(audio omitted: not supported by this provider)"})
        }
    }))
}

fn preserve_unsigned_anthropic_thinking(model: &str) -> bool {
    parse_anthropic_version(model).is_none()
}

fn anthropic_media(kind: &str, url: &str) -> Result<Value, ProviderError> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(json!({"type":kind,"source":{"type":"url","url":url}}));
    }
    let (mime, data) =
        parse_data_uri(url).ok_or_else(|| invalid_request(format!("invalid {kind} data URL")))?;
    if kind == "image"
        && !matches!(
            mime,
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    {
        return Err(invalid_request(format!(
            "unsupported Anthropic image type {mime}"
        )));
    }
    Ok(json!({"type":kind,"source":{"type":"base64","media_type":mime,"data":data}}))
}

fn inject_last_cache_control(messages: &mut [Value]) {
    if let Some(block) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|content| content.last_mut())
        .and_then(Value::as_object_mut)
    {
        let cacheable = block
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "text"
                        | "image"
                        | "document"
                        | "search_result"
                        | "tool_use"
                        | "tool_result"
                        | "server_tool_use"
                        | "web_search_tool_result"
                )
            });
        if cacheable {
            block.insert("cache_control".into(), json!({"type":"ephemeral"}));
        }
    }
}

fn google_body(request: &ProviderRequest) -> Result<Value, ProviderError> {
    validate_google_tool_result_groups(&request.history)?;
    let mut contents: Vec<Value> = Vec::new();
    let call_names = request
        .history
        .iter()
        .flat_map(|message| message.tool_calls.iter())
        .map(|call| (call.id.as_str(), call.name.as_str()))
        .collect::<HashMap<_, _>>();
    for message in &request.history {
        let role = if message.role == Role::Assistant {
            "model"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        if message.role == Role::System {
            parts.push(json!({"text":format!("<system>{}</system>", message.text("\n"))}));
        } else if message.role == Role::Tool {
            let id = message.tool_call_id.as_deref().unwrap_or_default();
            let name = call_names.get(id).copied().ok_or_else(|| {
                invalid_request(format!("Google tool result references unknown call {id}"))
            })?;
            parts.push(json!({"functionResponse":{"name":name,"response":{"output":message.text("\n")},"parts":[]}}));
            for media in &message.content {
                if !matches!(media, ContentPart::Text { .. }) {
                    parts.push(google_part(media)?);
                }
            }
        } else {
            for part in &message.content {
                parts.push(google_part(part)?);
            }
            for call in &message.tool_calls {
                let args: Value = serde_json::from_str(call.arguments.as_deref().unwrap_or("{}"))
                    .map_err(|error| {
                    invalid_request(format!("invalid Google tool arguments: {error}"))
                })?;
                let mut call_part = json!({"functionCall":{"name":call.name,"args":args}});
                if let Some(signature) = call.extras.get("thoughtSignature") {
                    call_part
                        .as_object_mut()
                        .expect("object")
                        .insert("thoughtSignature".into(), signature.clone());
                }
                parts.push(call_part);
            }
        }
        if parts.is_empty() {
            continue;
        }
        if let Some(last) = contents
            .last_mut()
            .filter(|value| value.get("role") == Some(&Value::String(role.into())))
        {
            last.get_mut("parts")
                .and_then(Value::as_array_mut)
                .expect("array")
                .extend(parts);
        } else {
            contents.push(json!({"role":role,"parts":parts}));
        }
    }
    let mut body = Map::from_iter([("contents".into(), Value::Array(contents))]);
    let mut generation_config = Map::new();
    if !request.system_prompt.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({"parts":[{"text":request.system_prompt}]}),
        );
    }
    if let Some(tokens) = request.max_completion_tokens {
        generation_config.insert("maxOutputTokens".into(), Value::from(tokens));
    }
    if let Some(effort) = &request.thinking_effort {
        generation_config.insert(
            "thinkingConfig".into(),
            google_thinking_config(&request.model, effort.as_str()),
        );
    }
    if let Some(format) = &request.response_format {
        generation_config.insert(
            "responseMimeType".into(),
            Value::String("application/json".into()),
        );
        if let ResponseFormat::JsonSchema { json_schema } = format {
            generation_config.insert("responseJsonSchema".into(), json_schema.schema.clone());
        }
    }
    for (source, target) in [
        ("temperature", "temperature"),
        ("topP", "topP"),
        ("topK", "topK"),
    ] {
        if let Some(value) = request.metadata.get(source) {
            generation_config.insert(target.into(), value.clone());
        }
    }
    let tools = request.wire_tools().map(|tool| json!({"functionDeclarations":[{
        "name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters
    }]})).collect::<Vec<_>>();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if !generation_config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation_config));
    }
    Ok(Value::Object(body))
}

fn validate_google_tool_result_groups(history: &[Message]) -> Result<(), ProviderError> {
    let mut pending = BTreeMap::<String, String>::new();
    for message in history {
        if !pending.is_empty() && message.role != Role::Tool {
            return Err(invalid_request(format!(
                "Google history is missing tool results for {}",
                pending.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            pending.extend(
                message
                    .tool_calls
                    .iter()
                    .map(|call| (call.id.clone(), call.name.clone())),
            );
        } else if message.role == Role::Tool {
            let id = message.tool_call_id.as_deref().unwrap_or_default();
            if pending.remove(id).is_none() {
                return Err(invalid_request(format!(
                    "Google history contains unexpected tool result {id}"
                )));
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid_request(format!(
            "Google history is missing tool results for {}",
            pending.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

fn google_part(part: &ContentPart) -> Result<Value, ProviderError> {
    Ok(match part {
        ContentPart::Text { text } => json!({"text":text}),
        ContentPart::Think { think, encrypted } => {
            json!({"text":think,"thought":true,"thoughtSignature":encrypted})
        }
        ContentPart::ImageUrl { image_url } => google_media(&image_url.url)?,
        ContentPart::AudioUrl { audio_url } => google_media(&audio_url.url)?,
        ContentPart::VideoUrl { video_url } => google_media(&video_url.url)?,
    })
}

fn google_media(url: &str) -> Result<Value, ProviderError> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(json!({"fileData":{"fileUri":url}}));
    }
    let (mime, data) =
        parse_data_uri(url).ok_or_else(|| invalid_request("invalid Google media data URL"))?;
    Ok(json!({"inlineData":{"mimeType":mime,"data":data}}))
}

fn google_url(core: &ProviderCore, endpoint: &GoogleEndpoint) -> (String, String, AuthStyle) {
    let model = if core.model.starts_with("models/") || core.model.starts_with("publishers/") {
        core.model.clone()
    } else {
        format!("publishers/google/models/{}", core.model)
    };
    match endpoint {
        GoogleEndpoint::Gemini => (
            core.base_url.clone(),
            format!(
                "/v1beta/models/{}:streamGenerateContent?alt=sse",
                core.model.trim_start_matches("models/")
            ),
            AuthStyle::GoogleApiKey,
        ),
        GoogleEndpoint::VertexApiKey => (
            core.base_url.clone(),
            format!("/v1beta1/{model}:streamGenerateContent?alt=sse"),
            AuthStyle::GoogleApiKey,
        ),
        GoogleEndpoint::VertexServiceAccount { project, location } => {
            let base = if core.base_url.is_empty() {
                if location == "us" {
                    "https://aiplatform.us.rep.googleapis.com".into()
                } else {
                    format!("https://{location}-aiplatform.googleapis.com")
                }
            } else {
                core.base_url.clone()
            };
            (
                base,
                format!("/v1beta1/projects/{project}/locations/{location}/{model}:streamGenerateContent?alt=sse"),
                AuthStyle::Bearer,
            )
        }
    }
}

fn openai_chat_stream(
    body: crate::http::ByteStream,
    trace_id: Option<String>,
) -> ProviderEventStream {
    let mut state = ChatChunkState::new(trace_id);
    decode_sse(body, Box::new(move |data, queue| state.decode(data, queue)))
}

struct ChatChunkState {
    started: bool,
    ended: bool,
    trace_id: Option<String>,
    seen_calls: HashSet<u64>,
    early_arguments: HashMap<u64, String>,
}

impl ChatChunkState {
    fn new(trace_id: Option<String>) -> Self {
        Self {
            started: false,
            ended: false,
            trace_id,
            seen_calls: HashSet::new(),
            early_arguments: HashMap::new(),
        }
    }

    fn decode(
        &mut self,
        data: &str,
        queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>,
    ) {
        if data == "[DONE]" {
            if !self.ended {
                queue.push_back(Ok(ProviderStreamEvent::ResponseEnd));
                self.ended = true;
            }
            return;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                queue.push_back(Err(malformed_error(format!(
                    "invalid chat completion event: {error}"
                ))));
                return;
            }
        };
        if !self.started {
            queue.push_back(Ok(ProviderStreamEvent::ResponseStart {
                id: value.get("id").and_then(Value::as_str).map(str::to_owned),
                trace_id: self
                    .trace_id
                    .clone()
                    .map_or(OptionalNullable::Missing, OptionalNullable::Value),
            }));
            self.started = true;
        }
        if let Some(usage) = value.get("usage") {
            push_usage(queue, chat_usage(usage));
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            if let Some(usage) = choice.get("usage") {
                push_usage(queue, chat_usage(usage));
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    push_part(queue, StreamPart::Text { text: text.into() });
                }
                for key in ["reasoning_content", "reasoning_details", "reasoning"] {
                    if let Some(think) = delta
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        push_part(
                            queue,
                            StreamPart::Think {
                                think: think.into(),
                                encrypted: None,
                            },
                        );
                        break;
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let function = call.get("function").unwrap_or(&Value::Null);
                        let name = function.get("name").and_then(Value::as_str);
                        let arguments = function
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if let Some(name) = name {
                            let mut initial =
                                self.early_arguments.remove(&index).unwrap_or_default();
                            initial.push_str(arguments);
                            self.seen_calls.insert(index);
                            push_part(
                                queue,
                                StreamPart::Function {
                                    id: sanitize_tool_id(
                                        call.get("id")
                                            .and_then(Value::as_str)
                                            .unwrap_or(&format!("call_{index}")),
                                    ),
                                    name: name.into(),
                                    arguments: (!initial.is_empty()).then_some(initial),
                                    extras: BTreeMap::new(),
                                    stream_index: Some(StreamIndex::Number(index)),
                                },
                            );
                        } else if !arguments.is_empty() {
                            if self.seen_calls.contains(&index) {
                                push_part(
                                    queue,
                                    StreamPart::ToolCallPart {
                                        arguments_part: Some(arguments.into()),
                                        index: Some(StreamIndex::Number(index)),
                                    },
                                );
                            } else {
                                self.early_arguments
                                    .entry(index)
                                    .or_default()
                                    .push_str(arguments);
                            }
                        }
                    }
                }
            }
            if let Some(raw) = choice.get("finish_reason").and_then(Value::as_str) {
                queue.push_back(Ok(ProviderStreamEvent::Finish {
                    reason: Some(chat_finish(raw)),
                    raw_reason: Some(raw.into()),
                }));
            }
        }
    }
}

fn openai_responses_stream(body: crate::http::ByteStream) -> ProviderEventStream {
    let mut state = ResponsesState::default();
    decode_sse(body, Box::new(move |data, queue| state.decode(data, queue)))
}

#[derive(Default)]
struct ResponsesState {
    started: bool,
    ended: bool,
    calls: HashMap<String, StreamIndex>,
    arguments: BTreeMap<StreamIndex, String>,
}

impl ResponsesState {
    fn decode(
        &mut self,
        data: &str,
        queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>,
    ) {
        if data == "[DONE]" {
            self.end(queue);
            return;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                queue.push_back(Err(malformed_error(format!(
                    "invalid Responses event: {error}"
                ))));
                return;
            }
        };
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.created" | "response.in_progress" => {
                if !self.started {
                    let id = value
                        .pointer("/response/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    queue.push_back(Ok(ProviderStreamEvent::ResponseStart {
                        id,
                        trace_id: OptionalNullable::Missing,
                    }));
                    self.started = true;
                }
            }
            "response.output_text.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    push_part(queue, StreamPart::Text { text: text.into() });
                }
            }
            "response.output_item.added" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = value
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .map(StreamIndex::Number)
                        .or_else(|| {
                            item.get("id")
                                .and_then(Value::as_str)
                                .map(|id| StreamIndex::String(id.into()))
                        })
                        .unwrap_or(StreamIndex::Number(0));
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.calls.insert(call_id.into(), index.clone());
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.calls.insert(item_id.into(), index.clone());
                    }
                    let initial_arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned);
                    if let Some(arguments) = &initial_arguments {
                        self.arguments.insert(index.clone(), arguments.clone());
                    }
                    push_part(
                        queue,
                        StreamPart::Function {
                            id: sanitize_tool_id(call_id),
                            name: item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                            arguments: initial_arguments,
                            extras: BTreeMap::new(),
                            stream_index: Some(index),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let index = self.responses_index(&value);
                if let (Some(index), Some(delta)) =
                    (index.as_ref(), value.get("delta").and_then(Value::as_str))
                {
                    self.arguments
                        .entry(index.clone())
                        .or_default()
                        .push_str(delta);
                }
                push_part(
                    queue,
                    StreamPart::ToolCallPart {
                        arguments_part: value
                            .get("delta")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        index,
                    },
                );
            }
            "response.function_call_arguments.done" => {
                let index = self.responses_index(&value);
                let final_arguments = value
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.complete_arguments(index, final_arguments, queue);
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(think) = value.get("delta").and_then(Value::as_str) {
                    push_part(
                        queue,
                        StreamPart::Think {
                            think: think.into(),
                            encrypted: None,
                        },
                    );
                }
            }
            "response.output_item.done" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                    if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) {
                        push_part(
                            queue,
                            StreamPart::Think {
                                think: String::new(),
                                encrypted: Some(encrypted.into()),
                            },
                        );
                    }
                } else if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = value
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .map(StreamIndex::Number)
                        .or_else(|| {
                            item.get("id")
                                .and_then(Value::as_str)
                                .and_then(|id| self.calls.get(id).cloned())
                        });
                    let final_arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.complete_arguments(index, final_arguments, queue);
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = value.get("response").unwrap_or(&Value::Null);
                if let Some(usage) = response.get("usage") {
                    push_usage(queue, responses_usage(usage));
                }
                let (reason, raw) = if event_type == "response.completed" {
                    (FinishReason::Completed, "completed")
                } else {
                    let raw = response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("incomplete");
                    (
                        match raw {
                            "max_output_tokens" => FinishReason::Truncated,
                            "content_filter" => FinishReason::Filtered,
                            _ => FinishReason::Other,
                        },
                        raw,
                    )
                };
                queue.push_back(Ok(ProviderStreamEvent::Finish {
                    reason: Some(reason),
                    raw_reason: Some(raw.into()),
                }));
                self.end(queue);
            }
            "response.failed" | "error" => {
                let message = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Responses API failed");
                queue.push_back(Err(ProviderError::new(ProviderErrorKind::Other, message)));
            }
            _ => {}
        }
    }

    fn end(&mut self, queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>) {
        if !self.ended {
            queue.push_back(Ok(ProviderStreamEvent::ResponseEnd));
            self.ended = true;
        }
    }

    fn responses_index(&self, value: &Value) -> Option<StreamIndex> {
        value
            .get("output_index")
            .and_then(Value::as_u64)
            .map(StreamIndex::Number)
            .or_else(|| {
                value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .and_then(|id| self.calls.get(id).cloned())
            })
    }

    fn complete_arguments(
        &mut self,
        index: Option<StreamIndex>,
        final_arguments: &str,
        queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>,
    ) {
        let Some(index) = index else { return };
        let accumulated = self.arguments.entry(index.clone()).or_default();
        if !final_arguments.starts_with(accumulated.as_str()) {
            queue.push_back(Err(malformed_error(
                "final Responses tool arguments contradict streamed arguments",
            )));
            return;
        }
        let suffix = &final_arguments[accumulated.len()..];
        if !suffix.is_empty() {
            push_part(
                queue,
                StreamPart::ToolCallPart {
                    arguments_part: Some(suffix.to_owned()),
                    index: Some(index),
                },
            );
            accumulated.push_str(suffix);
        }
    }
}

fn anthropic_stream(body: crate::http::ByteStream) -> ProviderEventStream {
    let mut state = AnthropicState::default();
    decode_sse(body, Box::new(move |data, queue| state.decode(data, queue)))
}

#[derive(Default)]
struct AnthropicState {
    started: bool,
    ended: bool,
}

impl AnthropicState {
    fn decode(
        &mut self,
        data: &str,
        queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>,
    ) {
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                queue.push_back(Err(malformed_error(format!(
                    "invalid Anthropic event: {error}"
                ))));
                return;
            }
        };
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                if !self.started {
                    queue.push_back(Ok(ProviderStreamEvent::ResponseStart {
                        id: value
                            .pointer("/message/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        trace_id: OptionalNullable::Missing,
                    }));
                    self.started = true;
                }
                if let Some(usage) = value.pointer("/message/usage") {
                    push_usage(queue, anthropic_usage(usage));
                }
            }
            "content_block_start" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = value.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                        {
                            push_part(queue, StreamPart::Text { text: text.into() });
                        }
                    }
                    Some("thinking") => {
                        if let Some(think) = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                        {
                            push_part(
                                queue,
                                StreamPart::Think {
                                    think: think.into(),
                                    encrypted: None,
                                },
                            );
                        }
                    }
                    Some("redacted_thinking") => push_part(
                        queue,
                        StreamPart::Think {
                            think: String::new(),
                            encrypted: block.get("data").and_then(Value::as_str).map(str::to_owned),
                        },
                    ),
                    Some("tool_use") => push_part(
                        queue,
                        StreamPart::Function {
                            id: sanitize_tool_id(
                                block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or(&format!("call_{index}")),
                            ),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                            arguments: None,
                            extras: BTreeMap::new(),
                            stream_index: Some(StreamIndex::Number(index)),
                        },
                    ),
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(StreamIndex::Number);
                let delta = value.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            push_part(queue, StreamPart::Text { text: text.into() });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(think) = delta.get("thinking").and_then(Value::as_str) {
                            push_part(
                                queue,
                                StreamPart::Think {
                                    think: think.into(),
                                    encrypted: None,
                                },
                            );
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                            push_part(
                                queue,
                                StreamPart::Think {
                                    think: String::new(),
                                    encrypted: Some(signature.into()),
                                },
                            );
                        }
                    }
                    Some("input_json_delta") => push_part(
                        queue,
                        StreamPart::ToolCallPart {
                            arguments_part: delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            index,
                        },
                    ),
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(usage) = value.get("usage") {
                    push_usage(queue, anthropic_usage(usage));
                }
                if let Some(raw) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    queue.push_back(Ok(ProviderStreamEvent::Finish {
                        reason: Some(anthropic_finish(raw)),
                        raw_reason: Some(raw.into()),
                    }));
                }
            }
            "message_stop" => {
                if !self.ended {
                    queue.push_back(Ok(ProviderStreamEvent::ResponseEnd));
                    self.ended = true;
                }
            }
            "error" => queue.push_back(Err(ProviderError::new(
                ProviderErrorKind::Other,
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error"),
            ))),
            _ => {}
        }
    }
}

fn google_stream(body: crate::http::ByteStream) -> ProviderEventStream {
    let mut state = GoogleState::default();
    decode_sse(body, Box::new(move |data, queue| state.decode(data, queue)))
}

#[derive(Default)]
struct GoogleState {
    started: bool,
    ended: bool,
    call_sequence: u64,
}

impl GoogleState {
    fn decode(
        &mut self,
        data: &str,
        queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>,
    ) {
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                queue.push_back(Err(malformed_error(format!(
                    "invalid Google event: {error}"
                ))));
                return;
            }
        };
        if !self.started {
            queue.push_back(Ok(ProviderStreamEvent::ResponseStart {
                id: value
                    .get("responseId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                trace_id: OptionalNullable::Missing,
            }));
            self.started = true;
        }
        if let Some(usage) = value.get("usageMetadata") {
            push_usage(queue, google_usage(usage));
        }
        let candidates = value
            .get("candidates")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for candidate in candidates {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if part
                            .get("thought")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            push_part(
                                queue,
                                StreamPart::Think {
                                    think: text.into(),
                                    encrypted: part
                                        .get("thoughtSignature")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                },
                            );
                        } else {
                            push_part(queue, StreamPart::Text { text: text.into() });
                        }
                    }
                    if let Some(call) = part.get("functionCall") {
                        let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                        let upstream = call.get("id").and_then(Value::as_str).unwrap_or("call");
                        self.call_sequence += 1;
                        let id = sanitize_tool_id(&format!(
                            "{name}_{upstream}_{:08x}",
                            self.call_sequence
                        ));
                        let mut extras = BTreeMap::new();
                        if let Some(signature) = part.get("thoughtSignature") {
                            extras.insert("thoughtSignature".into(), signature.clone());
                        }
                        push_part(
                            queue,
                            StreamPart::Function {
                                id,
                                name: name.into(),
                                arguments: Some(
                                    call.get("args")
                                        .cloned()
                                        .unwrap_or_else(|| json!({}))
                                        .to_string(),
                                ),
                                extras,
                                stream_index: Some(StreamIndex::Number(self.call_sequence)),
                            },
                        );
                    }
                }
            }
            if let Some(raw) = candidate.get("finishReason").and_then(Value::as_str) {
                queue.push_back(Ok(ProviderStreamEvent::Finish {
                    reason: Some(google_finish(raw)),
                    raw_reason: Some(raw.into()),
                }));
                if !self.ended {
                    queue.push_back(Ok(ProviderStreamEvent::ResponseEnd));
                    self.ended = true;
                }
            }
        }
    }
}

fn chat_usage(value: &Value) -> TokenUsage {
    let input = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| value.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input);
    TokenUsage {
        input_other: input - cached,
        output: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        input_cache_read: cached,
        input_cache_creation: 0,
    }
}

fn responses_usage(value: &Value) -> TokenUsage {
    let input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input);
    TokenUsage {
        input_other: input - cached,
        output: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        input_cache_read: cached,
        input_cache_creation: 0,
    }
}

fn anthropic_usage(value: &Value) -> TokenUsage {
    TokenUsage {
        input_other: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        input_cache_read: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        input_cache_creation: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn google_usage(value: &Value) -> TokenUsage {
    let input = value
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input);
    TokenUsage {
        input_other: input - cached,
        output: value
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        input_cache_read: cached,
        input_cache_creation: 0,
    }
}

fn push_usage(queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>, usage: TokenUsage) {
    queue.push_back(Ok(ProviderStreamEvent::Usage { usage }));
}

fn push_part(queue: &mut VecDeque<Result<ProviderStreamEvent, ProviderError>>, part: StreamPart) {
    queue.push_back(Ok(ProviderStreamEvent::Part { part }));
}

fn chat_finish(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Completed,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Truncated,
        "content_filter" => FinishReason::Filtered,
        _ => FinishReason::Other,
    }
}

fn anthropic_finish(raw: &str) -> FinishReason {
    match raw {
        "end_turn" | "stop_sequence" => FinishReason::Completed,
        "max_tokens" => FinishReason::Truncated,
        "tool_use" => FinishReason::ToolCalls,
        "pause_turn" => FinishReason::Paused,
        "refusal" => FinishReason::Filtered,
        _ => FinishReason::Other,
    }
}

fn google_finish(raw: &str) -> FinishReason {
    match raw {
        "STOP" => FinishReason::Completed,
        "MAX_TOKENS" => FinishReason::Truncated,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            FinishReason::Filtered
        }
        _ => FinishReason::Other,
    }
}

fn responses_developer_role(model: &str) -> bool {
    ["gpt-4.1", "gpt-5-codex", "o1", "o3", "o4-mini"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

fn reasoning_model(model: &str) -> bool {
    let bytes = model.as_bytes();
    bytes.first() == Some(&b'o') && bytes.get(1).is_some_and(u8::is_ascii_digit)
}

fn anthropic_thinking_budget(effort: &str) -> u64 {
    match effort {
        "low" => 1024,
        "medium" => 4096,
        "high" | "on" => 32_000,
        _ => 4096,
    }
}

fn anthropic_output_limit(model: &str, requested: Option<u64>) -> u64 {
    let recognized_claude = model.to_ascii_lowercase().contains("claude");
    let ceiling = recognized_claude
        .then(|| parse_anthropic_version(model))
        .flatten()
        .and_then(|(family, major, minor)| match family.as_str() {
            "fable" | "mythos" if major == 5 => Some(128_000),
            "opus" if major == 4 => Some(match minor.unwrap_or(0) {
                6.. => 128_000,
                5 => 64_000,
                _ => 32_000,
            }),
            "sonnet" if major == 5 => Some(128_000),
            "sonnet" if major == 4 => Some(if minor.unwrap_or(0) >= 6 {
                128_000
            } else {
                64_000
            }),
            "haiku" if major == 4 => Some(64_000),
            "opus" | "sonnet" | "haiku" if major == 3 => {
                Some(if minor.unwrap_or(0) >= 5 { 8192 } else { 4096 })
            }
            _ => None,
        });
    match (requested, ceiling) {
        (Some(requested), Some(ceiling)) => requested.min(ceiling),
        (Some(requested), None) => requested,
        (None, Some(ceiling)) => ceiling,
        (None, None) => 128_000,
    }
}

fn anthropic_adaptive_model(model: &str) -> bool {
    let normalized = model.to_ascii_lowercase();
    if normalized.contains("mythos-preview") {
        return true;
    }
    let Some((family, major, minor)) = parse_anthropic_version(model) else {
        return true;
    };
    match family.as_str() {
        "opus" => major > 4 || (major == 4 && minor.unwrap_or(0) >= 6),
        "sonnet" => major >= 5 || (major == 4 && minor.unwrap_or(0) >= 6),
        "haiku" => major > 4 || (major == 4 && minor.unwrap_or(0) >= 6),
        "fable" | "mythos" => major >= 5,
        _ => true,
    }
}

fn parse_anthropic_version(model: &str) -> Option<(String, u64, Option<u64>)> {
    let tokens = model
        .to_ascii_lowercase()
        .split(['-', '_', '.'])
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let families = ["opus", "sonnet", "haiku", "fable", "mythos"];
    for (index, token) in tokens.iter().enumerate() {
        if !families.contains(&token.as_str()) {
            continue;
        }
        if let Some(major) = tokens.get(index + 1).and_then(|value| value.parse().ok()) {
            let minor = tokens.get(index + 2).and_then(|value| value.parse().ok());
            return Some((token.clone(), major, minor));
        }
        if index >= 2 {
            if let (Ok(major), Ok(minor)) = (
                tokens[index - 2].parse::<u64>(),
                tokens[index - 1].parse::<u64>(),
            ) {
                return Some((token.clone(), major, Some(minor)));
            }
        }
        if index >= 1 {
            if let Ok(major) = tokens[index - 1].parse::<u64>() {
                return Some((token.clone(), major, None));
            }
        }
    }
    None
}

fn google_thinking_config(model: &str, effort: &str) -> Value {
    if model.starts_with("gemini-3") {
        json!({"thinkingLevel":match effort { "off" => "MINIMAL", "low" => "LOW", "high" => "HIGH", _ => "MEDIUM" },"includeThoughts":effort != "off"})
    } else {
        json!({"thinkingBudget":match effort { "off" => 0, "low" => 1024, "high" => 32_000, _ => 4096 },"includeThoughts":effort != "off"})
    }
}

fn copy_generation_metadata(body: &mut Map<String, Value>, metadata: &BTreeMap<String, Value>) {
    for key in [
        "temperature",
        "top_p",
        "top_k",
        "n",
        "presence_penalty",
        "frequency_penalty",
        "stop",
        "prompt_cache_key",
        "context_management",
        "metadata",
    ] {
        if let Some(value) = metadata.get(key) {
            body.insert(key.into(), value.clone());
        }
    }
    if let Some(extra) = metadata.get("extra_body").and_then(Value::as_object) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }
}

fn parse_data_uri(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("data:")?;
    let (metadata, data) = rest.split_once(',')?;
    let mime = metadata.strip_suffix(";base64")?;
    STANDARD.decode(data).ok()?;
    Some((mime, data))
}

fn is_codex_base_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("chatgpt.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().trim_end_matches('/') == "/backend-api/codex"
        && url.query().is_none()
        && url.fragment().is_none()
}

#[derive(Default)]
struct ToolIdMap {
    mapped: HashMap<String, String>,
    used: HashSet<String>,
}

impl ToolIdMap {
    fn register(&mut self, original: &str) -> String {
        if let Some(value) = self.mapped.get(original) {
            return value.clone();
        }
        let base = sanitize_tool_id(original);
        let mut candidate = base.clone();
        let mut suffix = 2;
        while !self.used.insert(candidate.clone()) {
            candidate = truncate_with_suffix(&base, suffix);
            suffix += 1;
        }
        self.mapped.insert(original.into(), candidate.clone());
        candidate
    }

    fn resolve(&mut self, original: &str) -> String {
        self.mapped
            .get(original)
            .cloned()
            .unwrap_or_else(|| self.register(original))
    }
}

fn sanitize_tool_id(value: &str) -> String {
    let prefix = value.split('|').next().unwrap_or(value);
    let mut output = prefix
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect::<String>();
    if output.is_empty() {
        output = "call".into();
    }
    output
}

fn truncate_with_suffix(base: &str, suffix: usize) -> String {
    let suffix = format!("_{suffix}");
    let keep = 64usize.saturating_sub(suffix.len());
    format!("{}{}", base.chars().take(keep).collect::<String>(), suffix)
}

fn append_multipart_field(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

fn escape_header_value(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if matches!(character, '\r' | '\n' | '"') {
                '_'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_util::{stream, FutureExt, StreamExt};
    use mycel_agent_protocol::{SecretString, StreamAssembler};

    use super::*;
    use crate::http::{ByteStream, HttpResponse, TransportFuture};

    type FakeResponse = (u16, BTreeMap<String, String>, Vec<Vec<u8>>);

    #[derive(Default)]
    struct FakeTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<FakeResponse>>,
    }

    impl FakeTransport {
        fn response(&self, status: u16, headers: BTreeMap<String, String>, chunks: &[&str]) {
            self.responses.lock().expect("responses").push_back((
                status,
                headers,
                chunks.iter().map(|v| v.as_bytes().to_vec()).collect(),
            ));
        }
    }

    impl HttpTransport for FakeTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("fixture response");
            async move {
                let body: ByteStream = Box::pin(stream::iter(
                    response
                        .2
                        .into_iter()
                        .map(|chunk| Ok(bytes::Bytes::from(chunk))),
                ));
                Ok(HttpResponse {
                    status: response.0,
                    headers: response.1,
                    body,
                })
            }
            .boxed()
        }
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            provider: "test".into(),
            model: "gpt-4.1".into(),
            system_prompt: "system".into(),
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "read".into(),
                parameters: json!({"type":"object"}),
                deferred: false,
            }],
            history: vec![Message::user("hello")],
            thinking_effort: None,
            max_completion_tokens: Some(64),
            response_format: None,
            metadata: BTreeMap::new(),
        }
    }

    fn auth() -> ProviderRequestAuth {
        ProviderRequestAuth {
            api_key: Some(SecretString::new("secret")),
            headers: BTreeMap::new(),
        }
    }

    async fn assemble(mut stream: ProviderEventStream) -> mycel_agent_protocol::GenerateResult {
        let mut assembler = StreamAssembler::default();
        while let Some(event) = stream.next().await {
            assembler
                .push(event.expect("provider event"))
                .expect("valid stream");
        }
        assembler.finish().expect("assembled")
    }

    #[tokio::test]
    async fn openai_chat_records_request_and_decodes_interleaved_tool_call() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, BTreeMap::new(), &[
            "da",
            "ta: {\"id\":\"r1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":\"}}]}}]}\r",
            "\n\r\n",
            "data: {\"id\":\"r1\",\"future\":\"ignored\"}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"bad|id\",\"function\":{\"name\":\"read\",\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let provider = OpenAiChatProvider::new(
            "gpt-4.1",
            Some("https://local.test/v1".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let result = assemble(provider.stream(&request(), &auth()).await.expect("stream")).await;
        assert_eq!(result.message.tool_calls[0].id, "bad");
        assert_eq!(
            result.message.tool_calls[0].arguments.as_deref(),
            Some("{\"x\":1}")
        );
        let sent = transport.requests.lock().expect("requests");
        let body: Value = serde_json::from_slice(&sent[0].body).expect("json");
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].url, "https://local.test/v1/chat/completions");
        assert_eq!(sent[0].timeout, REQUEST_TIMEOUT);
        assert_eq!(
            sent[0].headers,
            BTreeMap::from([
                ("accept".into(), "text/event-stream".into()),
                ("authorization".into(), "Bearer secret".into()),
                ("content-type".into(), "application/json".into()),
            ])
        );
        assert_eq!(
            body,
            json!({
                "model":"gpt-4.1",
                "messages":[
                    {"role":"system","content":"system"},
                    {"role":"user","content":"hello"}
                ],
                "stream":true,
                "stream_options":{"include_usage":true},
                "tools":[{"type":"function","function":{
                    "name":"read","description":"read","parameters":{"type":"object"}
                }}],
                "max_tokens":64
            })
        );
    }

    #[tokio::test]
    async fn anthropic_records_cache_control_and_decodes_signed_thinking() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, BTreeMap::new(), &[
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":2}}",
            "}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool|1\",\"name\":\"read\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n\n",
            "data: {\"type\":\"future_event\"}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ]);
        let provider = AnthropicProvider::new(
            "claude-sonnet-4",
            Some("https://local.test".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let mut anthropic_request = request();
        anthropic_request.model = "claude-sonnet-4".into();
        let result = assemble(
            provider
                .stream(&anthropic_request, &auth())
                .await
                .expect("stream"),
        )
        .await;
        assert!(
            matches!(&result.message.content[0], ContentPart::Think { encrypted: Some(value), .. } if value == "sig")
        );
        assert_eq!(result.message.tool_calls[0].id, "tool");
        assert_eq!(
            result.message.tool_calls[0].arguments.as_deref(),
            Some("{\"x\":1}")
        );
        let sent = transport.requests.lock().expect("requests");
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].url, "https://local.test/v1/messages");
        assert_eq!(
            sent[0].headers,
            BTreeMap::from([
                ("accept".into(), "text/event-stream".into()),
                (
                    "anthropic-beta".into(),
                    "interleaved-thinking-2025-05-14".into()
                ),
                ("anthropic-version".into(), "2023-06-01".into()),
                ("content-type".into(), "application/json".into()),
                ("x-api-key".into(), "secret".into()),
            ])
        );
        let body: Value = serde_json::from_slice(&sent[0].body).expect("Anthropic body");
        assert_eq!(
            body,
            json!({
                "model":"claude-sonnet-4",
                "messages":[{"role":"user","content":[{
                    "type":"text","text":"hello","cache_control":{"type":"ephemeral"}
                }]}],
                "max_tokens":64,
                "stream":true,
                "system":[{"type":"text","text":"system","cache_control":{"type":"ephemeral"}}],
                "tools":[{
                    "name":"read","description":"read","input_schema":{"type":"object"},
                    "cache_control":{"type":"ephemeral"}
                }]
            })
        );
    }

    #[tokio::test]
    async fn kimi_anthropic_protocol_uses_beta_route_and_replays_unsigned_thinking() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            BTreeMap::new(),
            &[
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m-kimi\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            ],
        );
        let provider = AnthropicProvider::kimi_protocol(
            "kimi-anthropic",
            "https://local.test/v1".into(),
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let mut kimi_request = request();
        kimi_request.model = "kimi-anthropic".into();
        kimi_request.thinking_effort =
            Some(mycel_agent_protocol::ThinkingEffort::new("high").expect("effort"));
        kimi_request.history.push(Message::assistant(
            vec![ContentPart::Think {
                think: "unsigned prior plan".into(),
                encrypted: None,
            }],
            Vec::new(),
        ));
        let result = assemble(
            provider
                .stream(&kimi_request, &auth())
                .await
                .expect("stream"),
        )
        .await;
        assert_eq!(result.message.text(""), "ok");
        let sent = transport.requests.lock().expect("requests");
        assert_eq!(sent[0].url, "https://local.test/v1/messages?beta=true");
        assert!(!sent[0].headers.contains_key("anthropic-beta"));
        let body: Value = serde_json::from_slice(&sent[0].body).expect("Kimi Anthropic body");
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({"type":"thinking","thinking":"unsigned prior plan"})
        );
        assert_eq!(body["thinking"], json!({"type":"enabled"}));
        assert_eq!(body["output_config"], json!({"effort":"high"}));
        assert!(body.get("betas").is_none());
    }

    #[tokio::test]
    async fn responses_ignores_unknown_events_and_finishes() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, BTreeMap::new(), &[
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n",
            "\n",
            "data: {\"type\":\"future.event\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"plan\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"cipher\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"x\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":1,\"arguments\":\"{\\\"x\\\":1}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
        ]);
        let provider = OpenAiResponsesProvider::new(
            "gpt-4.1",
            Some("https://local.test/v1".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .expect("provider")
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let mut responses_request = request();
        responses_request.history.push(Message::assistant(
            vec![ContentPart::Think {
                think: "prior plan".into(),
                encrypted: Some("prior-cipher".into()),
            }],
            Vec::new(),
        ));
        let result = assemble(
            provider
                .stream(&responses_request, &auth())
                .await
                .expect("stream"),
        )
        .await;
        assert_eq!(result.message.text(""), "ok");
        assert_eq!(
            result.message.tool_calls[0].arguments.as_deref(),
            Some("{\"x\":1}")
        );
        assert!(result.message.content.iter().any(
            |part| matches!(part, ContentPart::Think { encrypted: Some(value), .. } if value == "cipher")
        ));
        let sent = transport.requests.lock().expect("requests");
        assert_eq!(sent[0].method, "POST");
        assert_eq!(sent[0].url, "https://local.test/v1/responses");
        assert_eq!(
            sent[0].headers,
            BTreeMap::from([
                ("accept".into(), "text/event-stream".into()),
                ("authorization".into(), "Bearer secret".into()),
                ("content-type".into(), "application/json".into()),
            ])
        );
        let body: Value = serde_json::from_slice(&sent[0].body).expect("Responses body");
        assert_eq!(
            body,
            json!({
                "model":"gpt-4.1",
                "input":[
                    {"role":"user","content":[{"type":"input_text","text":"hello"}]},
                    {"type":"reasoning","summary":[{"type":"summary_text","text":"prior plan"}],"encrypted_content":"prior-cipher"}
                ],
                "instructions":"system",
                "store":false,
                "stream":true,
                "tools":[{
                    "type":"function","name":"read","description":"read",
                    "parameters":{"type":"object"},"strict":false
                }],
                "max_output_tokens":64
            })
        );
    }

    #[tokio::test]
    async fn codex_subscription_responses_preserve_app_server_headers_and_omit_token_limit() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            BTreeMap::new(),
            &[
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"codex\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            ],
        );
        let provider = OpenAiResponsesProvider::new(
            "gpt-4.1",
            Some("https://chatgpt.com/backend-api/codex".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .expect("Codex provider")
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let codex_auth = ProviderRequestAuth {
            api_key: Some(SecretString::new("subscription-token")),
            headers: BTreeMap::from([
                ("ChatGPT-Account-ID".into(), SecretString::new("acct")),
                ("originator".into(), SecretString::new("mycel")),
                ("version".into(), SecretString::new("1.2.3")),
            ]),
        };
        let result = assemble(
            provider
                .stream(&request(), &codex_auth)
                .await
                .expect("stream"),
        )
        .await;
        assert_eq!(result.message.text(""), "ok");
        let sent = transport.requests.lock().expect("requests");
        assert_eq!(
            sent[0].url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            sent[0].headers["authorization"],
            "Bearer subscription-token"
        );
        assert_eq!(sent[0].headers["ChatGPT-Account-ID"], "acct");
        assert_eq!(sent[0].headers["originator"], "mycel");
        assert_eq!(sent[0].headers["version"], "1.2.3");
        let body: Value = serde_json::from_slice(&sent[0].body).expect("Codex body");
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn google_uses_exact_gemini_stream_path() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, BTreeMap::new(), &[
            "data: {\"responseId\":\"g\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"plan\",\"thought\":true,\"thoughtSignature\":\"sig\"}]}}]}",
            "\n\n",
            "data: {\"future\":true}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"},{\"functionCall\":{\"id\":\"upstream\",\"name\":\"read\",\"args\":{\"x\":1}},\"thoughtSignature\":\"call-sig\"}]},\"finishReason\":\"STOP\"}]}\n\n",
        ]);
        let provider = GoogleProvider::new(
            "gemini-2.5-pro",
            GoogleEndpoint::Gemini,
            Some("https://local.test".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let mut google_request = request();
        google_request.model = "gemini-2.5-pro".into();
        let result = assemble(
            provider
                .stream(&google_request, &auth())
                .await
                .expect("stream"),
        )
        .await;
        assert_eq!(result.message.text(""), "ok");
        assert!(matches!(
            &result.message.content[0],
            ContentPart::Think {
                think,
                encrypted: Some(signature)
            } if think == "plan" && signature == "sig"
        ));
        assert_eq!(result.message.tool_calls[0].name, "read");
        assert_eq!(
            result.message.tool_calls[0].extras["thoughtSignature"],
            "call-sig"
        );
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(
            requests[0].url,
            "https://local.test/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].headers,
            BTreeMap::from([
                ("accept".into(), "text/event-stream".into()),
                ("content-type".into(), "application/json".into()),
                ("x-goog-api-key".into(), "secret".into()),
            ])
        );
        let body: Value = serde_json::from_slice(&requests[0].body).expect("Google JSON body");
        assert_eq!(
            body,
            json!({
                "contents":[{"role":"user","parts":[{"text":"hello"}]}],
                "systemInstruction":{"parts":[{"text":"system"}]},
                "generationConfig":{"maxOutputTokens":64},
                "tools":[{"functionDeclarations":[{
                    "name":"read","description":"read","parametersJsonSchema":{"type":"object"}
                }]}]
            })
        );
    }

    #[tokio::test]
    async fn vertex_service_account_uses_regional_project_path_and_bearer() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, BTreeMap::new(), &[
            "data: {\"responseId\":\"v\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"o",
            "k\"}]},\"finishReason\":\"STOP\"}]}\n\n",
        ]);
        let provider = GoogleProvider::new(
            "gemini-2.5-pro",
            GoogleEndpoint::VertexServiceAccount {
                project: "project-1".into(),
                location: "us-central1".into(),
            },
            None,
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let mut vertex_request = request();
        vertex_request.model = "gemini-2.5-pro".into();
        let result = assemble(
            provider
                .stream(&vertex_request, &auth())
                .await
                .expect("stream"),
        )
        .await;
        assert_eq!(result.message.text(""), "ok");
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(
            requests[0].url,
            "https://us-central1-aiplatform.googleapis.com/v1beta1/projects/project-1/locations/us-central1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            requests[0].headers,
            BTreeMap::from([
                ("accept".into(), "text/event-stream".into()),
                ("authorization".into(), "Bearer secret".into()),
                ("content-type".into(), "application/json".into()),
            ])
        );
    }

    #[tokio::test]
    async fn kimi_upload_is_multipart_and_returns_ms_uri() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, BTreeMap::new(), &["{\"id\":\"file-1\"}"]);
        let provider = KimiProvider::new(
            "kimi-k2",
            Some("https://local.test/v1".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let uri = provider
            .upload_video("clip\"\r\n.mp4", "video/mp4", b"video", &auth())
            .await
            .expect("upload");
        assert_eq!(uri, "ms://file-1");
        {
            let requests = transport.requests.lock().expect("requests");
            assert_eq!(requests[0].url, "https://local.test/v1/files");
            assert_eq!(requests[0].method, "POST");
            assert_eq!(requests[0].headers["accept"], "application/json");
            assert_eq!(requests[0].headers["authorization"], "Bearer secret");
            let content_type = &requests[0].headers["content-type"];
            let boundary = content_type
                .strip_prefix("multipart/form-data; boundary=")
                .expect("multipart boundary");
            let body = String::from_utf8_lossy(&requests[0].body).replace(boundary, "BOUNDARY");
            assert_eq!(
                body,
                "--BOUNDARY\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nvideo\r\n--BOUNDARY\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip___.mp4\"\r\nContent-Type: video/mp4\r\n\r\nvideo\r\n--BOUNDARY--\r\n"
            );
        }
        let error = provider
            .upload_video("clip.txt", "text/plain", b"bad", &auth())
            .await
            .expect_err("non-video MIME must fail before transport");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(transport.requests.lock().expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn kimi_stream_preserves_reasoning_trace_and_dynamic_tools() {
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            BTreeMap::from([("x-trace-id".into(), "trace-1".into())]),
            &[
                "data: {\"id\":\"k1\",\"choices\":[{\"delta\":{\"reasoning_content\":\"pl",
                "an\"}}]}\n\n",
                "data: {\"id\":\"k1\",\"unknown\":true}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        let provider = KimiProvider::new(
            "kimi-k2",
            Some("https://local.test/v1".into()),
            BTreeMap::new(),
            transport.clone(),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let mut kimi_request = request();
        kimi_request.model = "kimi-k2".into();
        kimi_request.thinking_effort =
            Some(mycel_agent_protocol::ThinkingEffort::new("high").expect("thinking effort"));
        kimi_request.history.insert(
            0,
            Message {
                role: Role::System,
                name: None,
                content: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
                tools: vec![ToolDefinition {
                    name: "$web_search".into(),
                    description: String::new(),
                    parameters: json!({"type":"object"}),
                    deferred: false,
                }],
            },
        );
        let result = assemble(
            provider
                .stream(&kimi_request, &auth())
                .await
                .expect("stream"),
        )
        .await;
        assert_eq!(result.trace_id.value().map(String::as_str), Some("trace-1"));
        assert!(
            matches!(&result.message.content[0], ContentPart::Think { think, .. } if think == "plan")
        );
        assert_eq!(result.message.text(""), "answer");
        let sent = transport.requests.lock().expect("requests");
        let body: Value = serde_json::from_slice(&sent[0].body).expect("Kimi body");
        assert_eq!(sent[0].url, "https://local.test/v1/chat/completions");
        assert_eq!(
            sent[0].headers,
            BTreeMap::from([
                ("accept".into(), "text/event-stream".into()),
                ("authorization".into(), "Bearer secret".into()),
                ("content-type".into(), "application/json".into()),
            ])
        );
        assert_eq!(
            body,
            json!({
                "model":"kimi-k2",
                "messages":[
                    {"role":"system","content":"system"},
                    {"role":"system","content":"","tools":[{
                        "type":"builtin_function","function":{"name":"$web_search"}
                    }]},
                    {"role":"user","content":"hello"}
                ],
                "stream":true,
                "stream_options":{"include_usage":true},
                "tools":[{"type":"function","function":{
                    "name":"read","description":"read","parameters":{"type":"object"}
                }}],
                "max_completion_tokens":64,
                "thinking":{"type":"enabled","effort":"high"}
            })
        );
    }

    #[test]
    fn kimi_schema_expands_refs_infers_types_and_preserves_cycles() {
        let schema = json!({
            "type":"object",
            "properties":{
                "mode":{"$ref":"#/$defs/mode"},
                "node":{"$ref":"#/$defs/node"}
            },
            "$defs":{
                "mode":{"type":"number","enum":["fast","safe"]},
                "node":{"type":"object","properties":{"next":{"$ref":"#/$defs/node"}}}
            }
        });
        let normalized = normalize_kimi_schema(&schema);
        assert_eq!(normalized["properties"]["mode"]["type"], "string");
        assert_eq!(
            normalized["properties"]["mode"]["enum"],
            json!(["fast", "safe"])
        );
        assert_eq!(
            normalized["properties"]["node"]["properties"]["next"]["$ref"],
            "#/$defs/node"
        );
    }

    #[test]
    fn anthropic_profiles_clamp_output_and_select_thinking_mode() {
        assert_eq!(anthropic_output_limit("claude-opus-4-5", None), 64_000);
        assert_eq!(
            anthropic_output_limit("claude-3-5-sonnet", Some(99_000)),
            8192
        );
        assert!(anthropic_adaptive_model("claude-opus-4-7"));
        assert!(!anthropic_adaptive_model("claude-sonnet-4-5"));
        assert!(anthropic_adaptive_model("custom-compatible-model"));

        let mut claude_request = request();
        claude_request.model = "claude-sonnet-4".into();
        claude_request.history.push(Message::assistant(
            vec![ContentPart::Think {
                think: "unsigned".into(),
                encrypted: None,
            }],
            Vec::new(),
        ));
        let body = anthropic_body(
            &claude_request,
            false,
            &["interleaved-thinking-2025-05-14".into()],
            false,
            false,
        )
        .expect("Anthropic body");
        assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    }
}
