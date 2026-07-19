//! Fixture-driven executability harness for v0.3 roadmap metric.
//!
//! Loads `tests/fixtures/executable_specs.jsonl` — a corpus of ≥12 genuinely
//! self-contained handoff specs — and asserts that every spec passes the
//! executability bar defined by `SelfSpec::is_executable()`.
//!
//! This harness evidences the DETERMINISTIC half of the v0.3 metric:
//! "at least 10 handoff specs can be reviewed and executed manually WITHOUT
//! reading the prior full transcript." A separate out-of-band blind-reviewer
//! pass (recorded in `docs/v0.3-blind-review-evidence.md`) evidences the
//! qualitative half. See ADR 0014.

use std::collections::HashSet;

use mycel_core::{ExecutabilityGap, SelfSpec};
use mycel_tests::load_jsonl;

// ── harness ───────────────────────────────────────────────────────────────────

#[test]
fn executable_specs_meet_roadmap_metric() {
    let specs: Vec<SelfSpec> = load_jsonl("tests/fixtures/executable_specs.jsonl");

    // ── corpus size ──────────────────────────────────────────────────────────

    assert!(
        specs.len() >= 10,
        "executable_specs.jsonl must contain at least 10 specs (got {})",
        specs.len()
    );

    // ── signature uniqueness ─────────────────────────────────────────────────

    let mut seen_sigs: HashSet<String> = HashSet::new();
    for (i, spec) in specs.iter().enumerate() {
        let sig = &spec.task.signature;
        assert!(
            seen_sigs.insert(sig.clone()),
            "duplicate signature at index {i}: {sig:?}"
        );
    }

    // ── executability checks ─────────────────────────────────────────────────

    let mut executable_count = 0usize;

    for (i, spec) in specs.iter().enumerate() {
        let gaps = spec.executability_gaps();
        if !gaps.is_empty() {
            let gap_names: Vec<&str> = gaps
                .iter()
                .map(|g| match g {
                    ExecutabilityGap::FailsValidation => "FailsValidation",
                    ExecutabilityGap::NoPrecondition => "NoPrecondition",
                    ExecutabilityGap::NoActionableSuccessCriterion => {
                        "NoActionableSuccessCriterion"
                    }
                    ExecutabilityGap::NoSourcedContext => "NoSourcedContext",
                    ExecutabilityGap::NoRefusalRisk => "NoRefusalRisk",
                })
                .collect();
            panic!(
                "spec at index {i} (sig: {:?}) is NOT executable.\n  gaps: {:?}\n",
                spec.task.signature, gap_names
            );
        }
        executable_count += 1;
    }

    assert!(
        executable_count >= 10,
        "at least 10 specs must be executable (got {executable_count})"
    );

    // ── summary table ────────────────────────────────────────────────────────

    println!(
        "\n=== executable specs harness (v0.3 roadmap metric) ===\n\
         total specs: {}\n\
         executable:  {}\n",
        specs.len(),
        executable_count,
    );

    // Print summary table header.
    println!(
        "  {:<4} {:<46} {:<5} {:<5} {:<5} exec",
        "idx", "signature (truncated)", "pre", "crit", "ctx"
    );
    println!("  {}", "-".repeat(74));

    for (i, spec) in specs.iter().enumerate() {
        let sig_truncated = {
            let s = &spec.task.signature;
            if s.chars().count() > 44 {
                format!(
                    "{}…",
                    &s[..s.char_indices().nth(43).map(|(b, _)| b).unwrap_or(s.len())]
                )
            } else {
                s.clone()
            }
        };
        println!(
            "  {idx:<4} {sig:<46} {pre:<5} {crit:<5} {ctx:<5} {exec}",
            idx = i,
            sig = sig_truncated,
            pre = spec.preconditions.len(),
            crit = spec.success_criteria.len(),
            ctx = spec.inherited_context.len(),
            exec = if spec.is_executable() { "y" } else { "n" }
        );
    }

    println!(
        "\n  metric: {executable_count}/{} specs executable (need >= 10) — PASS\n\
         ======================================================\n",
        specs.len()
    );
}
