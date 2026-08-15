use mycel_agent_protocol::Event;
use tokio::sync::broadcast;

/// Session-scoped event fanout.
///
/// Producers never depend on listeners: zero listeners is valid, and a slow
/// listener observes an explicit `Lagged` error from Tokio rather than
/// blocking durable runtime work.
#[derive(Clone, Debug)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Result<Self, EventBusError> {
        if capacity == 0 {
            return Err(EventBusError::ZeroCapacity);
        }
        let (sender, _) = broadcast::channel(capacity);
        Ok(Self { sender })
    }

    pub fn subscribe(&self) -> EventReceiver {
        EventReceiver {
            receiver: self.sender.subscribe(),
        }
    }

    pub fn publish(&self, event: Event) -> usize {
        self.sender.send(event).unwrap_or(0)
    }
}

pub struct EventReceiver {
    receiver: broadcast::Receiver<Event>,
}

impl EventReceiver {
    pub async fn recv(&mut self) -> Result<Event, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EventBusError {
    #[error("event bus capacity must be positive")]
    ZeroCapacity,
}

#[cfg(test)]
mod tests {
    use mycel_agent_protocol::{AgentEvent, Event};

    use super::*;

    #[tokio::test]
    async fn event_bus_fans_out_without_requiring_a_listener() {
        let bus = EventBus::new(4).expect("valid capacity");
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        let event = Event {
            agent_id: "main".to_owned(),
            session_id: "s1".to_owned(),
            event: AgentEvent::Warning {
                message: "notice".to_owned(),
                code: None,
            },
        };
        assert_eq!(bus.publish(event.clone()), 2);
        assert_eq!(first.recv().await.expect("first event"), event);
        assert_eq!(second.recv().await.expect("second event"), event);

        drop(first);
        drop(second);
        assert_eq!(bus.publish(event), 0);
    }
}
