use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use mycel_agent_protocol::{FileOperation, ToolDefinition, ToolInputDisplay};
use serde_json::Value;

use crate::{
    ExecutableTool, FileAccessMode, ToolAccess, ToolError, ToolFuture, ToolInvocation,
    ToolPrepareContext,
};

use super::{
    base_spec, glob_schema, grep_schema,
    output::{error_result, OutputBuffer},
    path::{is_sensitive_path, resolve_local_path, PathKind},
    process::{run_process, ProcessRequest},
    read::string_argument,
    LocalToolConfig,
};

const MAX_GLOB_MATCHES: usize = 100;
const DEFAULT_GREP_HEAD: usize = 250;

pub struct GlobTool {
    config: LocalToolConfig,
}

impl GlobTool {
    pub fn new(config: LocalToolConfig) -> Self {
        Self { config }
    }
}

impl ExecutableTool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Glob".to_owned(),
            description: "Find files by glob pattern with bounded output.".to_owned(),
            parameters: glob_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<crate::ToolExecutionSpec, ToolError> {
        let pattern = string_argument(arguments, "pattern")?;
        let root = search_root(&self.config, arguments).map_err(ToolError::Prepare)?;
        let mut spec = base_spec(
            ToolInputDisplay::FileIo {
                operation: FileOperation::Glob,
                path: root.to_string_lossy().into_owned(),
                detail: Some(format!("pattern: {pattern}")),
                content: None,
                before: None,
                after: None,
            },
            "Glob",
        );
        spec.accesses = vec![ToolAccess::file(root, FileAccessMode::Search)];
        spec.description = Some(format!("Searching {pattern}"));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let pattern = string_argument(&invocation.arguments, "pattern")?;
            let root = match search_root(&self.config, &invocation.arguments) {
                Ok(root) => root,
                Err(error) => return Ok(error_result(error)),
            };
            let mut args: Vec<OsString> = vec![
                "--files".into(),
                "--hidden".into(),
                "--null".into(),
                "--sortr=modified".into(),
            ];
            add_exclusion_globs(&mut args);
            if bool_argument(&invocation.arguments, "include_ignored") {
                args.push("--no-ignore".into());
            }
            args.extend(["--glob".into(), pattern.into(), "--".into(), ".".into()]);
            let outcome = run_process(ProcessRequest {
                program: Path::new("rg"),
                args: &args,
                cwd: &root,
                env: &[],
                timeout: self.config.search_timeout,
                cancellation: &invocation.cancellation,
                updates: Arc::clone(&invocation.updates),
                stream_updates: false,
            })
            .await;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => return Ok(error_result(format!("Glob requires ripgrep: {error}"))),
            };
            if outcome.cancelled {
                return Ok(error_result("Glob aborted"));
            }
            if outcome.timed_out && outcome.stdout.is_empty() {
                return Ok(error_result(format!(
                    "Glob timed out after {}s; use a narrower path or pattern",
                    self.config.search_timeout.as_secs_f64()
                )));
            }
            if !matches!(outcome.exit_code, Some(0 | 1)) && !outcome.timed_out {
                return Ok(error_result(search_error("Glob", &outcome.stderr)));
            }
            let mut matches = Vec::new();
            for raw in complete_nul_records(&outcome.stdout) {
                let Ok(raw) = std::str::from_utf8(raw) else {
                    continue;
                };
                if let Some(path) = safe_search_result(&self.config, &root, raw) {
                    matches.push(display_search_path(self.config.cwd(), &root, &path));
                    if matches.len() == MAX_GLOB_MATCHES {
                        break;
                    }
                }
            }
            let truncated =
                matches.len() == MAX_GLOB_MATCHES || outcome.raw_truncated || outcome.timed_out;
            let mut buffer = OutputBuffer::default();
            if matches.is_empty() {
                buffer.push("No non-sensitive matches found");
            } else {
                buffer.push(&matches.join("\n"));
            }
            if truncated {
                buffer.push("\n[Glob results truncated; use a narrower path or pattern]");
            }
            Ok(buffer.into_result(false, None))
        })
    }
}

pub struct GrepTool {
    config: LocalToolConfig,
}

impl GrepTool {
    pub fn new(config: LocalToolConfig) -> Self {
        Self { config }
    }
}

impl ExecutableTool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "Grep".to_owned(),
            description: "Search file contents with ripgrep and bounded output.".to_owned(),
            parameters: grep_schema(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<crate::ToolExecutionSpec, ToolError> {
        let pattern = string_argument(arguments, "pattern")?;
        let target = grep_target(&self.config, arguments).map_err(ToolError::Prepare)?;
        let mut spec = base_spec(
            ToolInputDisplay::FileIo {
                operation: FileOperation::Grep,
                path: target.path.to_string_lossy().into_owned(),
                detail: Some(format!("pattern: {pattern}")),
                content: None,
                before: None,
                after: None,
            },
            "Grep",
        );
        spec.accesses = vec![ToolAccess::file(target.path, FileAccessMode::Search)];
        spec.description = Some(format!("Searching for {pattern}"));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let pattern = string_argument(&invocation.arguments, "pattern")?;
            let target = match grep_target(&self.config, &invocation.arguments) {
                Ok(target) => target,
                Err(error) => return Ok(error_result(error)),
            };
            let mode = invocation
                .arguments
                .get("output_mode")
                .and_then(Value::as_str)
                .unwrap_or("files_with_matches");
            let mut args = grep_args(&invocation.arguments, mode);
            args.extend(["--".into(), pattern.into(), target.operand.clone()]);
            let outcome = match run_process(ProcessRequest {
                program: Path::new("rg"),
                args: &args,
                cwd: &target.cwd,
                env: &[],
                timeout: self.config.search_timeout,
                cancellation: &invocation.cancellation,
                updates: Arc::clone(&invocation.updates),
                stream_updates: false,
            })
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => return Ok(error_result(format!("Grep requires ripgrep: {error}"))),
            };
            if outcome.cancelled {
                return Ok(error_result("Grep aborted"));
            }
            if outcome.timed_out && outcome.stdout.is_empty() {
                return Ok(error_result(format!(
                    "Grep timed out after {}s; use a narrower path or pattern",
                    self.config.search_timeout.as_secs_f64()
                )));
            }
            if !matches!(outcome.exit_code, Some(0 | 1)) && !outcome.timed_out {
                return Ok(error_result(search_error("Grep", &outcome.stderr)));
            }

            let mut lines = if mode == "content" {
                parse_json_content(
                    &self.config,
                    &target.cwd,
                    &outcome.stdout,
                    invocation.arguments.get("-n").and_then(Value::as_bool) != Some(false),
                )
            } else if mode == "count_matches" {
                parse_nul_counts(&self.config, &target.cwd, &outcome.stdout)
            } else {
                parse_nul_files(&self.config, &target.cwd, &outcome.stdout)
            };
            if mode == "files_with_matches" {
                lines.sort_by(|left, right| {
                    modified_time(self.config.cwd(), right)
                        .cmp(&modified_time(self.config.cwd(), left))
                });
            }
            let offset = usize_argument(&invocation.arguments, "offset").unwrap_or(0);
            let head =
                usize_argument(&invocation.arguments, "head_limit").unwrap_or(DEFAULT_GREP_HEAD);
            let total = lines.len();
            let visible = lines.into_iter().skip(offset);
            let visible: Vec<_> = if head == 0 {
                visible.collect()
            } else {
                visible.take(head).collect()
            };
            let pagination_truncated = head > 0 && offset.saturating_add(visible.len()) < total;
            let mut buffer = OutputBuffer::default();
            if visible.is_empty() {
                buffer.push("No non-sensitive matches found");
            } else {
                buffer.push(&visible.join("\n"));
            }
            if pagination_truncated {
                buffer.push(&format!(
                    "\nResults truncated to {head} lines (total: {total}). Use offset={} to continue.",
                    offset + head
                ));
            }
            if outcome.raw_truncated {
                buffer.push("\n[ripgrep output truncated at 10 MiB]");
            }
            if outcome.timed_out {
                buffer.push("\n[Grep timed out; partial results returned]");
            }
            Ok(buffer.into_result(false, None))
        })
    }
}

fn search_root(config: &LocalToolConfig, arguments: &Value) -> Result<PathBuf, String> {
    let input = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
    resolve_local_path(config, input, PathKind::Directory)
}

struct GrepTarget {
    path: PathBuf,
    cwd: PathBuf,
    operand: OsString,
}

fn grep_target(config: &LocalToolConfig, arguments: &Value) -> Result<GrepTarget, String> {
    let input = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
    if let Ok(path) = resolve_local_path(config, input, PathKind::Directory) {
        return Ok(GrepTarget {
            cwd: path.clone(),
            path,
            operand: ".".into(),
        });
    }
    let path = resolve_local_path(config, input, PathKind::File)?;
    let cwd = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("file {input:?} has no parent directory"))?;
    let operand = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| format!("file {input:?} has no filename"))?;
    Ok(GrepTarget { path, cwd, operand })
}

fn add_exclusion_globs(args: &mut Vec<OsString>) {
    for pattern in ["!.git", "!.svn", "!.hg", "!.bzr", "!.jj", "!.sl"] {
        args.extend(["--glob".into(), pattern.into()]);
    }
    args.extend(["--glob".into(), "!**/.env".into()]);
    for name in ["id_rsa", "id_ed25519", "id_ecdsa"] {
        for pattern in [
            format!("!**/{name}"),
            format!("!**/{name}[-_]*"),
            format!("!**/{name}.bak"),
            format!("!**/{name}.backup"),
            format!("!**/{name}.copy"),
            format!("!**/{name}.disabled"),
            format!("!**/{name}.key"),
            format!("!**/{name}.old"),
            format!("!**/{name}.orig"),
            format!("!**/{name}.pem"),
            format!("!**/{name}.save"),
            format!("!**/{name}.tmp"),
        ] {
            args.extend(["--glob".into(), pattern.into()]);
        }
    }
    for pattern in [
        "!**/.aws/credentials",
        "!**/.aws/credentials/**",
        "!**/.gcp/credentials",
        "!**/.gcp/credentials/**",
    ] {
        args.extend(["--glob".into(), pattern.into()]);
    }
}

fn grep_args(arguments: &Value, mode: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["--hidden".into(), "--color=never".into()];
    add_exclusion_globs(&mut args);
    match mode {
        "content" => {
            args.push("--json".into());
            if arguments.get("-n").and_then(Value::as_bool) != Some(false) {
                args.push("--line-number".into());
            }
        }
        "count_matches" => args.extend(["--count-matches".into(), "--null".into()]),
        _ => args.extend(["--files-with-matches".into(), "--null".into()]),
    }
    if bool_argument(arguments, "-i") {
        args.push("--ignore-case".into());
    }
    if mode == "content" {
        if let Some(value) = usize_argument(arguments, "-C") {
            args.extend(["--context".into(), value.to_string().into()]);
        } else {
            for (name, flag) in [("-A", "--after-context"), ("-B", "--before-context")] {
                if let Some(value) = usize_argument(arguments, name) {
                    args.extend([flag.into(), value.to_string().into()]);
                }
            }
        }
    }
    if let Some(glob) = arguments.get("glob").and_then(Value::as_str) {
        args.extend(["--glob".into(), glob.into()]);
    }
    if let Some(kind) = arguments.get("type").and_then(Value::as_str) {
        args.extend(["--type".into(), kind.into()]);
    }
    if bool_argument(arguments, "multiline") {
        args.extend(["--multiline".into(), "--multiline-dotall".into()]);
    }
    if bool_argument(arguments, "include_ignored") {
        args.push("--no-ignore".into());
    }
    args
}

fn parse_nul_files(config: &LocalToolConfig, root: &Path, stdout: &[u8]) -> Vec<String> {
    complete_nul_records(stdout)
        .into_iter()
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .filter_map(|raw| safe_search_result(config, root, raw.trim_end_matches('\n')))
        .map(|path| display_search_path(config.cwd(), root, &path))
        .collect()
}

fn parse_nul_counts(config: &LocalToolConfig, root: &Path, stdout: &[u8]) -> Vec<String> {
    let mut output = Vec::new();
    let mut cursor = stdout;
    while let Some(index) = cursor.iter().position(|byte| *byte == 0) {
        let path = std::str::from_utf8(&cursor[..index]).ok();
        cursor = &cursor[index + 1..];
        let end = cursor
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(cursor.len());
        let count = std::str::from_utf8(&cursor[..end]).ok();
        cursor = if end < cursor.len() {
            &cursor[end + 1..]
        } else {
            &[]
        };
        if let (Some(path), Some(count)) = (path, count) {
            if let Some(path) = safe_search_result(config, root, path) {
                output.push(format!(
                    "{}:{count}",
                    display_search_path(config.cwd(), root, &path)
                ));
            }
        }
    }
    output
}

fn parse_json_content(
    config: &LocalToolConfig,
    root: &Path,
    stdout: &[u8],
    include_line_number: bool,
) -> Vec<String> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            matches!(
                record.get("type").and_then(Value::as_str),
                Some("match" | "context")
            )
        })
        .filter_map(|record| {
            let data = record.get("data")?;
            let raw_path = data.get("path")?.get("text")?.as_str()?;
            let path = safe_search_result(config, root, raw_path)?;
            let line = data
                .get("lines")?
                .get("text")?
                .as_str()?
                .trim_end_matches(['\r', '\n']);
            let line_number = data.get("line_number").and_then(Value::as_u64);
            let displayed = display_search_path(config.cwd(), root, &path);
            Some(match line_number.filter(|_| include_line_number) {
                Some(line_number) => format!("{displayed}:{line_number}:{line}"),
                None => format!("{displayed}:{line}"),
            })
        })
        .collect()
}

fn complete_nul_records(stdout: &[u8]) -> Vec<&[u8]> {
    let mut records: Vec<&[u8]> = stdout.split(|byte| *byte == 0).collect();
    if stdout.last() == Some(&0) {
        records.pop();
    } else {
        // A capped or interrupted record has no terminator and must never be
        // surfaced as though it were a complete filesystem path.
        records.pop();
    }
    records
        .into_iter()
        .filter(|record| !record.is_empty())
        .collect()
}

fn safe_search_result(config: &LocalToolConfig, root: &Path, raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let relative = raw.strip_prefix("./").unwrap_or(raw);
    let candidate = root.join(relative);
    if is_sensitive_path(&candidate) {
        return None;
    }
    resolve_local_path(config, &candidate.to_string_lossy(), PathKind::File).ok()
}

fn display_search_path(cwd: &Path, root: &Path, path: &Path) -> String {
    if root.starts_with(cwd) {
        path.strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn modified_time(root: &Path, displayed: &str) -> Option<std::time::SystemTime> {
    let path = if Path::new(displayed).is_absolute() {
        PathBuf::from(displayed)
    } else {
        root.join(displayed)
    };
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn search_error(tool: &str, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    if detail.trim().is_empty() {
        format!("{tool} failed")
    } else {
        format!("{tool} failed: {}", detail.trim())
    }
}

fn bool_argument(arguments: &Value, name: &str) -> bool {
    arguments
        .get(name)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn usize_argument(arguments: &Value, name: &str) -> Option<usize> {
    arguments
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}
