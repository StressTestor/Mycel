const MCP_NAME_PREFIX: &str = "mcp__";
const MCP_NAME_SEPARATOR: &str = "__";
const MAX_QUALIFIED_LENGTH: usize = 64;

/// Replaces unsafe provider tool-name characters with `_` and collapses
/// underscore runs so the `__` namespace separator remains unambiguous.
pub fn sanitize_mcp_name_part(part: &str) -> String {
    let mut output = String::with_capacity(part.len());
    let mut previous_underscore = false;
    for character in part.chars() {
        let safe = character.is_ascii_alphanumeric() || matches!(character, '_' | '-');
        let character = if safe { character } else { '_' };
        if character == '_' {
            if previous_underscore {
                continue;
            }
            previous_underscore = true;
        } else {
            previous_underscore = false;
        }
        output.push(character);
    }
    output
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with(MCP_NAME_PREFIX)
}

/// Produces the stable provider-facing tool name. Names longer than 64 bytes
/// retain their prefix and receive an eight-hex FNV-1a suffix.
pub fn qualify_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    let full = format!(
        "{MCP_NAME_PREFIX}{}{MCP_NAME_SEPARATOR}{}",
        sanitize_mcp_name_part(server_name),
        sanitize_mcp_name_part(tool_name)
    );
    if full.len() <= MAX_QUALIFIED_LENGTH {
        return full;
    }
    let hash = stable_hash_8(full.as_bytes());
    let head_length = MAX_QUALIFIED_LENGTH - hash.len() - 1;
    format!("{}_{}", &full[..head_length], hash)
}

pub(crate) fn stable_hash_8(bytes: &[u8]) -> String {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualification_matches_the_retained_ascii_contract() {
        assert_eq!(sanitize_mcp_name_part("git hub!!"), "git_hub_");
        assert_eq!(
            qualify_mcp_tool_name("git hub", "create/pr"),
            "mcp__git_hub__create_pr"
        );
        let long = qualify_mcp_tool_name(&"s".repeat(60), &"t".repeat(60));
        assert_eq!(long.len(), 64);
        assert!(long.starts_with("mcp__"));
        assert_eq!(
            long,
            qualify_mcp_tool_name(&"s".repeat(60), &"t".repeat(60))
        );
    }
}
