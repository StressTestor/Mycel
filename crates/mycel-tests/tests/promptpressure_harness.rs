//! Fixture-driven import harness for the PromptPressure tier import pipeline.
//!
//! Loads `tests/fixtures/promptpressure_records.jsonl`, imports the batch
//! into a fresh in-memory database, and asserts the v0.2 roadmap metrics:
//! - total imported >= 20
//! - zero label loss: every record's original tier string survives in the
//!   audit log under event `"promptpressure_import"`
//! - each run has the confidence that matches its tier

use mycel_core::{AuditLog, Confidence, PromptPressureImport, PromptPressureRecord, Substrate};
use mycel_tests::{load_jsonl, test_db};

// Fixed "now" for deterministic TTL assertions.
const NOW: i64 = 1_750_000_000;

#[test]
fn promptpressure_harness_import_and_label_fidelity() {
    // ── load fixtures ─────────────────────────────────────────────────────────
    let records: Vec<PromptPressureRecord> =
        load_jsonl("tests/fixtures/promptpressure_records.jsonl");

    let total = records.len();
    assert!(
        total >= 20,
        "fixture corpus must have at least 20 records for roadmap metric (got {total})"
    );

    // ── count per-tier in fixtures ────────────────────────────────────────────
    let fixture_verified = records
        .iter()
        .filter(|r| r.tier == mycel_core::PromptPressureTier::Verified)
        .count();
    let fixture_probable = records
        .iter()
        .filter(|r| r.tier == mycel_core::PromptPressureTier::Probable)
        .count();
    let fixture_speculative = records
        .iter()
        .filter(|r| r.tier == mycel_core::PromptPressureTier::Speculative)
        .count();

    // ── import ────────────────────────────────────────────────────────────────
    let db = test_db();
    let run_ids = PromptPressureImport::new(&db)
        .import_batch(&records, NOW)
        .expect("import_batch must succeed");

    assert_eq!(
        run_ids.len(),
        total,
        "import_batch must return one id per input record"
    );

    // ── build audit index: run_id → audit payload ─────────────────────────────
    let audit_log = AuditLog::new(&db).list().expect("list audit log");
    let import_events: Vec<_> = audit_log
        .iter()
        .filter(|e| e.event == "promptpressure_import")
        .collect();

    assert_eq!(
        import_events.len(),
        total,
        "audit log must have one promptpressure_import entry per record"
    );

    // Map run_id → audit payload for O(1) lookups.
    let mut audit_by_run_id: std::collections::HashMap<&str, &serde_json::Value> =
        std::collections::HashMap::new();
    for entry in &import_events {
        if let Some(run_id) = entry.payload.get("run_id").and_then(|v| v.as_str()) {
            audit_by_run_id.insert(run_id, &entry.payload);
        }
    }

    // ── per-record assertions (confidence + label fidelity) ───────────────────
    let sub = Substrate::new(&db);

    let mut labels_preserved = 0usize;
    let mut verified_labels = 0usize;
    let mut probable_labels = 0usize;
    let mut speculative_labels = 0usize;

    for (record, run_id) in records.iter().zip(run_ids.iter()) {
        // Confidence mapping matches the tier.
        let run = sub
            .get(run_id)
            .unwrap_or_else(|e| panic!("get run {} failed: {e}", run_id))
            .unwrap_or_else(|| panic!("run {} not found", run_id));

        let expected_confidence = record.tier.to_confidence();
        assert_eq!(
            run.confidence, expected_confidence,
            "run {} (source_id={}) has wrong confidence: got {:?}, want {:?}",
            run_id, record.source_id, run.confidence, expected_confidence
        );

        // Label fidelity: audit payload must contain the original tier string.
        let payload = audit_by_run_id
            .get(run_id.as_str())
            .unwrap_or_else(|| panic!("no audit entry found for run_id {run_id}"));

        let audited_tier = payload
            .get("tier")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("audit payload for run_id {run_id} is missing 'tier'"));

        let expected_tier_str = record.tier.as_str();
        assert_eq!(
            audited_tier, expected_tier_str,
            "label lost for run_id={run_id} source_id={}: expected tier '{}', got '{}'",
            record.source_id, expected_tier_str, audited_tier
        );

        // source_id must also be in the payload.
        let audited_source = payload
            .get("source_id")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("audit payload for run_id {run_id} is missing 'source_id'"));
        assert_eq!(
            audited_source, record.source_id,
            "source_id mismatch in audit payload for run_id={run_id}"
        );

        // confidence mapping consistent in audit payload too
        let audited_confidence = payload
            .get("confidence")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("audit payload for run_id {run_id} is missing 'confidence'"));
        let expected_conf_str = match expected_confidence {
            Confidence::Solid => "solid",
            Confidence::Directional => "directional",
            Confidence::Vibes => "vibes",
        };
        assert_eq!(
            audited_confidence, expected_conf_str,
            "confidence string mismatch in audit payload for run_id={run_id}"
        );

        labels_preserved += 1;
        match record.tier {
            mycel_core::PromptPressureTier::Verified => verified_labels += 1,
            mycel_core::PromptPressureTier::Probable => probable_labels += 1,
            mycel_core::PromptPressureTier::Speculative => speculative_labels += 1,
        }
    }

    // ── aggregate label-fidelity asserts ──────────────────────────────────────
    assert_eq!(
        labels_preserved, total,
        "zero label loss required: {labels_preserved}/{total} labels preserved"
    );
    assert_eq!(
        verified_labels, fixture_verified,
        "verified label fidelity: {verified_labels}/{fixture_verified}"
    );
    assert_eq!(
        probable_labels, fixture_probable,
        "probable label fidelity: {probable_labels}/{fixture_probable}"
    );
    assert_eq!(
        speculative_labels, fixture_speculative,
        "speculative label fidelity: {speculative_labels}/{fixture_speculative}"
    );

    // ── summary table ─────────────────────────────────────────────────────────
    println!(
        "\n=== promptpressure harness metrics ===\n\
         total imported:       {total}\n\
         verified records:     {fixture_verified}   labels preserved: {verified_labels}\n\
         probable records:     {fixture_probable}   labels preserved: {probable_labels}\n\
         speculative records:  {fixture_speculative}   labels preserved: {speculative_labels}\n\
         total labels preserved: {labels_preserved}/{total}\n\
         label loss:           {}\n\
         ======================================\n",
        total - labels_preserved
    );
}
