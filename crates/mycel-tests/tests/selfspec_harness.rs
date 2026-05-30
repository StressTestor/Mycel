//! Fixture-driven harness for SelfSpec validation and dedupe.
//!
//! Loads `tests/fixtures/selfspec_cases.jsonl` and `tests/fixtures/selfspec_dupes.jsonl`,
//! asserts all validation results match fixture expectations, and confirms the v0.3 dedupe
//! roadmap metric (≥15 near-duplicate specs collapsed).

use std::collections::BTreeSet;

use mycel_core::{
    dedupe_specs, Confidence, InheritedContext, SelfSpec, SpecValidationError, TaskIdentity,
};
use mycel_tests::load_jsonl;
use serde::Deserialize;

// ── fixture schema ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SpecCase {
    name: String,
    spec: SpecInput,
    expect_valid: bool,
    invalid_reasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SpecInput {
    task: TaskIdentityInput,
    preconditions: Vec<String>,
    success_criteria: Vec<String>,
    inherited_context: Vec<InheritedContextInput>,
    refusal_risks: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TaskIdentityInput {
    description: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
struct InheritedContextInput {
    claim: String,
    confidence: String,
    source: String,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_confidence(s: &str) -> Confidence {
    match s {
        "solid" => Confidence::Solid,
        "directional" => Confidence::Directional,
        "vibes" => Confidence::Vibes,
        _ => panic!("unknown confidence in fixture: {s}"),
    }
}

fn spec_from_input(input: SpecInput) -> SelfSpec {
    SelfSpec {
        task: TaskIdentity {
            description: input.task.description,
            signature: input.task.signature,
        },
        preconditions: input.preconditions,
        success_criteria: input.success_criteria,
        inherited_context: input
            .inherited_context
            .into_iter()
            .map(|ctx| InheritedContext {
                claim: ctx.claim,
                confidence: parse_confidence(&ctx.confidence),
                source: ctx.source,
            })
            .collect(),
        refusal_risks: input.refusal_risks,
    }
}

fn error_to_reason(e: &SpecValidationError) -> &'static str {
    e.as_reason()
}

// ── validation harness ────────────────────────────────────────────────────────

#[test]
fn selfspec_harness_validation_and_dedupe() {
    let cases: Vec<SpecCase> = load_jsonl("tests/fixtures/selfspec_cases.jsonl");

    assert!(
        cases.len() >= 30,
        "fixture corpus must have at least 30 cases (got {})",
        cases.len()
    );

    let mut valid_count = 0usize;
    let mut invalid_count = 0usize;
    let mut error_type_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for case in &cases {
        let spec = spec_from_input(SpecInput {
            task: TaskIdentityInput {
                description: case.spec.task.description.clone(),
                signature: case.spec.task.signature.clone(),
            },
            preconditions: case.spec.preconditions.clone(),
            success_criteria: case.spec.success_criteria.clone(),
            inherited_context: case
                .spec
                .inherited_context
                .iter()
                .map(|ctx| InheritedContextInput {
                    claim: ctx.claim.clone(),
                    confidence: ctx.confidence.clone(),
                    source: ctx.source.clone(),
                })
                .collect(),
            refusal_risks: case.spec.refusal_risks.clone(),
        });

        let result = spec.validate();

        if case.expect_valid {
            assert!(
                result.is_ok(),
                "[{}] expected valid but got errors: {:?}",
                case.name,
                result.err()
            );
            valid_count += 1;
        } else {
            let errors = match result {
                Err(e) => e,
                Ok(()) => panic!(
                    "[{}] expected invalid but validate() returned Ok(())",
                    case.name
                ),
            };

            // Compare error sets (not ordered vecs).
            let actual: BTreeSet<String> = errors
                .iter()
                .map(|e| error_to_reason(e).to_string())
                .collect();
            let expected: BTreeSet<String> = case.invalid_reasons.iter().cloned().collect();

            assert_eq!(
                actual, expected,
                "[{}] error set mismatch — actual: {:?}, expected: {:?}",
                case.name, actual, expected
            );

            for reason in &case.invalid_reasons {
                *error_type_counts.entry(reason.clone()).or_insert(0) += 1;
            }

            invalid_count += 1;
        }
    }

    println!(
        "\n=== selfspec validation harness ===\n\
         total:   {}\n\
         valid:   {}\n\
         invalid: {}\n\
         per-error-type counts:",
        cases.len(),
        valid_count,
        invalid_count,
    );
    for (reason, count) in &error_type_counts {
        println!("  {reason}: {count}");
    }
    println!("===================================\n");
}

// ── dedupe harness ────────────────────────────────────────────────────────────

#[test]
fn selfspec_harness_dedupe_meets_roadmap_metric() {
    let cases: Vec<SpecCase> = load_jsonl("tests/fixtures/selfspec_dupes.jsonl");

    let specs: Vec<SelfSpec> = cases.into_iter().map(|c| spec_from_input(c.spec)).collect();

    let before_count = specs.len();
    let (unique, duplicate_count) = dedupe_specs(specs);
    let after_count = unique.len();

    println!(
        "\n=== selfspec dedupe harness ===\n\
         before dedupe: {before_count}\n\
         after dedupe:  {after_count}\n\
         duplicates collapsed: {duplicate_count}\n\
         ==============================\n"
    );

    assert!(
        duplicate_count >= 15,
        "dedupe must collapse >= 15 near-duplicate specs (collapsed {duplicate_count})"
    );
}
