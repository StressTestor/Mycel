use mycel_agent_protocol::{ContentPart, ExecutableToolOutput, ExecutableToolResult, MediaUrl};
use serde_json::Value;

pub const MCP_MAX_OUTPUT_CHARS: usize = 100_000;
pub const MCP_MAX_BINARY_PART_BYTES: usize = 10 * 1024 * 1024;

const TRUNCATION_NOTICE: &str =
    "\n\n[Output truncated: exceeded 100000 character limit. Use pagination or a more specific query.]";

/// Converts the structural `tools/call` result into the runtime's canonical
/// tool output. Unknown content blocks are ignored, text is globally bounded,
/// and inline/linked media has an independent per-part size ceiling.
pub fn bounded_mcp_tool_result(
    value: &Value,
    qualified_tool_name: &str,
) -> Result<ExecutableToolResult, McpOutputError> {
    let object = value.as_object().ok_or(McpOutputError::ResultNotObject)?;
    if let Some(legacy) = object.get("toolResult") {
        let text = legacy
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| legacy.to_string());
        return Ok(budget_parts(
            vec![ContentPart::text(text)],
            false,
            qualified_tool_name,
        ));
    }
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or(McpOutputError::ContentNotArray)?;
    let mut parts = Vec::new();
    let mut binary_truncated = false;
    for block in content {
        if let Some(part) = convert_block(block, &mut binary_truncated) {
            parts.push(part);
        }
    }
    Ok(budget_parts(
        parts,
        object.get("isError").and_then(Value::as_bool) == Some(true),
        qualified_tool_name,
    )
    .with_truncated(binary_truncated))
}

trait WithTruncated {
    fn with_truncated(self, truncated: bool) -> Self;
}

impl WithTruncated for ExecutableToolResult {
    fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated |= truncated;
        self
    }
}

fn convert_block(block: &Value, truncated: &mut bool) -> Option<ContentPart> {
    let object = block.as_object()?;
    match object.get("type").and_then(Value::as_str)? {
        "text" => object
            .get("text")
            .and_then(Value::as_str)
            .map(ContentPart::text),
        "image" => inline_media(object, "image", "image/png", truncated),
        "audio" => inline_media(object, "audio", "audio/mpeg", truncated),
        "resource" => embedded_resource(object.get("resource")?, truncated),
        "resource_link" => resource_link(object, truncated),
        _ => None,
    }
}

fn inline_media(
    object: &serde_json::Map<String, Value>,
    kind: &str,
    default_mime: &str,
    truncated: &mut bool,
) -> Option<ContentPart> {
    let data = object.get("data")?.as_str()?;
    let mime = object
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or(default_mime);
    if base64_decoded_upper_bound(data.len()) > MCP_MAX_BINARY_PART_BYTES {
        *truncated = true;
        return Some(ContentPart::text(binary_notice(kind)));
    }
    media_part(kind, format!("data:{mime};base64,{data}"))
}

fn embedded_resource(resource: &Value, truncated: &mut bool) -> Option<ContentPart> {
    let object = resource.as_object()?;
    if let Some(text) = object.get("text").and_then(Value::as_str) {
        return Some(ContentPart::text(text));
    }
    let blob = object.get("blob").and_then(Value::as_str)?;
    let mime = object
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let kind = media_kind(mime)?;
    if base64_decoded_upper_bound(blob.len()) > MCP_MAX_BINARY_PART_BYTES {
        *truncated = true;
        return Some(ContentPart::text(binary_notice(kind)));
    }
    media_part(kind, format!("data:{mime};base64,{blob}"))
}

fn resource_link(
    object: &serde_json::Map<String, Value>,
    truncated: &mut bool,
) -> Option<ContentPart> {
    let uri = object.get("uri")?.as_str()?;
    let mime = object
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let kind = media_kind(mime)?;
    if uri.len() > encoded_binary_char_cap() {
        *truncated = true;
        return Some(ContentPart::text(binary_notice(kind)));
    }
    media_part(kind, uri.to_owned())
}

fn media_kind(mime: &str) -> Option<&'static str> {
    if mime.starts_with("image/") {
        Some("image")
    } else if mime.starts_with("audio/") {
        Some("audio")
    } else if mime.starts_with("video/") {
        Some("video")
    } else {
        None
    }
}

fn media_part(kind: &str, url: String) -> Option<ContentPart> {
    let url = MediaUrl { url, id: None };
    match kind {
        "image" => Some(ContentPart::ImageUrl { image_url: url }),
        "audio" => Some(ContentPart::AudioUrl { audio_url: url }),
        "video" => Some(ContentPart::VideoUrl { video_url: url }),
        _ => None,
    }
}

fn budget_parts(
    mut parts: Vec<ContentPart>,
    is_error: bool,
    qualified_tool_name: &str,
) -> ExecutableToolResult {
    // Oversized media is represented by a small runtime-generated notice.
    // Pull those notices out of the user-text budget before truncation so a
    // preceding 100k text block cannot erase the fact that media was dropped.
    // Exact matching plus deduplication bounds this administrative tail to
    // one notice per supported media kind.
    let mut binary_notices = std::collections::BTreeSet::new();
    parts.retain(|part| {
        let Some(text) = part.as_text() else {
            return true;
        };
        if [
            binary_notice("image"),
            binary_notice("audio"),
            binary_notice("video"),
        ]
        .contains(&text.to_owned())
        {
            binary_notices.insert(text.to_owned());
            false
        } else {
            true
        }
    });
    let has_media = parts.iter().any(is_media);
    let has_text = parts
        .iter()
        .any(|part| part.as_text().is_some_and(|text| !text.is_empty()));
    if has_media && !has_text {
        parts.insert(
            0,
            ContentPart::text(format!("<mcp_tool_result name=\"{qualified_tool_name}\">")),
        );
        parts.push(ContentPart::text("</mcp_tool_result>"));
    }

    let mut remaining = MCP_MAX_OUTPUT_CHARS;
    let mut output = Vec::new();
    let mut truncated = false;
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                let (prefix, used, cut) = char_prefix(&text, remaining);
                if used > 0 {
                    output.push(ContentPart::text(prefix));
                }
                remaining = remaining.saturating_sub(used);
                truncated |= cut;
            }
            ContentPart::Think { think, encrypted } => {
                let (prefix, used, cut) = char_prefix(&think, remaining);
                if used > 0 {
                    output.push(ContentPart::Think {
                        think: prefix,
                        encrypted: if cut { None } else { encrypted },
                    });
                }
                remaining = remaining.saturating_sub(used);
                truncated |= cut;
            }
            media => output.push(media),
        }
    }
    if truncated {
        append_notice(&mut output);
    }
    output.extend(binary_notices.into_iter().map(ContentPart::text));

    let output = if output.len() == 1 {
        match output.pop().expect("single output part") {
            ContentPart::Text { text } => ExecutableToolOutput::Text(text),
            part => ExecutableToolOutput::Parts(vec![part]),
        }
    } else {
        ExecutableToolOutput::Parts(output)
    };
    ExecutableToolResult {
        output,
        is_error,
        stop_turn: false,
        message: None,
        note: None,
        truncated,
    }
}

fn char_prefix(value: &str, maximum: usize) -> (String, usize, bool) {
    if maximum == 0 {
        return (String::new(), 0, !value.is_empty());
    }
    let mut end = value.len();
    let mut count = 0;
    for (index, _) in value.char_indices() {
        if count == maximum {
            end = index;
            break;
        }
        count += 1;
    }
    let cut = end < value.len();
    (value[..end].to_owned(), count.min(maximum), cut)
}

fn append_notice(parts: &mut Vec<ContentPart>) {
    for part in parts.iter_mut().rev() {
        if let ContentPart::Text { text } = part {
            text.push_str(TRUNCATION_NOTICE);
            return;
        }
    }
    parts.push(ContentPart::text(TRUNCATION_NOTICE));
}

fn is_media(part: &ContentPart) -> bool {
    matches!(
        part,
        ContentPart::ImageUrl { .. } | ContentPart::AudioUrl { .. } | ContentPart::VideoUrl { .. }
    )
}

fn base64_decoded_upper_bound(encoded_length: usize) -> usize {
    encoded_length.saturating_add(3) / 4 * 3
}

fn encoded_binary_char_cap() -> usize {
    MCP_MAX_BINARY_PART_BYTES
        .saturating_mul(4)
        .saturating_add(2)
        / 3
}

fn binary_notice(kind: &str) -> String {
    format!(
        "[{kind}_url dropped: exceeds {} MiB per-part limit. Try a smaller resource.]",
        MCP_MAX_BINARY_PART_BYTES / (1024 * 1024)
    )
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpOutputError {
    #[error("MCP tool result must be an object")]
    ResultNotObject,
    #[error("MCP tool result content must be an array")]
    ContentNotArray,
}

#[cfg(test)]
mod tests {
    use mycel_agent_protocol::ExecutableToolOutput;
    use serde_json::json;

    use super::*;

    #[test]
    fn text_and_binary_outputs_are_independently_bounded() {
        let result = bounded_mcp_tool_result(
            &json!({
                "content": [
                    {"type":"text", "text":"x".repeat(MCP_MAX_OUTPUT_CHARS + 5)},
                    {"type":"image", "data":"a".repeat(encoded_binary_char_cap() + 1), "mimeType":"image/png"}
                ]
            }),
            "mcp__s__t",
        )
        .expect("result");
        assert!(result.truncated);
        let ExecutableToolOutput::Parts(parts) = result.output else {
            panic!("expected mixed parts")
        };
        assert!(parts.iter().any(|part| part
            .as_text()
            .is_some_and(|text| text.contains("Output truncated"))));
        assert!(parts.iter().any(|part| part
            .as_text()
            .is_some_and(|text| text.contains("image_url dropped"))));
    }

    #[test]
    fn media_only_results_are_attributed() {
        let result = bounded_mcp_tool_result(
            &json!({"content":[{"type":"audio","data":"YQ==","mimeType":"audio/wav"}]}),
            "mcp__s__sound",
        )
        .expect("result");
        let ExecutableToolOutput::Parts(parts) = result.output else {
            panic!("expected parts")
        };
        assert_eq!(
            parts.first().and_then(ContentPart::as_text),
            Some("<mcp_tool_result name=\"mcp__s__sound\">")
        );
    }
}
