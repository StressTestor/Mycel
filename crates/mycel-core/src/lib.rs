use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CORE_CRATE_NAME: &str = "mycel-core";

pub type Result<T> = std::result::Result<T, MycelError>;

#[derive(Debug, thiserror::Error)]
pub enum MycelError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timestamp parse error: {0}")]
    Timestamp(#[from] chrono::ParseError),
    #[error("uuid parse error: {0}")]
    Uuid(#[from] uuid::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown {field} value: {value}")]
    UnknownEnum { field: &'static str, value: String },
    #[error("invalid sqlite identifier: {0}")]
    InvalidSqlIdentifier(String),
    #[error("at least one signature field must be populated")]
    EmptySignature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Antibody {
    pub id: Uuid,
    pub signature: Signature,
    pub source: AntibodySource,
    pub severity: Severity,
    pub confidence: Confidence,
    pub refusal_mode: RefusalMode,
    pub remediation: String,
    pub examples: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub hit_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub error_class: Option<String>,
    pub file_pattern: Option<String>,
    pub agent_role: Option<String>,
    /// Sentinel-derived antibodies populate only `tool_pattern` in v0.1.
    pub tool_pattern: Option<String>,
    pub scope: SignatureScope,
}

impl Signature {
    pub fn has_populated_field(&self) -> bool {
        self.error_class.is_some()
            || self.file_pattern.is_some()
            || self.agent_role.is_some()
            || self.tool_pattern.is_some()
    }

    fn matches(&self, run: &ProposedRun) -> bool {
        self.scope == run.scope
            && field_matches(&self.error_class, &run.error_class)
            && field_matches(&self.file_pattern, &run.file_path)
            && field_matches(&self.agent_role, &run.agent_role)
            && field_matches(&self.tool_pattern, &run.tool_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedRun {
    pub error_class: Option<String>,
    pub file_path: Option<String>,
    pub agent_role: Option<String>,
    pub tool_name: Option<String>,
    pub scope: SignatureScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentinelAuditEvent {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub tool_name: String,
    pub action: SentinelAction,
    pub mode: String,
    pub reason: Option<String>,
    pub matched_rule: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentinelAntibodyCandidate {
    pub source: SentinelSource,
    pub metadata: SentinelMetadata,
    pub antibody: Antibody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentinelSource {
    pub event_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub tool_name: String,
    pub action: SentinelAction,
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentinelMetadata {
    pub reason: Option<String>,
    pub matched_rule: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AntibodySource {
    SentinelBlock,
    FailedRun,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warn,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    Solid,
    Directional,
    Vibes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusalMode {
    Hard,
    Soft,
    LogOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureScope {
    Project,
    Global,
    Personal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SentinelAction {
    Block,
    Warn,
    Allow,
}

pub struct AntibodyStore {
    conn: Connection,
}

impl AntibodyStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn schema_version(&self) -> Result<u32> {
        let version: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(version)
    }

    pub fn insert_antibody(&self, antibody: &Antibody) -> Result<()> {
        validate_signature(&antibody.signature)?;
        self.conn.execute(
            "INSERT INTO antibodies (
                id, error_class, file_pattern, agent_role, tool_pattern, scope,
                source, severity, confidence, refusal_mode, remediation,
                examples_json, created_at, expires_at, hit_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                antibody.id.to_string(),
                antibody.signature.error_class,
                antibody.signature.file_pattern,
                antibody.signature.agent_role,
                antibody.signature.tool_pattern,
                antibody.signature.scope.as_str(),
                antibody.source.as_str(),
                antibody.severity.as_str(),
                antibody.confidence.as_str(),
                antibody.refusal_mode.as_str(),
                antibody.remediation,
                serde_json::to_string(&antibody.examples)?,
                antibody.created_at.to_rfc3339(),
                antibody.expires_at.map(|value| value.to_rfc3339()),
                antibody.hit_count,
            ],
        )?;
        Ok(())
    }

    pub fn get_antibody(&self, id: Uuid) -> Result<Option<Antibody>> {
        Ok(self
            .conn
            .query_row(
                "SELECT
                    id, error_class, file_pattern, agent_role, tool_pattern, scope,
                    source, severity, confidence, refusal_mode, remediation,
                    examples_json, created_at, expires_at, hit_count
                FROM antibodies
                WHERE id = ?1",
                params![id.to_string()],
                antibody_from_row,
            )
            .optional()?)
    }

    pub fn list_antibodies(&self) -> Result<Vec<Antibody>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                id, error_class, file_pattern, agent_role, tool_pattern, scope,
                source, severity, confidence, refusal_mode, remediation,
                examples_json, created_at, expires_at, hit_count
            FROM antibodies
            ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], antibody_from_row)?;
        Ok(collect_rows(rows)?)
    }

    pub fn update_antibody(&self, antibody: &Antibody) -> Result<()> {
        validate_signature(&antibody.signature)?;
        self.conn.execute(
            "UPDATE antibodies
            SET error_class = ?2,
                file_pattern = ?3,
                agent_role = ?4,
                tool_pattern = ?5,
                scope = ?6,
                source = ?7,
                severity = ?8,
                confidence = ?9,
                refusal_mode = ?10,
                remediation = ?11,
                examples_json = ?12,
                created_at = ?13,
                expires_at = ?14,
                hit_count = ?15
            WHERE id = ?1",
            params![
                antibody.id.to_string(),
                antibody.signature.error_class,
                antibody.signature.file_pattern,
                antibody.signature.agent_role,
                antibody.signature.tool_pattern,
                antibody.signature.scope.as_str(),
                antibody.source.as_str(),
                antibody.severity.as_str(),
                antibody.confidence.as_str(),
                antibody.refusal_mode.as_str(),
                antibody.remediation,
                serde_json::to_string(&antibody.examples)?,
                antibody.created_at.to_rfc3339(),
                antibody.expires_at.map(|value| value.to_rfc3339()),
                antibody.hit_count,
            ],
        )?;
        Ok(())
    }

    pub fn delete_antibody(&self, id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM antibodies WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn matching_antibodies(&self, run: &ProposedRun) -> Result<Vec<Antibody>> {
        let antibodies = self.list_antibodies()?;
        Ok(antibodies
            .into_iter()
            .filter(|antibody| antibody.signature.matches(run))
            .collect())
    }

    pub fn ingest_sentinel_audit_jsonl(
        &self,
        reader: impl Read,
        now: DateTime<Utc>,
    ) -> Result<Vec<SentinelAntibodyCandidate>> {
        let mut candidates = Vec::new();
        for line in BufReader::new(reader).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let raw: RawSentinelAuditEvent = serde_json::from_str(&line)?;
            let event = raw.into_event()?;
            self.insert_sentinel_event(&event, &line)?;
            candidates.push(event.into_candidate(now));
        }
        Ok(candidates)
    }

    pub fn sentinel_event_count(&self) -> Result<u32> {
        let count: u32 =
            self.conn
                .query_row("SELECT COUNT(*) FROM sentinel_audit_events", [], |row| {
                    row.get(0)
                })?;
        Ok(count)
    }

    pub fn sentinel_events_for_matched_rule(
        &self,
        matched_rule: &str,
    ) -> Result<Vec<SentinelAuditEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, tool_name, action, mode, reason, matched_rule
            FROM sentinel_audit_events
            WHERE matched_rule = ?1
            ORDER BY timestamp, id",
        )?;
        let rows = stmt.query_map(params![matched_rule], sentinel_event_from_row)?;
        Ok(collect_rows(rows)?)
    }

    pub fn has_sqlite_index(&self, table: &str, index: &str) -> Result<bool> {
        validate_identifier(table)?;
        let sql = format!("PRAGMA index_list({table})");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let index_name: String = row.get(1)?;
            if index_name == index {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS antibodies (
                id TEXT PRIMARY KEY NOT NULL,
                error_class TEXT,
                file_pattern TEXT,
                agent_role TEXT,
                tool_pattern TEXT,
                scope TEXT NOT NULL,
                source TEXT NOT NULL,
                severity TEXT NOT NULL,
                confidence TEXT NOT NULL,
                refusal_mode TEXT NOT NULL,
                remediation TEXT NOT NULL,
                examples_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                expires_at TEXT,
                hit_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_antibodies_tool_pattern
                ON antibodies(tool_pattern);
            CREATE INDEX IF NOT EXISTS idx_antibodies_scope
                ON antibodies(scope);
            CREATE TABLE IF NOT EXISTS sentinel_audit_events (
                id TEXT PRIMARY KEY NOT NULL,
                timestamp TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                action TEXT NOT NULL,
                mode TEXT NOT NULL,
                reason TEXT,
                matched_rule TEXT,
                raw_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sentinel_audit_events_matched_rule
                ON sentinel_audit_events(matched_rule);
            PRAGMA user_version = 2;",
        )?;
        Ok(())
    }

    fn insert_sentinel_event(&self, event: &SentinelAuditEvent, raw_json: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sentinel_audit_events (
                id, timestamp, tool_name, action, mode, reason, matched_rule, raw_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.id.to_string(),
                event.timestamp.to_rfc3339(),
                event.tool_name,
                event.action.as_str(),
                event.mode,
                event.reason,
                event.matched_rule,
                raw_json,
            ],
        )?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RawSentinelAuditEvent {
    timestamp: String,
    tool_name: String,
    action: String,
    reason: Option<String>,
    matched_rule: Option<String>,
    mode: String,
}

impl RawSentinelAuditEvent {
    fn into_event(self) -> Result<SentinelAuditEvent> {
        Ok(SentinelAuditEvent {
            id: Uuid::new_v4(),
            timestamp: parse_datetime_result(&self.timestamp)?,
            tool_name: self.tool_name,
            action: SentinelAction::parse_result(&self.action)?,
            mode: self.mode,
            reason: self.reason,
            matched_rule: self.matched_rule,
        })
    }
}

impl SentinelAuditEvent {
    fn into_candidate(self, now: DateTime<Utc>) -> SentinelAntibodyCandidate {
        let severity = self.action.severity();
        let refusal_mode = self.action.refusal_mode();
        let remediation = self.reason.clone().unwrap_or_else(|| {
            format!(
                "review Sentinel {} event for {}",
                self.action.as_str(),
                self.tool_name
            )
        });
        let examples = self
            .matched_rule
            .clone()
            .into_iter()
            .take(1)
            .collect::<Vec<_>>();

        SentinelAntibodyCandidate {
            source: SentinelSource {
                event_id: self.id,
                timestamp: self.timestamp,
                tool_name: self.tool_name.clone(),
                action: self.action,
                mode: self.mode.clone(),
            },
            metadata: SentinelMetadata {
                reason: self.reason,
                matched_rule: self.matched_rule,
            },
            antibody: Antibody {
                id: Uuid::new_v4(),
                signature: Signature {
                    error_class: None,
                    file_pattern: None,
                    agent_role: None,
                    tool_pattern: Some(self.tool_name),
                    scope: SignatureScope::Project,
                },
                source: AntibodySource::SentinelBlock,
                severity,
                confidence: confidence_from_mode(&self.mode),
                refusal_mode,
                remediation,
                examples,
                created_at: now,
                expires_at: None,
                hit_count: 0,
            },
        }
    }
}

fn antibody_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Antibody> {
    let id: String = row.get(0)?;
    let scope: String = row.get(5)?;
    let source: String = row.get(6)?;
    let severity: String = row.get(7)?;
    let confidence: String = row.get(8)?;
    let refusal_mode: String = row.get(9)?;
    let examples_json: String = row.get(11)?;
    let created_at: String = row.get(12)?;
    let expires_at: Option<String> = row.get(13)?;
    let hit_count: u32 = row.get(14)?;

    Ok(Antibody {
        id: parse_uuid(&id)?,
        signature: Signature {
            error_class: row.get(1)?,
            file_pattern: row.get(2)?,
            agent_role: row.get(3)?,
            tool_pattern: row.get(4)?,
            scope: SignatureScope::parse(&scope)?,
        },
        source: AntibodySource::parse(&source)?,
        severity: Severity::parse(&severity)?,
        confidence: Confidence::parse(&confidence)?,
        refusal_mode: RefusalMode::parse(&refusal_mode)?,
        remediation: row.get(10)?,
        examples: parse_examples(&examples_json)?,
        created_at: parse_datetime(&created_at)?,
        expires_at: parse_optional_datetime(expires_at)?,
        hit_count,
    })
}

fn sentinel_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SentinelAuditEvent> {
    let id: String = row.get(0)?;
    let timestamp: String = row.get(1)?;
    let action: String = row.get(3)?;
    Ok(SentinelAuditEvent {
        id: parse_uuid(&id)?,
        timestamp: parse_datetime(&timestamp)?,
        tool_name: row.get(2)?,
        action: SentinelAction::parse_sql(&action)?,
        mode: row.get(4)?,
        reason: row.get(5)?,
        matched_rule: row.get(6)?,
    })
}

fn collect_rows<T>(rows: impl Iterator<Item = rusqlite::Result<T>>) -> rusqlite::Result<Vec<T>> {
    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(items)
}

fn validate_signature(signature: &Signature) -> Result<()> {
    if signature.has_populated_field() {
        Ok(())
    } else {
        Err(MycelError::EmptySignature)
    }
}

fn field_matches(signature_value: &Option<String>, run_value: &Option<String>) -> bool {
    match signature_value {
        Some(expected) => run_value.as_ref() == Some(expected),
        None => true,
    }
}

fn parse_uuid(value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(to_sql_error)
}

fn parse_examples(value: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(value).map_err(to_sql_error)
}

fn parse_datetime(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    parse_datetime_result(value).map_err(to_sql_error)
}

fn parse_datetime_result(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn parse_optional_datetime(value: Option<String>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value.as_deref().map(parse_datetime).transpose()
}

fn to_sql_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn validate_identifier(value: &str) -> Result<()> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Ok(())
    } else {
        Err(MycelError::InvalidSqlIdentifier(value.to_string()))
    }
}

fn confidence_from_mode(mode: &str) -> Confidence {
    if mode == "enforce" {
        Confidence::Solid
    } else {
        Confidence::Directional
    }
}

macro_rules! string_enum {
    ($type_name:ident, $field:literal, [$($variant:ident => $value:literal),+ $(,)?]) => {
        impl $type_name {
            fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }

            fn parse(value: &str) -> rusqlite::Result<Self> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(to_sql_error(MycelError::UnknownEnum {
                        field: $field,
                        value: value.to_string(),
                    })),
                }
            }
        }
    };
}

string_enum!(AntibodySource, "source", [
    SentinelBlock => "sentinel_block",
    FailedRun => "failed_run",
    Manual => "manual",
]);

string_enum!(Severity, "severity", [
    Info => "info",
    Warn => "warn",
    Refuse => "refuse",
]);

string_enum!(Confidence, "confidence", [
    Solid => "solid",
    Directional => "directional",
    Vibes => "vibes",
]);

string_enum!(RefusalMode, "refusal_mode", [
    Hard => "hard",
    Soft => "soft",
    LogOnly => "log_only",
]);

string_enum!(SignatureScope, "scope", [
    Project => "project",
    Global => "global",
    Personal => "personal",
]);

impl SentinelAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Warn => "warn",
            Self::Allow => "allow",
        }
    }

    fn parse_result(value: &str) -> Result<Self> {
        match value {
            "block" => Ok(Self::Block),
            "warn" => Ok(Self::Warn),
            "allow" => Ok(Self::Allow),
            _ => Err(MycelError::UnknownEnum {
                field: "action",
                value: value.to_string(),
            }),
        }
    }

    fn parse_sql(value: &str) -> rusqlite::Result<Self> {
        Self::parse_result(value).map_err(to_sql_error)
    }

    fn severity(self) -> Severity {
        match self {
            Self::Block => Severity::Refuse,
            Self::Warn => Severity::Warn,
            Self::Allow => Severity::Info,
        }
    }

    fn refusal_mode(self) -> RefusalMode {
        match self {
            Self::Block => RefusalMode::Hard,
            Self::Warn => RefusalMode::Soft,
            Self::Allow => RefusalMode::LogOnly,
        }
    }
}
