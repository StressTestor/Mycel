use serde::{Deserialize, Serialize};

/// Provider-declared model features. Field names intentionally stay
/// snake_case because this structure crosses provider/catalog boundaries in
/// that form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub image_in: bool,
    pub video_in: bool,
    pub audio_in: bool,
    pub thinking: bool,
    pub tool_use: bool,
    pub max_context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamically_loaded_tools: Option<bool>,
}

impl ModelCapability {
    pub const UNKNOWN: Self = Self {
        image_in: false,
        video_in: false,
        audio_in: false,
        thinking: false,
        tool_use: false,
        max_context_tokens: 0,
        dynamically_loaded_tools: Some(false),
    };

    pub fn is_unknown(self) -> bool {
        !self.image_in
            && !self.video_in
            && !self.audio_in
            && !self.thinking
            && !self.tool_use
            && self.dynamically_loaded_tools != Some(true)
            && self.max_context_tokens == 0
    }
}
