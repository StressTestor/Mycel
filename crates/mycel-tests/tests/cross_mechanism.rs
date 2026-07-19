//! Cross-mechanism integration: the shared `TaskIdentity` payoff.
//!
//! v0.3 (self-spec), v0.4 (sclerotia), and v0.5 (spores) all build on ONE
//! `TaskIdentity` primitive. The load-bearing claim is that a single signature
//! lets the three mechanisms cross-reference the same task. The per-milestone
//! harnesses test each store in isolation; this test threads one signature
//! through all three stores in a single database and proves they line up.

use mycel_core::{
    Confidence, InheritedContext, Sclerotium, SclerotiumStore, SelfSpec, SpecStore, Spore,
    SporeKind, SporeStore, TaskIdentity, WakeCondition,
};
use mycel_tests::test_db;

/// The one task all three mechanisms describe. Two descriptions that differ only
/// in case/whitespace/trailing-punct must canonicalize to the SAME signature, so
/// the three records line up even though they were authored independently.
const SPEC_DESC: &str = "Fix the off-by-one in distill truncation";
const SCLEROTIUM_DESC: &str = "  fix the off-by-one in distill truncation  ";
const SPORE_DESC: &str = "Fix the off-by-one in distill truncation.";

fn self_spec() -> SelfSpec {
    SelfSpec {
        task: TaskIdentity::new(SPEC_DESC),
        preconditions: vec!["distill lives in crates/mycel-core/src/decay.rs".to_string()],
        success_criteria: vec!["cargo test -p mycel-core distill passes".to_string()],
        inherited_context: vec![InheritedContext {
            claim: "distill truncates at the last space within the limit".to_string(),
            confidence: Confidence::Solid,
            source: "run:decay-rs-distill".to_string(),
        }],
        refusal_risks: vec!["do not change distillation semantics for short summaries".to_string()],
    }
}

fn sclerotium() -> Sclerotium {
    Sclerotium {
        task: TaskIdentity::new(SCLEROTIUM_DESC),
        blocker: "needs a multibyte-boundary regression fixture".to_string(),
        attempted_paths: vec!["reproduced the panic with a boundary-straddling char".to_string()],
        next_command: "cargo test -p mycel-core distill".to_string(),
        wake_conditions: vec![WakeCondition::TimeReached { at: 0 }],
        inherited_context: vec![InheritedContext {
            claim: "the fix steps back to the nearest char boundary".to_string(),
            confidence: Confidence::Directional,
            source: "spec:fix-the-off-by-one-in-distill-truncation".to_string(),
        }],
    }
}

fn spore() -> Spore {
    Spore {
        task: TaskIdentity::new(SPORE_DESC),
        kind: SporeKind::CompletedWork,
        origin: "run:distill-fix".to_string(),
        confidence: Confidence::Solid,
        note: "completed: hardened distill against multibyte boundaries".to_string(),
    }
}

#[test]
fn shared_task_identity_threads_through_spec_sclerotium_and_spore() {
    // 1. The three independently-authored descriptions canonicalize identically.
    let sig = TaskIdentity::canonicalize(SPEC_DESC);
    assert_eq!(
        TaskIdentity::canonicalize(SCLEROTIUM_DESC),
        sig,
        "sclerotium description must canonicalize to the same signature as the spec"
    );
    assert_eq!(
        TaskIdentity::canonicalize(SPORE_DESC),
        sig,
        "spore description must canonicalize to the same signature as the spec"
    );

    // 2. One shared database; each mechanism writes its own record for the task.
    let db = test_db();
    SpecStore::new(&db)
        .insert(&self_spec(), 1000)
        .expect("insert self-spec");
    SclerotiumStore::new(&db)
        .insert(&sclerotium(), 1001)
        .expect("insert sclerotium");
    SporeStore::new(&db)
        .insert(&spore(), 1002)
        .expect("insert spore");

    // 3. Each store retrieves ITS record by the single shared signature.
    let specs = SpecStore::new(&db)
        .get_by_signature(&sig)
        .expect("query specs");
    let sclerotia = SclerotiumStore::new(&db)
        .get_by_signature(&sig)
        .expect("query sclerotia");
    let spores = SporeStore::new(&db)
        .get_by_signature(&sig)
        .expect("query spores");

    assert_eq!(
        specs.len(),
        1,
        "exactly one self-spec for the shared signature"
    );
    assert_eq!(
        sclerotia.len(),
        1,
        "exactly one sclerotium for the shared signature"
    );
    assert_eq!(
        spores.len(),
        1,
        "exactly one spore for the shared signature"
    );

    // 4. The cross-reference holds: all three carry the identical signature, and
    //    their stored task identities agree on it.
    assert_eq!(specs[0].task.signature, sig);
    assert_eq!(sclerotia[0].task.signature, sig);
    assert_eq!(spores[0].task.signature, sig);

    // 5. A different task's signature returns nothing from any store (no bleed).
    let other = TaskIdentity::canonicalize("Add an index on runs.expires_at");
    assert_ne!(other, sig);
    assert!(SpecStore::new(&db)
        .get_by_signature(&other)
        .unwrap()
        .is_empty());
    assert!(SclerotiumStore::new(&db)
        .get_by_signature(&other)
        .unwrap()
        .is_empty());
    assert!(SporeStore::new(&db)
        .get_by_signature(&other)
        .unwrap()
        .is_empty());

    println!(
        "\n=== cross-mechanism shared-identity ===\n\
         shared signature: {sig}\n\
         self-specs:  {}\n\
         sclerotia:   {}\n\
         spores:      {}\n\
         (one task, three mechanisms, one signature)\n\
         =======================================\n",
        specs.len(),
        sclerotia.len(),
        spores.len()
    );
}
