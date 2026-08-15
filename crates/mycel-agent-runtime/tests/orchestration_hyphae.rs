use mycel_agent_protocol::ThinkingEffort;
use mycel_agent_runtime::{HyphaeError, HyphaeReducer, HyphaeSwarmMode};

mod support;
use support::{ports_from, test_ports};

fn effort(value: &str) -> ThinkingEffort {
    ThinkingEffort::new(value).expect("effort")
}

#[test]
fn task_mode_is_session_durable_xhigh_and_one_shot_swarm() {
    let (ports, store, events) = test_ports(10);
    let hyphae = HyphaeReducer::open(ports, Some(effort("high"))).expect("open");
    let transition = hyphae
        .apply("Review the release candidate", true)
        .expect("apply");
    assert_eq!(
        transition
            .state
            .thinking_effort
            .as_ref()
            .map(|v| v.as_str()),
        Some("xhigh")
    );
    assert_eq!(transition.state.swarm_mode, HyphaeSwarmMode::Task);
    assert_eq!(
        transition.submit_prompt.as_deref(),
        Some("Review the release candidate")
    );
    assert_eq!(store.records.lock().expect("records").len(), 1);
    assert_eq!(events.events.lock().expect("events").len(), 1);

    let resumed = HyphaeReducer::open(
        ports_from(store.clone(), events.clone(), 20),
        Some(effort("medium")),
    )
    .expect("resume");
    assert_eq!(resumed.snapshot().swarm_mode, HyphaeSwarmMode::Task);
    resumed.finish_task().expect("finish task");
    let finished = resumed.snapshot();
    assert_eq!(finished.swarm_mode, HyphaeSwarmMode::Off);
    assert_eq!(
        finished
            .thinking_effort
            .as_ref()
            .map(|value| value.as_str()),
        Some("xhigh")
    );
}

#[test]
fn off_and_empty_toggle_do_not_restore_previous_effort() {
    let (ports, _store, _events) = test_ports(10);
    let hyphae = HyphaeReducer::open(ports, Some(effort("high"))).expect("open");
    hyphae.apply("", true).expect("toggle on");
    assert_eq!(hyphae.snapshot().swarm_mode, HyphaeSwarmMode::Standing);
    assert_eq!(
        hyphae
            .snapshot()
            .thinking_effort
            .as_ref()
            .map(|value| value.as_str()),
        Some("xhigh")
    );
    hyphae.apply("", false).expect("toggle off");
    assert_eq!(hyphae.snapshot().swarm_mode, HyphaeSwarmMode::Off);
    assert_eq!(
        hyphae
            .snapshot()
            .thinking_effort
            .as_ref()
            .map(|value| value.as_str()),
        Some("xhigh")
    );
}

#[test]
fn unsupported_xhigh_cannot_partially_enable_swarm() {
    let (ports, store, events) = test_ports(10);
    let hyphae = HyphaeReducer::open(ports, Some(effort("high"))).expect("open");
    assert!(matches!(
        hyphae.apply("on", false),
        Err(HyphaeError::XhighUnsupported)
    ));
    let state = hyphae.snapshot();
    assert_eq!(state.swarm_mode, HyphaeSwarmMode::Off);
    assert_eq!(
        state.thinking_effort.as_ref().map(|value| value.as_str()),
        Some("high")
    );
    assert!(store.records.lock().expect("records").is_empty());
    assert!(events.events.lock().expect("events").is_empty());
}
