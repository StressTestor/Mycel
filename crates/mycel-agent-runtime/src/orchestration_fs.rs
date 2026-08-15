//! Private, session-scoped persistence for orchestration reducers.

use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use crate::{OrchestrationRecord, OrchestrationStore, SessionId};

const LOG_NAME: &str = "orchestration.jsonl";
const TEMP_PREFIX: &str = ".orchestration.jsonl.tmp-";
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 1_000_000;

/// Filesystem-backed orchestration store.
///
/// Every store is rooted beneath one validated session ID. Updates replace a
/// fully synced temporary file atomically, so a multi-record append is either
/// entirely visible or absent after a crash. Recovery accepts only a malformed
/// unterminated final line; earlier corruption remains fatal.
pub struct FilesystemOrchestrationStore {
    directory: PathBuf,
    path: PathBuf,
    sequence: AtomicU64,
    lock: Mutex<()>,
}

impl FilesystemOrchestrationStore {
    pub fn open(root: impl AsRef<Path>, session_id: &SessionId) -> Result<Self, String> {
        let root = root.as_ref();
        create_private_directory(root)?;
        let root =
            fs::canonicalize(root).map_err(|error| io_error("resolve store root", &error))?;
        let directory = root.join(session_id.as_str());
        create_private_directory(&directory)?;
        let directory = fs::canonicalize(&directory)
            .map_err(|error| io_error("resolve session store directory", &error))?;
        if !directory.starts_with(&root) {
            return Err("session orchestration directory escaped its configured root".to_owned());
        }
        let store = Self {
            path: directory.join(LOG_NAME),
            directory,
            sequence: AtomicU64::new(0),
            lock: Mutex::new(()),
        };
        {
            let _guard = lock(&store.lock);
            store.recover_initial_file()?;
            if store.path.exists() {
                require_private_regular_file(&store.path)?;
                let (records, needs_rewrite) = store.read_records()?;
                if needs_rewrite {
                    store.write_records(&records)?;
                }
            }
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn recover_initial_file(&self) -> Result<(), String> {
        if self.path.exists() {
            return Ok(());
        }
        let mut candidates = fs::read_dir(&self.directory)
            .map_err(|error| io_error("read orchestration directory", &error))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(TEMP_PREFIX))
            })
            .collect::<Vec<_>>();
        candidates.sort();
        for candidate in candidates.into_iter().rev() {
            if require_private_regular_file(&candidate).is_err() {
                continue;
            }
            let bytes = match read_bounded(&candidate) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let Ok((_, needs_rewrite)) = decode_records(&bytes) else {
                continue;
            };
            if needs_rewrite {
                continue;
            }
            fs::rename(&candidate, &self.path)
                .map_err(|error| io_error("recover orchestration log", &error))?;
            set_private_file_mode(&self.path)?;
            sync_directory(&self.directory)?;
            return Ok(());
        }
        Ok(())
    }

    fn read_records(&self) -> Result<(Vec<OrchestrationRecord>, bool), String> {
        if !self.path.exists() {
            return Ok((Vec::new(), false));
        }
        require_private_regular_file(&self.path)?;
        decode_records(&read_bounded(&self.path)?)
    }

    fn write_records(&self, records: &[OrchestrationRecord]) -> Result<(), String> {
        if records.len() > MAX_RECORDS {
            return Err("orchestration record count exceeds its safety limit".to_owned());
        }
        let mut bytes = Vec::new();
        for record in records {
            validate_record(record)?;
            serde_json::to_writer(&mut bytes, record)
                .map_err(|_| "orchestration record serialization failed".to_owned())?;
            bytes.push(b'\n');
            if bytes.len() as u64 > MAX_LOG_BYTES {
                return Err("orchestration log exceeds its byte limit".to_owned());
            }
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let temporary = self.directory.join(format!(
            "{TEMP_PREFIX}{:08x}-{sequence:016x}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .map_err(|error| io_error("create orchestration transaction", &error))?;
        let result = (|| {
            file.write_all(&bytes)
                .map_err(|error| io_error("write orchestration transaction", &error))?;
            file.sync_all()
                .map_err(|error| io_error("sync orchestration transaction", &error))?;
            fs::rename(&temporary, &self.path)
                .map_err(|error| io_error("commit orchestration transaction", &error))?;
            set_private_file_mode(&self.path)?;
            sync_directory(&self.directory)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl OrchestrationStore for FilesystemOrchestrationStore {
    fn load(&self) -> Result<Vec<OrchestrationRecord>, String> {
        let _guard = lock(&self.lock);
        let (records, needs_rewrite) = self.read_records()?;
        if needs_rewrite {
            self.write_records(&records)?;
        }
        Ok(records)
    }

    fn append(&self, records: &[OrchestrationRecord]) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let _guard = lock(&self.lock);
        let (mut current, _) = self.read_records()?;
        if current.len().saturating_add(records.len()) > MAX_RECORDS {
            return Err("orchestration record count exceeds its safety limit".to_owned());
        }
        for record in records {
            validate_record(record)?;
        }
        current.extend_from_slice(records);
        self.write_records(&current)
    }
}

fn decode_records(bytes: &[u8]) -> Result<(Vec<OrchestrationRecord>, bool), String> {
    if bytes.len() as u64 > MAX_LOG_BYTES {
        return Err("orchestration log exceeds its byte limit".to_owned());
    }
    let terminated = bytes.ends_with(b"\n");
    let mut records = Vec::new();
    let mut needs_rewrite = !bytes.is_empty() && !terminated;
    let mut lines = bytes.split(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() && lines.peek().is_none() && terminated {
            break;
        }
        if records.len() >= MAX_RECORDS {
            return Err("orchestration record count exceeds its safety limit".to_owned());
        }
        match serde_json::from_slice::<OrchestrationRecord>(line) {
            Ok(record) => {
                validate_record(&record)?;
                records.push(record);
            }
            Err(_) if lines.peek().is_none() && !terminated => {
                needs_rewrite = true;
                break;
            }
            Err(_) => return Err("orchestration log contains corrupt record data".to_owned()),
        }
    }
    Ok((records, needs_rewrite))
}

fn validate_record(record: &OrchestrationRecord) -> Result<(), String> {
    if record.scope.is_empty()
        || record.scope.len() > 256
        || record.scope.chars().any(char::is_control)
    {
        return Err("orchestration record has an invalid scope".to_owned());
    }
    if record.action.is_empty()
        || record.action.len() > 256
        || record.action.chars().any(char::is_control)
    {
        return Err("orchestration record has an invalid action".to_owned());
    }
    if record
        .entity_id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
    {
        return Err("orchestration record has an invalid entity id".to_owned());
    }
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect orchestration log", &error))?;
    if metadata.len() > MAX_LOG_BYTES {
        return Err("orchestration log exceeds its byte limit".to_owned());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| io_error("open orchestration log", &error))?
        .take(MAX_LOG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("read orchestration log", &error))?;
    if bytes.len() as u64 > MAX_LOG_BYTES {
        return Err("orchestration log exceeds its byte limit".to_owned());
    }
    Ok(bytes)
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("orchestration path is not a real directory".to_owned())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|error| io_error("create orchestration directory", &error))?,
        Err(error) => return Err(io_error("inspect orchestration directory", &error)),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io_error("secure orchestration directory", &error))?;
    Ok(())
}

fn require_private_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect orchestration file", &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("orchestration log is not a real regular file".to_owned());
    }
    set_private_file_mode(path)
}

fn set_private_file_mode(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("secure orchestration file", &error))?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("sync orchestration directory", &error))
}

fn io_error(context: &str, error: &std::io::Error) -> String {
    format!("{context}: {}", error.kind())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
