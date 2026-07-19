//! Spores (work-discovery manifests) — v0.5 substrate ecology.
//!
//! A spore is a typed, inert manifest describing a unit of discoverable work: either
//! `CompletedWork` (something that finished and could seed follow-up) or `AdjacentWork`
//! (a noticed-but-not-done opportunity). Spores are *catalogued* — recorded and deduped —
//! and a germination *candidate* can be computed, but v0.5 NEVER launches an agent.
//!
//! Spores reuse the v0.3 `TaskIdentity` shared primitive (same signature space as
//! self-specs and sclerotia), so the same dedupe/cross-reference key applies.
//!
//! See ADR 0017 (spore manifest) and ADR 0018 (spore catalog and inert export).

use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{selfspec::TaskIdentity, Confidence};

// ---------------------------------------------------------------------------
// Spore kind
// ---------------------------------------------------------------------------

/// What sort of discovered work a spore describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SporeKind {
    /// Work that finished and may seed follow-up work.
    CompletedWork,
    /// Work noticed adjacent to the current task but not done.
    AdjacentWork,
}

impl SporeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SporeKind::CompletedWork => "completed_work",
            SporeKind::AdjacentWork => "adjacent_work",
        }
    }
}

// ---------------------------------------------------------------------------
// Spore manifest
// ---------------------------------------------------------------------------

/// A typed, inert work-discovery manifest. Catalogued, never germinated in v0.5.
///
/// Built on the v0.3 `TaskIdentity` shared primitive (same signature space as
/// self-specs and sclerotia). See ADR 0017.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spore {
    /// Shared identity primitive — description + deterministic signature.
    pub task: TaskIdentity,
    /// Whether this spore came from completed work or noticed-adjacent work.
    pub kind: SporeKind,
    /// Where the spore was discovered (source pointer, ADR 0012 format:
    /// `run:<id>`, `audit:<id>`, `spec:<sig>`, `note:<text>`).
    pub origin: String,
    /// Confidence that this is genuinely worth surfacing.
    pub confidence: Confidence,
    /// Short human-readable note on why this work is discoverable.
    pub note: String,
}

impl Spore {
    /// Collect all validation gaps. `Ok(())` iff fully valid.
    ///
    /// A spore must have a non-empty description+signature, a non-empty origin source
    /// pointer, and a non-empty note. Collect-all (no fail-fast).
    pub fn validate(&self) -> std::result::Result<(), Vec<SporeValidationError>> {
        let mut errors = Vec::new();
        if self.task.description.trim().is_empty() {
            errors.push(SporeValidationError::EmptyDescription);
        }
        if self.task.signature.is_empty() {
            errors.push(SporeValidationError::EmptySignature);
        }
        if self.origin.trim().is_empty() {
            errors.push(SporeValidationError::EmptyOrigin);
        }
        if self.note.trim().is_empty() {
            errors.push(SporeValidationError::EmptyNote);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Errors collected by `Spore::validate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SporeValidationError {
    EmptyDescription,
    EmptySignature,
    EmptyOrigin,
    EmptyNote,
}

// ---------------------------------------------------------------------------
// Adjacent-work classification
// ---------------------------------------------------------------------------

/// A raw, untyped "I noticed X" notice, before it becomes a typed spore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjacentWorkNotice {
    /// Free-text description of the noticed work.
    pub description: String,
    /// Where it was noticed (source pointer).
    pub origin: String,
    /// Optional confidence hint; defaults to `vibes` when absent (a raw notice
    /// is a hypothesis until reviewed).
    #[serde(default)]
    pub confidence: Option<Confidence>,
}

/// Classify a raw adjacent-work notice into a typed `AdjacentWork` spore.
///
/// Deterministic: the signature is `TaskIdentity::canonicalize(description)`, the kind is
/// always `AdjacentWork`, and the confidence defaults to `Vibes` (a raw notice is a
/// hypothesis) unless the notice carried one. The note records that this was a classified
/// adjacent-work observation.
pub fn classify_adjacent_work(notice: &AdjacentWorkNotice) -> Spore {
    Spore {
        task: TaskIdentity::new(&notice.description),
        kind: SporeKind::AdjacentWork,
        origin: notice.origin.clone(),
        confidence: notice.confidence.unwrap_or(Confidence::Vibes),
        note: format!("classified adjacent-work notice: {}", notice.description),
    }
}

// ---------------------------------------------------------------------------
// Dedupe
// ---------------------------------------------------------------------------

/// Collapse spores sharing a `(kind, signature)` key, keeping the FIRST occurrence
/// (stable). Returns `(unique_spores, duplicate_count)`.
///
/// Keying on `(kind, signature)` rather than signature alone means a completed-work
/// spore and an adjacent-work spore for the same task are kept distinct — they describe
/// different discoveries.
pub fn dedupe_spores(spores: Vec<Spore>) -> (Vec<Spore>, usize) {
    let mut seen = std::collections::BTreeSet::new();
    let mut unique = Vec::new();
    let mut duplicates = 0usize;
    for spore in spores {
        let key = (spore.kind.as_str(), spore.task.signature.clone());
        if seen.insert(key) {
            unique.push(spore);
        } else {
            duplicates += 1;
        }
    }
    (unique, duplicates)
}

// ---------------------------------------------------------------------------
// Germination candidate (NO germination — candidate only)
// ---------------------------------------------------------------------------

/// A germination *candidate* — a spore the catalog flags as potentially worth acting on.
///
/// v0.5 NEVER germinates (launches an agent). This is purely a surfaced suggestion for a
/// human to consider; constructing one has no side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GerminationCandidate {
    pub task: TaskIdentity,
    pub kind: SporeKind,
    pub confidence: Confidence,
    /// Always false in v0.5 — there is no germination path. Present so downstream
    /// tooling can assert the invariant explicitly.
    pub germinated: bool,
}

impl Spore {
    /// Produce a germination candidate from this spore. NEVER launches anything;
    /// `germinated` is always `false` in v0.5.
    pub fn germination_candidate(&self) -> GerminationCandidate {
        GerminationCandidate {
            task: self.task.clone(),
            kind: self.kind,
            confidence: self.confidence,
            germinated: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Inert interop export (loss-matrix shapes)
// ---------------------------------------------------------------------------

/// The four interop shapes from `docs/interop-loss-matrix.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteropShape {
    MycelNative,
    Hermes,
    OpenClaw,
    AgentSkills,
}

impl InteropShape {
    pub fn as_str(self) -> &'static str {
        match self {
            InteropShape::MycelNative => "mycel_native",
            InteropShape::Hermes => "hermes",
            InteropShape::OpenClaw => "openclaw",
            InteropShape::AgentSkills => "agentskills",
        }
    }
}

/// An exported spore as inert metadata for a foreign runtime, with the ecology fields
/// that were dropped declared explicitly (per the loss-matrix rule: exports must declare
/// lost features, not imply another runtime enforces Mycel policy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SporeExport {
    pub shape: InteropShape,
    /// The carried fields, as a flat key/value map the foreign runtime can read.
    pub fields: std::collections::BTreeMap<String, String>,
    /// Mycel ecology fields that do NOT survive this shape (recoverable only from the
    /// Mycel substrate, not from the export).
    pub dropped: Vec<String>,
    /// True only for `MycelNative` — the lossless shape.
    pub lossless: bool,
}

/// Export a spore into one of the interop loss-matrix shapes as INERT metadata.
///
/// `MycelNative` is lossless (carries the full manifest). The three foreign shapes carry a
/// degrading subset and declare the dropped ecology fields. No shape implies the foreign
/// runtime enforces Mycel policy — a spore is inert metadata everywhere but Mycel.
pub fn export_spore(spore: &Spore, shape: InteropShape) -> SporeExport {
    use std::collections::BTreeMap;
    let mut fields = BTreeMap::new();
    match shape {
        InteropShape::MycelNative => {
            fields.insert("description".to_string(), spore.task.description.clone());
            fields.insert("signature".to_string(), spore.task.signature.clone());
            fields.insert("kind".to_string(), spore.kind.as_str().to_string());
            fields.insert("origin".to_string(), spore.origin.clone());
            fields.insert(
                "confidence".to_string(),
                confidence_str(spore.confidence).to_string(),
            );
            fields.insert("note".to_string(), spore.note.clone());
            SporeExport {
                shape,
                fields,
                dropped: vec![],
                lossless: true,
            }
        }
        InteropShape::Hermes => {
            // name + notes survive; kind/origin/confidence are inert ecology.
            fields.insert("name".to_string(), spore.task.description.clone());
            fields.insert("notes".to_string(), spore.note.clone());
            SporeExport {
                shape,
                fields,
                dropped: vec![
                    "kind".to_string(),
                    "origin".to_string(),
                    "confidence".to_string(),
                    "signature".to_string(),
                ],
                lossless: false,
            }
        }
        InteropShape::OpenClaw => {
            // description + metadata note survive.
            fields.insert("description".to_string(), spore.task.description.clone());
            fields.insert("metadata".to_string(), spore.note.clone());
            SporeExport {
                shape,
                fields,
                dropped: vec![
                    "kind".to_string(),
                    "origin".to_string(),
                    "confidence".to_string(),
                    "signature".to_string(),
                ],
                lossless: false,
            }
        }
        InteropShape::AgentSkills => {
            // skill name + description survive; everything ecology-shaped degrades.
            fields.insert("name".to_string(), spore.task.description.clone());
            fields.insert("description".to_string(), spore.note.clone());
            SporeExport {
                shape,
                fields,
                dropped: vec![
                    "kind".to_string(),
                    "origin".to_string(),
                    "confidence".to_string(),
                    "signature".to_string(),
                ],
                lossless: false,
            }
        }
    }
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Solid => "solid",
        Confidence::Directional => "directional",
        Confidence::Vibes => "vibes",
    }
}

// ---------------------------------------------------------------------------
// Persistence — the local spore catalog
// ---------------------------------------------------------------------------

/// Borrow-based persistence layer for the local spore catalog.
///
/// Follows the `SpecStore`/`SclerotiumStore` pattern: borrows `&'a Db`, uses the shared
/// connection and schema. Records are stored as JSON plus indexed `signature` and `kind`
/// columns for catalog queries.
pub struct SporeStore<'a> {
    conn: &'a rusqlite::Connection,
}

impl<'a> SporeStore<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { conn: &db.conn }
    }

    /// Validate and catalog a spore. Returns the new catalog id on success.
    pub fn insert(&self, spore: &Spore, now: i64) -> crate::Result<String> {
        if let Err(errors) = spore.validate() {
            let summary = errors
                .iter()
                .map(|e| match e {
                    SporeValidationError::EmptyDescription => "empty_description",
                    SporeValidationError::EmptySignature => "empty_signature",
                    SporeValidationError::EmptyOrigin => "empty_origin",
                    SporeValidationError::EmptyNote => "empty_note",
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(crate::MycelError::InvalidSpec(summary));
        }
        let id = Uuid::new_v4().to_string();
        let record_json = serde_json::to_string(spore)?;
        self.conn.execute(
            "INSERT INTO spores (id, signature, kind, record_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                spore.task.signature,
                spore.kind.as_str(),
                record_json,
                now
            ],
        )?;
        Ok(id)
    }

    /// Catalog a batch of spores, deduping by `(kind, signature)` FIRST so the catalog
    /// never stores a repeat. Returns `(inserted_ids, duplicate_count)`.
    pub fn catalog(&self, spores: Vec<Spore>, now: i64) -> crate::Result<(Vec<String>, usize)> {
        let (unique, duplicates) = dedupe_spores(spores);
        let mut ids = Vec::new();
        for spore in &unique {
            ids.push(self.insert(spore, now)?);
        }
        Ok((ids, duplicates))
    }

    /// Return a single spore by catalog id, or `None`.
    pub fn get(&self, id: &str) -> crate::Result<Option<Spore>> {
        let result = self
            .conn
            .query_row(
                "SELECT record_json FROM spores WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match result {
            None => Ok(None),
            Some(json) => Ok(Some(
                serde_json::from_str(&json).map_err(crate::to_sql_error)?,
            )),
        }
    }

    /// Return all catalogued spores ordered by `(created_at, id)`.
    pub fn list(&self) -> crate::Result<Vec<Spore>> {
        let mut stmt = self
            .conn
            .prepare("SELECT record_json FROM spores ORDER BY created_at, id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut spores = Vec::new();
        for row in rows {
            spores.push(serde_json::from_str(&row?).map_err(crate::to_sql_error)?);
        }
        Ok(spores)
    }

    /// Return all catalogued spores sharing a task signature, ordered by
    /// `(created_at, id)`. Enables cross-mechanism lookup by the shared
    /// `TaskIdentity` signature (ADR 0012).
    pub fn get_by_signature(&self, signature: &str) -> crate::Result<Vec<Spore>> {
        let mut stmt = self.conn.prepare(
            "SELECT record_json FROM spores WHERE signature = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(params![signature], |row| row.get::<_, String>(0))?;
        let mut spores = Vec::new();
        for row in rows {
            spores.push(serde_json::from_str(&row?).map_err(crate::to_sql_error)?);
        }
        Ok(spores)
    }

    /// Count catalogued spores.
    pub fn count(&self) -> crate::Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM spores", [], |row| row.get(0))?;
        Ok(n as usize)
    }
}

// rusqlite's `.optional()` lives on a trait; provide it locally to avoid importing the
// extension trait into the public surface.
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
    use crate::Db;

    fn spore(desc: &str, kind: SporeKind) -> Spore {
        Spore {
            task: TaskIdentity::new(desc),
            kind,
            origin: "run:abc-123".to_string(),
            confidence: Confidence::Directional,
            note: "discovered during the v0.4 sclerotia work".to_string(),
        }
    }

    // ── kind ──────────────────────────────────────────────────────────────────

    #[test]
    fn spore_kind_as_str() {
        assert_eq!(SporeKind::CompletedWork.as_str(), "completed_work");
        assert_eq!(SporeKind::AdjacentWork.as_str(), "adjacent_work");
    }

    // ── validation ──────────────────────────────────────────────────────────────

    #[test]
    fn validate_complete_spore_ok() {
        assert!(spore("Add a spore catalog", SporeKind::CompletedWork)
            .validate()
            .is_ok());
    }

    #[test]
    fn validate_empty_description() {
        let mut s = spore("x", SporeKind::CompletedWork);
        s.task.description = "  ".to_string();
        assert!(s
            .validate()
            .unwrap_err()
            .contains(&SporeValidationError::EmptyDescription));
    }

    #[test]
    fn validate_empty_origin() {
        let mut s = spore("x", SporeKind::CompletedWork);
        s.origin = String::new();
        assert!(s
            .validate()
            .unwrap_err()
            .contains(&SporeValidationError::EmptyOrigin));
    }

    #[test]
    fn validate_collects_multiple_errors() {
        let s = Spore {
            task: TaskIdentity {
                description: String::new(),
                signature: String::new(),
            },
            kind: SporeKind::AdjacentWork,
            origin: String::new(),
            confidence: Confidence::Vibes,
            note: String::new(),
        };
        let errs = s.validate().unwrap_err();
        assert_eq!(errs.len(), 4);
    }

    // ── classify ──────────────────────────────────────────────────────────────

    #[test]
    fn classify_adjacent_work_produces_adjacent_spore() {
        let notice = AdjacentWorkNotice {
            description: "Refactor the projection renderer.".to_string(),
            origin: "note:saw-it-during-review".to_string(),
            confidence: None,
        };
        let s = classify_adjacent_work(&notice);
        assert_eq!(s.kind, SporeKind::AdjacentWork);
        assert_eq!(
            s.confidence,
            Confidence::Vibes,
            "raw notice defaults to vibes"
        );
        assert_eq!(
            s.task.signature,
            TaskIdentity::canonicalize(&notice.description)
        );
        assert!(s.validate().is_ok());
    }

    #[test]
    fn classify_adjacent_work_keeps_supplied_confidence() {
        let notice = AdjacentWorkNotice {
            description: "Add an index".to_string(),
            origin: "run:x".to_string(),
            confidence: Some(Confidence::Directional),
        };
        assert_eq!(
            classify_adjacent_work(&notice).confidence,
            Confidence::Directional
        );
    }

    // ── dedupe ──────────────────────────────────────────────────────────────────

    #[test]
    fn dedupe_collapses_same_kind_and_signature() {
        let spores = vec![
            spore("Add a spore catalog", SporeKind::CompletedWork),
            spore("add a spore catalog", SporeKind::CompletedWork), // canonicalizes the same
            spore("Add a spore catalog", SporeKind::CompletedWork),
        ];
        let (unique, dupes) = dedupe_spores(spores);
        assert_eq!(unique.len(), 1);
        assert_eq!(dupes, 2);
    }

    #[test]
    fn dedupe_keeps_different_kinds_for_same_signature() {
        let spores = vec![
            spore("Same task", SporeKind::CompletedWork),
            spore("Same task", SporeKind::AdjacentWork),
        ];
        let (unique, dupes) = dedupe_spores(spores);
        assert_eq!(unique.len(), 2, "different kinds are distinct discoveries");
        assert_eq!(dupes, 0);
    }

    // ── germination candidate (no germination) ───────────────────────────────────

    #[test]
    fn germination_candidate_never_germinates() {
        let c = spore("Catalog spores", SporeKind::CompletedWork).germination_candidate();
        assert!(!c.germinated, "v0.5 must never germinate");
    }

    // ── export ──────────────────────────────────────────────────────────────────

    #[test]
    fn export_mycel_native_is_lossless() {
        let e = export_spore(
            &spore("X work", SporeKind::CompletedWork),
            InteropShape::MycelNative,
        );
        assert!(e.lossless);
        assert!(e.dropped.is_empty());
        assert!(e.fields.contains_key("signature"));
        assert!(e.fields.contains_key("confidence"));
    }

    #[test]
    fn export_foreign_shapes_declare_dropped_fields() {
        for shape in [
            InteropShape::Hermes,
            InteropShape::OpenClaw,
            InteropShape::AgentSkills,
        ] {
            let e = export_spore(&spore("X work", SporeKind::AdjacentWork), shape);
            assert!(!e.lossless, "{:?} must not be lossless", shape);
            assert!(
                !e.dropped.is_empty(),
                "{:?} must declare dropped ecology fields",
                shape
            );
            // confidence is an ecology field that must NOT silently survive as enforced.
            assert!(
                !e.fields.contains_key("confidence"),
                "{:?} must not carry confidence as a live field",
                shape
            );
        }
    }

    // ── store ──────────────────────────────────────────────────────────────────

    #[test]
    fn store_insert_get_round_trip() {
        let db = Db::open_in_memory().expect("db");
        let store = SporeStore::new(&db);
        let s = spore("Persist a spore", SporeKind::CompletedWork);
        let id = store.insert(&s, 1000).expect("insert");
        assert_eq!(store.get(&id).expect("get").expect("exists"), s);
    }

    #[test]
    fn store_insert_invalid_returns_err() {
        let db = Db::open_in_memory().expect("db");
        let store = SporeStore::new(&db);
        let mut s = spore("x", SporeKind::CompletedWork);
        s.origin = String::new();
        assert!(store.insert(&s, 1000).is_err());
    }

    #[test]
    fn store_catalog_dedupes_before_storing() {
        let db = Db::open_in_memory().expect("db");
        let store = SporeStore::new(&db);
        let spores = vec![
            spore("Task one", SporeKind::CompletedWork),
            spore("task one", SporeKind::CompletedWork), // dup by canonicalization
            spore("Task two", SporeKind::AdjacentWork),
        ];
        let (ids, dupes) = store.catalog(spores, 1000).expect("catalog");
        assert_eq!(ids.len(), 2);
        assert_eq!(dupes, 1);
        assert_eq!(store.count().expect("count"), 2);
    }

    #[test]
    fn open_in_memory_user_version_stays_4_after_spores_table() {
        let db = Db::open_in_memory().expect("db");
        let v: u32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(v, 4, "user_version must stay 4 after spores table DDL");
    }
}
