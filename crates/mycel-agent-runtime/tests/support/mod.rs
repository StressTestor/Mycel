use std::sync::{Arc, Mutex};

use mycel_agent_runtime::{
    Clock, LiveEventSink, OrchestrationEvent, OrchestrationPorts, OrchestrationRecord,
    OrchestrationStore,
};

#[derive(Default)]
pub struct TestStore {
    pub records: Mutex<Vec<OrchestrationRecord>>,
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

pub struct TestClock(pub u64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

#[derive(Default)]
pub struct TestEvents {
    pub events: Mutex<Vec<OrchestrationEvent>>,
}

impl LiveEventSink for TestEvents {
    fn publish(&self, event: OrchestrationEvent) {
        self.events.lock().expect("events").push(event);
    }
}

pub fn ports_from(
    store: Arc<TestStore>,
    events: Arc<TestEvents>,
    now_ms: u64,
) -> OrchestrationPorts {
    OrchestrationPorts::new(store, events, Arc::new(TestClock(now_ms)))
}

pub fn test_ports(now_ms: u64) -> (OrchestrationPorts, Arc<TestStore>, Arc<TestEvents>) {
    let store = Arc::new(TestStore::default());
    let events = Arc::new(TestEvents::default());
    (
        ports_from(store.clone(), events.clone(), now_ms),
        store,
        events,
    )
}
