use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use chrono::{DateTime, SecondsFormat, Utc};
use mycel_agent_protocol::{ContentPart, PromptOrigin, Role, ToolCall};
use mycel_agent_runtime::ContextEntry;

const MAX_HINT_CHARS: usize = 60;
const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct MarkdownExport<'a> {
    pub session_id: &'a str,
    pub work_dir: &'a Path,
    pub history: &'a [ContextEntry],
    pub token_count: u64,
    pub now: DateTime<Utc>,
}

pub(crate) fn default_markdown_export_path(
    work_dir: &Path,
    session_id: &str,
    now: DateTime<Utc>,
) -> PathBuf {
    let short_id = session_id.chars().take(8).collect::<String>();
    let timestamp = now.format("%Y%m%d-%H%M%S");
    work_dir.join(format!("mycel-export-{short_id}-{timestamp}.md"))
}

pub(crate) fn build_export_markdown(input: &MarkdownExport<'_>) -> String {
    let turns = group_into_turns(input.history);
    let mut lines = vec![
        "---".to_owned(),
        format!("session_id: {}", input.session_id),
        format!(
            "exported_at: {}",
            input.now.to_rfc3339_opts(SecondsFormat::Millis, true)
        ),
        format!("work_dir: {}", input.work_dir.display()),
        format!("message_count: {}", input.history.len()),
        format!("token_count: {}", input.token_count),
        "---".to_owned(),
        String::new(),
        "# Mycel Session Export".to_owned(),
        String::new(),
        build_overview(input.history, &turns),
        String::new(),
    ];
    for (index, turn) in turns.iter().enumerate() {
        lines.push(format_turn(turn, index + 1));
    }
    lines.join("\n")
}

pub(crate) fn write_markdown_export(path: &Path, contents: &str) -> Result<(), String> {
    if contents.len() > MAX_EXPORT_BYTES {
        return Err(format!(
            "Markdown export exceeds the {} MiB limit",
            MAX_EXPORT_BYTES / (1024 * 1024)
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(format!(
                "Markdown export target {} must be a regular file, not a symlink or special file",
                path.display()
            ));
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(
        ".mycel-markdown-export-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|error| {
            format!(
                "could not atomically replace {} from {}: {error}",
                path.display(),
                temporary.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn is_internal(entry: &ContextEntry) -> bool {
    matches!(
        entry.origin,
        Some(
            PromptOrigin::Injection { .. }
                | PromptOrigin::SystemTrigger { .. }
                | PromptOrigin::CompactionSummary
                | PromptOrigin::HookResult { .. }
                | PromptOrigin::CronJob { .. }
        )
    )
}

fn group_into_turns(history: &[ContextEntry]) -> Vec<Vec<&ContextEntry>> {
    let mut turns = Vec::new();
    let mut current = Vec::new();
    for entry in history.iter().filter(|entry| !is_internal(entry)) {
        if entry.message.role == Role::User && !current.is_empty() {
            turns.push(std::mem::take(&mut current));
        }
        current.push(entry);
    }
    if !current.is_empty() {
        turns.push(current);
    }
    turns
}

fn build_overview(history: &[ContextEntry], turns: &[Vec<&ContextEntry>]) -> String {
    let topic = history
        .iter()
        .filter(|entry| !is_internal(entry) && entry.message.role == Role::User)
        .flat_map(|entry| entry.message.content.iter())
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join(" ");
    let topic = if topic.is_empty() {
        "(empty)".to_owned()
    } else {
        shorten(&topic, 80)
    };
    let tool_calls = history
        .iter()
        .map(|entry| entry.message.tool_calls.len())
        .sum::<usize>();
    format!(
        "## Overview\n\n- **Topic**: {topic}\n- **Conversation**: {} turns | {tool_calls} tool calls\n\n---",
        turns.len()
    )
}

fn format_turn(messages: &[&ContextEntry], turn_number: usize) -> String {
    let mut lines = vec![format!("## Turn {turn_number}"), String::new()];
    let mut calls = BTreeMap::<String, (String, String)>::new();
    let mut assistant_header = false;
    for entry in messages.iter().copied().filter(|entry| !is_internal(entry)) {
        match entry.message.role {
            Role::User => {
                lines.extend(["### User".to_owned(), String::new()]);
                append_content(&mut lines, &entry.message.content);
            }
            Role::Assistant => {
                if !assistant_header {
                    lines.extend(["### Assistant".to_owned(), String::new()]);
                    assistant_header = true;
                }
                append_content(&mut lines, &entry.message.content);
                for call in &entry.message.tool_calls {
                    let hint = extract_tool_call_hint(call.arguments.as_deref().unwrap_or("{}"));
                    calls.insert(call.id.clone(), (call.name.clone(), hint));
                    lines.extend([format_tool_call(call), String::new()]);
                }
            }
            Role::Tool => {
                let id = entry.message.tool_call_id.as_deref().unwrap_or("unknown");
                let (name, hint) = calls
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| ("unknown".to_owned(), String::new()));
                lines.extend([format_tool_result(entry, &name, &hint), String::new()]);
            }
            Role::System => {
                lines.extend(["### System".to_owned(), String::new()]);
                append_content(&mut lines, &entry.message.content);
            }
        }
    }
    lines.join("\n")
}

fn append_content(lines: &mut Vec<String>, content: &[ContentPart]) {
    for part in content {
        let text = format_content(part);
        if !text.trim().is_empty() {
            lines.extend([text, String::new()]);
        }
    }
}

fn format_content(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text } => text.clone(),
        ContentPart::Think { think, .. } if think.trim().is_empty() => String::new(),
        ContentPart::Think { think, .. } => {
            format!("<details><summary>Thinking</summary>\n\n{think}\n\n</details>")
        }
        ContentPart::ImageUrl { .. } => "[image]".to_owned(),
        ContentPart::AudioUrl { .. } => "[audio]".to_owned(),
        ContentPart::VideoUrl { .. } => "[video]".to_owned(),
    }
}

fn format_tool_call(call: &ToolCall) -> String {
    let raw = call.arguments.as_deref().unwrap_or("{}");
    let hint = extract_tool_call_hint(raw);
    let suffix = if hint.is_empty() {
        String::new()
    } else {
        format!(" (`{hint}`)")
    };
    let arguments = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| raw.to_owned());
    format!(
        "#### Tool Call: {}{suffix}\n<!-- call_id: {} -->\n```json\n{arguments}\n```",
        call.name, call.id
    )
}

fn format_tool_result(entry: &ContextEntry, name: &str, hint: &str) -> String {
    let suffix = if hint.is_empty() {
        String::new()
    } else {
        format!(" (`{hint}`)")
    };
    let text = entry
        .message
        .content
        .iter()
        .map(format_content)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<details><summary>Tool Result: {name}{suffix}</summary>\n\n<!-- call_id: {} -->\n{text}\n\n</details>",
        entry.message.tool_call_id.as_deref().unwrap_or("unknown")
    )
}

fn extract_tool_call_hint(arguments: &str) -> String {
    let Ok(serde_json::Value::Object(arguments)) =
        serde_json::from_str::<serde_json::Value>(arguments)
    else {
        return String::new();
    };
    for key in [
        "path",
        "file_path",
        "command",
        "query",
        "url",
        "name",
        "pattern",
    ] {
        if let Some(value) = arguments.get(key).and_then(serde_json::Value::as_str) {
            if !value.trim().is_empty() {
                return shorten(value, MAX_HINT_CHARS);
            }
        }
    }
    arguments
        .values()
        .filter_map(serde_json::Value::as_str)
        .find(|value| !value.is_empty() && value.chars().count() <= 80)
        .map(|value| shorten(value, MAX_HINT_CHARS))
        .unwrap_or_default()
}

fn shorten(text: &str, maximum: usize) -> String {
    let mut characters = text.chars();
    let shortened = characters.by_ref().take(maximum).collect::<String>();
    if characters.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mycel_agent_protocol::{Message, ToolCallKind};
    use tempfile::tempdir;

    use super::*;

    fn entry(message: Message, origin: Option<PromptOrigin>) -> ContextEntry {
        ContextEntry {
            message,
            origin,
            is_error: false,
            tool_call_displays: BTreeMap::new(),
            note: None,
        }
    }

    #[test]
    fn markdown_matches_retained_turn_tool_media_and_internal_filter_contract() {
        let call = ToolCall {
            kind: ToolCallKind::Function,
            id: "call-1".to_owned(),
            name: "Read".to_owned(),
            arguments: Some(r#"{"path":"/very/long/file.rs"}"#.to_owned()),
            extras: BTreeMap::new(),
        };
        let history = vec![
            entry(Message::user("visible question"), Some(PromptOrigin::User)),
            entry(
                Message::user("<cron-fire>secret envelope</cron-fire>"),
                Some(PromptOrigin::CronJob {
                    job_id: "job".to_owned(),
                    cron: "* * * * *".to_owned(),
                    recurring: true,
                    coalesced_count: 0,
                    stale: false,
                }),
            ),
            entry(
                Message::assistant(
                    vec![
                        ContentPart::Think {
                            think: "reasoning".to_owned(),
                            encrypted: None,
                        },
                        ContentPart::text("answer"),
                    ],
                    vec![call],
                ),
                None,
            ),
            entry(Message::tool("call-1", "file body"), None),
        ];
        let rendered = build_export_markdown(&MarkdownExport {
            session_id: "session-123456789",
            work_dir: Path::new("/workspace"),
            history: &history,
            token_count: 42,
            now: DateTime::parse_from_rfc3339("2026-08-14T12:34:56Z")
                .expect("time")
                .with_timezone(&Utc),
        });
        assert!(rendered.contains("# Mycel Session Export"));
        assert!(rendered.contains("## Turn 1"));
        assert!(rendered.contains("<details><summary>Thinking</summary>"));
        assert!(rendered.contains("#### Tool Call: Read (`/very/long/file.rs`)"));
        assert!(rendered.contains("Tool Result: Read (`/very/long/file.rs`)"));
        assert!(!rendered.contains("cron-fire"));
        assert!(rendered.contains("message_count: 4"));
    }

    #[cfg(unix)]
    #[test]
    fn writer_is_private_atomic_and_rejects_symlink_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempdir().expect("temp");
        let output = temp.path().join("nested/export.md");
        write_markdown_export(&output, "secret conversation").expect("write");
        assert_eq!(
            fs::read_to_string(&output).expect("read"),
            "secret conversation"
        );
        assert_eq!(
            fs::metadata(&output)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let link = temp.path().join("link.md");
        symlink(&output, &link).expect("symlink");
        let error = write_markdown_export(&link, "replace").expect_err("reject symlink");
        assert!(error.contains("symlink"));
        assert_eq!(
            fs::read_to_string(output).expect("preserved"),
            "secret conversation"
        );
    }
}
