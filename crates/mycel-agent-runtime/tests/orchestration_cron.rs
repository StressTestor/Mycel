use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use mycel_agent_runtime::{
    Clock, CronError, CronScheduler, LiveEventSink, OrchestrationEvent, OrchestrationPorts,
    OrchestrationRecord, OrchestrationStore,
};

#[derive(Default)]
struct MemoryStore {
    records: Mutex<Vec<OrchestrationRecord>>,
}

impl OrchestrationStore for MemoryStore {
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

#[derive(Default)]
struct MutableClock(AtomicU64);

impl MutableClock {
    fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for MutableClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct OrderingSink {
    store: Arc<MemoryStore>,
    all_durable_first: AtomicBool,
}

impl OrderingSink {
    fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            store,
            all_durable_first: AtomicBool::new(true),
        }
    }
}

impl LiveEventSink for OrderingSink {
    fn publish(&self, event: OrchestrationEvent) {
        let durable = self
            .store
            .records
            .lock()
            .expect("records")
            .last()
            .is_some_and(|record| {
                record.scope == event.scope
                    && record.action == event.action
                    && record.entity_id == event.entity_id
            });
        self.all_durable_first.fetch_and(durable, Ordering::SeqCst);
    }
}

fn fixture() -> (
    OrchestrationPorts,
    Arc<MemoryStore>,
    Arc<MutableClock>,
    Arc<OrderingSink>,
) {
    let store = Arc::new(MemoryStore::default());
    let clock = Arc::new(MutableClock::default());
    let events = Arc::new(OrderingSink::new(store.clone()));
    (
        OrchestrationPorts::new(store.clone(), events.clone(), clock.clone()),
        store,
        clock,
        events,
    )
}

#[test]
fn idle_gating_coalesces_due_fires_and_persists_cursor_across_restart() {
    let (ports, store, clock, events) = fixture();
    let scheduler = CronScheduler::open(ports.clone()).expect("open");
    scheduler
        .schedule("deadbeef", "* * * * *", "check status", true)
        .expect("schedule");

    clock.set(180_000);
    assert!(scheduler.tick(false).expect("busy tick").is_empty());
    assert_eq!(scheduler.snapshot().tasks[0].last_fired_at_ms, None);
    assert_eq!(
        scheduler.next_fire_at_ms("deadbeef").expect("next fire"),
        Some(60_000)
    );

    let fires = scheduler.tick(true).expect("idle tick");
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].coalesced_count, 3);
    assert_eq!(fires[0].scheduled_at_ms, 180_000);
    assert!(!fires[0].stale);
    assert!(scheduler.tick(true).expect("same instant").is_empty());

    clock.set(240_000);
    let resumed = CronScheduler::open(ports).expect("reopen");
    let resumed_fires = resumed.tick(true).expect("resumed tick");
    assert_eq!(resumed_fires.len(), 1);
    assert_eq!(resumed_fires[0].coalesced_count, 1);
    assert_eq!(resumed.snapshot().tasks[0].last_fired_at_ms, Some(240_000));
    assert!(events.all_durable_first.load(Ordering::SeqCst));
    assert!(store.records.lock().expect("records").len() >= 3);
}

#[test]
fn stale_recurring_and_overdue_one_shot_each_get_one_final_delivery() {
    const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

    let (ports, _store, clock, _events) = fixture();
    let scheduler = CronScheduler::open(ports).expect("open");
    scheduler
        .schedule("deadbeef", "* * * * *", "recurring", true)
        .expect("recurring");
    scheduler
        .schedule("c0ffee00", "* * * * *", "once", false)
        .expect("one shot");

    clock.set(SEVEN_DAYS_MS + 60_000);
    let fires = scheduler.tick(true).expect("tick");
    assert_eq!(fires.len(), 2);
    let recurring = fires
        .iter()
        .find(|fire| fire.task_id == "deadbeef")
        .expect("recurring fire");
    assert!(recurring.stale);
    assert_eq!(recurring.coalesced_count, 10_000);
    let one_shot = fires
        .iter()
        .find(|fire| fire.task_id == "c0ffee00")
        .expect("one-shot fire");
    assert!(!one_shot.stale);
    assert_eq!(one_shot.coalesced_count, 1);
    assert!(scheduler.snapshot().tasks.is_empty());
}

#[test]
fn invalid_and_non_firing_schedules_are_rejected_at_creation() {
    let (ports, _store, _clock, _events) = fixture();
    let scheduler = CronScheduler::open(ports).expect("open");
    assert!(matches!(
        scheduler.schedule("deadbeef", "* * * *", "bad", true),
        Err(CronError::InvalidExpression(_))
    ));
    assert!(matches!(
        scheduler.schedule("deadbeef", "0 0 31 2 *", "never", true),
        Err(CronError::NoFireWithinWindow)
    ));
    assert!(scheduler.snapshot().tasks.is_empty());
}
