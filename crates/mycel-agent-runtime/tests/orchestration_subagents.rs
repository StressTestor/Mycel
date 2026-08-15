use std::collections::{BTreeMap, BTreeSet};

use mycel_agent_runtime::{
    CapabilitySet, SubagentRegistry, SubagentStatus, SwarmError, SwarmPlanner, WorkerProfile,
};

mod support;
use support::test_ports;

fn capabilities(tools: &[&str], delegate: bool, swarm: bool, workflow: bool) -> CapabilitySet {
    CapabilitySet {
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        filesystem_roots: BTreeSet::from(["/workspace".to_owned()]),
        network: false,
        can_spawn_subagents: delegate,
        can_swarm: swarm,
        can_workflow: workflow,
    }
}

#[test]
fn child_capabilities_are_subsets_and_nonrecursive_profiles_cannot_delegate() {
    let (ports, _store, _events) = test_ports(10);
    let agents = SubagentRegistry::open(ports).expect("open");
    agents
        .register_root("main", capabilities(&["Read", "Edit"], true, true, true))
        .expect("root");

    let escalation = WorkerProfile {
        name: "bad".to_owned(),
        capabilities: capabilities(&["Read", "Bash"], false, false, false),
        allow_delegation: false,
    };
    assert!(agents
        .spawn("child-bad", "main", escalation, false)
        .is_err());

    let path_escape = WorkerProfile {
        name: "escape".to_owned(),
        capabilities: CapabilitySet {
            filesystem_roots: BTreeSet::from(["/workspace/../secret".to_owned()]),
            ..capabilities(&["Read"], false, false, false)
        },
        allow_delegation: false,
    };
    assert!(agents
        .spawn("child-escape", "main", path_escape, false)
        .is_err());

    let worker = WorkerProfile {
        name: "coder".to_owned(),
        capabilities: capabilities(&["Read"], false, false, false),
        allow_delegation: false,
    };
    agents
        .spawn("child-ok", "main", worker.clone(), false)
        .expect("valid child");
    assert!(agents
        .spawn("grandchild", "child-ok", worker, false)
        .is_err());
}

#[test]
fn parent_cancellation_spares_detached_children_and_restart_marks_only_missing_active_children_lost(
) {
    let (ports, store, events) = test_ports(100);
    let agents = SubagentRegistry::open(ports).expect("open");
    agents
        .register_root("main", capabilities(&["Read"], true, true, false))
        .expect("root");
    let worker = WorkerProfile {
        name: "explore".to_owned(),
        capabilities: capabilities(&["Read"], false, false, false),
        allow_delegation: false,
    };
    agents
        .spawn("foreground", "main", worker.clone(), false)
        .expect("foreground");
    agents
        .spawn("detached", "main", worker, true)
        .expect("detached");
    assert_eq!(
        agents
            .cancel_children("main", "parent cancelled")
            .expect("cancel"),
        vec!["foreground"]
    );
    assert_eq!(
        agents.get("foreground").expect("foreground").status,
        SubagentStatus::Cancelled
    );
    assert_eq!(
        agents.get("detached").expect("detached").status,
        SubagentStatus::Running
    );

    let reopened = SubagentRegistry::open(support::ports_from(store, events, 200)).expect("reopen");
    let lost = reopened.reconcile(&BTreeSet::new()).expect("reconcile");
    assert_eq!(
        lost.iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        ["detached"]
    );
    assert_eq!(lost[0].status, SubagentStatus::Lost);
}

#[test]
fn swarm_fan_out_and_concurrency_are_bounded() {
    assert!(matches!(
        SwarmPlanner::new(129, 3),
        Err(SwarmError::FanOutLimit)
    ));
    let planner = SwarmPlanner::new(4, 2).expect("planner");
    let plan = planner
        .plan(
            "review",
            "coder",
            &[
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned(),
            ],
            "Review {{item}}",
            &BTreeMap::new(),
        )
        .expect("plan");
    assert_eq!(plan.members.len(), 4);
    assert_eq!(
        plan.waves().iter().map(Vec::len).collect::<Vec<_>>(),
        [2, 2]
    );
    assert!(planner
        .plan(
            "too many",
            "coder",
            &["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            "Review {{item}}",
            &BTreeMap::new(),
        )
        .is_err());
}
