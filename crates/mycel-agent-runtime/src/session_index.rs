use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mycel_agent_protocol::{
    validate_record_sequence, validate_session_id as validate_protocol_session_id, AgentRecord,
    ContentPart, RecordKind, SessionIndexEntry, SessionIndexLine, SessionMeta, SessionSummary,
};
use serde_json::Value;
use thiserror::Error;

const INDEX_FILE: &str = "index.jsonl";
const INDEX_LOCK: &str = ".index.lock";
const META_FILE: &str = "meta.json";
const MAIN_RECORDS: &str = "agents/main/records.jsonl";
const DEFAULT_TITLE: &str = "New Session";
const MAX_TITLE_CHARS: usize = 200;
const MAX_LAST_PROMPT_CHARS: usize = 4_000;
const MAX_META_BYTES: u64 = 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const LOCK_ATTEMPTS: usize = 400;
const LOCK_RETRY: Duration = Duration::from_millis(5);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);
const SCHEMA_VERSION_KEY: &str = "sessionIndexSchemaVersion";

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq)]
pub struct SessionDiscovery {
    pub sessions: Vec<SessionSummary>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionIndex {
    home: PathBuf,
    sessions_root: PathBuf,
    index_path: PathBuf,
}

impl SessionIndex {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let sessions_root = home.join("sessions");
        let index_path = sessions_root.join(INDEX_FILE);
        Self {
            home,
            sessions_root,
            index_path,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn sessions_root(&self) -> &Path {
        &self.sessions_root
    }

    pub fn session_dir(&self, id: &str) -> Result<PathBuf, SessionIndexError> {
        validate_session_id(id)
            .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        Ok(self.sessions_root.join(id))
    }

    pub fn register_session(
        &self,
        id: &str,
        work_dir: &Path,
        additional_dirs: &[PathBuf],
    ) -> Result<SessionSummary, SessionIndexError> {
        self.register_metadata(id, work_dir, additional_dirs, None, None)
    }

    pub fn register_fork(
        &self,
        source_id: &str,
        target_id: &str,
        work_dir: &Path,
        additional_dirs: &[PathBuf],
        title: Option<&str>,
    ) -> Result<SessionSummary, SessionIndexError> {
        validate_session_id(source_id)
            .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        if source_id == target_id {
            return Err(SessionIndexError::InvalidSessionId(
                "fork source and target ids must differ".to_owned(),
            ));
        }
        if self.get(source_id)?.is_none() {
            return Err(SessionIndexError::SessionNotFound(source_id.to_owned()));
        }
        self.register_metadata(target_id, work_dir, additional_dirs, Some(source_id), title)
    }

    pub fn set_title(&self, id: &str, title: &str) -> Result<SessionSummary, SessionIndexError> {
        let title = validate_title(title)?;
        let _lock = self.acquire_lock()?;
        let mut metadata = self.read_required_metadata(id)?;
        metadata.title = title;
        metadata.is_custom_title = true;
        metadata.updated_at = now_millis().to_string();
        self.write_metadata(id, &metadata)?;
        let discovery = self.repair_locked()?;
        discovery
            .sessions
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| SessionIndexError::SessionNotFound(id.to_owned()))
    }

    pub fn set_additional_dirs(
        &self,
        id: &str,
        additional_dirs: &[PathBuf],
    ) -> Result<SessionSummary, SessionIndexError> {
        let _lock = self.acquire_lock()?;
        let mut metadata = self.read_required_metadata(id)?;
        let work_dir = metadata
            .work_dir
            .as_deref()
            .ok_or_else(|| SessionIndexError::CorruptSession {
                id: id.to_owned(),
                message: "session metadata omitted workDir".to_owned(),
            })
            .and_then(|path| canonical_directory(Path::new(path)))?;
        let mut normalized = additional_dirs
            .iter()
            .map(|path| canonical_directory(path))
            .collect::<Result<Vec<_>, _>>()?;
        normalized.sort();
        normalized.dedup();
        normalized.retain(|path| path != &work_dir);
        metadata.additional_dirs = normalized
            .iter()
            .map(|path| path_string(path))
            .collect::<Result<Vec<_>, _>>()?;
        metadata.updated_at = now_millis().to_string();
        self.write_metadata(id, &metadata)?;
        let discovery = self.repair_locked()?;
        discovery
            .sessions
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| SessionIndexError::SessionNotFound(id.to_owned()))
    }

    pub fn refresh(&self, id: &str) -> Result<SessionSummary, SessionIndexError> {
        validate_session_id(id)
            .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        let _lock = self.acquire_lock()?;
        let discovery = self.repair_locked()?;
        discovery
            .sessions
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| SessionIndexError::SessionNotFound(id.to_owned()))
    }

    pub fn get(&self, id: &str) -> Result<Option<SessionSummary>, SessionIndexError> {
        validate_session_id(id)
            .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        let expected = self.session_dir(id)?;
        let exists = fs::symlink_metadata(&expected).is_ok();
        let discovery = self.list(None)?;
        let found = discovery
            .sessions
            .into_iter()
            .find(|session| session.id == id);
        if found.is_none() && exists {
            return Err(SessionIndexError::CorruptSession {
                id: id.to_owned(),
                message: discovery
                    .warnings
                    .into_iter()
                    .find(|warning| warning.contains(&format!("session {id:?}")))
                    .unwrap_or_else(|| "session metadata or records are invalid".to_owned()),
            });
        }
        Ok(found)
    }

    pub fn list(&self, cwd: Option<&Path>) -> Result<SessionDiscovery, SessionIndexError> {
        let canonical_cwd = cwd.map(canonical_directory).transpose()?;
        let _lock = self.acquire_lock()?;
        let mut discovery = self.repair_locked()?;
        if let Some(cwd) = canonical_cwd {
            discovery
                .sessions
                .retain(|session| Path::new(&session.work_dir) == cwd);
        }
        Ok(discovery)
    }

    pub fn newest_for_cwd(&self, cwd: &Path) -> Result<Option<SessionSummary>, SessionIndexError> {
        Ok(self.list(Some(cwd))?.sessions.into_iter().next())
    }

    pub fn validate_resume(
        &self,
        id: &str,
        cwd: &Path,
    ) -> Result<SessionSummary, SessionIndexError> {
        let current = canonical_directory(cwd)?;
        let session = self
            .get(id)?
            .ok_or_else(|| SessionIndexError::SessionNotFound(id.to_owned()))?;
        let expected = PathBuf::from(&session.work_dir);
        if current != expected {
            return Err(SessionIndexError::CrossWorkingDirectory {
                id: id.to_owned(),
                current,
                expected,
            });
        }
        Ok(session)
    }

    fn register_metadata(
        &self,
        id: &str,
        work_dir: &Path,
        additional_dirs: &[PathBuf],
        forked_from: Option<&str>,
        title: Option<&str>,
    ) -> Result<SessionSummary, SessionIndexError> {
        validate_session_id(id)
            .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        if let Some(source) = forked_from {
            validate_session_id(source)
                .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        }
        let work_dir = canonical_directory(work_dir)?;
        let additional_dirs = additional_dirs
            .iter()
            .map(|path| canonical_directory(path))
            .collect::<Result<Vec<_>, _>>()?;
        let session_dir = self.session_dir(id)?;
        validate_session_directory(&self.sessions_root, id, &session_dir)?;
        let _lock = self.acquire_lock()?;
        let scan = scan_records(&session_dir.join(MAIN_RECORDS))?;
        let configured_title = title.map(validate_title).transpose()?;
        let fork_time = forked_from.map(|_| now_millis());
        let mut custom = BTreeMap::new();
        custom.insert(SCHEMA_VERSION_KEY.to_owned(), Value::from(1));
        let metadata = SessionMeta {
            created_at: fork_time.unwrap_or(scan.created_at).to_string(),
            updated_at: fork_time.unwrap_or(scan.updated_at).to_string(),
            title: configured_title
                .clone()
                .or(scan.title)
                .unwrap_or_else(|| DEFAULT_TITLE.to_owned()),
            is_custom_title: configured_title.is_some(),
            last_prompt: scan.last_prompt,
            forked_from: forked_from.map(str::to_owned),
            work_dir: Some(path_string(&work_dir)?),
            additional_dirs: additional_dirs
                .iter()
                .map(|path| path_string(path))
                .collect::<Result<Vec<_>, _>>()?,
            agents: BTreeMap::new(),
            custom,
        };
        self.write_metadata(id, &metadata)?;
        let discovery = self.repair_locked()?;
        discovery
            .sessions
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| SessionIndexError::SessionNotFound(id.to_owned()))
    }

    fn repair_locked(&self) -> Result<SessionDiscovery, SessionIndexError> {
        ensure_private_directory(&self.home)?;
        ensure_private_directory(&self.sessions_root)?;
        let (candidates, mut warnings) = self.read_index_candidates()?;
        let mut children = fs::read_dir(&self.sessions_root)
            .map_err(|source| io_error(&self.sessions_root, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| io_error(&self.sessions_root, source))?;
        children.sort_by_key(fs::DirEntry::file_name);

        let mut sessions = Vec::new();
        for child in children {
            let id = match child.file_name().into_string() {
                Ok(id) => id,
                Err(_) => {
                    warnings.push("ignored a non-UTF-8 session directory name".to_owned());
                    continue;
                }
            };
            if id == INDEX_FILE || id == INDEX_LOCK {
                continue;
            }
            let path = child.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| io_error(&path, source))?;
            if metadata.file_type().is_symlink() {
                warnings.push(format!(
                    "ignored session {id:?}: session directory is a symlink"
                ));
                continue;
            }
            if !metadata.is_dir() {
                continue;
            }
            if let Err(error) = validate_session_id(&id) {
                warnings.push(format!("ignored session {id:?}: {error}"));
                continue;
            }
            let candidate = candidates.get(&id);
            match self.repair_session(&id, &path, candidate) {
                Ok((summary, warning)) => {
                    sessions.push(summary);
                    if let Some(warning) = warning {
                        warnings.push(warning);
                    }
                }
                Err(error) => warnings.push(format!("ignored session {id:?}: {error}")),
            }
        }
        sort_summaries(&mut sessions);
        self.write_index(&sessions)?;
        Ok(SessionDiscovery { sessions, warnings })
    }

    fn repair_session(
        &self,
        id: &str,
        session_dir: &Path,
        candidate: Option<&SessionIndexEntry>,
    ) -> Result<(SessionSummary, Option<String>), SessionIndexError> {
        validate_session_directory(&self.sessions_root, id, session_dir)?;
        let scan = scan_records(&session_dir.join(MAIN_RECORDS))?;
        let meta_path = session_dir.join(META_FILE);
        let (mut metadata, repaired_meta) = match read_json_file::<SessionMeta>(
            &meta_path,
            MAX_META_BYTES,
        ) {
            Ok(Some(metadata)) => {
                validate_metadata(id, &metadata)?;
                (metadata, false)
            }
            Ok(None) | Err(_) => {
                let work_dir = candidate
                    .map(|entry| canonical_directory(Path::new(&entry.work_dir)))
                    .transpose()?
                    .ok_or_else(|| SessionIndexError::CorruptSession {
                        id: id.to_owned(),
                        message: "meta.json is missing or invalid and the index has no recoverable work directory"
                            .to_owned(),
                    })?;
                let mut custom = BTreeMap::new();
                custom.insert(SCHEMA_VERSION_KEY.to_owned(), Value::from(1));
                (
                    SessionMeta {
                        created_at: scan.created_at.to_string(),
                        updated_at: scan.updated_at.to_string(),
                        title: scan
                            .title
                            .clone()
                            .unwrap_or_else(|| DEFAULT_TITLE.to_owned()),
                        is_custom_title: false,
                        last_prompt: scan.last_prompt.clone(),
                        forked_from: None,
                        work_dir: Some(path_string(&work_dir)?),
                        additional_dirs: Vec::new(),
                        agents: BTreeMap::new(),
                        custom,
                    },
                    true,
                )
            }
        };

        let original = metadata.clone();
        metadata.created_at = scan.created_at.to_string();
        let persisted_updated = metadata.updated_at.parse::<u64>().unwrap_or_default();
        metadata.updated_at = persisted_updated.max(scan.updated_at).to_string();
        metadata.last_prompt = scan.last_prompt.clone();
        if !metadata.is_custom_title {
            metadata.title = scan
                .title
                .clone()
                .unwrap_or_else(|| DEFAULT_TITLE.to_owned());
        }
        validate_metadata(id, &metadata)?;
        if repaired_meta || metadata != original {
            self.write_metadata(id, &metadata)?;
        } else {
            enforce_private_file(&meta_path)?;
        }
        let summary = summary_from_metadata(id, session_dir, &metadata)?;
        let warning = if scan.ignored_truncated_final_line {
            Some(format!(
                "session {id:?} record log has a truncated final line; indexed the valid prefix"
            ))
        } else if repaired_meta {
            Some(format!("session {id:?} meta.json was rebuilt from records"))
        } else {
            None
        };
        Ok((summary, warning))
    }

    fn read_index_candidates(
        &self,
    ) -> Result<(BTreeMap<String, SessionIndexEntry>, Vec<String>), SessionIndexError> {
        let bytes = match read_file_limited(&self.index_path, MAX_INDEX_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok((BTreeMap::new(), Vec::new())),
            Err(error) => {
                return Ok((
                    BTreeMap::new(),
                    vec![format!("discarded unreadable session index: {error}")],
                ))
            }
        };
        let ended_with_newline = bytes.ends_with(b"\n");
        let mut entries = BTreeMap::new();
        let mut deleted = BTreeSet::new();
        let mut warnings = Vec::new();
        for (index, raw) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let parsed: SessionIndexLine = match serde_json::from_slice(raw) {
                Ok(parsed) => parsed,
                Err(error) => {
                    let final_truncated = index == bytes.split(|byte| *byte == b'\n').count() - 1
                        && !ended_with_newline;
                    warnings.push(if final_truncated {
                        "discarded truncated final session-index line".to_owned()
                    } else {
                        format!(
                            "discarded corrupt session-index line {}: {error}",
                            index + 1
                        )
                    });
                    continue;
                }
            };
            match parsed {
                SessionIndexLine::Entry(entry) => {
                    if validate_index_entry(&self.sessions_root, &entry).is_ok() {
                        deleted.remove(&entry.session_id);
                        entries.insert(entry.session_id.clone(), entry);
                    } else {
                        warnings.push(format!(
                            "discarded unsafe session-index entry on line {}",
                            index + 1
                        ));
                    }
                }
                SessionIndexLine::Deletion(deletion) => {
                    deleted.insert(deletion.session_id.clone());
                    entries.remove(&deletion.session_id);
                }
            }
        }
        for id in deleted {
            entries.remove(&id);
        }
        Ok((entries, warnings))
    }

    fn write_metadata(&self, id: &str, metadata: &SessionMeta) -> Result<(), SessionIndexError> {
        validate_metadata(id, metadata)?;
        let path = self.session_dir(id)?.join(META_FILE);
        let mut bytes =
            serde_json::to_vec_pretty(metadata).map_err(SessionIndexError::Serialize)?;
        bytes.push(b'\n');
        atomic_write(&path, &bytes)
    }

    fn read_required_metadata(&self, id: &str) -> Result<SessionMeta, SessionIndexError> {
        validate_session_id(id)
            .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
        let path = self.session_dir(id)?.join(META_FILE);
        let metadata = read_json_file(&path, MAX_META_BYTES)?
            .ok_or_else(|| SessionIndexError::SessionNotFound(id.to_owned()))?;
        validate_metadata(id, &metadata)?;
        Ok(metadata)
    }

    fn write_index(&self, sessions: &[SessionSummary]) -> Result<(), SessionIndexError> {
        let mut bytes = Vec::new();
        for session in sessions {
            let line = SessionIndexLine::Entry(SessionIndexEntry {
                session_id: session.id.clone(),
                session_dir: session.session_dir.clone(),
                work_dir: session.work_dir.clone(),
            });
            serde_json::to_writer(&mut bytes, &line).map_err(SessionIndexError::Serialize)?;
            bytes.push(b'\n');
        }
        if read_file_limited(&self.index_path, MAX_INDEX_BYTES)
            .ok()
            .flatten()
            .as_deref()
            == Some(bytes.as_slice())
        {
            return enforce_private_file(&self.index_path);
        }
        atomic_write(&self.index_path, &bytes)
    }

    fn acquire_lock(&self) -> Result<IndexLock, SessionIndexError> {
        ensure_private_directory(&self.home)?;
        ensure_private_directory(&self.sessions_root)?;
        let path = self.sessions_root.join(INDEX_LOCK);
        for _ in 0..LOCK_ATTEMPTS {
            match create_private_new(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .map_err(|source| io_error(&path, source))?;
                    file.sync_all().map_err(|source| io_error(&path, source))?;
                    return Ok(IndexLock { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = match fs::symlink_metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            // The owner may release the lock between our
                            // create_new failure and inspection. Retry instead
                            // of turning ordinary lock handoff into an I/O
                            // failure.
                            continue;
                        }
                        Err(source) => return Err(io_error(&path, source)),
                    };
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(SessionIndexError::UnsafePath(path));
                    }
                    if metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK_AGE)
                    {
                        let _ = fs::remove_file(&path);
                    } else {
                        thread::sleep(LOCK_RETRY);
                    }
                }
                Err(source) => return Err(io_error(&path, source)),
            }
        }
        Err(SessionIndexError::LockTimeout(path))
    }
}

struct IndexLock {
    path: PathBuf,
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct RecordScan {
    created_at: u64,
    updated_at: u64,
    title: Option<String>,
    last_prompt: Option<String>,
    ignored_truncated_final_line: bool,
}

fn scan_records(path: &Path) -> Result<RecordScan, SessionIndexError> {
    let bytes = read_file_limited(path, MAX_RECORD_BYTES)?.ok_or_else(|| {
        SessionIndexError::CorruptRecordLog {
            path: path.to_owned(),
            message: "record log is missing".to_owned(),
        }
    })?;
    let ended_with_newline = bytes.ends_with(b"\n");
    let chunks = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let mut records = Vec::new();
    let mut ignored_truncated_final_line = false;
    for (index, raw) in chunks.iter().enumerate() {
        if raw.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice::<AgentRecord>(raw) {
            Ok(record) => records.push(record),
            Err(_) if index + 1 == chunks.len() && !ended_with_newline => {
                ignored_truncated_final_line = true;
                break;
            }
            Err(error) => {
                return Err(SessionIndexError::CorruptRecordLog {
                    path: path.to_owned(),
                    message: format!("line {}: {error}", index + 1),
                })
            }
        }
    }
    validate_record_sequence(&records).map_err(|error| SessionIndexError::CorruptRecordLog {
        path: path.to_owned(),
        message: error.to_string(),
    })?;

    let created_at = records
        .first()
        .and_then(|record| record.payload.get("created_at"))
        .and_then(Value::as_u64)
        .or_else(|| records.first().and_then(|record| record.time))
        .unwrap_or_else(now_millis);
    let record_updated = records.iter().filter_map(|record| record.time).max();
    let file_updated = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(system_time_millis);
    let updated_at = record_updated
        .or(file_updated)
        .unwrap_or(created_at)
        .max(created_at);
    let prompts = records
        .iter()
        .filter(|record| {
            matches!(
                record.kind(),
                Some(RecordKind::TurnPrompt | RecordKind::TurnSteer)
            )
        })
        .filter_map(prompt_text)
        .collect::<Vec<_>>();
    Ok(RecordScan {
        created_at,
        updated_at,
        title: prompts
            .first()
            .map(|prompt| truncate_chars(prompt, MAX_TITLE_CHARS)),
        last_prompt: prompts.last().cloned(),
        ignored_truncated_final_line,
    })
}

fn prompt_text(record: &AgentRecord) -> Option<String> {
    let parts: Vec<ContentPart> =
        serde_json::from_value(record.payload.get("input")?.clone()).ok()?;
    let mut rendered = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } if !text.trim().is_empty() => rendered.push(text),
            ContentPart::ImageUrl { .. } => rendered.push("[image]".to_owned()),
            ContentPart::AudioUrl { .. } => rendered.push("[audio]".to_owned()),
            ContentPart::VideoUrl { .. } => rendered.push("[video]".to_owned()),
            ContentPart::Text { .. } | ContentPart::Think { .. } => {}
        }
    }
    let normalized = rendered
        .join("\n")
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!normalized.is_empty()).then(|| truncate_chars(&normalized, MAX_LAST_PROMPT_CHARS))
}

fn validate_metadata(id: &str, metadata: &SessionMeta) -> Result<(), SessionIndexError> {
    metadata
        .validate()
        .map_err(|error| SessionIndexError::CorruptSession {
            id: id.to_owned(),
            message: error.to_string(),
        })?;
    validate_title(&metadata.title)?;
    let work_dir =
        metadata
            .work_dir
            .as_deref()
            .ok_or_else(|| SessionIndexError::CorruptSession {
                id: id.to_owned(),
                message: "meta.json has no workDir".to_owned(),
            })?;
    let canonical = canonical_directory(Path::new(work_dir))?;
    if path_string(&canonical)? != work_dir {
        return Err(SessionIndexError::CorruptSession {
            id: id.to_owned(),
            message: "meta.json workDir is not canonical".to_owned(),
        });
    }
    let created = parse_timestamp(id, "createdAt", &metadata.created_at)?;
    let updated = parse_timestamp(id, "updatedAt", &metadata.updated_at)?;
    if updated < created {
        return Err(SessionIndexError::CorruptSession {
            id: id.to_owned(),
            message: "meta.json updatedAt precedes createdAt".to_owned(),
        });
    }
    if let Some(source) = &metadata.forked_from {
        validate_session_id(source).map_err(|error| SessionIndexError::CorruptSession {
            id: id.to_owned(),
            message: format!("invalid forkedFrom: {error}"),
        })?;
    }
    for additional_dir in &metadata.additional_dirs {
        let path = Path::new(additional_dir);
        if !path.is_absolute()
            || normalize_path(path) != path
            || path_string(path)? != *additional_dir
        {
            return Err(SessionIndexError::CorruptSession {
                id: id.to_owned(),
                message: "meta.json additionalDirs contains a non-canonical path".to_owned(),
            });
        }
    }
    Ok(())
}

fn summary_from_metadata(
    id: &str,
    session_dir: &Path,
    metadata: &SessionMeta,
) -> Result<SessionSummary, SessionIndexError> {
    validate_metadata(id, metadata)?;
    let mut custom = metadata.custom.clone();
    custom.insert(
        "isCustomTitle".to_owned(),
        Value::Bool(metadata.is_custom_title),
    );
    if let Some(source) = &metadata.forked_from {
        custom.insert("forkedFrom".to_owned(), Value::String(source.clone()));
    }
    let summary = SessionSummary {
        id: id.to_owned(),
        title: Some(metadata.title.clone()),
        last_prompt: metadata.last_prompt.clone(),
        work_dir: metadata.work_dir.clone().unwrap_or_default(),
        session_dir: path_string(session_dir)?,
        created_at: parse_timestamp(id, "createdAt", &metadata.created_at)?,
        updated_at: parse_timestamp(id, "updatedAt", &metadata.updated_at)?,
        archived: None,
        metadata: Some(custom),
        additional_dirs: metadata.additional_dirs.clone(),
    };
    summary
        .validate()
        .map_err(|error| SessionIndexError::CorruptSession {
            id: id.to_owned(),
            message: error.to_string(),
        })?;
    Ok(summary)
}

fn validate_index_entry(root: &Path, entry: &SessionIndexEntry) -> Result<(), SessionIndexError> {
    validate_session_id(&entry.session_id)
        .map_err(|error| SessionIndexError::InvalidSessionId(error.to_string()))?;
    let expected = root.join(&entry.session_id);
    if normalize_path(Path::new(&entry.session_dir)) != normalize_path(&expected) {
        return Err(SessionIndexError::UnsafePath(PathBuf::from(
            &entry.session_dir,
        )));
    }
    let canonical_work = canonical_directory(Path::new(&entry.work_dir))?;
    if path_string(&canonical_work)? != entry.work_dir {
        return Err(SessionIndexError::InvalidWorkingDirectory(PathBuf::from(
            &entry.work_dir,
        )));
    }
    Ok(())
}

fn validate_session_directory(
    root: &Path,
    id: &str,
    session_dir: &Path,
) -> Result<(), SessionIndexError> {
    let expected = root.join(id);
    if normalize_path(session_dir) != normalize_path(&expected) {
        return Err(SessionIndexError::UnsafePath(session_dir.to_owned()));
    }
    let metadata =
        fs::symlink_metadata(session_dir).map_err(|source| io_error(session_dir, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SessionIndexError::UnsafePath(session_dir.to_owned()));
    }
    Ok(())
}

fn validate_title(title: &str) -> Result<String, SessionIndexError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(SessionIndexError::EmptyTitle);
    }
    if title.chars().any(char::is_control) {
        return Err(SessionIndexError::UnsafeText("session title"));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(SessionIndexError::TitleTooLong);
    }
    Ok(title.to_owned())
}

fn validate_session_id(id: &str) -> Result<(), String> {
    validate_protocol_session_id(id).map_err(|error| error.to_string())?;
    if id.chars().any(char::is_control) {
        return Err("session id contains control characters".to_owned());
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf, SessionIndexError> {
    let canonical = fs::canonicalize(path)
        .map_err(|_| SessionIndexError::InvalidWorkingDirectory(path.to_owned()))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|_| SessionIndexError::InvalidWorkingDirectory(path.to_owned()))?;
    if !metadata.is_dir() {
        return Err(SessionIndexError::InvalidWorkingDirectory(path.to_owned()));
    }
    Ok(canonical)
}

fn parse_timestamp(id: &str, field: &str, value: &str) -> Result<u64, SessionIndexError> {
    value
        .parse()
        .map_err(|_| SessionIndexError::CorruptSession {
            id: id.to_owned(),
            message: format!("meta.json {field} is not a millisecond timestamp"),
        })
}

fn sort_summaries(sessions: &mut [SessionSummary]) {
    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn path_string(path: &Path) -> Result<String, SessionIndexError> {
    let value = path
        .to_str()
        .ok_or_else(|| SessionIndexError::NonUtf8Path(path.to_owned()))?;
    if value.chars().any(char::is_control) {
        return Err(SessionIndexError::UnsafePath(path.to_owned()));
    }
    Ok(value.to_owned())
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

fn ensure_private_directory(path: &Path) -> Result<(), SessionIndexError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SessionIndexError::UnsafePath(path.to_owned()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
        }
        Err(source) => return Err(io_error(path, source)),
    }
    set_private_directory(path)
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), SessionIndexError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<(), SessionIndexError> {
    Ok(())
}

fn read_json_file<T: serde::de::DeserializeOwned>(
    path: &Path,
    limit: u64,
) -> Result<Option<T>, SessionIndexError> {
    let Some(bytes) = read_file_limited(path, limit)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(SessionIndexError::Serialize)
}

fn read_file_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, SessionIndexError> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path, source)),
    };
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.len() > limit
    {
        return Err(SessionIndexError::UnsafePath(path.to_owned()));
    }
    let file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(path, source)),
    };
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(SessionIndexError::UnsafePath(path.to_owned()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    if bytes.len() as u64 > limit {
        return Err(SessionIndexError::UnsafePath(path.to_owned()));
    }
    Ok(Some(bytes))
}

fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

fn create_private_new(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SessionIndexError> {
    let parent = path
        .parent()
        .ok_or_else(|| SessionIndexError::UnsafePath(path.to_owned()))?;
    ensure_private_directory(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let temporary = parent.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file =
            create_private_new(&temporary).map_err(|source| io_error(&temporary, source))?;
        file.write_all(bytes)
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|source| io_error(path, source))?;
        enforce_private_file(path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn enforce_private_file(path: &Path) -> Result<(), SessionIndexError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SessionIndexError::UnsafePath(path.to_owned()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn enforce_private_file(path: &Path) -> Result<(), SessionIndexError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SessionIndexError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), SessionIndexError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), SessionIndexError> {
    Ok(())
}

fn system_time_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn now_millis() -> u64 {
    system_time_millis(SystemTime::now())
}

fn io_error(path: &Path, source: io::Error) -> SessionIndexError {
    SessionIndexError::Io {
        path: path.to_owned(),
        source,
    }
}

#[derive(Debug, Error)]
pub enum SessionIndexError {
    #[error("session index I/O failed for {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("session metadata serialization failed: {0}")]
    Serialize(serde_json::Error),
    #[error("invalid session id: {0}")]
    InvalidSessionId(String),
    #[error("invalid working directory {0}")]
    InvalidWorkingDirectory(PathBuf),
    #[error("path is not UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("unsafe session-index path {0}")]
    UnsafePath(PathBuf),
    #[error("session {0:?} was not found")]
    SessionNotFound(String),
    #[error("session {id:?} is corrupt: {message}")]
    CorruptSession { id: String, message: String },
    #[error("record log {path} is corrupt: {message}")]
    CorruptRecordLog { path: PathBuf, message: String },
    #[error("session title cannot be empty")]
    EmptyTitle,
    #[error("session title cannot exceed {MAX_TITLE_CHARS} characters")]
    TitleTooLong,
    #[error("{0} cannot contain control characters")]
    UnsafeText(&'static str),
    #[error("timed out waiting for session-index lock {0}")]
    LockTimeout(PathBuf),
    #[error(
        "session {id:?} was created under a different directory: expected {expected}, current {current}"
    )]
    CrossWorkingDirectory {
        id: String,
        current: PathBuf,
        expected: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    use serde_json::json;

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "mycel-session-index-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_records(root: &Path, id: &str, created: u64, updated: u64, prompt: &str) {
        let path = root.join("sessions").join(id).join(MAIN_RECORDS);
        fs::create_dir_all(path.parent().expect("record parent")).expect("record directories");
        let records = [
            json!({"type":"metadata","time":created,"protocol_version":"1.4","created_at":created}),
            json!({"type":"turn.prompt","time":updated,"input":[{"type":"text","text":prompt}],"origin":{"kind":"user"}}),
        ];
        let source = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, source).expect("records");
    }

    #[test]
    fn registers_filters_orders_and_validates_resume_cwd() {
        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&other).expect("other");
        write_records(temp.path(), "older", 1, 10, "older prompt");
        write_records(temp.path(), "newer", 2, 20, "newer prompt");
        write_records(temp.path(), "other-session", 3, 30, "other prompt");
        let index = SessionIndex::new(temp.path());
        index.register_session("older", &cwd, &[]).expect("older");
        index.register_session("newer", &cwd, &[]).expect("newer");
        index
            .register_session("other-session", &other, &[])
            .expect("other");

        let sessions = index.list(Some(&cwd)).expect("list").sessions;
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert_eq!(sessions[0].title.as_deref(), Some("newer prompt"));
        assert_eq!(
            index
                .newest_for_cwd(&cwd)
                .expect("newest")
                .expect("session")
                .id,
            "newer"
        );
        assert!(matches!(
            index.validate_resume("newer", &other),
            Err(SessionIndexError::CrossWorkingDirectory { .. })
        ));
    }

    #[test]
    fn repairs_corrupt_truncated_and_stale_index_from_metadata_and_records() {
        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        write_records(temp.path(), "session-1", 1, 10, "hello");
        let index = SessionIndex::new(temp.path());
        index
            .register_session("session-1", &cwd, &[])
            .expect("register");
        fs::write(
            &index.index_path,
            format!(
                "{{broken}}\n{{\"sessionId\":\"stale\",\"sessionDir\":{:?},\"workDir\":{:?}}}\n{{\"sessionId\"",
                temp.path().join("sessions/stale").display().to_string(),
                cwd.display().to_string()
            ),
        )
        .expect("corrupt index");

        let discovery = index.list(None).expect("repair");
        assert_eq!(discovery.sessions.len(), 1);
        assert_eq!(discovery.sessions[0].id, "session-1");
        assert!(discovery
            .warnings
            .iter()
            .any(|warning| warning.contains("corrupt")));
        assert!(discovery
            .warnings
            .iter()
            .any(|warning| warning.contains("truncated")));
        let repaired = fs::read_to_string(&index.index_path).expect("repaired index");
        assert!(repaired.contains("session-1"));
        assert!(!repaired.contains("stale"));
    }

    #[test]
    fn truncated_record_tail_indexes_valid_prefix_with_warning() {
        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        write_records(temp.path(), "session-1", 1, 10, "complete prompt");
        let records = temp.path().join("sessions/session-1").join(MAIN_RECORDS);
        fs::OpenOptions::new()
            .append(true)
            .open(&records)
            .expect("open records")
            .write_all(b"{\"type\":\"turn.prompt\"")
            .expect("truncated tail");
        let index = SessionIndex::new(temp.path());
        index
            .register_session("session-1", &cwd, &[])
            .expect("register valid prefix");

        let discovery = index.list(None).expect("list");
        assert_eq!(
            discovery.sessions[0].last_prompt.as_deref(),
            Some("complete prompt")
        );
        assert!(discovery
            .warnings
            .iter()
            .any(|warning| warning.contains("truncated final line")));
    }

    #[test]
    fn missing_meta_is_rebuilt_from_record_and_recoverable_index_workdir() {
        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        write_records(temp.path(), "session-1", 1, 10, "recovered title");
        let index = SessionIndex::new(temp.path());
        index
            .register_session("session-1", &cwd, &[])
            .expect("register");
        fs::remove_file(temp.path().join("sessions/session-1/meta.json")).expect("remove meta");

        let discovery = index.list(None).expect("repair");
        assert_eq!(
            discovery.sessions[0].title.as_deref(),
            Some("recovered title")
        );
        assert!(discovery
            .warnings
            .iter()
            .any(|warning| warning.contains("rebuilt")));
        assert!(temp.path().join("sessions/session-1/meta.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_session_and_writes_private_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        write_records(temp.path(), "safe", 1, 2, "hello");
        let outside = TestDir::new().expect("outside");
        symlink(outside.path(), temp.path().join("sessions/escape")).expect("symlink");
        let index = SessionIndex::new(temp.path());
        index.register_session("safe", &cwd, &[]).expect("register");
        let discovery = index.list(None).expect("list");
        assert_eq!(discovery.sessions.len(), 1);
        assert!(discovery
            .warnings
            .iter()
            .any(|warning| warning.contains("symlink")));
        assert_eq!(
            fs::metadata(temp.path().join("sessions/safe/meta.json"))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(temp.path().join("sessions/index.jsonl"))
                .expect("index")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn cwd_filter_canonicalizes_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        let alias = temp.path().join("workspace-alias");
        fs::create_dir_all(&cwd).expect("cwd");
        symlink(&cwd, &alias).expect("cwd alias");
        write_records(temp.path(), "session-1", 1, 2, "hello");
        let index = SessionIndex::new(temp.path());
        index
            .register_session("session-1", &cwd, &[])
            .expect("register");

        assert_eq!(
            index
                .newest_for_cwd(&alias)
                .expect("alias lookup")
                .expect("session")
                .id,
            "session-1"
        );
    }

    #[test]
    fn concurrent_writers_do_not_lose_index_entries() {
        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        for index in 0..8 {
            write_records(
                temp.path(),
                &format!("session-{index}"),
                index + 1,
                index + 10,
                &format!("prompt {index}"),
            );
        }
        let session_index = Arc::new(SessionIndex::new(temp.path()));
        let mut writers = Vec::new();
        for index in 0..8 {
            let session_index = Arc::clone(&session_index);
            let cwd = cwd.clone();
            writers.push(thread::spawn(move || {
                session_index
                    .register_session(&format!("session-{index}"), &cwd, &[])
                    .expect("register")
            }));
        }
        for writer in writers {
            writer.join().expect("writer");
        }
        assert_eq!(session_index.list(None).expect("list").sessions.len(), 8);
    }

    #[test]
    fn title_and_fork_metadata_are_bounded_and_preserved_on_refresh() {
        let temp = TestDir::new().expect("temp");
        let cwd = temp.path().join("workspace");
        fs::create_dir_all(&cwd).expect("cwd");
        write_records(temp.path(), "source", 1, 2, "source prompt");
        write_records(temp.path(), "fork", 3, 4, "fork prompt");
        let index = SessionIndex::new(temp.path());
        index.register_session("source", &cwd, &[]).expect("source");
        let fork = index
            .register_fork("source", "fork", &cwd, &[], Some("Fork title"))
            .expect("fork");
        assert_eq!(fork.title.as_deref(), Some("Fork title"));
        assert_eq!(
            fork.metadata
                .as_ref()
                .and_then(|metadata| metadata.get("forkedFrom"))
                .and_then(Value::as_str),
            Some("source")
        );
        let renamed = index.set_title("fork", "Custom title").expect("title");
        assert_eq!(renamed.title.as_deref(), Some("Custom title"));
        let first_root = temp.path().join("first-root");
        let second_root = temp.path().join("second-root");
        fs::create_dir_all(&first_root).expect("first root");
        fs::create_dir_all(&second_root).expect("second root");
        let first_root = fs::canonicalize(first_root).expect("canonical first root");
        let second_root = fs::canonicalize(second_root).expect("canonical second root");
        let updated = index
            .set_additional_dirs(
                "fork",
                &[
                    second_root.clone(),
                    cwd.clone(),
                    first_root.clone(),
                    second_root.clone(),
                ],
            )
            .expect("additional dirs");
        assert_eq!(
            updated.additional_dirs,
            vec![
                first_root.to_string_lossy().into_owned(),
                second_root.to_string_lossy().into_owned()
            ]
        );
        assert_eq!(updated.title.as_deref(), Some("Custom title"));
        assert_eq!(
            updated
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("forkedFrom"))
                .and_then(Value::as_str),
            Some("source")
        );
        assert!(matches!(
            index.set_title("fork", &"x".repeat(MAX_TITLE_CHARS + 1)),
            Err(SessionIndexError::TitleTooLong)
        ));
        assert!(matches!(
            index.set_title("fork", "unsafe\u{1b}title"),
            Err(SessionIndexError::UnsafeText("session title"))
        ));
        assert!(matches!(
            index.register_fork("missing", "fork", &cwd, &[], None),
            Err(SessionIndexError::SessionNotFound(id)) if id == "missing"
        ));
        assert_eq!(
            index.refresh("fork").expect("refresh").title.as_deref(),
            Some("Custom title")
        );
    }
}
