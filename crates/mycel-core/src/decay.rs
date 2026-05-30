//! TTL-tiered decay engine for the run substrate.
//!
//! `DecayEngine::run(now)` is deterministic and idempotent:
//! rows that already have a `decay_state` are skipped, so calling
//! `run` twice at different timestamps produces no additional mutations.

use rusqlite::params;
use serde::Serialize;

use crate::{AuditLog, Confidence, Db, DecayState, Substrate};

// ── constants ─────────────────────────────────────────────────────────────────

const DISTILL_MAX_CHARS: usize = 80;

// ── public types ──────────────────────────────────────────────────────────────

/// Per-run ids grouped by the outcome of a single `DecayEngine::run` call.
#[derive(Debug, Default, Clone)]
pub struct DecayReport {
    /// Runs transitioned to `retained` (Solid + expired).
    pub retained: Vec<String>,
    /// Runs transitioned to `distilled` (Directional + expired).
    pub distilled: Vec<String>,
    /// Runs transitioned to `decayed` (Vibes + expired).
    pub decayed: Vec<String>,
    /// Rows with `no_compost = true` — left untouched.
    pub preserved: Vec<String>,
    /// Rows whose TTL has not expired yet (or have no TTL) — left untouched.
    pub skipped_live: Vec<String>,
}

/// Apply the TTL-tiered decay policy to the substrate database.
pub struct DecayEngine<'a> {
    db: &'a Db,
}

impl<'a> DecayEngine<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// Process all runs as of unix timestamp `now`.
    ///
    /// Idempotent: rows that already have a `decay_state` are skipped.
    pub fn run(&self, now: i64) -> rusqlite::Result<DecayReport> {
        let runs = Substrate::new(self.db)
            .list()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

        let mut report = DecayReport::default();

        for run in runs {
            // Already processed — idempotency guard.
            if run.decay_state.is_some() {
                continue;
            }

            // no_compost rows are never decayed.
            if run.no_compost {
                report.preserved.push(run.id);
                continue;
            }

            // Not yet expired (or no TTL set).
            let expired = match run.expires_at {
                Some(exp) => exp <= now,
                None => false,
            };
            if !expired {
                report.skipped_live.push(run.id);
                continue;
            }

            // Expired and compostable — apply tiered policy.
            match run.confidence {
                Confidence::Solid => {
                    self.db.conn.execute(
                        "UPDATE runs SET decay_state = 'retained', decayed_at = ?1 WHERE id = ?2",
                        params![now, run.id],
                    )?;
                    audit_decay(self.db, &run.id, DecayState::Retained, run.confidence, now);
                    report.retained.push(run.id);
                }
                Confidence::Directional => {
                    let distilled = distill(&run.summary);
                    self.db.conn.execute(
                        "UPDATE runs SET decay_state = 'distilled', distilled_summary = ?1, decayed_at = ?2 WHERE id = ?3",
                        params![distilled, now, run.id],
                    )?;
                    audit_decay(self.db, &run.id, DecayState::Distilled, run.confidence, now);
                    report.distilled.push(run.id);
                }
                Confidence::Vibes => {
                    self.db.conn.execute(
                        "UPDATE runs SET decay_state = 'decayed', decayed_at = ?1 WHERE id = ?2",
                        params![now, run.id],
                    )?;
                    audit_decay(self.db, &run.id, DecayState::Decayed, run.confidence, now);
                    report.decayed.push(run.id);
                }
            }
        }

        Ok(report)
    }
}

// ── distill ───────────────────────────────────────────────────────────────────

/// Collapse whitespace and truncate to `DISTILL_MAX_CHARS` chars, cutting at
/// the last space boundary and appending `'…'` when the summary is long.
pub(crate) fn distill(summary: &str) -> String {
    let collapsed: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= DISTILL_MAX_CHARS {
        return collapsed;
    }
    let truncated = &collapsed[..DISTILL_MAX_CHARS];
    let cut = truncated.rfind(' ').unwrap_or(DISTILL_MAX_CHARS);
    let mut result = collapsed[..cut].trim_end().to_string();
    result.push('…');
    result
}

// ── audit helper ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct DecayAuditPayload<'a> {
    run_id: &'a str,
    action: &'static str,
    confidence: &'static str,
    now: i64,
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Solid => "solid",
        Confidence::Directional => "directional",
        Confidence::Vibes => "vibes",
    }
}

/// Fire-and-forget audit append. Silently drops errors rather than unwinding
/// the decay transaction — the audit log is best-effort.
fn audit_decay(db: &Db, run_id: &str, state: DecayState, confidence: Confidence, now: i64) {
    let payload = DecayAuditPayload {
        run_id,
        action: state.as_str(),
        confidence: confidence_str(confidence),
        now,
    };
    let _ = AuditLog::new(db).append("decay", &payload);
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Confidence, Db, RunKind, RunStatus, Substrate};

    fn test_db() -> Db {
        Db::open_in_memory().expect("open in-memory db")
    }

    // ── distill unit tests ───────────────────────────────────────────────────

    #[test]
    fn distill_short_passes_through() {
        let short = "hello world";
        assert_eq!(distill(short), short);
    }

    #[test]
    fn distill_long_is_truncated_ends_with_ellipsis() {
        // > 80 chars
        let long = "the quick brown fox jumps over the lazy dog and then some extra words to push it past eighty characters total";
        let result = distill(long);
        assert!(result.len() < long.len(), "should be shorter than input");
        assert!(result.ends_with('…'), "should end with ellipsis");
    }

    #[test]
    fn distill_exactly_80_chars_passes_through() {
        // Exactly 80 ASCII chars
        let s = "a".repeat(80);
        assert_eq!(distill(&s), s);
    }

    #[test]
    fn distill_collapses_whitespace() {
        let messy = "  hello   world  ";
        assert_eq!(distill(messy), "hello world");
    }

    // ── migration test ───────────────────────────────────────────────────────

    #[test]
    fn schema_version_is_4_after_migration() {
        let db = test_db();
        let version: u32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("query user_version");
        assert_eq!(version, 4, "schema version must be 4 after migration");
    }

    #[test]
    fn runs_table_has_decay_columns() {
        let db = test_db();
        let mut stmt = db.conn.prepare("PRAGMA table_info(runs)").expect("prepare");
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query")
            .filter_map(|r| r.ok())
            .collect();

        for col in &[
            "expires_at",
            "no_compost",
            "decay_state",
            "decayed_at",
            "distilled_summary",
        ] {
            assert!(
                columns.contains(&col.to_string()),
                "runs table must have column `{col}`"
            );
        }
    }

    // ── idempotency test ─────────────────────────────────────────────────────

    #[test]
    fn decay_is_idempotent_for_vibes_run() {
        let db = test_db();
        let sub = Substrate::new(&db);

        let id = sub
            .insert_with_decay(
                RunKind::Observation,
                RunStatus::Applied,
                "noisy vibes run",
                Confidence::Vibes,
                Some(500),
                false,
            )
            .expect("insert");

        let engine = DecayEngine::new(&db);

        // First pass — should decay it.
        let r1 = engine.run(1000).expect("first run");
        assert_eq!(r1.decayed.len(), 1);
        assert_eq!(r1.retained.len(), 0);
        assert_eq!(r1.distilled.len(), 0);

        // Second pass at a later timestamp — nothing new to process.
        let r2 = engine.run(2000).expect("second run");
        assert_eq!(r2.decayed.len(), 0, "second run must be idempotent");
        assert_eq!(r2.retained.len(), 0);
        assert_eq!(r2.distilled.len(), 0);

        // The row's decay_state is still decayed.
        let run = sub.get(&id).expect("get").expect("must exist");
        assert_eq!(run.decay_state.as_deref(), Some("decayed"));
    }

    // ── audit test ───────────────────────────────────────────────────────────

    #[test]
    fn decay_appends_audit_event_for_directional_run() {
        let db = test_db();
        let sub = Substrate::new(&db);

        let id = sub
            .insert_with_decay(
                RunKind::Observation,
                RunStatus::Applied,
                "a directional run with a meaningful summary",
                Confidence::Directional,
                Some(500),
                false,
            )
            .expect("insert");

        DecayEngine::new(&db).run(1000).expect("run engine");

        let log = crate::AuditLog::new(&db).list().expect("list audit log");
        let decay_events: Vec<_> = log.iter().filter(|e| e.event == "decay").collect();
        assert!(
            !decay_events.is_empty(),
            "audit log must contain a decay event"
        );

        let payload_str = decay_events[0].payload.to_string();
        assert!(
            payload_str.contains(&id),
            "audit payload must reference the run id"
        );
    }

    // ── distilled_summary invariant ──────────────────────────────────────────

    #[test]
    fn distilled_rows_have_distilled_summary_decayed_rows_do_not() {
        let db = test_db();
        let sub = Substrate::new(&db);

        let dir_id = sub
            .insert_with_decay(
                RunKind::Observation,
                RunStatus::Applied,
                "directional summary",
                Confidence::Directional,
                Some(100),
                false,
            )
            .expect("insert directional");
        let vibes_id = sub
            .insert_with_decay(
                RunKind::Observation,
                RunStatus::Applied,
                "vibes summary",
                Confidence::Vibes,
                Some(100),
                false,
            )
            .expect("insert vibes");

        DecayEngine::new(&db).run(200).expect("run engine");

        let dir_run = sub.get(&dir_id).expect("get").expect("must exist");
        assert_eq!(dir_run.decay_state.as_deref(), Some("distilled"));
        assert!(
            dir_run.distilled_summary.is_some(),
            "distilled rows must have distilled_summary"
        );

        let vibes_run = sub.get(&vibes_id).expect("get").expect("must exist");
        assert_eq!(vibes_run.decay_state.as_deref(), Some("decayed"));
        assert!(
            vibes_run.distilled_summary.is_none(),
            "decayed rows must NOT have distilled_summary"
        );
    }
}
