use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;

use crate::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAccessMode {
    Read,
    Write,
    ReadWrite,
    Search,
}

impl FileAccessMode {
    fn writes(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolAccess {
    None,
    All,
    File { path: PathBuf, mode: FileAccessMode },
}

impl ToolAccess {
    pub fn file(path: impl Into<PathBuf>, mode: FileAccessMode) -> Self {
        Self::File {
            path: normalize_path(&path.into()),
            mode,
        }
    }

    pub fn conflicts_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, _) | (_, Self::None) => false,
            (Self::All, _) | (_, Self::All) => true,
            (
                Self::File {
                    path: left,
                    mode: left_mode,
                },
                Self::File {
                    path: right,
                    mode: right_mode,
                },
            ) => paths_overlap(left, right) && (left_mode.writes() || right_mode.writes()),
        }
    }
}

/// Fair conflict scheduler for tool execution.
///
/// A request may bypass earlier requests only when it conflicts with neither
/// active work nor those earlier waiters. This preserves useful parallelism
/// while preventing an unbounded stream of readers from starving a writer.
#[derive(Clone, Default)]
pub struct ToolScheduler {
    inner: Arc<SchedulerInner>,
}

#[derive(Default)]
struct SchedulerInner {
    state: Mutex<SchedulerState>,
    notify: Notify,
}

#[derive(Default)]
struct SchedulerState {
    next_ticket: u64,
    active: BTreeMap<u64, Vec<ToolAccess>>,
    waiting: BTreeMap<u64, Vec<ToolAccess>>,
}

impl ToolScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn acquire(
        &self,
        mut accesses: Vec<ToolAccess>,
        cancellation: &CancellationToken,
    ) -> Result<ToolPermit, ScheduleError> {
        if cancellation.is_cancelled() {
            return Err(ScheduleError::Cancelled);
        }
        if accesses.is_empty() {
            accesses.push(ToolAccess::All);
        }
        for access in &mut accesses {
            if let ToolAccess::File { path, .. } = access {
                *path = normalize_path(path);
            }
        }

        let ticket = {
            let mut state = lock(&self.inner.state);
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.waiting.insert(ticket, accesses.clone());
            ticket
        };

        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut state = lock(&self.inner.state);
                if cancellation.is_cancelled() {
                    state.waiting.remove(&ticket);
                    drop(state);
                    self.inner.notify.notify_waiters();
                    return Err(ScheduleError::Cancelled);
                }
                let conflicts_active = state
                    .active
                    .values()
                    .any(|active| access_sets_conflict(&accesses, active));
                let conflicts_earlier = state
                    .waiting
                    .range(..ticket)
                    .any(|(_, earlier)| access_sets_conflict(&accesses, earlier));
                if !conflicts_active && !conflicts_earlier {
                    state.waiting.remove(&ticket);
                    state.active.insert(ticket, accesses);
                    return Ok(ToolPermit {
                        ticket,
                        inner: Arc::clone(&self.inner),
                        released: false,
                    });
                }
            }

            tokio::select! {
                _ = cancellation.cancelled() => {
                    let mut state = lock(&self.inner.state);
                    state.waiting.remove(&ticket);
                    drop(state);
                    self.inner.notify.notify_waiters();
                    return Err(ScheduleError::Cancelled);
                }
                _ = &mut notified => {}
            }
        }
    }

    pub fn active_count(&self) -> usize {
        lock(&self.inner.state).active.len()
    }

    pub fn queued_count(&self) -> usize {
        lock(&self.inner.state).waiting.len()
    }
}

pub struct ToolPermit {
    ticket: u64,
    inner: Arc<SchedulerInner>,
    released: bool,
}

impl ToolPermit {
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        lock(&self.inner.state).active.remove(&self.ticket);
        self.released = true;
        self.inner.notify.notify_waiters();
    }
}

impl Drop for ToolPermit {
    fn drop(&mut self) {
        self.release_inner();
    }
}

fn access_sets_conflict(left: &[ToolAccess], right: &[ToolAccess]) -> bool {
    left.iter()
        .any(|left| right.iter().any(|right| left.conflicts_with(right)))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut prefix: Option<OsString> = None;
    let mut rooted = false;
    let mut components: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if components
                    .last()
                    .is_some_and(|value| value.as_encoded_bytes() != b"..")
                {
                    components.pop();
                } else if !rooted {
                    components.push(OsString::from(".."));
                }
            }
            Component::Normal(value) => components.push(value.to_owned()),
        }
    }
    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    if rooted {
        normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }
    normalized.extend(components);
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    normalized
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("tool scheduling was cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn access_conflicts_use_recursive_path_overlap() {
        let read = ToolAccess::file("a/./b", FileAccessMode::Read);
        let search = ToolAccess::file("a/b/c", FileAccessMode::Search);
        let write = ToolAccess::file("a/x/../b/c", FileAccessMode::Write);
        assert!(!read.conflicts_with(&search));
        assert!(read.conflicts_with(&write));
        assert!(!ToolAccess::All.conflicts_with(&ToolAccess::None));
    }

    #[tokio::test]
    async fn cancellation_removes_a_waiter() {
        let scheduler = ToolScheduler::new();
        let running = scheduler
            .acquire(
                vec![ToolAccess::file("a", FileAccessMode::Write)],
                &CancellationToken::new(),
            )
            .await
            .expect("running");
        let cancellation = CancellationToken::new();
        let waiter_scheduler = scheduler.clone();
        let waiter_token = cancellation.clone();
        let waiter = tokio::spawn(async move {
            waiter_scheduler
                .acquire(
                    vec![ToolAccess::file("a", FileAccessMode::Read)],
                    &waiter_token,
                )
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(scheduler.queued_count(), 1);
        cancellation.cancel();
        assert!(matches!(
            waiter.await.expect("join"),
            Err(ScheduleError::Cancelled)
        ));
        assert_eq!(scheduler.queued_count(), 0);
        drop(running);
    }

    #[tokio::test]
    async fn an_earlier_writer_is_not_starved_by_later_readers() {
        let scheduler = ToolScheduler::new();
        let initial = scheduler
            .acquire(
                vec![ToolAccess::file("tree", FileAccessMode::Read)],
                &CancellationToken::new(),
            )
            .await
            .expect("initial read");

        let (writer_acquired, writer_rx) = oneshot::channel();
        let writer_scheduler = scheduler.clone();
        let writer = tokio::spawn(async move {
            let permit = writer_scheduler
                .acquire(
                    vec![ToolAccess::file("tree", FileAccessMode::Write)],
                    &CancellationToken::new(),
                )
                .await
                .expect("writer");
            let _ = writer_acquired.send(());
            tokio::task::yield_now().await;
            drop(permit);
        });
        while scheduler.queued_count() != 1 {
            tokio::task::yield_now().await;
        }

        let (reader_acquired, mut reader_rx) = oneshot::channel();
        let reader_scheduler = scheduler.clone();
        let reader = tokio::spawn(async move {
            let permit = reader_scheduler
                .acquire(
                    vec![ToolAccess::file("tree", FileAccessMode::Read)],
                    &CancellationToken::new(),
                )
                .await
                .expect("reader");
            let _ = reader_acquired.send(());
            drop(permit);
        });
        while scheduler.queued_count() != 2 {
            tokio::task::yield_now().await;
        }
        drop(initial);
        writer_rx.await.expect("writer must acquire first");
        assert!(matches!(
            reader_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        writer.await.expect("writer join");
        reader.await.expect("reader join");
    }
}
