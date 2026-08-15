use std::{collections::BTreeMap, sync::Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{OrchestrationError, OrchestrationPorts};

const BACKGROUND_SCOPE: &str = "background";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    Process,
    Subagent,
    Question,
    Workflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BackgroundMode {
    Foreground,
    Detached { keep_alive: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
    Lost,
}

impl BackgroundStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskState {
    pub id: String,
    pub kind: BackgroundKind,
    pub description: String,
    pub mode: BackgroundMode,
    pub status: BackgroundStatus,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub stop_reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundBoard {
    pub tasks: BTreeMap<String, BackgroundTaskState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundShutdown {
    StopAll,
    StopUnlessKeepAlive,
}

pub struct BackgroundRegistry {
    ports: OrchestrationPorts,
    state: Mutex<BackgroundBoard>,
}

impl BackgroundRegistry {
    pub fn open(ports: OrchestrationPorts) -> Result<Self, BackgroundError> {
        let state = ports.restore(BACKGROUND_SCOPE)?;
        Ok(Self {
            ports,
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> BackgroundBoard {
        lock(&self.state).clone()
    }

    pub fn get(&self, id: &str) -> Option<BackgroundTaskState> {
        lock(&self.state).tasks.get(id).cloned()
    }

    pub fn list(&self, active_only: bool) -> Vec<BackgroundTaskState> {
        lock(&self.state)
            .tasks
            .values()
            .filter(|task| !active_only || !task.status.is_terminal())
            .cloned()
            .collect()
    }

    pub fn register(
        &self,
        id: &str,
        kind: BackgroundKind,
        description: &str,
        mode: BackgroundMode,
        timeout_ms: Option<u64>,
    ) -> Result<BackgroundTaskState, BackgroundError> {
        validate_task_id(id)?;
        let description = description.trim();
        if description.is_empty() {
            return Err(BackgroundError::EmptyDescription);
        }
        if timeout_ms == Some(0) {
            return Err(BackgroundError::ZeroTimeout);
        }
        let mut state = lock(&self.state);
        if state.tasks.contains_key(id) {
            return Err(BackgroundError::Duplicate(id.to_owned()));
        }
        let task = BackgroundTaskState {
            id: id.to_owned(),
            kind,
            description: description.to_owned(),
            mode,
            status: BackgroundStatus::Running,
            started_at_ms: self.ports.now_ms(),
            ended_at_ms: None,
            timeout_ms,
            stop_reason: None,
        };
        let mut next = state.clone();
        next.tasks.insert(id.to_owned(), task.clone());
        self.commit(&mut state, next, "registered", Some(id), json!({}))?;
        Ok(task)
    }

    pub fn detach(&self, id: &str, keep_alive: bool) -> Result<bool, BackgroundError> {
        let mut state = lock(&self.state);
        let task = state.tasks.get(id).ok_or(BackgroundError::NotFound)?;
        if task.status.is_terminal() {
            return Err(BackgroundError::Terminal(id.to_owned()));
        }
        let mode = BackgroundMode::Detached { keep_alive };
        if task.mode == mode {
            return Ok(false);
        }
        let mut next = state.clone();
        next.tasks.get_mut(id).expect("checked task").mode = mode;
        self.commit(
            &mut state,
            next,
            "detached",
            Some(id),
            json!({"keepAlive": keep_alive}),
        )?;
        Ok(true)
    }

    pub fn settle(
        &self,
        id: &str,
        status: BackgroundStatus,
        reason: Option<&str>,
    ) -> Result<BackgroundTaskState, BackgroundError> {
        if !status.is_terminal() || status == BackgroundStatus::Lost {
            return Err(BackgroundError::InvalidSettlement);
        }
        let mut state = lock(&self.state);
        let current = state.tasks.get(id).ok_or(BackgroundError::NotFound)?;
        if current.status.is_terminal() {
            return Err(BackgroundError::Terminal(id.to_owned()));
        }
        let mut next = state.clone();
        let task = next.tasks.get_mut(id).expect("checked task");
        task.status = status;
        task.ended_at_ms = Some(self.ports.now_ms());
        task.stop_reason = normalize_optional_reason(reason)?;
        let settled = task.clone();
        self.commit(
            &mut state,
            next,
            "settled",
            Some(id),
            json!({"status": status, "reason": reason}),
        )?;
        Ok(settled)
    }

    pub fn shutdown(&self, policy: BackgroundShutdown) -> Result<Vec<String>, BackgroundError> {
        let mut state = lock(&self.state);
        let stopped = state
            .tasks
            .values()
            .filter(|task| {
                task.status == BackgroundStatus::Running
                    && match (policy, task.mode) {
                        (BackgroundShutdown::StopAll, _) => true,
                        (BackgroundShutdown::StopUnlessKeepAlive, BackgroundMode::Foreground) => {
                            true
                        }
                        (
                            BackgroundShutdown::StopUnlessKeepAlive,
                            BackgroundMode::Detached { keep_alive },
                        ) => !keep_alive,
                    }
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        if stopped.is_empty() {
            return Ok(stopped);
        }
        let now = self.ports.now_ms();
        let mut next = state.clone();
        for id in &stopped {
            let task = next.tasks.get_mut(id).expect("selected task");
            task.status = BackgroundStatus::Killed;
            task.ended_at_ms = Some(now);
            task.stop_reason = Some("session shutdown".to_owned());
        }
        self.commit(
            &mut state,
            next,
            "shutdown",
            None,
            json!({"taskIds": stopped}),
        )?;
        Ok(stopped)
    }

    /// Marks every persisted running task without a live executor as lost.
    pub fn reconcile(
        &self,
        active_executor_ids: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<BackgroundTaskState>, BackgroundError> {
        let mut state = lock(&self.state);
        let ids = state
            .tasks
            .values()
            .filter(|task| {
                task.status == BackgroundStatus::Running && !active_executor_ids.contains(&task.id)
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.ports.now_ms();
        let mut next = state.clone();
        for id in &ids {
            let task = next.tasks.get_mut(id).expect("selected task");
            task.status = BackgroundStatus::Lost;
            task.ended_at_ms = Some(now);
            task.stop_reason = Some("executor was not present after restart".to_owned());
        }
        let lost = ids
            .iter()
            .filter_map(|id| next.tasks.get(id).cloned())
            .collect::<Vec<_>>();
        self.commit(
            &mut state,
            next,
            "reconciled_lost",
            None,
            json!({"taskIds": ids}),
        )?;
        Ok(lost)
    }

    fn commit(
        &self,
        state: &mut BackgroundBoard,
        next: BackgroundBoard,
        action: &str,
        entity_id: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), BackgroundError> {
        let event = self
            .ports
            .persist(BACKGROUND_SCOPE, action, entity_id, &next, detail)?;
        *state = next;
        self.ports.publish(event);
        Ok(())
    }
}

fn validate_task_id(id: &str) -> Result<(), BackgroundError> {
    let Some((prefix, suffix)) = id.rsplit_once('-') else {
        return Err(BackgroundError::InvalidId(id.to_owned()));
    };
    if prefix.is_empty()
        || suffix.len() != 8
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(BackgroundError::InvalidId(id.to_owned()));
    }
    Ok(())
}

fn normalize_optional_reason(reason: Option<&str>) -> Result<Option<String>, BackgroundError> {
    let Some(reason) = reason else {
        return Ok(None);
    };
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(BackgroundError::EmptyReason);
    }
    Ok(Some(reason.to_owned()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum BackgroundError {
    #[error("invalid background task id {0:?}")]
    InvalidId(String),
    #[error("background task {0:?} already exists")]
    Duplicate(String),
    #[error("background task not found")]
    NotFound,
    #[error("background task {0:?} is already terminal")]
    Terminal(String),
    #[error("background task description must not be empty")]
    EmptyDescription,
    #[error("background task timeout must be positive")]
    ZeroTimeout,
    #[error("background task reason must not be empty")]
    EmptyReason,
    #[error("invalid background settlement status")]
    InvalidSettlement,
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}
