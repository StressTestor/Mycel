use mycel_agent_protocol::{FileOperation, ToolDefinition, ToolInputDisplay};
use serde_json::Value;

use crate::{
    validate_json_schema, ExecutableTool, FileAccessMode, ToolAccess, ToolError, ToolFuture,
    ToolInvocation, ToolPrepareContext,
};

use super::{
    base_spec, edit_schema,
    output::{error_result, text_result},
    path::{resolve_local_path, PathKind},
    read::{materialize_model_text, model_text, read_file_limited, string_argument},
    write::atomic_write,
    LocalToolConfig, MAX_FILE_BYTES,
};

pub struct EditTool {
    config: LocalToolConfig,
}

impl EditTool {
    pub fn new(config: LocalToolConfig) -> Self {
        Self { config }
    }
}

impl ExecutableTool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Edit".to_owned(),
            description: "Replace exact text in an existing UTF-8 file.".to_owned(),
            parameters: edit_schema(),
            deferred: false,
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        validate_json_schema(&edit_schema(), arguments)?;
        for name in ["old_string", "new_string"] {
            if string_argument(arguments, name)?.len() as u64 > MAX_FILE_BYTES {
                return Err(ToolError::InvalidArguments {
                    path: format!("$.{name}"),
                    message: format!("text exceeds {MAX_FILE_BYTES} UTF-8 bytes"),
                });
            }
        }
        Ok(())
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<crate::ToolExecutionSpec, ToolError> {
        let input = string_argument(arguments, "path")?;
        let path = resolve_local_path(&self.config, input, PathKind::WritableFile)
            .map_err(ToolError::Prepare)?;
        let mut spec = base_spec(
            ToolInputDisplay::FileIo {
                operation: FileOperation::Edit,
                path: path.to_string_lossy().into_owned(),
                detail: None,
                content: None,
                before: Some(string_argument(arguments, "old_string")?.to_owned()),
                after: Some(string_argument(arguments, "new_string")?.to_owned()),
            },
            "Edit",
        );
        spec.accesses = vec![ToolAccess::file(path, FileAccessMode::ReadWrite)];
        spec.description = Some(format!("Editing {input}"));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let display_path = string_argument(&invocation.arguments, "path")?;
            let old = string_argument(&invocation.arguments, "old_string")?;
            let new = string_argument(&invocation.arguments, "new_string")?;
            if old == new {
                return Ok(error_result(
                    "No changes to make: old_string and new_string are exactly the same.",
                ));
            }
            let path = match resolve_local_path(&self.config, display_path, PathKind::WritableFile)
            {
                Ok(path) if path.is_file() => path,
                Ok(_) => return Ok(error_result(format!("{display_path} does not exist"))),
                Err(error) => return Ok(error_result(error)),
            };
            let raw = match read_file_limited(&path, &invocation.cancellation).await {
                Ok(raw) => raw,
                Err(error) => return Ok(error_result(error)),
            };
            let raw = match String::from_utf8(raw) {
                Ok(raw) if !raw.contains('\0') => raw,
                _ => return Ok(error_result(format!("{display_path} is not UTF-8 text"))),
            };
            let (model, style) = model_text(&raw);
            let count = model.match_indices(old).count();
            if count == 0 {
                return Ok(error_result(format!(
                    "old_string not found in {display_path}; read the file again before editing"
                )));
            }
            let replace_all = invocation
                .arguments
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if count > 1 && !replace_all {
                return Ok(error_result(format!(
                    "old_string is not unique in {display_path} (found {count} occurrences); add context or set replace_all=true"
                )));
            }
            let edited = if replace_all {
                model.replace(old, new)
            } else {
                model.replacen(old, new, 1)
            };
            let materialized = materialize_model_text(&edited, style);
            if materialized.len() as u64 > MAX_FILE_BYTES {
                return Ok(error_result(format!(
                    "edited file exceeds the {MAX_FILE_BYTES} byte local-tool limit"
                )));
            }
            if let Err(error) =
                atomic_write(&path, materialized.as_bytes(), &invocation.cancellation).await
            {
                return Ok(error_result(format!(
                    "failed to edit {display_path}: {error}"
                )));
            }
            Ok(text_result(format!(
                "Replaced {} occurrence{} in {display_path}",
                if replace_all { count } else { 1 },
                if replace_all && count != 1 { "s" } else { "" }
            )))
        })
    }
}
