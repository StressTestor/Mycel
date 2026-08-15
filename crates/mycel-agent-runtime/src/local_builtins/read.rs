use std::path::Path;

use mycel_agent_protocol::{
    ExecutableToolOutput, ExecutableToolResult, FileOperation, ToolDefinition, ToolInputDisplay,
    ToolUpdate, ToolUpdateKind,
};
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::{
    ExecutableTool, FileAccessMode, ToolAccess, ToolError, ToolFuture, ToolInvocation,
    ToolPrepareContext,
};

use super::{
    base_spec,
    output::error_result,
    path::{resolve_local_path, PathKind},
    read_schema, LocalToolConfig, MAX_FILE_BYTES,
};

const MAX_LINES: usize = 1_000;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_RENDERED_BYTES: usize = 100 * 1024;

pub struct ReadTool {
    config: LocalToolConfig,
}

impl ReadTool {
    pub fn new(config: LocalToolConfig) -> Self {
        Self { config }
    }
}

impl ExecutableTool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Read".to_owned(),
            description: "Read a UTF-8 text file with numbered, bounded output.".to_owned(),
            parameters: read_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<crate::ToolExecutionSpec, ToolError> {
        let input = string_argument(arguments, "path")?;
        let path =
            resolve_local_path(&self.config, input, PathKind::File).map_err(ToolError::Prepare)?;
        let mut spec = base_spec(
            ToolInputDisplay::FileIo {
                operation: FileOperation::Read,
                path: path.to_string_lossy().into_owned(),
                detail: None,
                content: None,
                before: None,
                after: None,
            },
            "Read",
        );
        spec.accesses = vec![ToolAccess::file(path, FileAccessMode::Read)];
        spec.description = Some(format!("Reading {input}"));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let display_path = string_argument(&invocation.arguments, "path")?;
            let path = match resolve_local_path(&self.config, display_path, PathKind::File) {
                Ok(path) => path,
                Err(error) => return Ok(error_result(error)),
            };
            let bytes = match read_file_limited(&path, &invocation.cancellation).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Ok(error_result(format!(
                        "failed to read {display_path}: {error}"
                    )))
                }
            };
            if invocation.cancellation.is_cancelled() {
                return Ok(error_result("Read aborted"));
            }
            let text = match String::from_utf8(bytes) {
                Ok(text) if !text.contains('\0') => text,
                _ => return Ok(error_result(not_text_message(display_path))),
            };
            let offset = invocation
                .arguments
                .get("line_offset")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let requested = invocation
                .arguments
                .get("n_lines")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(MAX_LINES);
            let result = render_read(&text, offset, requested);
            invocation.updates.emit(ToolUpdate {
                kind: ToolUpdateKind::Progress,
                text: Some(format!("read {} bytes", text.len())),
                percent: Some(100.0),
                custom_kind: None,
                custom_data: None,
            });
            Ok(result)
        })
    }
}

pub(super) async fn read_file_limited(
    path: &Path,
    cancellation: &crate::CancellationToken,
) -> Result<Vec<u8>, String> {
    if cancellation.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    let read = async {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|error| error.to_string())?;
        let mut reader = file.take(MAX_FILE_BYTES + 1);
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(format!(
                "file exceeds the {} byte local-tool limit",
                MAX_FILE_BYTES
            ));
        }
        Ok(bytes)
    };
    tokio::select! {
        result = read => result,
        () = cancellation.cancelled() => Err("operation cancelled".to_owned()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineEnding {
    Lf,
    CrLf,
    Mixed,
}

pub(super) fn model_text(text: &str) -> (String, LineEnding) {
    let has_crlf = text.contains("\r\n");
    let without_crlf = text.replace("\r\n", "");
    let has_lf = without_crlf.contains('\n');
    let has_lone_cr = without_crlf.contains('\r');
    let style = if has_lone_cr || has_crlf && has_lf {
        LineEnding::Mixed
    } else if has_crlf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let model = match style {
        LineEnding::CrLf => text.replace("\r\n", "\n"),
        LineEnding::Lf | LineEnding::Mixed => text.to_owned(),
    };
    (model, style)
}

pub(super) fn materialize_model_text(text: &str, style: LineEnding) -> String {
    match style {
        LineEnding::CrLf => text.replace('\n', "\r\n"),
        LineEnding::Lf | LineEnding::Mixed => text.to_owned(),
    }
}

fn render_read(text: &str, offset: i64, requested: usize) -> ExecutableToolResult {
    let (_, style) = model_text(text);
    let raw_lines: Vec<&str> = text.split_terminator('\n').collect();
    let total_lines = raw_lines.len();
    let start = if offset < 0 {
        total_lines.saturating_sub(offset.unsigned_abs() as usize)
    } else {
        usize::try_from(offset - 1)
            .unwrap_or(usize::MAX)
            .min(total_lines)
    };
    let effective = requested.min(MAX_LINES);
    let end = start.saturating_add(effective).min(total_lines);
    let mut rendered = Vec::new();
    let mut line_was_truncated = false;
    for (index, raw) in raw_lines[start..end].iter().enumerate() {
        let mut line = (*raw).to_owned();
        if style == LineEnding::CrLf && line.ends_with('\r') {
            line.pop();
        }
        let char_count = line.chars().count();
        if char_count > MAX_LINE_CHARS {
            line = line
                .chars()
                .take(MAX_LINE_CHARS.saturating_sub(3))
                .collect::<String>();
            line.push_str("...");
            line_was_truncated = true;
        }
        if style == LineEnding::Mixed {
            line = line.replace('\r', "\\r");
        }
        rendered.push(format!("{}\t{}", start + index + 1, line));
    }

    let mut byte_truncated = false;
    if offset < 0 {
        while rendered.join("\n").len() > MAX_RENDERED_BYTES && !rendered.is_empty() {
            rendered.remove(0);
            byte_truncated = true;
        }
    } else {
        let mut kept = Vec::new();
        let mut bytes = 0;
        for line in rendered {
            let added = line.len() + usize::from(!kept.is_empty());
            if !kept.is_empty() && bytes + added > MAX_RENDERED_BYTES {
                byte_truncated = true;
                break;
            }
            bytes += added;
            kept.push(line);
        }
        rendered = kept;
    }
    let max_lines_reached = requested > MAX_LINES || end < total_lines && end - start >= MAX_LINES;
    let truncated = line_was_truncated || byte_truncated || max_lines_reached;
    let mut output = rendered.join("\n");
    if truncated {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("[Read output truncated; request a narrower line range to continue]");
    }
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(output),
        is_error: false,
        stop_turn: false,
        message: None,
        note: None,
        truncated,
    }
}

fn not_text_message(path: &str) -> String {
    format!("{path:?} is not readable as UTF-8 text; use Bash or a binary-aware tool")
}

pub(super) fn string_argument<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments {
            path: format!("$.{name}"),
            message: "expected a string".to_owned(),
        })
}
