use std::path::{Path, PathBuf};

use mycel_agent_protocol::{
    FileOperation, ToolDefinition, ToolInputDisplay, ToolUpdate, ToolUpdateKind,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::{
    validate_json_schema, ExecutableTool, FileAccessMode, ToolAccess, ToolError, ToolFuture,
    ToolInvocation, ToolPrepareContext,
};

use super::{
    base_spec,
    output::{error_result, text_result},
    path::{resolve_local_path, PathKind},
    read::{read_file_limited, string_argument},
    write_schema, LocalToolConfig, MAX_FILE_BYTES,
};

pub struct WriteTool {
    config: LocalToolConfig,
}

impl WriteTool {
    pub fn new(config: LocalToolConfig) -> Self {
        Self { config }
    }
}

impl ExecutableTool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Write".to_owned(),
            description: "Create, overwrite, or append exact file content.".to_owned(),
            parameters: write_schema(),
            deferred: false,
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        validate_json_schema(&write_schema(), arguments)?;
        let content = string_argument(arguments, "content")?;
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(ToolError::InvalidArguments {
                path: "$.content".to_owned(),
                message: format!("content exceeds {MAX_FILE_BYTES} UTF-8 bytes"),
            });
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
        let content = string_argument(arguments, "content")?;
        let mut spec = base_spec(
            ToolInputDisplay::FileIo {
                operation: FileOperation::Write,
                path: path.to_string_lossy().into_owned(),
                detail: arguments
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                content: Some(content.to_owned()),
                before: None,
                after: None,
            },
            "Write",
        );
        spec.accesses = vec![ToolAccess::file(path, FileAccessMode::Write)];
        spec.description = Some(format!("Writing {input}"));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let display_path = string_argument(&invocation.arguments, "path")?;
            let content = string_argument(&invocation.arguments, "content")?;
            let mode = invocation
                .arguments
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("overwrite");
            let path = match resolve_local_path(&self.config, display_path, PathKind::WritableFile)
            {
                Ok(path) => path,
                Err(error) => return Ok(error_result(error)),
            };
            if let Some(parent) = path.parent() {
                if let Err(error) = tokio::fs::create_dir_all(parent).await {
                    return Ok(error_result(format!(
                        "failed to create parent directory: {error}"
                    )));
                }
            }
            let path = match resolve_local_path(&self.config, display_path, PathKind::WritableFile)
            {
                Ok(path) => path,
                Err(error) => return Ok(error_result(error)),
            };
            let write_result = if mode == "append" {
                append_file(&path, content.as_bytes(), &invocation.cancellation).await
            } else {
                atomic_write(&path, content.as_bytes(), &invocation.cancellation).await
            };
            if let Err(error) = write_result {
                return Ok(error_result(format!(
                    "failed to write {display_path}: {error}"
                )));
            }
            invocation.updates.emit(ToolUpdate {
                kind: ToolUpdateKind::Progress,
                text: Some(format!("wrote {} bytes", content.len())),
                percent: Some(100.0),
                custom_kind: None,
                custom_data: None,
            });
            Ok(text_result(format!(
                "{} {} bytes to {display_path}",
                if mode == "append" {
                    "Appended"
                } else {
                    "Wrote"
                },
                content.len()
            )))
        })
    }
}

pub(super) async fn atomic_write(
    path: &Path,
    bytes: &[u8],
    cancellation: &crate::CancellationToken,
) -> Result<(), String> {
    if cancellation.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    let parent = path.parent().ok_or("target has no parent")?;
    let temp = temporary_path(parent);
    let inherited_permissions = tokio::fs::symlink_metadata(path)
        .await
        .ok()
        .filter(|metadata| !metadata.file_type().is_symlink())
        .map(|metadata| metadata.permissions());
    let write_temp = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = options
            .open(&temp)
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(bytes)
            .await
            .map_err(|error| error.to_string())?;
        file.flush().await.map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        drop(file);
        if let Some(permissions) = inherited_permissions {
            tokio::fs::set_permissions(&temp, permissions)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    };
    let result = tokio::select! {
        result = write_temp => result,
        () = cancellation.cancelled() => Err("operation cancelled".to_owned()),
    };
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
        return result;
    }
    if cancellation.is_cancelled() {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err("operation cancelled".to_owned());
    }
    if let Err(error) = tokio::fs::rename(&temp, path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.to_string());
    }
    if let Ok(parent_handle) = tokio::fs::File::open(parent).await {
        parent_handle
            .sync_all()
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn append_file(
    path: &Path,
    bytes: &[u8],
    cancellation: &crate::CancellationToken,
) -> Result<(), String> {
    if cancellation.is_cancelled() {
        return Err("operation cancelled".to_owned());
    }
    let mut combined = if path.exists() {
        read_file_limited(path, cancellation).await?
    } else {
        Vec::new()
    };
    if combined.len().saturating_add(bytes.len()) as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "appended file would exceed the {MAX_FILE_BYTES} byte local-tool limit"
        ));
    }
    combined.extend_from_slice(bytes);
    atomic_write(path, &combined, cancellation).await
}

fn temporary_path(parent: &Path) -> PathBuf {
    parent.join(format!(".mycel-write-{}.tmp", crate::RequestId::generate()))
}
