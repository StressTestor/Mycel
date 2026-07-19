//! Sclerotia (dormant work records) — v0.4 substrate ecology.
//!
//! A sclerotium is a dormant work record: a typed capsule that captures a blocked
//! task's identity, blocker, attempted paths, next command, and typed wake conditions.
//! Records survive the session; a resuming agent (human or automated) can evaluate
//! whether conditions are met before deciding to act.
//!
//! See ADR 0015 (sclerotia) and ADR 0016 (wake-condition vocabulary).

use std::collections::BTreeSet;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    selfspec::{InheritedContext, TaskIdentity},
    AntibodyStore, EvaluationOutcome, ProposedRun, SignatureScope,
};

// ---------------------------------------------------------------------------
// WakeCondition vocabulary
// ---------------------------------------------------------------------------

/// Typed, deterministically-evaluable wake conditions. See ADR 0016.
///
/// All variants are evaluated cheaply against a caller-supplied `WakeWorld`
/// (no hidden clock reads, no filesystem calls — per ADR 0008 time-injection).
/// `Manual` never auto-wakes: it models "only a human revisit unblocks this".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WakeCondition {
    /// Wake once wall-clock unix time >= `at`.
    TimeReached { at: i64 },
    /// Wake when the given path exists in the evaluation world.
    FileExists { path: String },
    /// Wake when the given path is absent from the evaluation world.
    FileAbsent { path: String },
    /// Wake when another run/spec/task signature has reached a terminal state.
    DependencyResolved { signature: String },
    /// Wake when a named flag is set in the evaluation world (manual/monitoring signal).
    SignalRaised { name: String },
    /// Always requires human revisit. `is_met` always returns `false`.
    ///
    /// Models "ready whenever a human decides to pick this up again."
    /// Automated wakeable-detection never fires `Manual` records.
    Manual,
}

impl WakeCondition {
    /// Evaluate the condition against a supplied world. Deterministic; no I/O.
    ///
    /// `Manual` always returns `false` — it requires explicit human intervention.
    pub fn is_met(&self, world: &WakeWorld) -> bool {
        match self {
            WakeCondition::TimeReached { at } => world.now >= *at,
            WakeCondition::FileExists { path } => world.existing_paths.contains(path),
            WakeCondition::FileAbsent { path } => !world.existing_paths.contains(path),
            WakeCondition::DependencyResolved { signature } => {
                world.resolved_signatures.contains(signature)
            }
            WakeCondition::SignalRaised { name } => world.raised_signals.contains(name),
            WakeCondition::Manual => false,
        }
    }
}

// ---------------------------------------------------------------------------
// WakeWorld — deterministic evaluation inputs
// ---------------------------------------------------------------------------

/// Caller-supplied evaluation context for wake conditions. No hidden state.
///
/// Build this from your runtime snapshot and pass it to `WakeCondition::is_met`
/// or `Sclerotium::is_wakeable`. No I/O happens inside the evaluation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WakeWorld {
    /// Current unix timestamp (seconds).
    pub now: i64,
    /// File paths that exist in the evaluation context.
    #[serde(default)]
    pub existing_paths: BTreeSet<String>,
    /// Task/spec/run signatures that have reached a terminal state.
    #[serde(default)]
    pub resolved_signatures: BTreeSet<String>,
    /// Named signal flags that are currently raised.
    #[serde(default)]
    pub raised_signals: BTreeSet<String>,
}

// ---------------------------------------------------------------------------
// Sclerotium record
// ---------------------------------------------------------------------------

/// A dormant work record. Captures enough context for a resuming agent to
/// understand what was blocked, what was tried, and what to do next.
///
/// Built on the v0.3 `TaskIdentity` shared primitive (metric 3 — task identity reuse).
/// See ADR 0015.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sclerotium {
    /// Shared v0.3 primitive — description + deterministic signature.
    pub task: TaskIdentity,
    /// What blocked progress.
    pub blocker: String,
    /// Paths already attempted before the record was written.
    pub attempted_paths: Vec<String>,
    /// The concrete next command a resuming agent should run.
    pub next_command: String,
    /// All conditions must be met for the record to be wakeable (AND semantics).
    /// Empty => not wakeable.
    pub wake_conditions: Vec<WakeCondition>,
    /// Confidence-tagged facts carried forward from prior context.
    pub inherited_context: Vec<InheritedContext>,
}

impl Sclerotium {
    /// Wakeable iff `wake_conditions` is non-empty AND every condition is met.
    pub fn is_wakeable(&self, world: &WakeWorld) -> bool {
        !self.wake_conditions.is_empty()
            && self.wake_conditions.iter().all(|cond| cond.is_met(world))
    }

    /// Collect all validation gaps. `Ok(())` iff fully valid.
    ///
    /// Validates: non-empty description, non-empty signature, non-empty blocker,
    /// non-empty next_command, at least one wake condition. Collect-all (no fail-fast).
    pub fn validate(&self) -> std::result::Result<(), Vec<SclerotiumValidationError>> {
        let mut errors = Vec::new();

        if self.task.description.trim().is_empty() {
            errors.push(SclerotiumValidationError::EmptyDescription);
        }
        if self.task.signature.is_empty() {
            errors.push(SclerotiumValidationError::EmptySignature);
        }
        if self.blocker.trim().is_empty() {
            errors.push(SclerotiumValidationError::EmptyBlocker);
        }
        if self.next_command.trim().is_empty() {
            errors.push(SclerotiumValidationError::EmptyNextCommand);
        }
        if self.wake_conditions.is_empty() {
            errors.push(SclerotiumValidationError::NoWakeCondition);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Errors collected by `Sclerotium::validate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SclerotiumValidationError {
    EmptyDescription,
    EmptySignature,
    EmptyBlocker,
    EmptyNextCommand,
    NoWakeCondition,
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Borrow-based persistence layer for `Sclerotium` records.
///
/// Follows the `SpecStore<'a>` pattern from v0.3: borrows `&'a Db`, uses the
/// shared connection and schema.
pub struct SclerotiumStore<'a> {
    conn: &'a rusqlite::Connection,
}

impl<'a> SclerotiumStore<'a> {
    /// Create a store view over a shared `Db`.
    pub fn new(db: &'a crate::Db) -> Self {
        Self { conn: &db.conn }
    }

    /// Validate and insert a `Sclerotium`. Returns the new record id on success.
    ///
    /// On validation failure returns `Err(MycelError::InvalidSpec(...))` summarizing
    /// all gaps. `now` is injected by the caller (ADR 0008 time-injection).
    pub fn insert(&self, s: &Sclerotium, now: i64) -> crate::Result<String> {
        if let Err(errors) = s.validate() {
            let summary = errors
                .iter()
                .map(|e| match e {
                    SclerotiumValidationError::EmptyDescription => "empty_description",
                    SclerotiumValidationError::EmptySignature => "empty_signature",
                    SclerotiumValidationError::EmptyBlocker => "empty_blocker",
                    SclerotiumValidationError::EmptyNextCommand => "empty_next_command",
                    SclerotiumValidationError::NoWakeCondition => "no_wake_condition",
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(crate::MycelError::InvalidSpec(summary));
        }

        let id = Uuid::new_v4().to_string();
        let record_json = serde_json::to_string(s)?;

        self.conn.execute(
            "INSERT INTO sclerotia (id, signature, record_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, s.task.signature, record_json, now],
        )?;

        Ok(id)
    }

    /// Return a single sclerotium by id, or `None` if not found.
    pub fn get(&self, id: &str) -> crate::Result<Option<Sclerotium>> {
        let result = self
            .conn
            .query_row(
                "SELECT record_json FROM sclerotia WHERE id = ?1",
                params![id],
                |row| {
                    let json: String = row.get(0)?;
                    Ok(json)
                },
            )
            .optional()?;

        match result {
            None => Ok(None),
            Some(json) => {
                let s: Sclerotium = serde_json::from_str(&json).map_err(crate::to_sql_error)?;
                Ok(Some(s))
            }
        }
    }

    /// Return all sclerotia ordered by `(created_at, id)`.
    pub fn list(&self) -> crate::Result<Vec<Sclerotium>> {
        let mut stmt = self
            .conn
            .prepare("SELECT record_json FROM sclerotia ORDER BY created_at, id")?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut records = Vec::new();
        for row in rows {
            let json = row?;
            let s: Sclerotium = serde_json::from_str(&json).map_err(crate::to_sql_error)?;
            records.push(s);
        }
        Ok(records)
    }

    /// Return all dormant records sharing a task signature, ordered by
    /// `(created_at, id)`. Enables cross-mechanism lookup by the shared
    /// `TaskIdentity` signature (ADR 0012).
    pub fn get_by_signature(&self, signature: &str) -> crate::Result<Vec<Sclerotium>> {
        let mut stmt = self.conn.prepare(
            "SELECT record_json FROM sclerotia WHERE signature = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(params![signature], |row| row.get::<_, String>(0))?;
        let mut records = Vec::new();
        for row in rows {
            records.push(serde_json::from_str(&row?).map_err(crate::to_sql_error)?);
        }
        Ok(records)
    }

    /// Return all sclerotia that are currently wakeable in the given world.
    ///
    /// Deserializes each record and filters by `is_wakeable(world)`.
    pub fn list_wakeable(&self, world: &WakeWorld) -> crate::Result<Vec<Sclerotium>> {
        let all = self.list()?;
        Ok(all.into_iter().filter(|s| s.is_wakeable(world)).collect())
    }
}

// ---------------------------------------------------------------------------
// Antibody-gated resume (metric 4)
// ---------------------------------------------------------------------------

/// The three-way outcome of a resume evaluation. Never auto-executes — a human
/// must confirm before any action is taken.
///
/// See ADR 0015 for the safety property.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDecision {
    /// Not wakeable yet — wake conditions not fully met.
    NotWakeable,
    /// Wakeable but the antibody evaluator refused the `next_command`.
    BlockedByAntibody,
    /// Wakeable and antibody-allowed (or warned) — a human may now manually confirm.
    ReadyForManualResume,
}

/// Evaluate whether a dormant record may be resumed. Pure — no audit side effects.
///
/// ## Safety property
/// NEVER auto-executes. Returns a decision for a human to confirm. The caller is
/// responsible for auditing the decision (e.g. via `AuditLog::append`).
///
/// ## Logic
/// 1. If `!s.is_wakeable(world)` → `NotWakeable`.
/// 2. Map `next_command` to a `ProposedRun`:
///    - `tool_name` = first whitespace-separated token of `next_command` (or the full
///      string if no whitespace).
///    - `command` = full `next_command` string.
///    - `scope` = `SignatureScope::Project`.
/// 3. `store.evaluate_run(&run, now)`.
/// 4. If outcome == `Refuse` → `BlockedByAntibody`.
/// 5. Otherwise (Warn or Allow) → `ReadyForManualResume`.
pub fn evaluate_resume(
    s: &Sclerotium,
    world: &WakeWorld,
    store: &AntibodyStore,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::Result<ResumeDecision> {
    if !s.is_wakeable(world) {
        return Ok(ResumeDecision::NotWakeable);
    }

    // A compound command (e.g. `cd /tmp && rm -rf x`) hides its dangerous tool
    // behind a leading benign one. Split on shell separators and evaluate EACH
    // sub-command's leading token as a tool_name, so a `tool_pattern` antibody
    // cannot be evaded by chaining. The full command is carried on every run so
    // `command_pattern` (substring) antibodies still match the whole line.
    for tool_name in resume_tool_names(&s.next_command) {
        let run = ProposedRun {
            error_class: None,
            file_path: None,
            agent_role: None,
            tool_name: Some(tool_name),
            command: Some(s.next_command.clone()),
            scope: SignatureScope::Project,
        };
        if store.evaluate_run(&run, now)?.outcome == EvaluationOutcome::Refuse {
            return Ok(ResumeDecision::BlockedByAntibody);
        }
    }

    Ok(ResumeDecision::ReadyForManualResume)
}

/// Extract the candidate tool names from a (possibly compound) command line.
///
/// Splits on shell control operators (`&&`, `||`, `;`, `|`) and returns the
/// leading whitespace-separated token of each non-empty segment, deduplicated in
/// first-seen order. A command with no recognised tool token falls back to the
/// whole trimmed command so the antibody evaluator still sees something.
pub(crate) fn resume_tool_names(command: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for segment in command.split(['&', '|', ';']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if let Some(token) = segment.split_whitespace().next() {
            let token = token.to_string();
            if !names.contains(&token) {
                names.push(token);
            }
        }
    }
    if names.is_empty() {
        let trimmed = command.trim();
        if !trimmed.is_empty() {
            names.push(trimmed.to_string());
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Helper: extract the rusqlite Optional trait
// ---------------------------------------------------------------------------

trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Antibody, AntibodySource, Confidence, Db, RefusalMode, Severity, Signature};
    use chrono::Utc;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn now_world() -> WakeWorld {
        WakeWorld {
            now: 1_000_000,
            existing_paths: BTreeSet::new(),
            resolved_signatures: BTreeSet::new(),
            raised_signals: BTreeSet::new(),
        }
    }

    fn valid_sclerotium(description: &str) -> Sclerotium {
        Sclerotium {
            task: TaskIdentity::new(description),
            blocker: "compilation error in module X".to_string(),
            attempted_paths: vec!["tried fixing import paths".to_string()],
            next_command: "cargo build -p mycel-core".to_string(),
            wake_conditions: vec![WakeCondition::Manual],
            inherited_context: vec![InheritedContext {
                claim: "the schema was at version 4".to_string(),
                confidence: Confidence::Solid,
                source: "run:abc-123".to_string(),
            }],
        }
    }

    fn wakeable_sclerotium(description: &str) -> Sclerotium {
        let mut s = valid_sclerotium(description);
        // TimeReached at 0 — always met for any world.now >= 0
        s.wake_conditions = vec![WakeCondition::TimeReached { at: 0 }];
        s
    }

    // ── WakeCondition: TimeReached ────────────────────────────────────────────

    #[test]
    fn time_reached_met_when_now_equals_at() {
        let cond = WakeCondition::TimeReached { at: 1000 };
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };
        assert!(cond.is_met(&world), "at == now should be met");
    }

    #[test]
    fn time_reached_met_when_now_exceeds_at() {
        let cond = WakeCondition::TimeReached { at: 999 };
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };
        assert!(cond.is_met(&world));
    }

    #[test]
    fn time_reached_not_met_when_now_is_before_at() {
        let cond = WakeCondition::TimeReached { at: 1001 };
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };
        assert!(!cond.is_met(&world), "now < at should not be met");
    }

    // ── WakeCondition: FileExists ─────────────────────────────────────────────

    #[test]
    fn file_exists_met_when_path_present() {
        let cond = WakeCondition::FileExists {
            path: "/tmp/flag".to_string(),
        };
        let mut world = WakeWorld::default();
        world.existing_paths.insert("/tmp/flag".to_string());
        assert!(cond.is_met(&world));
    }

    #[test]
    fn file_exists_not_met_when_path_absent() {
        let cond = WakeCondition::FileExists {
            path: "/tmp/flag".to_string(),
        };
        let world = WakeWorld::default();
        assert!(!cond.is_met(&world));
    }

    // ── WakeCondition: FileAbsent ─────────────────────────────────────────────

    #[test]
    fn file_absent_met_when_path_not_present() {
        let cond = WakeCondition::FileAbsent {
            path: "/tmp/lock".to_string(),
        };
        let world = WakeWorld::default();
        assert!(cond.is_met(&world));
    }

    #[test]
    fn file_absent_not_met_when_path_exists() {
        let cond = WakeCondition::FileAbsent {
            path: "/tmp/lock".to_string(),
        };
        let mut world = WakeWorld::default();
        world.existing_paths.insert("/tmp/lock".to_string());
        assert!(!cond.is_met(&world));
    }

    // ── WakeCondition: DependencyResolved ────────────────────────────────────

    #[test]
    fn dependency_resolved_met_when_signature_present() {
        let cond = WakeCondition::DependencyResolved {
            signature: "fix-the-auth-bug".to_string(),
        };
        let mut world = WakeWorld::default();
        world
            .resolved_signatures
            .insert("fix-the-auth-bug".to_string());
        assert!(cond.is_met(&world));
    }

    #[test]
    fn dependency_resolved_not_met_when_signature_absent() {
        let cond = WakeCondition::DependencyResolved {
            signature: "fix-the-auth-bug".to_string(),
        };
        let world = WakeWorld::default();
        assert!(!cond.is_met(&world));
    }

    // ── WakeCondition: SignalRaised ───────────────────────────────────────────

    #[test]
    fn signal_raised_met_when_name_in_world() {
        let cond = WakeCondition::SignalRaised {
            name: "ci_green".to_string(),
        };
        let mut world = WakeWorld::default();
        world.raised_signals.insert("ci_green".to_string());
        assert!(cond.is_met(&world));
    }

    #[test]
    fn signal_raised_not_met_when_name_absent() {
        let cond = WakeCondition::SignalRaised {
            name: "ci_green".to_string(),
        };
        let world = WakeWorld::default();
        assert!(!cond.is_met(&world));
    }

    // ── WakeCondition: Manual ─────────────────────────────────────────────────

    #[test]
    fn manual_is_never_met() {
        let cond = WakeCondition::Manual;
        // Even with everything raised, Manual is always false.
        let mut world = WakeWorld {
            now: i64::MAX,
            ..Default::default()
        };
        world.existing_paths.insert("anything".to_string());
        world.resolved_signatures.insert("sig".to_string());
        world.raised_signals.insert("flag".to_string());
        assert!(!cond.is_met(&world), "Manual must always return false");
    }

    // ── is_wakeable: AND semantics ────────────────────────────────────────────

    #[test]
    fn is_wakeable_empty_conditions_is_false() {
        let mut s = valid_sclerotium("Fix compile error");
        s.wake_conditions = vec![];
        assert!(!s.is_wakeable(&WakeWorld::default()));
    }

    #[test]
    fn is_wakeable_all_conditions_met_is_true() {
        let mut s = valid_sclerotium("Fix compile error");
        s.wake_conditions = vec![
            WakeCondition::TimeReached { at: 0 },
            WakeCondition::SignalRaised {
                name: "ready".to_string(),
            },
        ];
        let mut world = WakeWorld {
            now: 100,
            ..Default::default()
        };
        world.raised_signals.insert("ready".to_string());
        assert!(s.is_wakeable(&world));
    }

    #[test]
    fn is_wakeable_one_unmet_condition_is_false() {
        let mut s = valid_sclerotium("Fix compile error");
        s.wake_conditions = vec![
            WakeCondition::TimeReached { at: 0 },
            WakeCondition::SignalRaised {
                name: "not-raised".to_string(),
            },
        ];
        let world = WakeWorld {
            now: 100,
            ..Default::default()
        };
        assert!(
            !s.is_wakeable(&world),
            "one unmet condition blocks wakeable"
        );
    }

    // ── validate ─────────────────────────────────────────────────────────────

    #[test]
    fn validate_complete_record_ok() {
        let s = valid_sclerotium("Implement the decay engine");
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_empty_description() {
        let mut s = valid_sclerotium("something");
        s.task.description = "   ".to_string();
        let errs = s.validate().unwrap_err();
        assert!(errs.contains(&SclerotiumValidationError::EmptyDescription));
    }

    #[test]
    fn validate_empty_signature() {
        let mut s = valid_sclerotium("something");
        s.task.signature = String::new();
        let errs = s.validate().unwrap_err();
        assert!(errs.contains(&SclerotiumValidationError::EmptySignature));
    }

    #[test]
    fn validate_empty_blocker() {
        let mut s = valid_sclerotium("something");
        s.blocker = "   ".to_string();
        let errs = s.validate().unwrap_err();
        assert!(errs.contains(&SclerotiumValidationError::EmptyBlocker));
    }

    #[test]
    fn validate_empty_next_command() {
        let mut s = valid_sclerotium("something");
        s.next_command = String::new();
        let errs = s.validate().unwrap_err();
        assert!(errs.contains(&SclerotiumValidationError::EmptyNextCommand));
    }

    #[test]
    fn validate_no_wake_condition() {
        let mut s = valid_sclerotium("something");
        s.wake_conditions.clear();
        let errs = s.validate().unwrap_err();
        assert!(errs.contains(&SclerotiumValidationError::NoWakeCondition));
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let s = Sclerotium {
            task: TaskIdentity {
                description: String::new(),
                signature: String::new(),
            },
            blocker: String::new(),
            attempted_paths: vec![],
            next_command: String::new(),
            wake_conditions: vec![],
            inherited_context: vec![],
        };
        let errs = s.validate().unwrap_err();
        assert!(errs.contains(&SclerotiumValidationError::EmptyDescription));
        assert!(errs.contains(&SclerotiumValidationError::EmptySignature));
        assert!(errs.contains(&SclerotiumValidationError::EmptyBlocker));
        assert!(errs.contains(&SclerotiumValidationError::EmptyNextCommand));
        assert!(errs.contains(&SclerotiumValidationError::NoWakeCondition));
        assert_eq!(errs.len(), 5);
    }

    // ── task identity reuse (metric 3) ────────────────────────────────────────

    #[test]
    fn task_identity_signature_matches_canonicalize() {
        let description = "Fix the bug.";
        let s = valid_sclerotium(description);
        assert_eq!(
            s.task.signature,
            TaskIdentity::canonicalize(description),
            "Sclerotium signature must equal TaskIdentity::canonicalize of description"
        );
        assert_eq!(s.task.signature, "fix-the-bug");
    }

    // ── SclerotiumStore round-trip ─────────────────────────────────────────────

    #[test]
    fn store_insert_and_get_round_trips() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let store = SclerotiumStore::new(&db);

        let s = wakeable_sclerotium("Implement the decay engine");
        let id = store.insert(&s, 1000).expect("insert valid sclerotium");
        assert!(!id.is_empty());

        let restored = store.get(&id).expect("get").expect("record exists");
        assert_eq!(restored, s, "restored record must equal original");
    }

    #[test]
    fn store_insert_invalid_returns_err() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let store = SclerotiumStore::new(&db);

        let invalid = Sclerotium {
            task: TaskIdentity {
                description: String::new(),
                signature: String::new(),
            },
            blocker: String::new(),
            attempted_paths: vec![],
            next_command: String::new(),
            wake_conditions: vec![],
            inherited_context: vec![],
        };
        let result = store.insert(&invalid, 1000);
        assert!(result.is_err(), "invalid sclerotium must return Err");
    }

    #[test]
    fn store_list_returns_all_records() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let store = SclerotiumStore::new(&db);

        let a = wakeable_sclerotium("Task alpha");
        let b = wakeable_sclerotium("Task beta");
        store.insert(&a, 1000).expect("insert a");
        store.insert(&b, 2000).expect("insert b");

        let all = store.list().expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], a);
        assert_eq!(all[1], b);
    }

    #[test]
    fn open_in_memory_user_version_stays_4_after_sclerotia_table() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let version: u32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("pragma user_version");
        assert_eq!(
            version, 4,
            "user_version must remain 4 after sclerotia table DDL"
        );
    }

    // ── evaluate_resume (metric 4) ────────────────────────────────────────────

    /// Seed an AntibodyStore with one refusing antibody for tool_name == "rm".
    fn refusing_store() -> AntibodyStore {
        let store = AntibodyStore::open_in_memory().expect("open antibody store");
        let antibody = Antibody {
            id: uuid::Uuid::new_v4(),
            signature: Signature {
                error_class: None,
                file_pattern: None,
                agent_role: None,
                tool_pattern: Some("rm".to_string()),
                command_pattern: None,
                scope: SignatureScope::Project,
            },
            source: AntibodySource::Manual,
            severity: Severity::Refuse,
            confidence: Confidence::Solid,
            refusal_mode: RefusalMode::Hard,
            remediation: "do not delete files without explicit approval".to_string(),
            examples: vec!["rm -rf /".to_string()],
            created_at: Utc::now(),
            expires_at: None,
            hit_count: 0,
        };
        store
            .insert_antibody(&antibody)
            .expect("insert refusing antibody");
        store
    }

    #[test]
    fn evaluate_resume_not_wakeable_returns_not_wakeable() {
        let store = refusing_store();
        // Manual => never auto-wakes.
        let s = valid_sclerotium("Fix the test suite");
        let world = now_world();
        let decision = evaluate_resume(&s, &world, &store, Utc::now()).expect("evaluate_resume");
        assert_eq!(
            decision,
            ResumeDecision::NotWakeable,
            "Manual condition must yield NotWakeable"
        );
    }

    #[test]
    fn evaluate_resume_wakeable_refused_command_returns_blocked_by_antibody() {
        let store = refusing_store();
        // next_command starts with "rm" — the refusing antibody matches.
        let mut s = wakeable_sclerotium("Clean up temp files");
        s.next_command = "rm -rf /tmp/mycel-work".to_string();
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };
        let decision = evaluate_resume(&s, &world, &store, Utc::now()).expect("evaluate_resume");
        assert_eq!(
            decision,
            ResumeDecision::BlockedByAntibody,
            "refused next_command must yield BlockedByAntibody"
        );
    }

    #[test]
    fn evaluate_resume_wakeable_safe_command_returns_ready() {
        let store = refusing_store();
        // next_command starts with "cargo" — not matched by the rm antibody.
        let s = wakeable_sclerotium("Run the build");
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };
        let decision = evaluate_resume(&s, &world, &store, Utc::now()).expect("evaluate_resume");
        assert_eq!(
            decision,
            ResumeDecision::ReadyForManualResume,
            "safe next_command must yield ReadyForManualResume"
        );
    }

    #[test]
    fn evaluate_resume_never_auto_executes() {
        // Verify exhaustively that no path returns something that could be interpreted
        // as "execute now". The three variants are: NotWakeable, BlockedByAntibody,
        // ReadyForManualResume — none of these auto-spawn anything.
        let store = refusing_store();
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };

        let s1 = valid_sclerotium("not wakeable");
        let d1 = evaluate_resume(&s1, &world, &store, Utc::now()).unwrap();
        assert_ne!(d1, ResumeDecision::ReadyForManualResume); // is NotWakeable

        let mut s2 = wakeable_sclerotium("blocked");
        s2.next_command = "rm target/".to_string();
        let d2 = evaluate_resume(&s2, &world, &store, Utc::now()).unwrap();
        assert_eq!(d2, ResumeDecision::BlockedByAntibody);

        let s3 = wakeable_sclerotium("safe");
        let d3 = evaluate_resume(&s3, &world, &store, Utc::now()).unwrap();
        assert_eq!(d3, ResumeDecision::ReadyForManualResume);
        // ReadyForManualResume means a human must still confirm — no command ran.
    }

    #[test]
    fn resume_tool_names_splits_compound_commands() {
        assert_eq!(resume_tool_names("rm -rf /tmp/x"), vec!["rm"]);
        assert_eq!(
            resume_tool_names("cd /tmp && rm -rf x"),
            vec!["cd", "rm"],
            "every sub-command's leading token must be extracted"
        );
        assert_eq!(
            resume_tool_names("cat f | grep x ; rm y"),
            vec!["cat", "grep", "rm"]
        );
        // empty / separator-only input falls back to nothing meaningful.
        assert!(resume_tool_names("   ").is_empty());
        // a command with no separator and no whitespace is its own token.
        assert_eq!(resume_tool_names("status"), vec!["status"]);
    }

    #[test]
    fn evaluate_resume_blocks_compound_command_hiding_rm() {
        // Regression: a refusing antibody on tool `rm` must NOT be evadable by
        // chaining `rm` behind a benign leading command. Before the fix,
        // tool_name was only the first token (`cd`), so the gate was bypassed.
        let store = refusing_store(); // refuses tool_pattern "rm"
        let mut s = wakeable_sclerotium("Clean up via a chained command");
        s.next_command = "cd /tmp && rm -rf x".to_string();
        let world = WakeWorld {
            now: 1000,
            ..Default::default()
        };
        let decision = evaluate_resume(&s, &world, &store, Utc::now()).expect("evaluate_resume");
        assert_eq!(
            decision,
            ResumeDecision::BlockedByAntibody,
            "compound command hiding `rm` behind `cd` must still be blocked"
        );
    }
}
