//! Fixture-driven spore harness for the v0.5 spore-based-discovery milestone.
//!
//! Metric blocks (ROADMAP v0.5):
//!   1. catalog at least 25 spores from fixture runs.
//!   2. classify at least 20 adjacent-work notices into typed candidate records.
//!   3. dedupe at least 15 repeated spores.
//!   4. export at least 10 spores into the interop loss-matrix format.
//!   5. no spore triggers an agent launch.

use mycel_core::{
    classify_adjacent_work, dedupe_spores, export_spore, AdjacentWorkNotice, Db, InteropShape,
    Spore, SporeKind, SporeStore, TaskIdentity,
};
use mycel_tests::{load_jsonl, test_db};

// ── metric 1: catalog >= 25 spores ──────────────────────────────────────────────

#[test]
fn catalog_at_least_25_spores_from_fixtures() {
    let spores: Vec<Spore> = load_jsonl("tests/fixtures/spore_records.jsonl");
    // every fixture record must be valid and carry a self-spec-compatible identity.
    for (i, s) in spores.iter().enumerate() {
        assert!(
            s.validate().is_ok(),
            "[spore {i}] fixture must be valid: {:?}",
            s.validate().err()
        );
        assert_eq!(
            s.task.signature,
            TaskIdentity::canonicalize(&s.task.description),
            "[spore {i}] signature must equal canonicalize(description)"
        );
    }

    let db = test_db();
    let store = SporeStore::new(&db);
    let (ids, duplicates) = store
        .catalog(spores.clone(), 1000)
        .expect("catalog fixtures");
    let stored = store.count().expect("count");

    let completed = spores
        .iter()
        .filter(|s| s.kind == SporeKind::CompletedWork)
        .count();
    let adjacent = spores
        .iter()
        .filter(|s| s.kind == SporeKind::AdjacentWork)
        .count();

    println!(
        "\n=== spore catalog metrics ===\n\
         fixture spores:   {}\n\
         completed_work:   {completed}\n\
         adjacent_work:    {adjacent}\n\
         catalogued (ids): {}\n\
         duplicates:       {duplicates}\n\
         stored count:     {stored}\n\
         =============================\n",
        spores.len(),
        ids.len()
    );

    assert!(
        stored >= 25,
        "must catalog >= 25 spores from fixtures (stored {stored})"
    );
    assert_eq!(ids.len(), stored, "every catalogued id must be stored");
}

// ── metric 2: classify >= 20 adjacent-work notices ──────────────────────────────

#[test]
fn classify_at_least_20_adjacent_work_notices() {
    let notices: Vec<AdjacentWorkNotice> = load_jsonl("tests/fixtures/adjacent_work_notices.jsonl");
    assert!(
        notices.len() >= 20,
        "need >= 20 adjacent-work notices (got {})",
        notices.len()
    );

    let mut classified = 0usize;
    for (i, notice) in notices.iter().enumerate() {
        let spore = classify_adjacent_work(notice);
        // a classified notice is always an AdjacentWork spore with a valid identity.
        assert_eq!(
            spore.kind,
            SporeKind::AdjacentWork,
            "[notice {i}] classification must yield AdjacentWork"
        );
        assert_eq!(
            spore.task.signature,
            TaskIdentity::canonicalize(&notice.description),
            "[notice {i}] classified signature must equal canonicalize(description)"
        );
        assert!(
            spore.validate().is_ok(),
            "[notice {i}] classified spore must be valid: {:?}",
            spore.validate().err()
        );
        classified += 1;
    }

    println!(
        "\n=== adjacent-work classification metrics ===\n\
         notices:    {}\n\
         classified: {classified}\n\
         (all typed as AdjacentWork with a canonical signature)\n\
         ===========================================\n",
        notices.len()
    );

    assert!(
        classified >= 20,
        "must classify >= 20 adjacent-work notices (got {classified})"
    );
}

// ── metric 3: dedupe >= 15 repeated spores ──────────────────────────────────────

#[test]
fn dedupe_at_least_15_repeated_spores() {
    let spores: Vec<Spore> = load_jsonl("tests/fixtures/spore_dupes.jsonl");
    let before = spores.len();
    let (unique, duplicates) = dedupe_spores(spores);

    println!(
        "\n=== spore dedupe metrics ===\n\
         before:     {before}\n\
         unique:     {}\n\
         duplicates: {duplicates}\n\
         ============================\n",
        unique.len()
    );

    assert!(
        duplicates >= 15,
        "must dedupe >= 15 repeated spores (got {duplicates})"
    );
    assert_eq!(
        unique.len() + duplicates,
        before,
        "unique + duplicates must equal the input count"
    );
}

// ── metric 4: export >= 10 spores to interop loss-matrix shapes ──────────────────

#[test]
fn export_at_least_10_spores_to_interop_shapes() {
    let spores: Vec<Spore> = load_jsonl("tests/fixtures/spore_records.jsonl");
    let sample: Vec<&Spore> = spores.iter().take(10).collect();
    assert!(sample.len() >= 10, "need >= 10 spores to export");

    let mut exported = 0usize;
    let mut lossless = 0usize;
    let mut lossy_with_declared_drops = 0usize;

    for (i, s) in sample.iter().enumerate() {
        // Mycel-native is lossless; the three foreign shapes must declare dropped fields.
        let native = export_spore(s, InteropShape::MycelNative);
        assert!(native.lossless, "[spore {i}] mycel-native must be lossless");
        assert!(
            native.dropped.is_empty(),
            "[spore {i}] mycel-native must drop nothing"
        );
        lossless += 1;
        exported += 1;

        for shape in [
            InteropShape::Hermes,
            InteropShape::OpenClaw,
            InteropShape::AgentSkills,
        ] {
            let e = export_spore(s, shape);
            assert!(!e.lossless, "[spore {i}] {shape:?} must not be lossless");
            assert!(
                !e.dropped.is_empty(),
                "[spore {i}] {shape:?} must declare dropped ecology fields"
            );
            // the export must NOT carry confidence as a live field (it would imply the
            // foreign runtime enforces Mycel policy).
            assert!(
                !e.fields.contains_key("confidence"),
                "[spore {i}] {shape:?} must not carry confidence as live"
            );
            lossy_with_declared_drops += 1;
            exported += 1;
        }
    }

    println!(
        "\n=== spore interop export metrics ===\n\
         spores exported:           {}\n\
         total shape-exports:       {exported}\n\
         lossless (mycel-native):   {lossless}\n\
         lossy w/ declared drops:   {lossy_with_declared_drops}\n\
         ===================================\n",
        sample.len()
    );

    assert!(
        sample.len() >= 10,
        "must export >= 10 spores (got {})",
        sample.len()
    );
}

// ── metric 5: no spore triggers an agent launch ─────────────────────────────────

#[test]
fn no_spore_germinates_in_v05() {
    let spores: Vec<Spore> = load_jsonl("tests/fixtures/spore_records.jsonl");
    let mut candidates = 0usize;
    for s in &spores {
        let candidate = s.germination_candidate();
        assert!(
            !candidate.germinated,
            "v0.5 must NEVER germinate: spore '{}' reported germinated=true",
            s.task.signature
        );
        candidates += 1;
    }

    println!(
        "\n=== spore germination-safety metrics ===\n\
         germination candidates produced: {candidates}\n\
         agents launched:                 0\n\
         (every candidate.germinated == false)\n\
         =======================================\n"
    );

    assert_eq!(candidates, spores.len(), "a candidate per spore, all inert");
}

// ── integration: catalog then export from the live store ────────────────────────

#[test]
fn catalogued_spores_export_from_the_store() {
    let spores: Vec<Spore> = load_jsonl("tests/fixtures/spore_records.jsonl");
    let db: Db = test_db();
    let store = SporeStore::new(&db);
    store.catalog(spores, 1000).expect("catalog");

    let listed = store.list().expect("list");
    assert!(listed.len() >= 25, "catalog must hold >= 25 spores");

    // export the first 10 listed spores to the lossless shape and confirm round-trippable.
    let mut native_exports = 0usize;
    for s in listed.iter().take(10) {
        let e = export_spore(s, InteropShape::MycelNative);
        assert!(e.lossless);
        assert_eq!(e.fields.get("signature"), Some(&s.task.signature));
        native_exports += 1;
    }
    assert_eq!(native_exports, 10);
}
