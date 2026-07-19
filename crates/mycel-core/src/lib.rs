use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod decay;
pub mod projection;
pub mod promptpressure;
pub mod sclerotia;
pub mod selfspec;
pub mod spore;

pub use decay::{DecayEngine, DecayReport};
pub use projection::{render_compost_md, render_substrate_md, run_maintenance, MaintenanceReport};
pub use promptpressure::{
    PromptPressureImport, PromptPressureRecord, PromptPressureTier, TTL_PROBABLE, TTL_SPECULATIVE,
    TTL_VERIFIED,
};
pub use sclerotia::{
    evaluate_resume, ResumeDecision, Sclerotium, SclerotiumStore, SclerotiumValidationError,
    WakeCondition, WakeWorld,
};
pub use selfspec::{
    dedupe_specs, ExecutabilityGap, InheritedContext, SelfSpec, SpecStore, SpecValidationError,
    TaskIdentity,
};
pub use spore::{
    classify_adjacent_work, dedupe_spores, export_spore, AdjacentWorkNotice, GerminationCandidate,
    InteropShape, Spore, SporeExport, SporeKind, SporeStore, SporeValidationError,
};

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
    #[error("invalid audit log path: {0}")]
    InvalidAuditPath(PathBuf),
    #[error("at least one signature field must be populated")]
    EmptySignature,
    #[error("invalid spec: {0}")]
    InvalidSpec(String),
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
    pub tool_pattern: Option<String>,
    pub command_pattern: Option<String>,
    pub scope: SignatureScope,
}

impl Signature {
    pub fn has_populated_field(&self) -> bool {
        self.error_class.is_some()
            || self.file_pattern.is_some()
            || self.agent_role.is_some()
            || self.tool_pattern.is_some()
            || self.command_pattern.is_some()
    }

    fn matches(&self, run: &ProposedRun) -> bool {
        self.scope == run.scope
            && field_matches(&self.error_class, &run.error_class)
            && glob_field_matches(&self.file_pattern, &run.file_path)
            && field_matches(&self.agent_role, &run.agent_role)
            && field_matches(&self.tool_pattern, &run.tool_name)
            && command_matches(&self.command_pattern, &run.command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedRun {
    pub error_class: Option<String>,
    pub file_path: Option<String>,
    pub agent_role: Option<String>,
    pub tool_name: Option<String>,
    pub command: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub outcome: EvaluationOutcome,
    pub matches: Vec<EvaluationMatch>,
}

#[derive(Debug, Clone)]
pub struct SubstratePaths {
    pub db_path: PathBuf,
    pub workspace_dir: PathBuf,
    pub audit_log_path: PathBuf,
    pub audit_max_bytes: u64,
}

pub struct SubstrateRuntime {
    store: AntibodyStore,
    workspace_dir: PathBuf,
    audit_log_path: PathBuf,
    audit_max_bytes: u64,
    projection_due_at: Option<DateTime<Utc>>,
}

impl Evaluation {
    pub fn refusal(&self) -> Option<&EvaluationMatch> {
        if self.outcome == EvaluationOutcome::Refuse {
            self.matches
                .iter()
                .find(|matched| matched.outcome == EvaluationOutcome::Refuse)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationMatch {
    pub antibody_id: Uuid,
    pub outcome: EvaluationOutcome,
    pub severity: Severity,
    pub refusal_mode: RefusalMode,
    pub remediation: String,
    pub source_pointer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationOutcome {
    Refuse,
    Warn,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AntibodySource {
    SentinelBlock,
    FailedRun,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Solid,
    Directional,
    Vibes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalMode {
    Hard,
    Soft,
    LogOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureScope {
    Project,
    Global,
    Personal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
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
                id, error_class, file_pattern, agent_role, tool_pattern, command_pattern, scope,
                source, severity, confidence, refusal_mode, remediation,
                examples_json, created_at, expires_at, hit_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                antibody.id.to_string(),
                antibody.signature.error_class,
                antibody.signature.file_pattern,
                antibody.signature.agent_role,
                antibody.signature.tool_pattern,
                antibody.signature.command_pattern,
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
                    id, error_class, file_pattern, agent_role, tool_pattern, command_pattern, scope,
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
                id, error_class, file_pattern, agent_role, tool_pattern, command_pattern, scope,
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
                command_pattern = ?6,
                scope = ?7,
                source = ?8,
                severity = ?9,
                confidence = ?10,
                refusal_mode = ?11,
                remediation = ?12,
                examples_json = ?13,
                created_at = ?14,
                expires_at = ?15,
                hit_count = ?16
            WHERE id = ?1",
            params![
                antibody.id.to_string(),
                antibody.signature.error_class,
                antibody.signature.file_pattern,
                antibody.signature.agent_role,
                antibody.signature.tool_pattern,
                antibody.signature.command_pattern,
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

    pub fn evaluate_run(&self, run: &ProposedRun, now: DateTime<Utc>) -> Result<Evaluation> {
        let mut matches = self
            .matching_antibodies(run)?
            .into_iter()
            .filter(|antibody| !is_expired(antibody, now))
            .map(EvaluationMatch::from_antibody)
            .collect::<Vec<_>>();
        matches.sort_by_key(|matched| matched.outcome.rank());

        let outcome = matches
            .iter()
            .map(|matched| matched.outcome)
            .min_by_key(|outcome| outcome.rank())
            .unwrap_or(EvaluationOutcome::Allow);

        Ok(Evaluation { outcome, matches })
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
                command_pattern TEXT,
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
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                summary TEXT NOT NULL,
                confidence TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                -- expires_at: absolute unix timestamp when this record's ttl expires; NULL means no ttl.
                expires_at INTEGER,
                -- no_compost: 0/1 boolean. when 1, maintenance preserves this record regardless of confidence/ttl.
                no_compost INTEGER NOT NULL DEFAULT 0,
                -- decay_state: NULL = live/unprocessed; 'retained'|'distilled'|'decayed' after a maintenance pass.
                decay_state TEXT,
                -- decayed_at: unix timestamp when a maintenance pass acted on this record; NULL otherwise.
                decayed_at INTEGER,
                -- distilled_summary: compressed gist set when a record is distilled; NULL otherwise.
                distilled_summary TEXT
            );",
        )?;
        let version: u32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 3 {
            let has_command_pattern: bool = self
                .conn
                .prepare("PRAGMA table_info(antibodies)")?
                .query_map([], |row| {
                    let name: String = row.get(1)?;
                    Ok(name == "command_pattern")
                })?
                .any(|r| r.unwrap_or(false));
            if !has_command_pattern {
                self.conn
                    .execute_batch("ALTER TABLE antibodies ADD COLUMN command_pattern TEXT;")?;
            }
            self.conn.execute_batch("PRAGMA user_version = 3;")?;
        }
        if version < 4 {
            // v3 -> v4: add decay columns to runs table.
            // The runs table is brand-new in v4, created with all columns above via CREATE TABLE IF
            // NOT EXISTS. On a fresh DB the table already has the columns, so these ALTERs are
            // skipped. On a pre-release DB that had runs without decay columns, they would be added.
            let has_expires_at: bool = self
                .conn
                .prepare("PRAGMA table_info(runs)")?
                .query_map([], |row| {
                    let name: String = row.get(1)?;
                    Ok(name == "expires_at")
                })?
                .any(|r| r.unwrap_or(false));
            if !has_expires_at {
                self.conn.execute_batch(
                    "ALTER TABLE runs ADD COLUMN expires_at INTEGER;
                     ALTER TABLE runs ADD COLUMN no_compost INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE runs ADD COLUMN decay_state TEXT;
                     ALTER TABLE runs ADD COLUMN decayed_at INTEGER;
                     ALTER TABLE runs ADD COLUMN distilled_summary TEXT;",
                )?;
            }
            self.conn.execute_batch("PRAGMA user_version = 4;")?;
        }
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

impl SubstrateRuntime {
    pub fn open(paths: SubstratePaths) -> Result<Self> {
        let store = AntibodyStore::open(paths.db_path)?;
        Ok(Self {
            store,
            workspace_dir: paths.workspace_dir,
            audit_log_path: paths.audit_log_path,
            audit_max_bytes: paths.audit_max_bytes,
            projection_due_at: None,
        })
    }

    pub fn insert_antibody(&mut self, antibody: &Antibody, now: DateTime<Utc>) -> Result<()> {
        self.store.insert_antibody(antibody)?;
        self.after_mutation("antibody.inserted", antibody.id, now)
    }

    pub fn update_antibody(&mut self, antibody: &Antibody, now: DateTime<Utc>) -> Result<()> {
        self.store.update_antibody(antibody)?;
        self.after_mutation("antibody.updated", antibody.id, now)
    }

    pub fn delete_antibody(&mut self, id: Uuid, now: DateTime<Utc>) -> Result<()> {
        self.store.delete_antibody(id)?;
        self.after_mutation("antibody.deleted", id, now)
    }

    pub fn flush_projections(&mut self, now: DateTime<Utc>) -> Result<bool> {
        let Some(due_at) = self.projection_due_at else {
            return Ok(false);
        };
        if now < due_at {
            return Ok(false);
        }

        fs::create_dir_all(&self.workspace_dir)?;
        fs::write(
            self.workspace_dir.join("SUBSTRATE.md"),
            render_substrate_projection(&self.store.list_antibodies()?),
        )?;
        self.projection_due_at = None;
        Ok(true)
    }

    fn after_mutation(
        &mut self,
        event_type: &'static str,
        antibody_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.append_audit_event(MutationAuditEvent {
            timestamp: now,
            event_type,
            antibody_id,
        })?;
        self.projection_due_at = Some(now + chrono::Duration::milliseconds(500));
        Ok(())
    }

    fn append_audit_event(&self, event: MutationAuditEvent) -> Result<()> {
        if let Some(parent) = self.audit_log_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let line = format!("{}\n", serde_json::to_string(&event)?);
        if self.should_rotate(line.len() as u64)? {
            self.rotate_audit_log()?;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_log_path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    fn should_rotate(&self, next_len: u64) -> Result<bool> {
        if self.audit_max_bytes == 0 || !self.audit_log_path.exists() {
            return Ok(false);
        }
        let current_len = fs::metadata(&self.audit_log_path)?.len();
        Ok(current_len > 0 && current_len + next_len > self.audit_max_bytes)
    }

    fn rotate_audit_log(&self) -> Result<()> {
        let rotated = rotated_audit_path(&self.audit_log_path)?;
        if rotated.exists() {
            fs::remove_file(&rotated)?;
        }
        fs::rename(&self.audit_log_path, rotated)?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct MutationAuditEvent {
    timestamp: DateTime<Utc>,
    event_type: &'static str,
    antibody_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessMetrics {
    pub antibody_count: usize,
    pub sentinel_event_count: usize,
    pub eval_fixture_count: usize,
    pub pass_count: usize,
    pub fail_count: usize,
    pub safe_fixture_count: usize,
    pub false_positive_count: usize,
    pub false_positive_rate: f64,
    pub expiry_fixture_count: usize,
    pub expiry_pass_count: usize,
    pub refusals_missing_remediation: usize,
    pub refusals_missing_source_pointer: usize,
    pub gate_scope_counts: GateScopeCounts,
    pub interop_loss_matrix_shapes: usize,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GateScopeCounts {
    pub agent_launch: usize,
    pub tool_invocation: usize,
    pub substrate_mutation: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureLabel {
    Safe,
    Unsafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateScope {
    AgentLaunch,
    ToolInvocation,
    SubstrateMutation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationFixture {
    pub name: String,
    pub label: FixtureLabel,
    pub gate_scope: GateScope,
    pub run: ProposedRun,
    pub expected: EvaluationOutcome,
    pub evaluated_at: DateTime<Utc>,
    pub tags: Vec<String>,
}

pub fn run_v0_1_harness(now: DateTime<Utc>) -> Result<HarnessMetrics> {
    let store = AntibodyStore::open_in_memory()?;
    let antibodies = seed_v0_1_antibodies(now);
    for antibody in &antibodies {
        store.insert_antibody(antibody)?;
    }

    let sentinel_candidates =
        store.ingest_sentinel_audit_jsonl(seed_v0_1_sentinel_events().as_bytes(), now)?;
    let fixtures = seed_v0_1_eval_fixtures(now);

    let mut pass_count = 0usize;
    let mut safe_fixture_count = 0usize;
    let mut false_positive_count = 0usize;
    let mut expiry_fixture_count = 0usize;
    let mut expiry_pass_count = 0usize;
    let mut refusals_missing_remediation = 0usize;
    let mut refusals_missing_source_pointer = 0usize;
    let mut gate_scope_counts = GateScopeCounts::default();

    for fixture in &fixtures {
        let evaluation = store.evaluate_run(&fixture.run, fixture.evaluated_at)?;
        let passed = evaluation.outcome == fixture.expected;
        if passed {
            pass_count += 1;
            gate_scope_counts.increment(fixture.gate_scope);
        }

        if fixture.label == FixtureLabel::Safe {
            safe_fixture_count += 1;
            if evaluation.outcome != EvaluationOutcome::Allow {
                false_positive_count += 1;
            }
        }

        if fixture.tags.iter().any(|tag| tag == "expiry") {
            expiry_fixture_count += 1;
            if passed {
                expiry_pass_count += 1;
            }
        }

        if evaluation.outcome == EvaluationOutcome::Refuse {
            match evaluation.refusal() {
                Some(refusal) => {
                    if refusal.remediation.is_empty() {
                        refusals_missing_remediation += 1;
                    }
                    if refusal.source_pointer.is_empty() {
                        refusals_missing_source_pointer += 1;
                    }
                }
                None => {
                    refusals_missing_remediation += 1;
                    refusals_missing_source_pointer += 1;
                }
            }
        }
    }

    let false_positive_rate = if safe_fixture_count == 0 {
        0.0
    } else {
        false_positive_count as f64 / safe_fixture_count as f64
    };

    Ok(HarnessMetrics {
        antibody_count: antibodies.len(),
        sentinel_event_count: sentinel_candidates.len(),
        eval_fixture_count: fixtures.len(),
        pass_count,
        fail_count: fixtures.len() - pass_count,
        safe_fixture_count,
        false_positive_count,
        false_positive_rate,
        expiry_fixture_count,
        expiry_pass_count,
        refusals_missing_remediation,
        refusals_missing_source_pointer,
        gate_scope_counts,
        interop_loss_matrix_shapes: 4,
    })
}

impl GateScopeCounts {
    fn increment(&mut self, scope: GateScope) {
        match scope {
            GateScope::AgentLaunch => self.agent_launch += 1,
            GateScope::ToolInvocation => self.tool_invocation += 1,
            GateScope::SubstrateMutation => self.substrate_mutation += 1,
        }
    }
}

fn seed_v0_1_antibodies(now: DateTime<Utc>) -> Vec<Antibody> {
    let mut antibodies = vec![
        harness_antibody(
            now,
            Some("shell"),
            None,
            Severity::Refuse,
            RefusalMode::Hard,
            "use a narrower command or ask before touching protected paths",
            None,
        ),
        harness_antibody(
            now,
            Some("cargo"),
            None,
            Severity::Warn,
            RefusalMode::Soft,
            "inspect the prior cargo failure before rerunning",
            None,
        ),
        harness_antibody(
            now,
            Some("git"),
            None,
            Severity::Info,
            RefusalMode::LogOnly,
            "record git lineage only",
            None,
        ),
        harness_antibody(
            now,
            Some("python"),
            Some("secret_access"),
            Severity::Refuse,
            RefusalMode::Hard,
            "remove secret material from the attempted script",
            None,
        ),
    ];

    for index in 0..12 {
        antibodies.push(harness_antibody(
            now,
            Some(&format!("expiry-tool-{index}")),
            None,
            Severity::Refuse,
            RefusalMode::Hard,
            "expired antibodies should stop gating proposed runs",
            Some(now - chrono::Duration::minutes(index + 1)),
        ));
    }

    for index in 0..9 {
        antibodies.push(harness_antibody(
            now,
            Some(&format!("rare-tool-{index}")),
            None,
            Severity::Warn,
            RefusalMode::Soft,
            "rare tool fixture",
            None,
        ));
    }

    antibodies
}

fn harness_antibody(
    now: DateTime<Utc>,
    tool_pattern: Option<&str>,
    error_class: Option<&str>,
    severity: Severity,
    refusal_mode: RefusalMode,
    remediation: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Antibody {
    Antibody {
        id: Uuid::new_v4(),
        signature: Signature {
            error_class: error_class.map(str::to_string),
            file_pattern: None,
            agent_role: None,
            tool_pattern: tool_pattern.map(str::to_string),
            command_pattern: None,
            scope: SignatureScope::Project,
        },
        source: AntibodySource::Manual,
        severity,
        confidence: Confidence::Solid,
        refusal_mode,
        remediation: remediation.to_string(),
        examples: vec!["v0.1 harness seed".to_string()],
        created_at: now,
        expires_at,
        hit_count: 0,
    }
}

fn seed_v0_1_eval_fixtures(now: DateTime<Utc>) -> Vec<EvaluationFixture> {
    let mut fixtures = Vec::new();
    push_repeated_fixture(
        &mut fixtures,
        "shell-refuse",
        20,
        FixtureLabel::Unsafe,
        GateScope::ToolInvocation,
        harness_run("shell", None),
        EvaluationOutcome::Refuse,
        now,
        &[],
    );
    push_repeated_fixture(
        &mut fixtures,
        "cargo-warn",
        10,
        FixtureLabel::Unsafe,
        GateScope::AgentLaunch,
        harness_run("cargo", None),
        EvaluationOutcome::Warn,
        now,
        &[],
    );
    push_repeated_fixture(
        &mut fixtures,
        "python-secret-refuse",
        5,
        FixtureLabel::Unsafe,
        GateScope::SubstrateMutation,
        harness_run("python", Some("secret_access")),
        EvaluationOutcome::Refuse,
        now,
        &[],
    );

    for tool in ["read", "apply_patch", "git", "node", "python"] {
        push_repeated_fixture(
            &mut fixtures,
            &format!("{tool}-safe"),
            5,
            FixtureLabel::Safe,
            GateScope::ToolInvocation,
            harness_run(tool, None),
            EvaluationOutcome::Allow,
            now,
            &[],
        );
    }

    for index in 0..12 {
        fixtures.push(EvaluationFixture {
            name: format!("expiry-allows-{index}"),
            label: FixtureLabel::Safe,
            gate_scope: GateScope::AgentLaunch,
            run: harness_run(&format!("expiry-tool-{index}"), None),
            expected: EvaluationOutcome::Allow,
            evaluated_at: now,
            tags: vec!["expiry".to_string()],
        });
    }

    fixtures
}

#[allow(clippy::too_many_arguments)]
fn push_repeated_fixture(
    fixtures: &mut Vec<EvaluationFixture>,
    prefix: &str,
    count: usize,
    label: FixtureLabel,
    gate_scope: GateScope,
    run: ProposedRun,
    expected: EvaluationOutcome,
    evaluated_at: DateTime<Utc>,
    tags: &[&str],
) {
    for index in 0..count {
        fixtures.push(EvaluationFixture {
            name: format!("{prefix}-{index}"),
            label,
            gate_scope,
            run: run.clone(),
            expected,
            evaluated_at,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
        });
    }
}

fn harness_run(tool_name: &str, error_class: Option<&str>) -> ProposedRun {
    ProposedRun {
        error_class: error_class.map(str::to_string),
        file_path: None,
        agent_role: None,
        tool_name: Some(tool_name.to_string()),
        command: None,
        scope: SignatureScope::Project,
    }
}

fn seed_v0_1_sentinel_events() -> String {
    [
        r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"block","reason":"blocked ssh key access","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:01:00Z","tool_name":"shell","action":"warn","reason":"outside project","matched_rule":"allow.paths: src/**","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:02:00Z","tool_name":"apply_patch","action":"allow","reason":null,"matched_rule":null,"mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:03:00Z","tool_name":"read","action":"allow","reason":"read allowed","matched_rule":"allow.tools: read","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:04:00Z","tool_name":"write","action":"block","reason":"blocked env write","matched_rule":"deny.paths: .env","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:05:00Z","tool_name":"network","action":"warn","reason":"network audit","matched_rule":"warn.tools: network","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:06:00Z","tool_name":"shell","action":"block","reason":"rm denied","matched_rule":"deny.commands: rm -rf","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:07:00Z","tool_name":"git","action":"allow","reason":"status allowed","matched_rule":"allow.commands: git status","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:08:00Z","tool_name":"python","action":"warn","reason":null,"matched_rule":"warn.tools: python","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:09:00Z","tool_name":"shell","action":"block","reason":"secret pattern","matched_rule":"deny.secrets: OPENAI_API_KEY","mode":"enforce"}"#,
    ]
    .join("\n")
}

impl EvaluationMatch {
    fn from_antibody(antibody: Antibody) -> Self {
        let outcome = EvaluationOutcome::from_policy(antibody.severity, antibody.refusal_mode);
        Self {
            antibody_id: antibody.id,
            outcome,
            severity: antibody.severity,
            refusal_mode: antibody.refusal_mode,
            remediation: antibody.remediation,
            source_pointer: format!("antibody:{}", antibody.id),
        }
    }
}

impl EvaluationOutcome {
    fn from_policy(severity: Severity, refusal_mode: RefusalMode) -> Self {
        match (severity, refusal_mode) {
            (Severity::Refuse, RefusalMode::Hard) => Self::Refuse,
            (_, RefusalMode::LogOnly) | (Severity::Info, _) => Self::Allow,
            _ => Self::Warn,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Refuse => 0,
            Self::Warn => 1,
            Self::Allow => 2,
        }
    }
}

fn is_expired(antibody: &Antibody, now: DateTime<Utc>) -> bool {
    antibody
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
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

        let (parsed_error_class, parsed_file_pattern, parsed_command_pattern) =
            parse_matched_rule(self.matched_rule.as_deref());

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
                    error_class: parsed_error_class,
                    file_pattern: parsed_file_pattern,
                    agent_role: None,
                    tool_pattern: Some(self.tool_name),
                    command_pattern: parsed_command_pattern,
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

fn parse_matched_rule(
    matched_rule: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(rule) = matched_rule else {
        return (None, None, None);
    };
    let Some((prefix, value)) = rule.split_once(": ") else {
        return (None, None, None);
    };
    let value = value.to_string();
    match prefix {
        "deny.paths" | "allow.paths" => (None, Some(value), None),
        "deny.commands" | "allow.commands" | "warn.commands" => (None, None, Some(value)),
        "deny.secrets" | "warn.secrets" => (Some(value), None, None),
        _ => (None, None, None),
    }
}

fn antibody_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Antibody> {
    let id: String = row.get(0)?;
    let scope: String = row.get(6)?;
    let source: String = row.get(7)?;
    let severity: String = row.get(8)?;
    let confidence: String = row.get(9)?;
    let refusal_mode: String = row.get(10)?;
    let examples_json: String = row.get(12)?;
    let created_at: String = row.get(13)?;
    let expires_at: Option<String> = row.get(14)?;
    let hit_count: u32 = row.get(15)?;

    Ok(Antibody {
        id: parse_uuid(&id)?,
        signature: Signature {
            error_class: row.get(1)?,
            file_pattern: row.get(2)?,
            agent_role: row.get(3)?,
            tool_pattern: row.get(4)?,
            command_pattern: row.get(5)?,
            scope: SignatureScope::parse(&scope)?,
        },
        source: AntibodySource::parse(&source)?,
        severity: Severity::parse(&severity)?,
        confidence: Confidence::parse(&confidence)?,
        refusal_mode: RefusalMode::parse(&refusal_mode)?,
        remediation: row.get(11)?,
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

fn glob_field_matches(signature_value: &Option<String>, run_value: &Option<String>) -> bool {
    match signature_value {
        Some(pattern) => match run_value {
            Some(value) => glob_matches(pattern, value),
            None => false,
        },
        None => true,
    }
}

fn command_matches(signature_value: &Option<String>, run_value: &Option<String>) -> bool {
    match signature_value {
        Some(pattern) => match run_value {
            Some(value) => value.contains(pattern.as_str()),
            None => false,
        },
        None => true,
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return value == pattern;
    }
    let regex_str = glob_to_regex(pattern);
    match Regex::new(&regex_str) {
        Ok(re) => re.is_match(value),
        Err(_) => value == pattern,
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    regex.push_str(".*");
                    i += 2;
                    if i < chars.len() && chars[i] == '/' {
                        i += 1;
                    }
                } else {
                    regex.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                regex.push_str("[^/]");
                i += 1;
            }
            '.' => {
                regex.push_str("\\.");
                i += 1;
            }
            '(' | ')' | '[' | ']' | '{' | '}' | '+' | '^' | '$' | '|' | '\\' => {
                regex.push('\\');
                regex.push(chars[i]);
                i += 1;
            }
            c => {
                regex.push(c);
                i += 1;
            }
        }
    }

    regex.push('$');
    regex
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

pub(crate) fn to_sql_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
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

fn render_substrate_projection(antibodies: &[Antibody]) -> String {
    let mut output = String::from(
        "<!-- generated by Mycel; projection only; not an input surface. do not edit. -->\n\
         # substrate\n\n\
         ## antibodies\n\n",
    );

    if antibodies.is_empty() {
        output.push_str("none\n");
        return output;
    }

    output.push_str("| id | scope | tool | command | severity | refusal mode | remediation |\n");
    output.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for antibody in antibodies {
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            antibody.id,
            antibody.signature.scope.as_str(),
            projection_value(&antibody.signature.tool_pattern),
            projection_value(&antibody.signature.command_pattern),
            antibody.severity.as_str(),
            antibody.refusal_mode.as_str(),
            escape_projection_cell(&antibody.remediation),
        ));
    }
    output
}

fn projection_value(value: &Option<String>) -> String {
    value.as_deref().unwrap_or("-").to_string()
}

fn escape_projection_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn rotated_audit_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| MycelError::InvalidAuditPath(path.to_path_buf()))?;
    let rotated_name = if let Some((stem, extension)) = file_name.rsplit_once('.') {
        format!("{stem}.1.{extension}")
    } else {
        format!("{file_name}.1")
    };
    Ok(path.with_file_name(rotated_name))
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

// ---------------------------------------------------------------------------
// Substrate: runs table (v0.2 decay-pruned context)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    // canonical variants (spec v0.2)
    Observation,
    Mutation,
    // kept for schema compat / migration
    Task,
    Eval,
    Maintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    // canonical variants (spec v0.2)
    Applied,
    Reverted,
    // kept for schema compat / migration
    Running,
    Done,
    Failed,
}

/// Typed representation of the `decay_state` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecayState {
    Retained,
    Distilled,
    Decayed,
}

impl DecayState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retained => "retained",
            Self::Distilled => "distilled",
            Self::Decayed => "decayed",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "retained" => Some(Self::Retained),
            "distilled" => Some(Self::Distilled),
            "decayed" => Some(Self::Decayed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub kind: RunKind,
    pub status: RunStatus,
    pub summary: String,
    pub confidence: Confidence,
    pub created_at: i64,
    /// Absolute unix timestamp when the record's ttl expires. None = no ttl.
    pub expires_at: Option<i64>,
    /// When true, maintenance preserves this record 100% regardless of confidence/ttl.
    pub no_compost: bool,
    /// None = live/unprocessed; Some("retained"|"distilled"|"decayed") after a maintenance pass.
    pub decay_state: Option<String>,
    /// Unix timestamp when a maintenance pass acted on this record. None otherwise.
    pub decayed_at: Option<i64>,
    /// Compressed gist set when a record is distilled. None otherwise.
    pub distilled_summary: Option<String>,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Mutation => "mutation",
            Self::Task => "task",
            Self::Eval => "eval",
            Self::Maintenance => "maintenance",
        }
    }

    pub fn parse_sql(value: &str) -> rusqlite::Result<Self> {
        match value {
            "observation" => Ok(Self::Observation),
            "mutation" => Ok(Self::Mutation),
            "task" => Ok(Self::Task),
            "eval" => Ok(Self::Eval),
            "maintenance" => Ok(Self::Maintenance),
            _ => Err(to_sql_error(MycelError::UnknownEnum {
                field: "kind",
                value: value.to_string(),
            })),
        }
    }
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Reverted => "reverted",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse_sql(value: &str) -> rusqlite::Result<Self> {
        match value {
            "applied" => Ok(Self::Applied),
            "reverted" => Ok(Self::Reverted),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            _ => Err(to_sql_error(MycelError::UnknownEnum {
                field: "status",
                value: value.to_string(),
            })),
        }
    }
}

/// Shared database handle. Runs migrations on open. All substrate types borrow from it.
pub struct Db {
    pub(crate) conn: Connection,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.run_migrations()?;
        Ok(db)
    }

    fn run_migrations(&self) -> Result<()> {
        self.conn.execute_batch(FULL_SCHEMA_SQL)?;
        Ok(())
    }
}

/// SQL for the full schema (antibodies + sentinel + runs + audit_log), idempotent.
const FULL_SCHEMA_SQL: &str = "
    CREATE TABLE IF NOT EXISTS antibodies (
        id TEXT PRIMARY KEY NOT NULL,
        error_class TEXT,
        file_pattern TEXT,
        agent_role TEXT,
        tool_pattern TEXT,
        command_pattern TEXT,
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
    CREATE INDEX IF NOT EXISTS idx_antibodies_tool_pattern ON antibodies(tool_pattern);
    CREATE INDEX IF NOT EXISTS idx_antibodies_scope ON antibodies(scope);
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
    CREATE TABLE IF NOT EXISTS runs (
        id              TEXT    PRIMARY KEY NOT NULL,
        kind            TEXT    NOT NULL,
        status          TEXT    NOT NULL,
        summary         TEXT    NOT NULL,
        confidence      TEXT    NOT NULL,
        created_at      INTEGER NOT NULL,
        expires_at      INTEGER,
        no_compost      INTEGER NOT NULL DEFAULT 0,
        decay_state     TEXT,
        decayed_at      INTEGER,
        distilled_summary TEXT
    );
    CREATE TABLE IF NOT EXISTS audit_log (
        id      TEXT    PRIMARY KEY NOT NULL,
        event   TEXT    NOT NULL,
        payload TEXT    NOT NULL,
        ts      INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS specs (
        id          TEXT    PRIMARY KEY NOT NULL,
        signature   TEXT    NOT NULL,
        spec_json   TEXT    NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_specs_signature ON specs(signature);
    CREATE TABLE IF NOT EXISTS sclerotia (
        id          TEXT    PRIMARY KEY NOT NULL,
        signature   TEXT    NOT NULL,
        record_json TEXT    NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_sclerotia_signature ON sclerotia(signature);
    CREATE TABLE IF NOT EXISTS spores (
        id          TEXT    PRIMARY KEY NOT NULL,
        signature   TEXT    NOT NULL,
        kind        TEXT    NOT NULL,
        record_json TEXT    NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_spores_signature ON spores(signature);
    CREATE INDEX IF NOT EXISTS idx_spores_kind ON spores(kind);
    PRAGMA user_version = 4;
";

/// Borrow-based access layer for the `runs` table.
pub struct Substrate<'a> {
    conn: &'a Connection,
}

impl<'a> Substrate<'a> {
    /// Create a substrate view over a shared `Db`.
    pub fn new(db: &'a Db) -> Self {
        Self { conn: &db.conn }
    }

    /// Insert a run with default decay values (expires_at NULL, no_compost false).
    /// `created_at` is set to the current unix timestamp.
    pub fn insert(
        &self,
        kind: RunKind,
        status: RunStatus,
        summary: &str,
        confidence: Confidence,
    ) -> Result<String> {
        self.insert_with_decay(kind, status, summary, confidence, None, false)
    }

    /// Insert a run with explicit TTL and preservation flag.
    /// `created_at` is auto-set to current unix timestamp.
    pub fn insert_with_decay(
        &self,
        kind: RunKind,
        status: RunStatus,
        summary: &str,
        confidence: Confidence,
        expires_at: Option<i64>,
        no_compost: bool,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now_ts = unix_now();
        self.conn.execute(
            "INSERT INTO runs (
                id, kind, status, summary, confidence, created_at,
                expires_at, no_compost, decay_state, decayed_at, distilled_summary
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL)",
            params![
                id,
                kind.as_str(),
                status.as_str(),
                summary,
                confidence.as_str(),
                now_ts,
                expires_at,
                no_compost as i64,
            ],
        )?;
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, kind, status, summary, confidence, created_at,
                        expires_at, no_compost, decay_state, decayed_at, distilled_summary
                 FROM runs WHERE id = ?1",
                params![id],
                row_to_run,
            )
            .optional()?)
    }

    pub fn list(&self) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, status, summary, confidence, created_at,
                    expires_at, no_compost, decay_state, decayed_at, distilled_summary
             FROM runs ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], row_to_run)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    }
}

/// Append-only audit log backed by the `audit_log` table.
pub struct AuditLog<'a> {
    conn: &'a Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub event: String,
    pub payload: serde_json::Value,
    pub ts: i64,
}

impl<'a> AuditLog<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { conn: &db.conn }
    }

    /// Append an event with a serializable payload; returns the new entry id.
    pub fn append<T: Serialize>(&self, event: &str, payload: &T) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let payload_json = serde_json::to_string(payload)?;
        let ts = unix_now();
        self.conn.execute(
            "INSERT INTO audit_log (id, event, payload, ts) VALUES (?1, ?2, ?3, ?4)",
            params![id, event, payload_json, ts],
        )?;
        Ok(id)
    }

    /// Return all audit entries ordered by ts.
    pub fn list(&self) -> Result<Vec<AuditEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, event, payload, ts FROM audit_log ORDER BY ts, id")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let event: String = row.get(1)?;
            let payload_str: String = row.get(2)?;
            let ts: i64 = row.get(3)?;
            Ok((id, event, payload_str, ts))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (id, event, payload_str, ts) = row?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).map_err(to_sql_error)?;
            entries.push(AuditEntry {
                id,
                event,
                payload,
                ts,
            });
        }
        Ok(entries)
    }
}

fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    let kind: String = row.get(1)?;
    let status: String = row.get(2)?;
    let confidence: String = row.get(4)?;
    let no_compost_int: i64 = row.get(7)?;
    Ok(Run {
        id: row.get(0)?,
        kind: RunKind::parse_sql(&kind)?,
        status: RunStatus::parse_sql(&status)?,
        summary: row.get(3)?,
        confidence: Confidence::parse(&confidence)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        no_compost: no_compost_int != 0,
        decay_state: row.get(8)?,
        decayed_at: row.get(9)?,
        distilled_summary: row.get(10)?,
    })
}

#[cfg(test)]
mod substrate_tests {
    use super::*;

    #[test]
    fn insert_with_decay_round_trips_fields() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let substrate = Substrate::new(&db);
        let expires = unix_now() + 3600;

        let id = substrate
            .insert_with_decay(
                RunKind::Observation,
                RunStatus::Applied,
                "test summary",
                Confidence::Solid,
                Some(expires),
                true,
            )
            .expect("insert_with_decay");

        let run = substrate.get(&id).expect("get").expect("run exists");
        assert_eq!(run.id, id);
        assert_eq!(run.kind, RunKind::Observation);
        assert_eq!(run.status, RunStatus::Applied);
        assert_eq!(run.summary, "test summary");
        assert_eq!(run.confidence, Confidence::Solid);
        assert_eq!(run.expires_at, Some(expires));
        assert!(run.no_compost, "no_compost should be true");
        assert!(run.decay_state.is_none(), "decay_state should be None");
        assert!(run.decayed_at.is_none(), "decayed_at should be None");
        assert!(
            run.distilled_summary.is_none(),
            "distilled_summary should be None"
        );
    }

    #[test]
    fn insert_defaults_decay_fields_to_none() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let substrate = Substrate::new(&db);

        let id = substrate
            .insert(
                RunKind::Eval,
                RunStatus::Done,
                "default summary",
                Confidence::Directional,
            )
            .expect("insert");

        let run = substrate.get(&id).expect("get").expect("run exists");
        assert_eq!(run.expires_at, None, "expires_at should be None");
        assert!(!run.no_compost, "no_compost should be false");
        assert!(run.decay_state.is_none(), "decay_state should be None");
        assert!(run.decayed_at.is_none(), "decayed_at should be None");
        assert!(
            run.distilled_summary.is_none(),
            "distilled_summary should be None"
        );
    }
}
