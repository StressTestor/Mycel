use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use mycel_agent_protocol::{
    validate_record_sequence, AgentRecord, RecordKind, WireCompatibility, CURRENT_WIRE_VERSION,
};
use serde_json::Value;
use tokio::{
    fs::{self, File, OpenOptions},
    io::AsyncWriteExt,
    sync::Mutex,
};

use crate::RequestId;

use super::{migrate_records, MigrationError};

#[derive(Clone, Debug, PartialEq)]
pub struct RecordRead {
    pub records: Vec<AgentRecord>,
    pub ignored_truncated_final_line: bool,
    /// Byte offset immediately after the last valid record boundary.
    pub valid_bytes: u64,
    pub missing_final_newline: bool,
    pub compatibility: Option<WireCompatibility>,
}

impl RecordRead {
    pub fn prepare_replay(self) -> Result<PreparedReplay, RecordLogError> {
        match self.compatibility {
            None | Some(WireCompatibility::Current) => Ok(PreparedReplay {
                records: self.records,
                rewrite_after_replay: false,
                ignored_truncated_final_line: self.ignored_truncated_final_line,
                warning: None,
            }),
            Some(WireCompatibility::NeedsMigration { from }) => Ok(PreparedReplay {
                records: migrate_records(&self.records, from)?,
                rewrite_after_replay: true,
                ignored_truncated_final_line: self.ignored_truncated_final_line,
                warning: None,
            }),
            Some(WireCompatibility::Newer { found }) => Ok(PreparedReplay {
                records: self.records,
                rewrite_after_replay: false,
                ignored_truncated_final_line: self.ignored_truncated_final_line,
                warning: Some(format!(
                    "session wire protocol {found} is newer than runtime protocol {CURRENT_WIRE_VERSION}; replaying without migration"
                )),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedReplay {
    pub records: Vec<AgentRecord>,
    pub rewrite_after_replay: bool,
    pub ignored_truncated_final_line: bool,
    pub warning: Option<String>,
}

/// Ordered, fsync-backed JSONL record persistence.
pub struct RecordLog {
    path: PathBuf,
    writer: Mutex<WriterState>,
}

struct WriterState {
    file: File,
    metadata_initialized: bool,
    latched_write_error: Option<String>,
    closed: bool,
}

impl RecordLog {
    pub async fn open(path: impl Into<PathBuf>) -> Result<(Arc<Self>, RecordRead), RecordLogError> {
        let path = path.into();
        let existed = fs::try_exists(&path)
            .await
            .map_err(|source| RecordLogError::Io {
                path: path.clone(),
                source,
            })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| RecordLogError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let read = if existed {
            read_record_file(&path).await?
        } else {
            RecordRead {
                records: Vec::new(),
                ignored_truncated_final_line: false,
                valid_bytes: 0,
                missing_final_newline: false,
                compatibility: None,
            }
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .await
            .map_err(|source| RecordLogError::Io {
                path: path.clone(),
                source,
            })?;
        if read.ignored_truncated_final_line {
            file.set_len(read.valid_bytes)
                .await
                .map_err(|source| RecordLogError::Io {
                    path: path.clone(),
                    source,
                })?;
            file.sync_data()
                .await
                .map_err(|source| RecordLogError::Io {
                    path: path.clone(),
                    source,
                })?;
        } else if read.missing_final_newline {
            file.write_all(b"\n")
                .await
                .map_err(|source| RecordLogError::Io {
                    path: path.clone(),
                    source,
                })?;
            file.sync_data()
                .await
                .map_err(|source| RecordLogError::Io {
                    path: path.clone(),
                    source,
                })?;
        }
        if !existed {
            sync_parent_directory(&path).await?;
        }
        let log = Arc::new(Self {
            path,
            writer: Mutex::new(WriterState {
                file,
                metadata_initialized: !read.records.is_empty(),
                latched_write_error: None,
                closed: false,
            }),
        });
        Ok((log, read))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn ensure_metadata(&self) -> Result<(), RecordLogError> {
        let mut writer = self.writer.lock().await;
        writer.ready(&self.path)?;
        if writer.metadata_initialized {
            return Ok(());
        }
        let mut bytes = Vec::new();
        encode_line(&metadata_record(), &mut bytes)?;
        if let Err(source) = writer.file.write_all(&bytes).await {
            return Err(writer.latch(&self.path, source));
        }
        if let Err(source) = writer.file.sync_data().await {
            return Err(writer.latch(&self.path, source));
        }
        writer.metadata_initialized = true;
        Ok(())
    }

    /// Appends one record and waits for it to reach stable storage.
    ///
    /// The first non-metadata append automatically prefixes metadata in the
    /// same write. Any I/O failure latches; all later writes fail without
    /// touching the file.
    pub async fn append(&self, mut record: AgentRecord) -> Result<(), RecordLogError> {
        record.time.get_or_insert_with(now_millis);
        record.validate().map_err(RecordLogError::InvalidRecord)?;

        let mut writer = self.writer.lock().await;
        writer.ready(&self.path)?;
        if writer.metadata_initialized && record.kind() == Some(RecordKind::Metadata) {
            return Err(RecordLogError::DuplicateMetadata);
        }

        let mut bytes = Vec::new();
        if !writer.metadata_initialized && record.kind() != Some(RecordKind::Metadata) {
            encode_line(&metadata_record(), &mut bytes)?;
        }
        encode_line(&record, &mut bytes)?;

        if let Err(source) = writer.file.write_all(&bytes).await {
            return Err(writer.latch(&self.path, source));
        }
        if let Err(source) = writer.file.sync_data().await {
            return Err(writer.latch(&self.path, source));
        }
        writer.metadata_initialized = true;
        Ok(())
    }

    /// Atomically replaces the complete log after a successful pure replay.
    pub async fn rewrite(&self, records: &[AgentRecord]) -> Result<(), RecordLogError> {
        validate_record_sequence(records).map_err(RecordLogError::InvalidRecord)?;
        let mut bytes = Vec::new();
        for record in records {
            encode_line(record, &mut bytes)?;
        }

        let mut writer = self.writer.lock().await;
        writer.ready(&self.path)?;
        let temporary = self
            .path
            .with_extension(format!("rewrite-{}.tmp", RequestId::generate().as_str()));
        let result = async {
            let mut replacement = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .await?;
            replacement.write_all(&bytes).await?;
            replacement.sync_all().await?;
            drop(replacement);
            fs::rename(&temporary, &self.path).await?;
            sync_parent_directory_io(&self.path).await?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)
                .await
        }
        .await;
        match result {
            Ok(file) => {
                writer.file = file;
                writer.metadata_initialized = true;
                Ok(())
            }
            Err(source) => {
                let _ = fs::remove_file(&temporary).await;
                Err(writer.latch(&self.path, source))
            }
        }
    }

    pub async fn flush(&self) -> Result<(), RecordLogError> {
        let mut writer = self.writer.lock().await;
        writer.ready(&self.path)?;
        if let Err(source) = writer.file.sync_all().await {
            return Err(writer.latch(&self.path, source));
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<(), RecordLogError> {
        let mut writer = self.writer.lock().await;
        writer.ready(&self.path)?;
        if let Err(source) = writer.file.sync_all().await {
            return Err(writer.latch(&self.path, source));
        }
        writer.closed = true;
        Ok(())
    }
}

impl WriterState {
    fn ready(&self, path: &Path) -> Result<(), RecordLogError> {
        if let Some(message) = &self.latched_write_error {
            return Err(RecordLogError::WriteLatched {
                path: path.to_path_buf(),
                message: message.clone(),
            });
        }
        if self.closed {
            return Err(RecordLogError::Closed(path.to_path_buf()));
        }
        Ok(())
    }

    fn latch(&mut self, path: &Path, source: io::Error) -> RecordLogError {
        let message = source.to_string();
        self.latched_write_error = Some(message.clone());
        RecordLogError::WriteFailed {
            path: path.to_path_buf(),
            message,
        }
    }
}

pub async fn read_record_file(path: impl AsRef<Path>) -> Result<RecordRead, RecordLogError> {
    let path = path.as_ref();
    let bytes = fs::read(path).await.map_err(|source| RecordLogError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_record_bytes(path, &bytes)
}

fn parse_record_bytes(path: &Path, bytes: &[u8]) -> Result<RecordRead, RecordLogError> {
    if bytes.is_empty() {
        return Ok(RecordRead {
            records: Vec::new(),
            ignored_truncated_final_line: false,
            valid_bytes: 0,
            missing_final_newline: false,
            compatibility: None,
        });
    }
    let ends_with_newline = bytes.last() == Some(&b'\n');
    let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    let last_data_index = if ends_with_newline {
        lines.len().saturating_sub(2)
    } else {
        lines.len().saturating_sub(1)
    };
    let mut records = Vec::new();
    let mut ignored_truncated_final_line = false;
    for (index, line) in lines.iter().enumerate() {
        if ends_with_newline && index == lines.len() - 1 {
            break;
        }
        let parsed = serde_json::from_slice::<AgentRecord>(line);
        match parsed {
            Ok(record) => {
                record
                    .validate()
                    .map_err(|source| RecordLogError::CorruptRecord {
                        path: path.to_path_buf(),
                        line: index + 1,
                        message: source.to_string(),
                    })?;
                records.push(record);
            }
            Err(_source) if !ends_with_newline && index == last_data_index => {
                ignored_truncated_final_line = true;
                break;
            }
            Err(source) => {
                return Err(RecordLogError::CorruptRecord {
                    path: path.to_path_buf(),
                    line: index + 1,
                    message: source.to_string(),
                });
            }
        }
    }
    let compatibility = if records.is_empty() {
        None
    } else {
        Some(validate_record_sequence(&records).map_err(|source| {
            RecordLogError::InvalidSequence {
                path: path.to_path_buf(),
                message: source.to_string(),
            }
        })?)
    };
    let valid_bytes = if ignored_truncated_final_line {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index.saturating_add(1))
    } else {
        bytes.len()
    };
    Ok(RecordRead {
        records,
        ignored_truncated_final_line,
        valid_bytes: valid_bytes.try_into().unwrap_or(u64::MAX),
        missing_final_newline: !ends_with_newline && !ignored_truncated_final_line,
        compatibility,
    })
}

fn metadata_record() -> AgentRecord {
    let mut payload = BTreeMap::new();
    payload.insert(
        "protocol_version".to_owned(),
        Value::String(CURRENT_WIRE_VERSION.to_string()),
    );
    payload.insert("created_at".to_owned(), Value::from(now_millis()));
    AgentRecord {
        record_type: RecordKind::Metadata.as_str().to_owned(),
        time: Some(now_millis()),
        payload,
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn encode_line(record: &AgentRecord, output: &mut Vec<u8>) -> Result<(), RecordLogError> {
    serde_json::to_writer(&mut *output, record).map_err(RecordLogError::Serialize)?;
    output.push(b'\n');
    Ok(())
}

async fn sync_parent_directory(path: &Path) -> Result<(), RecordLogError> {
    sync_parent_directory_io(path)
        .await
        .map_err(|source| RecordLogError::Io {
            path: path.parent().unwrap_or(path).to_path_buf(),
            source,
        })
}

async fn sync_parent_directory_io(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let parent = parent.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
            .await
            .map_err(io::Error::other)??;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum RecordLogError {
    #[error("record I/O failed for {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("record write failed for {path}: {message}")]
    WriteFailed { path: PathBuf, message: String },
    #[error("record writes are latched for {path}: {message}")]
    WriteLatched { path: PathBuf, message: String },
    #[error("record log is closed: {0}")]
    Closed(PathBuf),
    #[error("record log metadata is already initialized")]
    DuplicateMetadata,
    #[error("record serialization failed: {0}")]
    Serialize(serde_json::Error),
    #[error("invalid record: {0}")]
    InvalidRecord(mycel_agent_protocol::RecordError),
    #[error("corrupt record at {path}, line {line}: {message}")]
    CorruptRecord {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[error("invalid record sequence in {path}: {message}")]
    InvalidSequence { path: PathBuf, message: String },
    #[error(transparent)]
    Migration(#[from] MigrationError),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    static TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mycel-runtime-{name}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn record(kind: RecordKind) -> AgentRecord {
        let mut payload = BTreeMap::new();
        if kind == RecordKind::ContextUpdateTokenCount {
            payload.insert("tokenCount".to_owned(), json!(10));
        }
        AgentRecord::new(kind, payload)
    }

    #[tokio::test]
    async fn appends_metadata_first_and_round_trips() {
        let directory = temp_path("roundtrip");
        let path = directory.join("records.jsonl");
        let (log, read) = RecordLog::open(&path).await.expect("open");
        assert!(read.records.is_empty());
        log.append(record(RecordKind::ContextUpdateTokenCount))
            .await
            .expect("append");
        log.close().await.expect("close");

        let read = read_record_file(&path).await.expect("read");
        assert_eq!(read.records.len(), 2);
        assert_eq!(read.records[0].kind(), Some(RecordKind::Metadata));
        assert_eq!(
            read.records[1].kind(),
            Some(RecordKind::ContextUpdateTokenCount)
        );
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn ignores_only_a_malformed_final_unterminated_line() {
        let directory = temp_path("truncated");
        fs::create_dir_all(&directory).await.expect("mkdir");
        let path = directory.join("records.jsonl");
        let metadata = serde_json::to_string(&metadata_record()).expect("metadata");
        fs::write(&path, format!("{metadata}\n{{\"type\":\"turn.prompt\""))
            .await
            .expect("fixture");
        let read = read_record_file(&path).await.expect("read");
        assert_eq!(read.records.len(), 1);
        assert!(read.ignored_truncated_final_line);

        let (log, recovered) = RecordLog::open(&path).await.expect("recover");
        assert!(recovered.ignored_truncated_final_line);
        log.append(record(RecordKind::ContextUpdateTokenCount))
            .await
            .expect("append after recovery");
        log.close().await.expect("close");
        let repaired = read_record_file(&path).await.expect("repaired read");
        assert_eq!(repaired.records.len(), 2);
        assert!(!repaired.ignored_truncated_final_line);
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn normalizes_a_valid_final_record_without_newline_before_append() {
        let directory = temp_path("unterminated-valid");
        fs::create_dir_all(&directory).await.expect("mkdir");
        let path = directory.join("records.jsonl");
        let metadata = serde_json::to_string(&metadata_record()).expect("metadata");
        fs::write(&path, metadata).await.expect("fixture");
        let (log, read) = RecordLog::open(&path).await.expect("open");
        assert!(read.missing_final_newline);
        log.append(record(RecordKind::ContextUpdateTokenCount))
            .await
            .expect("append");
        log.close().await.expect("close");
        assert_eq!(
            read_record_file(&path).await.expect("read").records.len(),
            2
        );
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn rejects_corruption_before_the_final_line() {
        let directory = temp_path("corrupt");
        fs::create_dir_all(&directory).await.expect("mkdir");
        let path = directory.join("records.jsonl");
        let metadata = serde_json::to_string(&metadata_record()).expect("metadata");
        fs::write(
            &path,
            format!("{metadata}\nnot-json\n{{\"type\":\"turn.cancel\"}}\n"),
        )
        .await
        .expect("fixture");
        assert!(matches!(
            read_record_file(&path).await,
            Err(RecordLogError::CorruptRecord { line: 2, .. })
        ));
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn closed_writer_stays_closed() {
        let directory = temp_path("closed");
        let path = directory.join("records.jsonl");
        let (log, _) = RecordLog::open(&path).await.expect("open");
        log.ensure_metadata().await.expect("metadata");
        log.close().await.expect("close");
        assert!(matches!(
            log.append(record(RecordKind::TurnCancel)).await,
            Err(RecordLogError::Closed(_))
        ));
        let _ = fs::remove_dir_all(directory).await;
    }

    #[tokio::test]
    async fn a_latched_write_error_rejects_every_later_write() {
        let directory = temp_path("latched");
        let path = directory.join("records.jsonl");
        let (log, _) = RecordLog::open(&path).await.expect("open");
        {
            let mut writer = log.writer.lock().await;
            writer.latched_write_error = Some("injected write failure".to_owned());
        }
        for _ in 0..2 {
            assert!(matches!(
                log.append(record(RecordKind::TurnCancel)).await,
                Err(RecordLogError::WriteLatched { .. })
            ));
        }
        let _ = fs::remove_dir_all(directory).await;
    }
}
