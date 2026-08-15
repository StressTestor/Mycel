use std::collections::BTreeSet;

use mycel_agent_runtime::{
    BackgroundKind, BackgroundMode, BackgroundRegistry, BackgroundShutdown, BackgroundStatus,
};

mod support;
use support::test_ports;

#[test]
fn graceful_shutdown_respects_keep_alive_then_restart_marks_missing_work_lost() {
    let (ports, store, events) = test_ports(1_000);
    let tasks = BackgroundRegistry::open(ports).expect("open");
    tasks
        .register(
            "process-abcdefgh",
            BackgroundKind::Process,
            "foreground",
            BackgroundMode::Foreground,
            None,
        )
        .expect("foreground");
    tasks
        .register(
            "workflow-abcdefgh",
            BackgroundKind::Workflow,
            "durable workflow",
            BackgroundMode::Detached { keep_alive: true },
            Some(60_000),
        )
        .expect("workflow");

    let stopped = tasks
        .shutdown(BackgroundShutdown::StopUnlessKeepAlive)
        .expect("shutdown");
    assert_eq!(stopped, vec!["process-abcdefgh"]);
    assert_eq!(
        tasks.get("process-abcdefgh").expect("foreground").status,
        BackgroundStatus::Killed
    );
    assert_eq!(
        tasks.get("workflow-abcdefgh").expect("workflow").status,
        BackgroundStatus::Running
    );

    let reopened =
        BackgroundRegistry::open(support::ports_from(store.clone(), events.clone(), 2_000))
            .expect("reopen");
    let lost = reopened
        .reconcile(&BTreeSet::new())
        .expect("reconcile missing executors");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].id, "workflow-abcdefgh");
    assert_eq!(lost[0].status, BackgroundStatus::Lost);
    assert_eq!(lost[0].ended_at_ms, Some(2_000));
}

#[test]
fn detach_is_idempotent_and_terminal_tasks_cannot_be_rewritten() {
    let (ports, _store, _events) = test_ports(10);
    let tasks = BackgroundRegistry::open(ports).expect("open");
    tasks
        .register(
            "agent-abcdefgh",
            BackgroundKind::Subagent,
            "child",
            BackgroundMode::Foreground,
            None,
        )
        .expect("register");
    assert!(tasks.detach("agent-abcdefgh", false).expect("detach"));
    assert!(!tasks.detach("agent-abcdefgh", false).expect("repeat"));
    tasks
        .settle("agent-abcdefgh", BackgroundStatus::Completed, None)
        .expect("settle");
    assert!(tasks
        .settle("agent-abcdefgh", BackgroundStatus::Failed, Some("late"))
        .is_err());
}
