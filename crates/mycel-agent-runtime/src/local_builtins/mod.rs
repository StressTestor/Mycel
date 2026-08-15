//! Local, provider-neutral implementations of Mycel's core filesystem and
//! shell tools. Authorization and hooks deliberately live in `TurnEngine`;
//! this module only resolves safe local resources and executes the operation.

mod bash;
mod edit;
pub(crate) mod output;
mod path;
pub(crate) mod process;
mod read;
mod search;
mod write;

use std::{
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};

use crate::{
    BackgroundStatus, CancellationToken, ToolExecutionSpec, ToolRegistry, ToolRegistryError,
    ToolUpdateSink,
};

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::ReadTool;
pub use search::{GlobTool, GrepTool};
pub use write::WriteTool;

pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

pub struct ForegroundProcessTask {
    pub task_id: String,
    pub cancellation: CancellationToken,
    pub detach: CancellationToken,
    pub updates: Arc<dyn ToolUpdateSink>,
}

pub trait ForegroundProcessPort: Send + Sync {
    fn register(
        &self,
        description: &str,
        timeout: Duration,
    ) -> Result<ForegroundProcessTask, String>;

    fn settle(
        &self,
        task_id: &str,
        status: BackgroundStatus,
        reason: Option<&str>,
    ) -> Result<(), String>;
}

#[derive(Clone, Debug)]
pub struct LocalToolConfig {
    cwd: Arc<PathBuf>,
    additional_dirs: Arc<Vec<PathBuf>>,
    allowed_writable_files: Arc<Vec<PathBuf>>,
    shell: Arc<PathBuf>,
    search_timeout: Duration,
}

impl LocalToolConfig {
    pub fn new<I, P>(
        cwd: impl Into<PathBuf>,
        additional_dirs: I,
    ) -> Result<Self, LocalToolConfigError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let cwd = canonical_directory(cwd.into())?;
        let mut additional_dirs = additional_dirs
            .into_iter()
            .map(|path| canonical_directory(path.into()))
            .collect::<Result<Vec<_>, _>>()?;
        additional_dirs.sort();
        additional_dirs.dedup();
        additional_dirs.retain(|path| path != &cwd);
        let shell = if is_executable_file(std::path::Path::new("/bin/bash")) {
            PathBuf::from("/bin/bash")
        } else {
            PathBuf::from("/bin/sh")
        };
        Ok(Self {
            cwd: Arc::new(cwd),
            additional_dirs: Arc::new(additional_dirs),
            allowed_writable_files: Arc::new(Vec::new()),
            shell: Arc::new(shell),
            search_timeout: SEARCH_TIMEOUT,
        })
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn additional_dirs(&self) -> &[PathBuf] {
        &self.additional_dirs
    }

    /// Grants Write/Edit access to exact files outside the workspace roots.
    ///
    /// This is deliberately not a directory grant: Read, search, shell cwd,
    /// and media resolution do not consult it. Every parent must already
    /// exist, and symlink components or targets are rejected before the
    /// canonical leaf is retained.
    pub fn with_allowed_files<I, P>(mut self, files: I) -> Result<Self, LocalToolConfigError>
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let mut allowed = files
            .into_iter()
            .map(|path| canonical_writable_file(path.into()))
            .collect::<Result<Vec<_>, _>>()?;
        allowed.sort();
        allowed.dedup();
        self.allowed_writable_files = Arc::new(allowed);
        Ok(self)
    }

    pub fn shell(&self) -> &std::path::Path {
        &self.shell
    }

    pub fn with_shell(mut self, shell: impl Into<PathBuf>) -> Result<Self, LocalToolConfigError> {
        let shell = shell.into();
        if !shell.is_absolute() || !is_executable_file(&shell) {
            return Err(LocalToolConfigError::InvalidShell(shell));
        }
        self.shell = Arc::new(shell);
        Ok(self)
    }

    /// Overrides ripgrep's deadline. This also makes timeout contract tests
    /// deterministic without changing global process state.
    pub fn with_search_timeout(mut self, timeout: Duration) -> Self {
        self.search_timeout = timeout;
        self
    }

    fn roots(&self) -> impl Iterator<Item = &std::path::Path> {
        std::iter::once(self.cwd()).chain(self.additional_dirs.iter().map(PathBuf::as_path))
    }

    pub(crate) fn allowed_writable_files(&self) -> &[PathBuf] {
        &self.allowed_writable_files
    }
}

fn canonical_writable_file(path: PathBuf) -> Result<PathBuf, LocalToolConfigError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(LocalToolConfigError::InvalidAllowedFile {
            path,
            reason: "path must be an absolute file path".to_owned(),
        });
    }
    reject_allowed_file_symlinks(&path)?;
    let parent = path
        .parent()
        .ok_or_else(|| LocalToolConfigError::InvalidAllowedFile {
            path: path.clone(),
            reason: "path has no parent directory".to_owned(),
        })?;
    let canonical_parent =
        std::fs::canonicalize(parent).map_err(|source| LocalToolConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    if !canonical_parent.is_dir() {
        return Err(LocalToolConfigError::NotDirectory(canonical_parent));
    }
    let candidate = canonical_parent.join(path.file_name().expect("checked above"));
    match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(LocalToolConfigError::InvalidAllowedFile {
                path: candidate,
                reason: "symlink targets are not allowed".to_owned(),
            })
        }
        Ok(metadata) if !metadata.is_file() => Err(LocalToolConfigError::InvalidAllowedFile {
            path: candidate,
            reason: "path is not a regular file".to_owned(),
        }),
        Ok(_) => std::fs::canonicalize(&candidate).map_err(|source| LocalToolConfigError::Io {
            path: candidate,
            source,
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(candidate),
        Err(source) => Err(LocalToolConfigError::Io {
            path: candidate,
            source,
        }),
    }
}

fn reject_allowed_file_symlinks(path: &Path) -> Result<(), LocalToolConfigError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(LocalToolConfigError::InvalidAllowedFile {
                    path: path.to_path_buf(),
                    reason: "parent-directory components are not allowed".to_owned(),
                })
            }
            Component::Normal(value) => current.push(value),
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LocalToolConfigError::InvalidAllowedFile {
                    path: path.to_path_buf(),
                    reason: format!("symlink component {current:?} is not allowed"),
                })
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(source) => {
                return Err(LocalToolConfigError::Io {
                    path: current,
                    source,
                })
            }
        }
    }
    Ok(())
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn canonical_directory(path: PathBuf) -> Result<PathBuf, LocalToolConfigError> {
    let canonical = std::fs::canonicalize(&path).map_err(|source| LocalToolConfigError::Io {
        path: path.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(LocalToolConfigError::NotDirectory(canonical));
    }
    Ok(canonical)
}

pub fn register_local_builtins(
    registry: &ToolRegistry,
    config: LocalToolConfig,
) -> Result<(), ToolRegistryError> {
    register_local_builtins_with_process_port(registry, config, None)
}

pub fn register_local_builtins_with_process_port(
    registry: &ToolRegistry,
    config: LocalToolConfig,
    process_port: Option<Arc<dyn ForegroundProcessPort>>,
) -> Result<(), ToolRegistryError> {
    registry.register(Arc::new(ReadTool::new(config.clone())))?;
    registry.register(Arc::new(WriteTool::new(config.clone())))?;
    registry.register(Arc::new(EditTool::new(config.clone())))?;
    registry.register(Arc::new(GlobTool::new(config.clone())))?;
    registry.register(Arc::new(GrepTool::new(config.clone())))?;
    let bash = BashTool::new(config);
    let bash = match process_port {
        Some(port) => bash.with_foreground_process_port(port),
        None => bash,
    };
    registry.register(Arc::new(bash))?;
    Ok(())
}

pub(super) fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub(super) fn read_schema() -> Value {
    object_schema(
        json!({
            "path": {"type":"string", "minLength":1},
            "line_offset": {"anyOf":[
                {"type":"integer", "minimum":1},
                {"type":"integer", "minimum":-1000, "maximum":-1}
            ]},
            "n_lines": {"type":"integer", "minimum":1},
        }),
        &["path"],
    )
}

pub(super) fn write_schema() -> Value {
    object_schema(
        json!({
            "path": {"type":"string", "minLength":1},
            "content": {"type":"string"},
            "mode": {"type":"string", "enum":["overwrite", "append"]},
        }),
        &["path", "content"],
    )
}

pub(super) fn edit_schema() -> Value {
    object_schema(
        json!({
            "path": {"type":"string", "minLength":1},
            "old_string": {"type":"string", "minLength":1},
            "new_string": {"type":"string"},
            "replace_all": {"type":"boolean"},
        }),
        &["path", "old_string", "new_string"],
    )
}

pub(super) fn glob_schema() -> Value {
    object_schema(
        json!({
            "pattern": {"type":"string", "minLength":1},
            "path": {"type":"string", "minLength":1},
            "include_ignored": {"type":"boolean"},
            "include_dirs": {"type":"boolean"},
        }),
        &["pattern"],
    )
}

pub(super) fn grep_schema() -> Value {
    object_schema(
        json!({
            "pattern": {"type":"string"},
            "path": {"type":"string", "minLength":1},
            "glob": {"type":"string", "minLength":1},
            "type": {"type":"string", "minLength":1},
            "output_mode": {"type":"string", "enum":["content", "files_with_matches", "count_matches"]},
            "-i": {"type":"boolean"}, "-n": {"type":"boolean"},
            "-A": {"type":"integer", "minimum":0}, "-B": {"type":"integer", "minimum":0},
            "-C": {"type":"integer", "minimum":0}, "head_limit": {"type":"integer", "minimum":0},
            "offset": {"type":"integer", "minimum":0}, "multiline": {"type":"boolean"},
            "include_ignored": {"type":"boolean"},
        }),
        &["pattern"],
    )
}

pub(super) fn bash_schema() -> Value {
    // Background process ownership belongs to the runtime background host and
    // is intentionally not advertised until that concrete adapter is wired.
    object_schema(
        json!({
            "command": {"type":"string", "minLength":1},
            "cwd": {"type":"string", "minLength":1},
            "timeout": {"type":"integer", "minimum":1, "maximum":300, "default":60},
        }),
        &["command"],
    )
}

pub(super) fn base_spec(
    display: mycel_agent_protocol::ToolInputDisplay,
    action: &str,
) -> ToolExecutionSpec {
    ToolExecutionSpec::new(display, action)
}

#[derive(Debug, thiserror::Error)]
pub enum LocalToolConfigError {
    #[error("local tool path {path:?} could not be resolved: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("local tool root {0:?} is not a directory")]
    NotDirectory(PathBuf),
    #[error("local tool shell {0:?} must be an absolute executable file")]
    InvalidShell(PathBuf),
    #[error("local tool allowed file {path:?} is invalid: {reason}")]
    InvalidAllowedFile { path: PathBuf, reason: String },
}
