use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::Notify;

/// Cloneable cooperative cancellation shared by sessions, turns, and tools.
///
/// Cancellation is monotonic. `cancelled` uses a register-then-recheck loop so
/// a cancellation racing waiter registration cannot be missed.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) -> bool {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.notify.notify_waiters();
        true
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_is_monotonic_and_wakes_waiters() {
        let token = CancellationToken::new();
        let waiter = token.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        assert!(token.cancel());
        assert!(!token.cancel());
        task.await.expect("waiter must wake");
        token.cancelled().await;
    }
}
