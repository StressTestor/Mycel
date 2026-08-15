use mycel_agent_protocol::ModelCapability;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFamily {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
    Kimi,
    Gemini,
    Vertex,
}

pub fn detect_capability(family: ProviderFamily, model: &str) -> ModelCapability {
    let model = model.to_ascii_lowercase();
    let mut capability = ModelCapability::UNKNOWN;
    match family {
        ProviderFamily::OpenAiChat | ProviderFamily::OpenAiResponses => {
            if reasoning_model(&model) {
                capability.thinking = true;
                capability.tool_use = true;
            } else if ["gpt-4o", "gpt-4-turbo", "gpt-4.1", "gpt-4.5"]
                .iter()
                .any(|prefix| model.starts_with(prefix))
            {
                capability.image_in = true;
                capability.tool_use = true;
            } else if model.starts_with("gpt-3.5-turbo") {
                capability.tool_use = true;
            }
        }
        ProviderFamily::Anthropic => {
            if [
                "claude-opus-4",
                "claude-sonnet-4",
                "claude-haiku-4",
                "claude-fable",
            ]
            .iter()
            .any(|prefix| model.starts_with(prefix))
            {
                capability.image_in = true;
                capability.thinking = true;
                capability.tool_use = true;
            } else if ["claude-3-", "claude-3.5-", "claude-3.7-"]
                .iter()
                .any(|prefix| model.starts_with(prefix))
            {
                capability.image_in = true;
                capability.tool_use = true;
            }
        }
        ProviderFamily::Gemini | ProviderFamily::Vertex => {
            if ["gemini-1.5-", "gemini-2.0-", "gemini-2.5-"]
                .iter()
                .any(|prefix| model.starts_with(prefix))
            {
                capability.image_in = true;
                capability.video_in = true;
                capability.audio_in = true;
                capability.tool_use = true;
                capability.thinking =
                    model.starts_with("gemini-2.5-") || model.contains("thinking");
            }
        }
        ProviderFamily::Kimi => {}
    }
    capability
}

fn reasoning_model(model: &str) -> bool {
    let bytes = model.as_bytes();
    bytes.first() == Some(&b'o') && bytes.get(1).is_some_and(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_static_provider_capabilities() {
        assert!(detect_capability(ProviderFamily::OpenAiResponses, "o3").thinking);
        let gemini = detect_capability(ProviderFamily::Gemini, "gemini-2.5-pro");
        assert!(gemini.video_in && gemini.audio_in && gemini.thinking && gemini.tool_use);
        assert!(detect_capability(ProviderFamily::Kimi, "kimi-k2").is_unknown());
    }

    #[test]
    fn capability_catalog_is_prefix_strict_at_model_edges() {
        let o3 = detect_capability(ProviderFamily::OpenAiChat, "O3-2025-04-16");
        assert!(o3.thinking && o3.tool_use && !o3.image_in);
        assert!(detect_capability(ProviderFamily::OpenAiChat, "omni").is_unknown());

        let gpt = detect_capability(ProviderFamily::OpenAiResponses, "gpt-4.1-mini");
        assert!(gpt.image_in && gpt.tool_use && !gpt.thinking);

        let claude_legacy =
            detect_capability(ProviderFamily::Anthropic, "claude-3-5-sonnet-latest");
        assert!(claude_legacy.image_in && claude_legacy.tool_use && !claude_legacy.thinking);
        let claude_four = detect_capability(ProviderFamily::Anthropic, "claude-fable-4-7");
        assert!(claude_four.image_in && claude_four.tool_use && claude_four.thinking);

        let gemini_15 = detect_capability(ProviderFamily::Vertex, "gemini-1.5-pro");
        assert!(gemini_15.image_in && gemini_15.video_in && gemini_15.audio_in);
        assert!(!gemini_15.thinking);
        assert!(detect_capability(ProviderFamily::Gemini, "gemini-3-pro").is_unknown());
        assert!(detect_capability(ProviderFamily::Kimi, "unknown").is_unknown());
    }
}
