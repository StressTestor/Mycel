use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

/// Wall clock used by durable orchestration state. Implementations may use a
/// simulated clock; reducers never read system time directly.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// Append-only orchestration record. `state` is the complete reducer snapshot
/// after the named transition, making replay deterministic and allowing newer
/// readers to ignore scopes they do not understand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationRecord {
    pub scope: String,
    pub action: String,
    pub entity_id: Option<String>,
    pub at_ms: u64,
    pub state: Value,
    #[serde(default)]
    pub detail: Value,
}

/// Live projection of a committed orchestration transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationEvent {
    pub scope: String,
    pub action: String,
    pub entity_id: Option<String>,
    pub at_ms: u64,
    #[serde(default)]
    pub detail: Value,
}

pub trait OrchestrationStore: Send + Sync {
    fn load(&self) -> Result<Vec<OrchestrationRecord>, String>;
    fn append(&self, records: &[OrchestrationRecord]) -> Result<(), String>;
}

pub trait LiveEventSink: Send + Sync {
    fn publish(&self, event: OrchestrationEvent);
}

#[derive(Clone)]
pub struct OrchestrationPorts {
    store: Arc<dyn OrchestrationStore>,
    events: Arc<dyn LiveEventSink>,
    clock: Arc<dyn Clock>,
}

impl OrchestrationPorts {
    pub fn new(
        store: Arc<dyn OrchestrationStore>,
        events: Arc<dyn LiveEventSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            events,
            clock,
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    pub(crate) fn restore<T>(&self, scope: &str) -> Result<T, OrchestrationError>
    where
        T: Default + DeserializeOwned,
    {
        let records = self.store.load().map_err(OrchestrationError::Store)?;
        let Some(record) = records.iter().rev().find(|record| record.scope == scope) else {
            return Ok(T::default());
        };
        serde_json::from_value(record.state.clone()).map_err(OrchestrationError::Deserialize)
    }

    pub(crate) fn persist<T: Serialize>(
        &self,
        scope: &str,
        action: &str,
        entity_id: Option<&str>,
        state: &T,
        detail: Value,
    ) -> Result<OrchestrationEvent, OrchestrationError> {
        let at_ms = self.now_ms();
        let state = serde_json::to_value(state).map_err(OrchestrationError::Serialize)?;
        let entity_id = entity_id.map(str::to_owned);
        let record = OrchestrationRecord {
            scope: scope.to_owned(),
            action: action.to_owned(),
            entity_id: entity_id.clone(),
            at_ms,
            state,
            detail: detail.clone(),
        };
        self.store
            .append(std::slice::from_ref(&record))
            .map_err(OrchestrationError::Store)?;
        Ok(OrchestrationEvent {
            scope: scope.to_owned(),
            action: action.to_owned(),
            entity_id,
            at_ms,
            detail,
        })
    }

    pub(crate) fn publish(&self, event: OrchestrationEvent) {
        self.events.publish(event);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestrationError {
    #[error("orchestration store failed: {0}")]
    Store(String),
    #[error("orchestration state serialization failed: {0}")]
    Serialize(serde_json::Error),
    #[error("orchestration state replay failed: {0}")]
    Deserialize(serde_json::Error),
}
