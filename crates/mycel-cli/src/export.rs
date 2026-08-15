use std::{
    collections::{BTreeMap, HashSet},
    fmt, fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use mycel_agent_protocol::{
    validate_record_sequence, validate_session_id, AgentRecord, SessionMeta,
};
use mycel_agent_runtime::SessionIndex;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    cli::ExportArgs,
    runtime::{AdapterOutput, RuntimeCompletion},
};

const MANIFEST_NAME: &str = "manifest.json";
const MAIN_RECORDS: &str = "agents/main/records.jsonl";
const GLOBAL_LOG_NAME: &str = "logs/global/mycel.log";
const MAX_ENTRY_COUNT: usize = 4_096;
const MAX_ENTRY_SIZE: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_SIZE: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_NAME_BYTES: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExportSummary {
    pub id: String,
    pub title: Option<String>,
    pub work_dir: Option<PathBuf>,
    pub session_dir: PathBuf,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionExportLookupError {
    Failed(String),
}

impl fmt::Display for SessionExportLookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SessionExportLookupError {}

/// Session discovery boundary backed by the runtime's durable session index.
pub trait SessionExportStore: Send + Sync {
    fn find_by_id(
        &self,
        sessions_root: &Path,
        id: &str,
    ) -> Result<Option<SessionExportSummary>, SessionExportLookupError>;

    fn newest_for_cwd(
        &self,
        sessions_root: &Path,
        cwd: &Path,
    ) -> Result<Option<SessionExportSummary>, SessionExportLookupError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FilesystemSessionExportStore;

impl SessionExportStore for FilesystemSessionExportStore {
    fn find_by_id(
        &self,
        sessions_root: &Path,
        id: &str,
    ) -> Result<Option<SessionExportSummary>, SessionExportLookupError> {
        session_index(sessions_root)?
            .get(id)
            .map_err(|error| SessionExportLookupError::Failed(error.to_string()))
            .map(|summary| summary.map(export_summary))
    }

    fn newest_for_cwd(
        &self,
        sessions_root: &Path,
        cwd: &Path,
    ) -> Result<Option<SessionExportSummary>, SessionExportLookupError> {
        session_index(sessions_root)?
            .newest_for_cwd(cwd)
            .map_err(|error| SessionExportLookupError::Failed(error.to_string()))
            .map(|summary| summary.map(export_summary))
    }
}

fn session_index(sessions_root: &Path) -> Result<SessionIndex, SessionExportLookupError> {
    let home = sessions_root.parent().ok_or_else(|| {
        SessionExportLookupError::Failed(format!(
            "session root {} has no Mycel home parent",
            sessions_root.display()
        ))
    })?;
    Ok(SessionIndex::new(home))
}

fn export_summary(summary: mycel_agent_protocol::SessionSummary) -> SessionExportSummary {
    SessionExportSummary {
        id: summary.id,
        title: summary.title,
        work_dir: Some(PathBuf::from(summary.work_dir)),
        session_dir: PathBuf::from(summary.session_dir),
        updated_at: summary.updated_at,
    }
}

/// Owns the interactive confirmation prompt and response. Process-backed
/// implementations render immediately; tests can inject deterministic input.
pub trait ExportConfirmation: Send + Sync {
    fn confirm(&self, prompt: &str, default_yes: bool) -> io::Result<bool>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessExportConfirmation;

impl ExportConfirmation for ProcessExportConfirmation {
    fn confirm(&self, prompt: &str, default_yes: bool) -> io::Result<bool> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(prompt.as_bytes())?;
        stderr.flush()?;
        drop(stderr);

        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "confirmation input closed",
            ));
        }
        let answer = answer.trim().to_ascii_lowercase();
        if answer.is_empty() {
            Ok(default_yes)
        } else if answer == "y" || answer == "yes" {
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(crate) fn run_export(
    args: &ExportArgs,
    home: &Path,
    cwd: &Path,
    store: &dyn SessionExportStore,
    confirmation: &dyn ExportConfirmation,
    version: &str,
) -> AdapterOutput {
    match try_export(args, home, cwd, store, confirmation, version) {
        Ok(ExportOutcome::Written(path)) => {
            AdapterOutput::success(format!("{}\n", path.display()), String::new())
        }
        Ok(ExportOutcome::Cancelled) => {
            AdapterOutput::success("Export cancelled.\n", String::new())
        }
        Err(message) => AdapterOutput {
            stdout: String::new(),
            stderr: format!("{message}\n"),
            completion: RuntimeCompletion::failure(),
        },
    }
}

enum ExportOutcome {
    Written(PathBuf),
    Cancelled,
}

fn try_export(
    args: &ExportArgs,
    home: &Path,
    cwd: &Path,
    store: &dyn SessionExportStore,
    confirmation: &dyn ExportConfirmation,
    version: &str,
) -> Result<ExportOutcome, String> {
    let sessions_root = home.join("sessions");
    let requested_id = args
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let summary = if let Some(id) = requested_id {
        let summary = store
            .find_by_id(&sessions_root, id)
            .map_err(|error| format!("Could not resolve session {id:?}: {error}"))?
            .ok_or_else(|| format!("Session {id:?} was not found."))?;
        validate_summary_location(&summary, &sessions_root)?;
        summary
    } else {
        let summary = store
            .newest_for_cwd(&sessions_root, cwd)
            .map_err(|error| format!("Could not resolve the previous session: {error}"))?;
        let Some(summary) = summary else {
            return Err("No previous session found to export.".to_owned());
        };
        validate_summary_location(&summary, &sessions_root)?;
        if !args.yes {
            let label = summary.title.as_ref().map_or_else(
                || summary.id.clone(),
                |title| format!("{title} ({})", summary.id),
            );
            let prompt = format!("Export previous session {label:?}? [Y/n] ");
            let confirmed = confirmation
                .confirm(&prompt, true)
                .map_err(|error| format!("Could not read export confirmation: {error}"))?;
            if !confirmed {
                return Ok(ExportOutcome::Cancelled);
            }
        }
        summary
    };

    let output = args.output.as_ref().map_or_else(
        || cwd.join(default_export_name(&summary.id)),
        |path| {
            let combined = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            normalize_path(&combined)
        },
    );
    if output.exists() && !args.yes {
        let prompt = format!(
            "Output {} already exists. Overwrite? [y/N] ",
            output.display()
        );
        let confirmed = confirmation
            .confirm(&prompt, false)
            .map_err(|error| format!("Could not read overwrite confirmation: {error}"))?;
        if !confirmed {
            return Ok(ExportOutcome::Cancelled);
        }
    }

    let entries = collect_export_entries(&summary, home, args.include_global_log, version)?;
    write_zip_atomic(&output, &entries).map_err(|error| {
        format!(
            "Could not write session export to {}: {error}",
            output.display()
        )
    })?;
    Ok(ExportOutcome::Written(output))
}

fn validate_summary_location(
    summary: &SessionExportSummary,
    sessions_root: &Path,
) -> Result<(), String> {
    validate_session_id(&summary.id)
        .map_err(|error| format!("invalid session id {:?}: {error}", summary.id))?;
    let canonical_root = fs::canonicalize(sessions_root).map_err(|error| {
        format!(
            "could not resolve session root {}: {error}",
            sessions_root.display()
        )
    })?;
    let canonical_session = fs::canonicalize(&summary.session_dir).map_err(|error| {
        format!(
            "could not resolve session directory {}: {error}",
            summary.session_dir.display()
        )
    })?;
    if !canonical_session.starts_with(&canonical_root) || canonical_session == canonical_root {
        return Err(format!(
            "session directory {} escapes session root {}",
            summary.session_dir.display(),
            sessions_root.display()
        ));
    }
    Ok(())
}

fn default_export_name(id: &str) -> String {
    let short: String = id.chars().take(8).collect();
    format!("mycel-debug-{short}.zip")
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Debug, Clone)]
struct ArchiveEntry {
    name: String,
    data: Vec<u8>,
}

fn collect_export_entries(
    summary: &SessionExportSummary,
    home: &Path,
    include_global_log: bool,
    version: &str,
) -> Result<Vec<ArchiveEntry>, String> {
    validate_session_id(&summary.id)
        .map_err(|error| format!("invalid session id {:?}: {error}", summary.id))?;
    let root_metadata = fs::symlink_metadata(&summary.session_dir).map_err(|error| {
        format!(
            "Session {:?} has no exportable directory at {}: {error}",
            summary.id,
            summary.session_dir.display()
        )
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(format!(
            "Session {:?} does not have a regular exportable directory at {}",
            summary.id,
            summary.session_dir.display()
        ));
    }

    let mut entries = Vec::new();
    let mut total_size = 0_u64;
    collect_directory(
        &summary.session_dir,
        &summary.session_dir,
        &mut entries,
        &mut total_size,
    )?;
    if entries.is_empty() {
        return Err(format!("Session {:?} has no exportable files.", summary.id));
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    validate_record_logs(&entries)?;
    validate_meta_entry(&entries)?;

    let mut names: HashSet<String> = entries.iter().map(|entry| entry.name.clone()).collect();
    if !names.insert(MANIFEST_NAME.to_owned()) {
        return Err(format!(
            "session contains reserved archive path {MANIFEST_NAME:?}"
        ));
    }
    if include_global_log {
        if names.contains(GLOBAL_LOG_NAME) {
            return Err(format!(
                "session contains reserved archive path {GLOBAL_LOG_NAME:?}"
            ));
        }
        let global_log = home.join("logs").join("mycel.log");
        match read_regular_file(&global_log, &mut total_size) {
            Ok(Some(data)) => {
                entries.push(ArchiveEntry {
                    name: GLOBAL_LOG_NAME.to_owned(),
                    data,
                });
            }
            Ok(None) => {}
            Err(message) => return Err(message),
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    if entries.len().saturating_add(1) > MAX_ENTRY_COUNT {
        return Err(format!(
            "session export exceeds the {MAX_ENTRY_COUNT} entry limit"
        ));
    }

    let manifest = build_manifest(summary, version, entries.iter().map(|entry| &entry.name))?;
    add_size(&mut total_size, manifest.len() as u64, MANIFEST_NAME)?;
    let mut archive = Vec::with_capacity(entries.len() + 1);
    archive.push(ArchiveEntry {
        name: MANIFEST_NAME.to_owned(),
        data: manifest,
    });
    archive.extend(entries);
    Ok(archive)
}

fn collect_directory(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<ArchiveEntry>,
    total_size: &mut u64,
) -> Result<(), String> {
    let mut children = fs::read_dir(directory)
        .map_err(|error| format!("could not read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("could not enumerate {}: {error}", directory.display()))?;
    children.sort_by_key(fs::DirEntry::file_name);
    for child in children {
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing to export symbolic link {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_directory(root, &path, entries, total_size)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "refusing to export non-regular file {}",
                path.display()
            ));
        }
        if entries.len() >= MAX_ENTRY_COUNT {
            return Err(format!(
                "session export exceeds the {MAX_ENTRY_COUNT} entry limit"
            ));
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            format!(
                "session file {} escaped export root {}",
                path.display(),
                root.display()
            )
        })?;
        let name = archive_name(relative)?;
        let data = read_file_limited(&path, &name)?;
        add_size(total_size, data.len() as u64, &name)?;
        entries.push(ArchiveEntry { name, data });
    }
    Ok(())
}

fn archive_name(path: &Path) -> Result<String, String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(format!("unsafe archive path {:?}", path));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| format!("archive path is not UTF-8: {path:?}"))?;
                if part.is_empty() || part.contains('\0') || part.contains('\\') {
                    return Err(format!("unsafe archive path {path:?}"));
                }
                parts.push(part);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(format!("unsafe archive path {path:?}")),
        }
    }
    let name = parts.join("/");
    if name.is_empty() || name.len() > MAX_ARCHIVE_NAME_BYTES {
        return Err(format!("unsafe archive path {path:?}"));
    }
    Ok(name)
}

fn add_size(total: &mut u64, size: u64, name: &str) -> Result<(), String> {
    if size > MAX_ENTRY_SIZE {
        return Err(format!(
            "archive entry {name:?} exceeds the {MAX_ENTRY_SIZE} byte limit"
        ));
    }
    *total = total
        .checked_add(size)
        .ok_or_else(|| "session export size overflow".to_owned())?;
    if *total > MAX_ARCHIVE_SIZE {
        return Err(format!(
            "session export exceeds the {MAX_ARCHIVE_SIZE} byte limit"
        ));
    }
    Ok(())
}

fn read_regular_file(path: &Path, total_size: &mut u64) -> Result<Option<Vec<u8>>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "refusing to export non-regular log {}",
            path.display()
        ));
    }
    let data = read_file_limited(path, GLOBAL_LOG_NAME)?;
    add_size(total_size, data.len() as u64, GLOBAL_LOG_NAME)?;
    Ok(Some(data))
}

fn read_file_limited(path: &Path, archive_name: &str) -> Result<Vec<u8>, String> {
    let file = open_no_follow(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "refusing to export non-regular file {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_ENTRY_SIZE {
        return Err(format!(
            "archive entry {archive_name:?} exceeds the {MAX_ENTRY_SIZE} byte limit"
        ));
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ENTRY_SIZE + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if data.len() as u64 > MAX_ENTRY_SIZE {
        return Err(format!(
            "archive entry {archive_name:?} exceeds the {MAX_ENTRY_SIZE} byte limit"
        ));
    }
    Ok(data)
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

fn validate_record_logs(entries: &[ArchiveEntry]) -> Result<(), String> {
    if !entries.iter().any(|entry| entry.name == MAIN_RECORDS) {
        return Err(format!(
            "session is missing required record log {MAIN_RECORDS}"
        ));
    }
    for entry in entries
        .iter()
        .filter(|entry| entry.name.ends_with("/records.jsonl"))
    {
        let source = std::str::from_utf8(&entry.data)
            .map_err(|error| format!("{} is not UTF-8: {error}", entry.name))?;
        let mut records = Vec::new();
        for (index, line) in source.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: AgentRecord = serde_json::from_str(line).map_err(|error| {
                format!("{}:{} is malformed JSON: {error}", entry.name, index + 1)
            })?;
            records.push(record);
        }
        validate_record_sequence(&records)
            .map_err(|error| format!("{} is not a valid record log: {error}", entry.name))?;
    }
    Ok(())
}

fn validate_meta_entry(entries: &[ArchiveEntry]) -> Result<(), String> {
    let Some(entry) = entries.iter().find(|entry| entry.name == "meta.json") else {
        return Ok(());
    };
    let metadata: SessionMeta = serde_json::from_slice(&entry.data)
        .map_err(|error| format!("meta.json is malformed: {error}"))?;
    metadata
        .validate()
        .map_err(|error| format!("meta.json is invalid: {error}"))
}

fn build_manifest<'a>(
    summary: &SessionExportSummary,
    version: &str,
    entry_names: impl Iterator<Item = &'a String>,
) -> Result<Vec<u8>, String> {
    let mut manifest = BTreeMap::new();
    manifest.insert("exportedBy", Value::String("mycel-cli".to_owned()));
    manifest.insert(
        "entries",
        Value::Array(entry_names.cloned().map(Value::String).collect()),
    );
    manifest.insert("mycelVersion", Value::String(version.to_owned()));
    manifest.insert("schemaVersion", Value::from(1));
    manifest.insert("sessionId", Value::String(summary.id.clone()));
    if let Some(title) = &summary.title {
        manifest.insert("title", Value::String(title.clone()));
    }
    if let Some(work_dir) = &summary.work_dir {
        manifest.insert(
            "workspaceDir",
            Value::String(work_dir.to_string_lossy().into_owned()),
        );
    }
    let mut bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("could not encode export manifest: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_zip_atomic(path: &Path, entries: &[ArchiveEntry]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mycel-export.zip");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        write_stored_zip(&mut file, entries)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_stored_zip(writer: &mut dyn Write, entries: &[ArchiveEntry]) -> io::Result<()> {
    let mut central = Vec::new();
    let mut offset = 0_u32;
    for entry in entries {
        validate_zip_entry(entry)?;
        let name = entry.name.as_bytes();
        let size = u32::try_from(entry.data.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP entry is too large"))?;
        let name_len = u16::try_from(name.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ZIP name is too long"))?;
        let crc = crc32(&entry.data);

        write_u32(writer, 0x0403_4b50)?;
        write_u16(writer, 20)?;
        write_u16(writer, 1 << 11)?;
        write_u16(writer, 0)?;
        write_u16(writer, 0)?;
        write_u16(writer, 33)?;
        write_u32(writer, crc)?;
        write_u32(writer, size)?;
        write_u32(writer, size)?;
        write_u16(writer, name_len)?;
        write_u16(writer, 0)?;
        writer.write_all(name)?;
        writer.write_all(&entry.data)?;

        central.push((entry, crc, size, name_len, offset));
        let local_size = 30_u32
            .checked_add(u32::from(name_len))
            .and_then(|value| value.checked_add(size))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ZIP size overflow"))?;
        offset = offset
            .checked_add(local_size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ZIP offset overflow"))?;
    }

    let central_offset = offset;
    let mut central_size = 0_u32;
    for (entry, crc, size, name_len, local_offset) in central {
        write_u32(writer, 0x0201_4b50)?;
        write_u16(writer, 20)?;
        write_u16(writer, 20)?;
        write_u16(writer, 1 << 11)?;
        write_u16(writer, 0)?;
        write_u16(writer, 0)?;
        write_u16(writer, 33)?;
        write_u32(writer, crc)?;
        write_u32(writer, size)?;
        write_u32(writer, size)?;
        write_u16(writer, name_len)?;
        write_u16(writer, 0)?;
        write_u16(writer, 0)?;
        write_u16(writer, 0)?;
        write_u16(writer, 0)?;
        write_u32(writer, 0)?;
        write_u32(writer, local_offset)?;
        writer.write_all(entry.name.as_bytes())?;
        central_size = central_size
            .checked_add(46 + u32::from(name_len))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ZIP size overflow"))?;
    }
    let count = u16::try_from(entries.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many ZIP entries"))?;
    write_u32(writer, 0x0605_4b50)?;
    write_u16(writer, 0)?;
    write_u16(writer, 0)?;
    write_u16(writer, count)?;
    write_u16(writer, count)?;
    write_u32(writer, central_size)?;
    write_u32(writer, central_offset)?;
    write_u16(writer, 0)
}

fn validate_zip_entry(entry: &ArchiveEntry) -> io::Result<()> {
    let normalized = archive_name(Path::new(&entry.name))
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    if normalized != entry.name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZIP entry name is not normalized",
        ));
    }
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn write_u16(writer: &mut dyn Write, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u32(writer: &mut dyn Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use tempfile::TempDir;

    use super::*;

    const VALID_RECORDS: &str =
        "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",\"created_at\":0}\n";

    struct MemoryStore {
        summary: Option<SessionExportSummary>,
        cwd_unavailable: bool,
    }

    impl SessionExportStore for MemoryStore {
        fn find_by_id(
            &self,
            _sessions_root: &Path,
            id: &str,
        ) -> Result<Option<SessionExportSummary>, SessionExportLookupError> {
            Ok(self.summary.clone().filter(|summary| summary.id == id))
        }

        fn newest_for_cwd(
            &self,
            _sessions_root: &Path,
            _cwd: &Path,
        ) -> Result<Option<SessionExportSummary>, SessionExportLookupError> {
            if self.cwd_unavailable {
                Err(SessionExportLookupError::Failed("index failure".to_owned()))
            } else {
                Ok(self.summary.clone())
            }
        }
    }

    #[derive(Default)]
    struct ScriptedConfirmation {
        answers: Mutex<VecDeque<bool>>,
        prompts: Mutex<Vec<String>>,
    }

    impl ScriptedConfirmation {
        fn with_answers(answers: impl IntoIterator<Item = bool>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().collect()),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    impl ExportConfirmation for ScriptedConfirmation {
        fn confirm(&self, prompt: &str, _default_yes: bool) -> io::Result<bool> {
            self.prompts
                .lock()
                .expect("prompts")
                .push(prompt.to_owned());
            self.answers
                .lock()
                .expect("answers")
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no answer"))
        }
    }

    fn fixture() -> (TempDir, SessionExportSummary) {
        let temp = TempDir::new().expect("temp");
        let session_dir = temp.path().join("sessions/session-1");
        fs::create_dir_all(session_dir.join("agents/main")).expect("dirs");
        fs::write(session_dir.join(MAIN_RECORDS), VALID_RECORDS).expect("records");
        fs::write(session_dir.join("note.txt"), "hello\n").expect("note");
        let summary = SessionExportSummary {
            id: "session-1".to_owned(),
            title: Some("First turn".to_owned()),
            work_dir: Some(temp.path().to_owned()),
            session_dir,
            updated_at: 7,
        };
        (temp, summary)
    }

    fn args(id: Option<&str>, output: &Path) -> ExportArgs {
        ExportArgs {
            session_id: id.map(str::to_owned),
            output: Some(output.to_owned()),
            yes: false,
            include_global_log: true,
        }
    }

    #[test]
    fn explicit_session_writes_deterministic_byte_valid_zip_with_global_log() {
        let (temp, summary) = fixture();
        fs::create_dir_all(temp.path().join("logs")).expect("logs");
        fs::write(temp.path().join("logs/mycel.log"), "diagnostic\n").expect("log");
        let store = MemoryStore {
            summary: Some(summary),
            cwd_unavailable: false,
        };
        let confirmation = ScriptedConfirmation::default();
        let first = temp.path().join("first.zip");
        let second = temp.path().join("second.zip");

        let output = run_export(
            &args(Some("session-1"), &first),
            temp.path(),
            temp.path(),
            &store,
            &confirmation,
            "0.2.0",
        );
        assert_eq!(output.completion, RuntimeCompletion::success());
        let output = run_export(
            &args(Some("session-1"), &second),
            temp.path(),
            temp.path(),
            &store,
            &confirmation,
            "0.2.0",
        );
        assert_eq!(output.completion, RuntimeCompletion::success());
        let first_bytes = fs::read(first).expect("first zip");
        assert_eq!(first_bytes, fs::read(second).expect("second zip"));
        let parsed = parse_stored_zip(&first_bytes);
        assert_eq!(
            parsed.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "agents/main/records.jsonl",
                "logs/global/mycel.log",
                "manifest.json",
                "note.txt"
            ]
        );
        assert_eq!(parsed["logs/global/mycel.log"], b"diagnostic\n");
        assert!(String::from_utf8_lossy(&parsed["manifest.json"])
            .contains("\"sessionId\": \"session-1\""));
        assert!(first_bytes.windows(4).any(|bytes| bytes == b"PK\x01\x02"));
        assert!(first_bytes.ends_with(&[0, 0]));
    }

    #[test]
    fn newest_session_uses_exact_confirmation_prompt() {
        let (temp, summary) = fixture();
        let store = MemoryStore {
            summary: Some(summary),
            cwd_unavailable: false,
        };
        let confirmation = ScriptedConfirmation::with_answers([true]);
        let output_path = temp.path().join("export.zip");
        let output = run_export(
            &args(None, &output_path),
            temp.path(),
            temp.path(),
            &store,
            &confirmation,
            "0.2.0",
        );
        assert_eq!(output.completion, RuntimeCompletion::success());
        assert_eq!(
            confirmation.prompts.lock().expect("prompts").as_slice(),
            ["Export previous session \"First turn (session-1)\"? [Y/n] "]
        );
    }

    #[test]
    fn cancellation_is_success_and_does_not_write() {
        let (temp, summary) = fixture();
        let store = MemoryStore {
            summary: Some(summary),
            cwd_unavailable: false,
        };
        let confirmation = ScriptedConfirmation::with_answers([false]);
        let output_path = temp.path().join("export.zip");
        let output = run_export(
            &args(None, &output_path),
            temp.path(),
            temp.path(),
            &store,
            &confirmation,
            "0.2.0",
        );
        assert_eq!(output.stdout, "Export cancelled.\n");
        assert_eq!(output.completion, RuntimeCompletion::success());
        assert!(!output_path.exists());
    }

    #[test]
    fn missing_previous_session_uses_retained_failure_text() {
        let temp = TempDir::new().expect("temp");
        let store = MemoryStore {
            summary: None,
            cwd_unavailable: false,
        };
        let output = run_export(
            &ExportArgs {
                session_id: None,
                output: None,
                yes: false,
                include_global_log: true,
            },
            temp.path(),
            temp.path(),
            &store,
            &ScriptedConfirmation::default(),
            "0.2.0",
        );
        assert_eq!(output.stderr, "No previous session found to export.\n");
        assert_eq!(output.completion, RuntimeCompletion::failure());
    }

    #[test]
    fn no_include_global_log_omits_the_active_log() {
        let (temp, summary) = fixture();
        fs::create_dir_all(temp.path().join("logs")).expect("logs");
        fs::write(temp.path().join("logs/mycel.log"), "diagnostic\n").expect("log");
        let entries =
            collect_export_entries(&summary, temp.path(), false, "0.2.0").expect("export entries");
        assert!(!entries.iter().any(|entry| entry.name == GLOBAL_LOG_NAME));
    }

    #[test]
    fn overwrite_requires_confirmation_and_preserves_declined_target() {
        let (temp, summary) = fixture();
        let store = MemoryStore {
            summary: Some(summary),
            cwd_unavailable: false,
        };
        let confirmation = ScriptedConfirmation::with_answers([false]);
        let output_path = temp.path().join("export.zip");
        fs::write(&output_path, "keep").expect("existing");
        let output = run_export(
            &args(Some("session-1"), &output_path),
            temp.path(),
            temp.path(),
            &store,
            &confirmation,
            "0.2.0",
        );
        assert_eq!(output.stdout, "Export cancelled.\n");
        assert_eq!(fs::read(output_path).expect("existing"), b"keep");
        assert!(confirmation.prompts.lock().expect("prompts")[0].ends_with("[y/N] "));
    }

    #[test]
    fn malformed_record_log_fails_without_partial_output() {
        let (temp, summary) = fixture();
        fs::write(summary.session_dir.join(MAIN_RECORDS), "{broken\n").expect("records");
        let store = MemoryStore {
            summary: Some(summary),
            cwd_unavailable: false,
        };
        let output_path = temp.path().join("export.zip");
        let output = run_export(
            &args(Some("session-1"), &output_path),
            temp.path(),
            temp.path(),
            &store,
            &ScriptedConfirmation::default(),
            "0.2.0",
        );
        assert_eq!(output.completion, RuntimeCompletion::failure());
        assert!(output.stderr.contains("is malformed JSON"));
        assert!(!output_path.exists());
    }

    #[test]
    fn injected_index_cannot_export_outside_the_session_root() {
        let (temp, mut summary) = fixture();
        let outside = TempDir::new().expect("outside");
        fs::create_dir_all(outside.path().join("agents/main")).expect("outside session");
        fs::write(outside.path().join(MAIN_RECORDS), VALID_RECORDS).expect("outside records");
        summary.session_dir = outside.path().to_owned();
        let store = MemoryStore {
            summary: Some(summary),
            cwd_unavailable: false,
        };
        let output = run_export(
            &args(Some("session-1"), &temp.path().join("export.zip")),
            temp.path(),
            temp.path(),
            &store,
            &ScriptedConfirmation::default(),
            "0.2.0",
        );
        assert_eq!(output.completion, RuntimeCompletion::failure());
        assert!(output.stderr.contains("escapes session root"));
    }

    #[test]
    fn corrupt_meta_is_rejected_by_filesystem_store() {
        let (temp, summary) = fixture();
        fs::write(summary.session_dir.join("meta.json"), "{}\n").expect("meta");
        let error = FilesystemSessionExportStore
            .find_by_id(&temp.path().join("sessions"), "session-1")
            .expect_err("corrupt meta");
        assert!(error.to_string().contains("invalid"));
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_path_traversal_is_rejected() {
        use std::os::unix::fs::symlink;

        let (temp, summary) = fixture();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "secret").expect("outside");
        symlink(&outside, summary.session_dir.join("escape.txt")).expect("symlink");
        let error = collect_export_entries(&summary, temp.path(), false, "0.2.0")
            .expect_err("symlink must fail");
        assert!(error.contains("symbolic link"));
        assert!(archive_name(Path::new("../escape")).is_err());
        assert!(archive_name(Path::new("/absolute")).is_err());
    }

    #[test]
    fn production_cwd_lookup_exports_newest_indexed_session() {
        let (temp, _) = fixture();
        SessionIndex::new(temp.path())
            .register_session("session-1", temp.path(), &[])
            .expect("index session");
        let output_path = temp.path().join("indexed.zip");
        let output = run_export(
            &ExportArgs {
                session_id: None,
                output: Some(output_path.clone()),
                yes: true,
                include_global_log: false,
            },
            temp.path(),
            temp.path(),
            &FilesystemSessionExportStore,
            &ScriptedConfirmation::default(),
            "0.2.0",
        );
        assert_eq!(output.completion, RuntimeCompletion::success());
        assert!(output_path.is_file());
    }

    #[test]
    fn stored_zip_headers_crc_and_eocd_are_byte_valid() {
        let entries = vec![ArchiveEntry {
            name: "a.txt".to_owned(),
            data: b"abc".to_vec(),
        }];
        let mut bytes = Vec::new();
        write_stored_zip(&mut bytes, &entries).expect("zip");
        assert_eq!(&bytes[0..4], b"PK\x03\x04");
        assert_eq!(u16_at(&bytes, 8), 0);
        assert_eq!(u32_at(&bytes, 14), crc32(b"abc"));
        assert_eq!(u32_at(&bytes, 18), 3);
        let eocd = bytes.len() - 22;
        assert_eq!(&bytes[eocd..eocd + 4], b"PK\x05\x06");
        assert_eq!(u16_at(&bytes, eocd + 10), 1);
        assert_eq!(parse_stored_zip(&bytes)["a.txt"], b"abc");
    }

    fn parse_stored_zip(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut entries = BTreeMap::new();
        let mut offset = 0;
        while bytes.get(offset..offset + 4) == Some(b"PK\x03\x04") {
            let crc = u32_at(bytes, offset + 14);
            let size = u32_at(bytes, offset + 18) as usize;
            let name_len = u16_at(bytes, offset + 26) as usize;
            let extra_len = u16_at(bytes, offset + 28) as usize;
            let name_start = offset + 30;
            let data_start = name_start + name_len + extra_len;
            let name =
                String::from_utf8(bytes[name_start..name_start + name_len].to_vec()).expect("name");
            let data = bytes[data_start..data_start + size].to_vec();
            assert_eq!(crc32(&data), crc);
            entries.insert(name, data);
            offset = data_start + size;
        }
        entries
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("u16"))
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32"))
    }
}
