//! Self-spec on death: structured task intent records.
//!
//! See ADR 0012 (task identity) and ADR 0013 (self-spec schema).

use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Confidence;

// ---------------------------------------------------------------------------
// TaskIdentity
// ---------------------------------------------------------------------------

/// Stable identity header for a task. Shared primitive for v0.3 self-specs,
/// v0.4 sclerotia, and v0.5 spores. See ADR 0012.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIdentity {
    pub description: String,
    pub signature: String,
}

impl TaskIdentity {
    /// Construct a `TaskIdentity`; `signature` is derived deterministically from `description`.
    pub fn new(description: &str) -> Self {
        Self {
            description: description.to_string(),
            signature: Self::canonicalize(description),
        }
    }

    /// Deterministic canonical dedupe key. See ADR 0012 for the exact rule.
    ///
    /// Steps (applied in order):
    /// 1. lowercase
    /// 2. split on whitespace and rejoin with a single space (trims + collapses internal runs)
    /// 3. strip trailing punctuation from the set `.!?,;:`
    /// 4. replace remaining spaces with `-`
    pub fn canonicalize(description: &str) -> String {
        // Step 1 + 2: lowercase, trim, collapse internal whitespace.
        let normalized = description
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(" ");
        // Step 3: strip trailing punctuation run from the defined set.
        let stripped = normalized.trim_end_matches(|c: char| ".!?,;:".contains(c));
        // Step 4: replace spaces with hyphens.
        stripped.replace(' ', "-")
    }
}

// ---------------------------------------------------------------------------
// InheritedContext
// ---------------------------------------------------------------------------

/// A confidence-tagged fact carried forward into a self-spec. See ADR 0012.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedContext {
    pub claim: String,
    pub confidence: Confidence,
    /// Source pointer in `run:<id>`, `audit:<id>`, `spec:<sig>`, or `note:<text>` format.
    pub source: String,
}

// ---------------------------------------------------------------------------
// SelfSpec
// ---------------------------------------------------------------------------

/// Full structured self-spec record. See ADR 0013.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfSpec {
    pub task: TaskIdentity,
    pub preconditions: Vec<String>,
    pub success_criteria: Vec<String>,
    pub inherited_context: Vec<InheritedContext>,
    pub refusal_risks: Vec<String>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Error variants collected by `SelfSpec::validate`. See ADR 0013.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpecValidationError {
    EmptyDescription,
    EmptySignature,
    MissingPreconditions,
    MissingSuccessCriteria,
    MissingInheritedContext,
    MissingRefusalRisks,
    InheritedContextMissingSource,
}

impl SpecValidationError {
    /// String key used in fixtures and error summaries.
    pub fn as_reason(&self) -> &'static str {
        match self {
            Self::EmptyDescription => "empty_description",
            Self::EmptySignature => "empty_signature",
            Self::MissingPreconditions => "missing_preconditions",
            Self::MissingSuccessCriteria => "missing_success_criteria",
            Self::MissingInheritedContext => "missing_inherited_context",
            Self::MissingRefusalRisks => "missing_refusal_risks",
            Self::InheritedContextMissingSource => "inherited_context_missing_source",
        }
    }
}

impl SelfSpec {
    /// Collect ALL validation gaps (do not fail-fast). `Ok(())` iff fully valid.
    pub fn validate(&self) -> std::result::Result<(), Vec<SpecValidationError>> {
        let mut errors = Vec::new();

        if self.task.description.trim().is_empty() {
            errors.push(SpecValidationError::EmptyDescription);
        }
        if self.task.signature.is_empty() {
            errors.push(SpecValidationError::EmptySignature);
        }
        if self.preconditions.is_empty() {
            errors.push(SpecValidationError::MissingPreconditions);
        }
        if self.success_criteria.is_empty() {
            errors.push(SpecValidationError::MissingSuccessCriteria);
        }
        if self.inherited_context.is_empty() {
            errors.push(SpecValidationError::MissingInheritedContext);
        }
        if self.refusal_risks.is_empty() {
            errors.push(SpecValidationError::MissingRefusalRisks);
        }
        // Raise at most once even if multiple items have empty sources.
        if self
            .inherited_context
            .iter()
            .any(|ctx| ctx.source.trim().is_empty())
        {
            errors.push(SpecValidationError::InheritedContextMissingSource);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

// ---------------------------------------------------------------------------
// Dedupe
// ---------------------------------------------------------------------------

/// Collapse specs sharing a task signature, keeping the FIRST occurrence (stable).
/// Returns `(unique_specs, duplicate_count)`.
pub fn dedupe_specs(specs: Vec<SelfSpec>) -> (Vec<SelfSpec>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    let total = specs.len();

    for spec in specs {
        if seen.insert(spec.task.signature.clone()) {
            unique.push(spec);
        }
    }

    let duplicate_count = total - unique.len();
    (unique, duplicate_count)
}

// ---------------------------------------------------------------------------
// SpecStore
// ---------------------------------------------------------------------------

/// Persistence layer for `SelfSpec` records. Borrows from `Db`. See ADR 0013.
pub struct SpecStore<'a> {
    conn: &'a rusqlite::Connection,
}

impl<'a> SpecStore<'a> {
    /// Create a spec store view over a shared `Db`. Mirrors the `Substrate::new` pattern.
    pub fn new(db: &'a crate::Db) -> Self {
        Self { conn: &db.conn }
    }

    /// Validate and insert a `SelfSpec`. Returns the new record id on success.
    ///
    /// On validation failure returns `Err(MycelError::InvalidSpec(...))` summarizing all gaps.
    /// `now` is the unix timestamp to store as `created_at` (ADR 0008 time-injection).
    pub fn insert(&self, spec: &SelfSpec, now: i64) -> crate::Result<String> {
        if let Err(errors) = spec.validate() {
            let summary = errors
                .iter()
                .map(|e| e.as_reason())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(crate::MycelError::InvalidSpec(summary));
        }

        let id = Uuid::new_v4().to_string();
        let spec_json = serde_json::to_string(spec)?;

        self.conn.execute(
            "INSERT INTO specs (id, signature, spec_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, spec.task.signature, spec_json, now],
        )?;

        Ok(id)
    }

    /// Return all specs with the given task signature, deserialized from JSON.
    pub fn get_by_signature(&self, signature: &str) -> crate::Result<Vec<SelfSpec>> {
        let mut stmt = self
            .conn
            .prepare("SELECT spec_json FROM specs WHERE signature = ?1 ORDER BY created_at, id")?;
        let rows = stmt.query_map(params![signature], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut specs = Vec::new();
        for row in rows {
            let json = row?;
            let spec: SelfSpec = serde_json::from_str(&json).map_err(crate::to_sql_error)?;
            specs.push(spec);
        }
        Ok(specs)
    }

    /// Return all specs ordered by `(created_at, id)`.
    pub fn list(&self) -> crate::Result<Vec<SelfSpec>> {
        let mut stmt = self
            .conn
            .prepare("SELECT spec_json FROM specs ORDER BY created_at, id")?;
        let rows = stmt.query_map([], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        let mut specs = Vec::new();
        for row in rows {
            let json = row?;
            let spec: SelfSpec = serde_json::from_str(&json).map_err(crate::to_sql_error)?;
            specs.push(spec);
        }
        Ok(specs)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn valid_spec(description: &str) -> SelfSpec {
        SelfSpec {
            task: TaskIdentity::new(description),
            preconditions: vec!["the db is reachable".to_string()],
            success_criteria: vec!["all tests pass".to_string()],
            inherited_context: vec![InheritedContext {
                claim: "the schema was at version 4".to_string(),
                confidence: Confidence::Solid,
                source: "run:abc-123".to_string(),
            }],
            refusal_risks: vec!["the table might not exist".to_string()],
        }
    }

    // ── canonicalize ─────────────────────────────────────────────────────────

    #[test]
    fn canonicalize_normalizes_case_whitespace_trailing_punct() {
        let sig1 = TaskIdentity::canonicalize("Fix the bug.");
        let sig2 = TaskIdentity::canonicalize("  fix   the BUG  ");
        let sig3 = TaskIdentity::canonicalize("Fix the bug");

        assert_eq!(sig1, "fix-the-bug");
        assert_eq!(sig2, "fix-the-bug");
        assert_eq!(sig3, "fix-the-bug");
    }

    #[test]
    fn canonicalize_different_descriptions_yield_different_signatures() {
        let sig_a = TaskIdentity::canonicalize("Fix the bug.");
        let sig_b = TaskIdentity::canonicalize("Refactor the auth module");
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn canonicalize_strips_run_of_trailing_punct() {
        let sig = TaskIdentity::canonicalize("Done!?");
        assert_eq!(sig, "done");
    }

    #[test]
    fn canonicalize_empty_string() {
        assert_eq!(TaskIdentity::canonicalize(""), "");
    }

    // ── validate ─────────────────────────────────────────────────────────────

    #[test]
    fn validate_complete_spec_is_ok() {
        let spec = valid_spec("Write the self-spec module");
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validate_empty_description() {
        let mut spec = valid_spec("something");
        spec.task.description = "   ".to_string();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::EmptyDescription));
    }

    #[test]
    fn validate_empty_signature() {
        let mut spec = valid_spec("something");
        spec.task.signature = String::new();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::EmptySignature));
    }

    #[test]
    fn validate_missing_preconditions() {
        let mut spec = valid_spec("something");
        spec.preconditions.clear();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::MissingPreconditions));
    }

    #[test]
    fn validate_missing_success_criteria() {
        let mut spec = valid_spec("something");
        spec.success_criteria.clear();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::MissingSuccessCriteria));
    }

    #[test]
    fn validate_missing_inherited_context() {
        let mut spec = valid_spec("something");
        spec.inherited_context.clear();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::MissingInheritedContext));
    }

    #[test]
    fn validate_missing_refusal_risks() {
        let mut spec = valid_spec("something");
        spec.refusal_risks.clear();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::MissingRefusalRisks));
    }

    #[test]
    fn validate_inherited_context_missing_source() {
        let mut spec = valid_spec("something");
        spec.inherited_context[0].source = "  ".to_string();
        let errs = spec.validate().unwrap_err();
        assert!(errs.contains(&SpecValidationError::InheritedContextMissingSource));
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let spec = SelfSpec {
            task: TaskIdentity {
                description: String::new(),
                signature: String::new(),
            },
            preconditions: vec![],
            success_criteria: vec![],
            inherited_context: vec![],
            refusal_risks: vec![],
        };
        let errs = spec.validate().unwrap_err();
        // All 6 independent variants should fire (no inherited_context means
        // InheritedContextMissingSource can't fire — no items to check).
        assert!(errs.contains(&SpecValidationError::EmptyDescription));
        assert!(errs.contains(&SpecValidationError::EmptySignature));
        assert!(errs.contains(&SpecValidationError::MissingPreconditions));
        assert!(errs.contains(&SpecValidationError::MissingSuccessCriteria));
        assert!(errs.contains(&SpecValidationError::MissingInheritedContext));
        assert!(errs.contains(&SpecValidationError::MissingRefusalRisks));
        assert_eq!(errs.len(), 6);
    }

    #[test]
    fn validate_empty_context_does_not_raise_missing_source() {
        let mut spec = valid_spec("something");
        spec.inherited_context.clear();
        let errs = spec.validate().unwrap_err();
        assert!(!errs.contains(&SpecValidationError::InheritedContextMissingSource));
        assert!(errs.contains(&SpecValidationError::MissingInheritedContext));
    }

    // ── dedupe ───────────────────────────────────────────────────────────────

    #[test]
    fn dedupe_collapses_duplicates_keeps_first() {
        let a = valid_spec("Fix the bug.");
        let b = valid_spec("  fix   the BUG  "); // same signature
        let c = valid_spec("Refactor the auth module");

        let first_description = a.task.description.clone();
        let (unique, dupes) = dedupe_specs(vec![a, b, c]);

        assert_eq!(unique.len(), 2);
        assert_eq!(dupes, 1);
        assert_eq!(unique[0].task.description, first_description);
    }

    #[test]
    fn dedupe_no_duplicates_returns_all() {
        let a = valid_spec("Task alpha");
        let b = valid_spec("Task beta");
        let c = valid_spec("Task gamma");
        let (unique, dupes) = dedupe_specs(vec![a, b, c]);
        assert_eq!(unique.len(), 3);
        assert_eq!(dupes, 0);
    }

    // ── SpecStore round-trip ─────────────────────────────────────────────────

    #[test]
    fn spec_store_insert_and_get_by_signature() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let store = SpecStore::new(&db);

        let spec = valid_spec("Write the self-spec module");
        let id = store.insert(&spec, 1000).expect("insert valid spec");
        assert!(!id.is_empty());

        let results = store
            .get_by_signature(&spec.task.signature)
            .expect("get_by_signature");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], spec);
    }

    #[test]
    fn spec_store_insert_invalid_returns_err() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let store = SpecStore::new(&db);

        let invalid = SelfSpec {
            task: TaskIdentity {
                description: String::new(),
                signature: String::new(),
            },
            preconditions: vec![],
            success_criteria: vec![],
            inherited_context: vec![],
            refusal_risks: vec![],
        };
        let result = store.insert(&invalid, 1000);
        assert!(
            result.is_err(),
            "inserting an invalid spec should return Err"
        );
    }

    #[test]
    fn spec_store_list_round_trips_multiple() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let store = SpecStore::new(&db);

        let a = valid_spec("Task alpha");
        let b = valid_spec("Task beta");
        store.insert(&a, 1000).expect("insert a");
        store.insert(&b, 2000).expect("insert b");

        let all = store.list().expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], a);
        assert_eq!(all[1], b);
    }

    #[test]
    fn open_in_memory_user_version_stays_4() {
        let db = Db::open_in_memory().expect("open in-memory db");
        let version: u32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("pragma user_version");
        assert_eq!(
            version, 4,
            "user_version must remain 4 after specs table DDL"
        );
    }
}
