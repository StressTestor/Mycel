use std::sync::{Arc, Mutex};

use mycel_agent_runtime::{
    Clock, GoalBudgetLimits, GoalBudgetPort, GoalOrchestrator, GoalStatus, LiveEventSink,
    OrchestrationEvent, OrchestrationPorts, OrchestrationRecord, OrchestrationStore, PromotionGate,
};

#[derive(Default)]
struct TestStore {
    records: Mutex<Vec<OrchestrationRecord>>,
}

impl OrchestrationStore for TestStore {
    fn load(&self) -> Result<Vec<OrchestrationRecord>, String> {
        Ok(self.records.lock().expect("records").clone())
    }

    fn append(&self, records: &[OrchestrationRecord]) -> Result<(), String> {
        self.records
            .lock()
            .expect("records")
            .extend_from_slice(records);
        Ok(())
    }
}

struct TestClock;

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        42
    }
}

struct OrderingEvents {
    store: Arc<TestStore>,
    events: Mutex<Vec<OrchestrationEvent>>,
}

impl LiveEventSink for OrderingEvents {
    fn publish(&self, event: OrchestrationEvent) {
        assert!(
            !self.store.records.lock().expect("records").is_empty(),
            "the durable transition must exist before its live event"
        );
        self.events.lock().expect("events").push(event);
    }
}

fn ports(store: Arc<TestStore>) -> OrchestrationPorts {
    OrchestrationPorts::new(
        store.clone(),
        Arc::new(OrderingEvents {
            store,
            events: Mutex::new(Vec::new()),
        }),
        Arc::new(TestClock),
    )
}

#[test]
fn blocked_goal_never_promotes_and_concurrent_completion_promotes_once() {
    let store = Arc::new(TestStore::default());
    let goals = Arc::new(GoalOrchestrator::open(ports(store.clone())).expect("open"));
    goals.create("g0", "current", false).expect("create");
    goals.enqueue("g1", "first queued").expect("queue first");
    goals.enqueue("g2", "second queued").expect("queue second");
    goals.block("waiting for access").expect("block");

    assert_eq!(
        goals
            .promote_next(PromotionGate::ready())
            .expect("promotion check"),
        None
    );
    let blocked = goals.snapshot();
    assert_eq!(
        blocked.current.expect("current").status,
        GoalStatus::Blocked
    );
    assert_eq!(blocked.queue.len(), 2);

    goals.resume().expect("resume");
    goals.complete("done").expect("complete");
    let contenders: Vec<_> = (0..8)
        .map(|_| {
            let goals = goals.clone();
            std::thread::spawn(move || goals.promote_next(PromotionGate::ready()))
        })
        .collect();
    let promoted = contenders
        .into_iter()
        .filter_map(|thread| thread.join().expect("join").expect("promote"))
        .collect::<Vec<_>>();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].id, "g1");
    assert_eq!(goals.snapshot().queue.len(), 1);

    let reopened = GoalOrchestrator::open(ports(store)).expect("reopen");
    assert_eq!(reopened.snapshot(), goals.snapshot());
}

#[test]
fn replace_next_pause_resume_and_cancel_have_explicit_queue_semantics() {
    let store = Arc::new(TestStore::default());
    let goals = GoalOrchestrator::open(ports(store)).expect("open");
    goals.create("g0", "original", false).expect("create");
    goals.enqueue("g1", "first queued").expect("enqueue");
    goals.enqueue("g2", "second queued").expect("enqueue");

    goals
        .create("replacement", "replacement", true)
        .expect("replace");
    assert_eq!(goals.snapshot().queue.len(), 2);
    assert_eq!(goals.next().expect("explicit next").id, "g1");
    goals.pause(Some("operator pause")).expect("pause");
    assert_eq!(
        goals.snapshot().current.expect("paused").status,
        GoalStatus::Paused
    );
    goals.resume().expect("resume");
    assert_eq!(goals.cancel(Some("superseded")).expect("cancel").id, "g1");

    let cancelled = goals.snapshot();
    assert!(cancelled.current.is_none());
    assert!(!cancelled.promotion_pending);
    assert_eq!(cancelled.queue.len(), 1);
    assert_eq!(
        goals
            .promote_next(PromotionGate::ready())
            .expect("promotion"),
        None
    );
    assert_eq!(goals.next().expect("manual next").id, "g2");
}

#[tokio::test]
async fn canonical_goal_budget_port_persists_usage_and_blocks_at_the_limit() {
    let store = Arc::new(TestStore::default());
    let goals = GoalOrchestrator::open(ports(store.clone())).expect("open");
    goals
        .create("budgeted", "bounded work", false)
        .expect("create");
    let configured = GoalBudgetPort::set_budget(
        &goals,
        GoalBudgetLimits {
            turn_budget: Some(2),
            token_budget: Some(100),
            wall_clock_budget_ms: None,
        },
    )
    .await
    .expect("set budget");
    assert_eq!(configured.limits.turn_budget, Some(2));

    let first = goals.record_turn_usage(30).expect("first usage");
    assert_eq!((first.turns_used, first.tokens_used), (1, 30));
    assert!(!first.over_budget);
    let exhausted = goals.record_turn_usage(20).expect("second usage");
    assert!(exhausted.over_budget);
    assert_eq!(
        goals.snapshot().current.expect("current").status,
        GoalStatus::Blocked
    );
    assert!(goals.record_turn_usage(1).is_err());

    let reopened = GoalOrchestrator::open(ports(store)).expect("reopen");
    assert_eq!(reopened.budget_snapshot(), exhausted);
    assert!(GoalBudgetPort::set_budget(
        &reopened,
        GoalBudgetLimits {
            turn_budget: Some(1),
            ..GoalBudgetLimits::default()
        }
    )
    .await
    .is_err());
}
