use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    BuiltinPortFuture, GoalBudgetLimits, GoalBudgetPort, GoalBudgetSnapshot, OrchestrationError,
    OrchestrationPorts,
};

const GOAL_SCOPE: &str = "goal";
const MAX_OBJECTIVE_CHARS: usize = 4_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub reason: Option<String>,
    #[serde(default)]
    pub budget: GoalBudgetState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBudgetState {
    pub turn_budget: Option<u64>,
    pub token_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
    pub turns_used: u64,
    pub tokens_used: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedGoal {
    pub id: String,
    pub objective: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalBoard {
    pub current: Option<Goal>,
    pub queue: Vec<QueuedGoal>,
    pub promotion_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromotionGate {
    pub session_matches: bool,
    pub idle: bool,
    pub user_queue_empty: bool,
    pub dispatch_pending: bool,
    pub compacting: bool,
}

impl PromotionGate {
    pub const fn ready() -> Self {
        Self {
            session_matches: true,
            idle: true,
            user_queue_empty: true,
            dispatch_pending: false,
            compacting: false,
        }
    }

    fn permits(self) -> bool {
        self.session_matches
            && self.idle
            && self.user_queue_empty
            && !self.dispatch_pending
            && !self.compacting
    }
}

pub struct GoalOrchestrator {
    ports: OrchestrationPorts,
    state: Mutex<GoalBoard>,
}

impl GoalOrchestrator {
    pub fn open(ports: OrchestrationPorts) -> Result<Self, GoalError> {
        let state = ports.restore(GOAL_SCOPE)?;
        Ok(Self {
            ports,
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> GoalBoard {
        lock(&self.state).clone()
    }

    pub fn create(&self, id: &str, objective: &str, replace: bool) -> Result<Goal, GoalError> {
        validate_id(id)?;
        let objective = normalize_objective(objective)?;
        let mut state = lock(&self.state);
        if state.current.is_some() && !replace {
            return Err(GoalError::AlreadyExists);
        }
        if contains_id(&state, id) && state.current.as_ref().is_none_or(|goal| goal.id != id) {
            return Err(GoalError::DuplicateId(id.to_owned()));
        }
        let now = self.ports.now_ms();
        let goal = Goal {
            id: id.to_owned(),
            objective,
            status: GoalStatus::Active,
            created_at_ms: now,
            updated_at_ms: now,
            reason: None,
            budget: GoalBudgetState::default(),
        };
        let mut next = state.clone();
        next.current = Some(goal.clone());
        next.promotion_pending = false;
        self.commit(
            &mut state,
            next,
            if replace { "replaced" } else { "created" },
            Some(id),
            json!({"replace": replace}),
        )?;
        Ok(goal)
    }

    pub fn enqueue(&self, id: &str, objective: &str) -> Result<QueuedGoal, GoalError> {
        validate_id(id)?;
        let objective = normalize_objective(objective)?;
        let mut state = lock(&self.state);
        if contains_id(&state, id) {
            return Err(GoalError::DuplicateId(id.to_owned()));
        }
        let now = self.ports.now_ms();
        let queued = QueuedGoal {
            id: id.to_owned(),
            objective,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let mut next = state.clone();
        next.queue.push(queued.clone());
        let position = next.queue.len();
        self.commit(
            &mut state,
            next,
            "queued",
            Some(id),
            json!({"position": position}),
        )?;
        Ok(queued)
    }

    pub fn pause(&self, reason: Option<&str>) -> Result<(), GoalError> {
        let reason = reason.map(normalize_reason).transpose()?;
        self.set_stopped(GoalStatus::Paused, reason.as_deref(), "paused")
    }

    pub fn block(&self, reason: &str) -> Result<(), GoalError> {
        let reason = normalize_reason(reason)?;
        self.set_stopped(GoalStatus::Blocked, Some(&reason), "blocked")
    }

    pub fn resume(&self) -> Result<(), GoalError> {
        let mut state = lock(&self.state);
        let current = state.current.as_ref().ok_or(GoalError::NotFound)?;
        if current.status == GoalStatus::Active {
            return Err(GoalError::InvalidTransition("goal is already active"));
        }
        let id = current.id.clone();
        let mut next = state.clone();
        let goal = next.current.as_mut().expect("checked current");
        goal.status = GoalStatus::Active;
        goal.updated_at_ms = self.ports.now_ms();
        goal.reason = None;
        self.commit(&mut state, next, "resumed", Some(&id), json!({}))
    }

    pub fn complete(&self, reason: &str) -> Result<Goal, GoalError> {
        let reason = normalize_reason(reason)?;
        let mut state = lock(&self.state);
        let current = state.current.as_ref().ok_or(GoalError::NotFound)?;
        if current.status != GoalStatus::Active {
            return Err(GoalError::InvalidTransition(
                "only an active goal may complete",
            ));
        }
        let completed = current.clone();
        let mut next = state.clone();
        next.current = None;
        next.promotion_pending = !next.queue.is_empty();
        self.commit(
            &mut state,
            next,
            "completed",
            Some(&completed.id),
            json!({
                "reason": reason,
                "objective": completed.objective.clone(),
                "budget": completed.budget,
            }),
        )?;
        Ok(completed)
    }

    pub fn cancel(&self, reason: Option<&str>) -> Result<Goal, GoalError> {
        let reason = reason.map(normalize_reason).transpose()?;
        let mut state = lock(&self.state);
        let cancelled = state.current.clone().ok_or(GoalError::NotFound)?;
        let mut next = state.clone();
        next.current = None;
        next.promotion_pending = false;
        self.commit(
            &mut state,
            next,
            "cancelled",
            Some(&cancelled.id),
            json!({"reason": reason}),
        )?;
        Ok(cancelled)
    }

    pub fn promote_next(&self, gate: PromotionGate) -> Result<Option<Goal>, GoalError> {
        let mut state = lock(&self.state);
        if !gate.permits()
            || state.current.is_some()
            || !state.promotion_pending
            || state.queue.is_empty()
        {
            return Ok(None);
        }
        let queued = state.queue[0].clone();
        let now = self.ports.now_ms();
        let goal = Goal {
            id: queued.id.clone(),
            objective: queued.objective.clone(),
            status: GoalStatus::Active,
            created_at_ms: queued.created_at_ms,
            updated_at_ms: now,
            reason: None,
            budget: GoalBudgetState::default(),
        };
        let mut next = state.clone();
        next.queue.remove(0);
        next.current = Some(goal.clone());
        next.promotion_pending = false;
        self.commit(&mut state, next, "promoted", Some(&goal.id), json!({}))?;
        Ok(Some(goal))
    }

    pub fn next(&self) -> Result<Goal, GoalError> {
        let mut state = lock(&self.state);
        let queued = state.queue.first().cloned().ok_or(GoalError::QueueEmpty)?;
        let now = self.ports.now_ms();
        let goal = Goal {
            id: queued.id.clone(),
            objective: queued.objective,
            status: GoalStatus::Active,
            created_at_ms: queued.created_at_ms,
            updated_at_ms: now,
            reason: None,
            budget: GoalBudgetState::default(),
        };
        let replaced = state.current.as_ref().map(|goal| goal.id.clone());
        let mut next = state.clone();
        next.queue.remove(0);
        next.current = Some(goal.clone());
        next.promotion_pending = false;
        self.commit(
            &mut state,
            next,
            "next",
            Some(&goal.id),
            json!({"replacedGoalId": replaced}),
        )?;
        Ok(goal)
    }

    /// Durable goal-budget view used by both the built-in and the turn driver.
    pub fn budget_snapshot(&self) -> GoalBudgetSnapshot {
        let state = lock(&self.state);
        budget_snapshot(&state, self.ports.now_ms())
    }

    /// Merge non-empty limits into the current goal. Limits at or below usage
    /// are rejected rather than retroactively overrunning an active goal.
    pub fn set_budget_limits(
        &self,
        limits: GoalBudgetLimits,
    ) -> Result<GoalBudgetSnapshot, GoalError> {
        validate_limits(limits)?;
        let mut state = lock(&self.state);
        let current = state.current.as_ref().ok_or(GoalError::NotFound)?;
        let mut merged = current.budget;
        if let Some(limit) = limits.turn_budget {
            merged.turn_budget = Some(limit);
        }
        if let Some(limit) = limits.token_budget {
            merged.token_budget = Some(limit);
        }
        if let Some(limit) = limits.wall_clock_budget_ms {
            merged.wall_clock_budget_ms = Some(limit);
        }
        let now = self.ports.now_ms();
        if budget_reached(merged, now.saturating_sub(current.created_at_ms)) {
            return Err(GoalError::BudgetAlreadyReached);
        }
        let id = current.id.clone();
        let mut next = state.clone();
        let goal = next.current.as_mut().expect("checked current");
        goal.budget = merged;
        goal.updated_at_ms = now;
        self.commit(
            &mut state,
            next,
            "budget_updated",
            Some(&id),
            json!({
                "turnBudget": limits.turn_budget,
                "tokenBudget": limits.token_budget,
                "wallClockBudgetMs": limits.wall_clock_budget_ms,
            }),
        )?;
        Ok(budget_snapshot(&state, now))
    }

    /// Record one completed model turn and its provider token usage. Reaching
    /// any configured limit blocks the goal in the same durable transition.
    pub fn record_turn_usage(&self, tokens: u64) -> Result<GoalBudgetSnapshot, GoalError> {
        let mut state = lock(&self.state);
        let current = state.current.as_ref().ok_or(GoalError::NotFound)?;
        if current.status != GoalStatus::Active {
            return Err(GoalError::InvalidTransition(
                "only an active goal may record usage",
            ));
        }
        let id = current.id.clone();
        let mut next = state.clone();
        let now = self.ports.now_ms();
        let goal = next.current.as_mut().expect("checked current");
        goal.budget.turns_used = goal
            .budget
            .turns_used
            .checked_add(1)
            .ok_or(GoalError::UsageOverflow)?;
        goal.budget.tokens_used = goal
            .budget
            .tokens_used
            .checked_add(tokens)
            .ok_or(GoalError::UsageOverflow)?;
        goal.updated_at_ms = now;
        let exhausted = budget_reached(goal.budget, now.saturating_sub(goal.created_at_ms));
        if exhausted {
            goal.status = GoalStatus::Blocked;
            goal.reason = Some("goal budget exhausted".to_owned());
        }
        self.commit(
            &mut state,
            next,
            if exhausted {
                "budget_exhausted"
            } else {
                "usage_recorded"
            },
            Some(&id),
            json!({"turns": 1, "tokens": tokens}),
        )?;
        Ok(budget_snapshot(&state, now))
    }

    /// Enforce elapsed wall time between turns, recording the blocked state
    /// exactly once if the wall-clock limit has been reached.
    pub fn enforce_budget(&self) -> Result<GoalBudgetSnapshot, GoalError> {
        let mut state = lock(&self.state);
        let now = self.ports.now_ms();
        let snapshot = budget_snapshot(&state, now);
        let Some(current) = state.current.as_ref() else {
            return Ok(snapshot);
        };
        if !snapshot.over_budget || current.status != GoalStatus::Active {
            return Ok(snapshot);
        }
        let id = current.id.clone();
        let mut next = state.clone();
        let goal = next.current.as_mut().expect("checked current");
        goal.status = GoalStatus::Blocked;
        goal.reason = Some("goal budget exhausted".to_owned());
        goal.updated_at_ms = now;
        self.commit(
            &mut state,
            next,
            "budget_exhausted",
            Some(&id),
            json!({"source": "wall_clock"}),
        )?;
        Ok(budget_snapshot(&state, now))
    }

    fn set_stopped(
        &self,
        status: GoalStatus,
        reason: Option<&str>,
        action: &str,
    ) -> Result<(), GoalError> {
        let mut state = lock(&self.state);
        let current = state.current.as_ref().ok_or(GoalError::NotFound)?;
        if current.status != GoalStatus::Active {
            return Err(GoalError::InvalidTransition(
                "only an active goal may be stopped",
            ));
        }
        let id = current.id.clone();
        let mut next = state.clone();
        let goal = next.current.as_mut().expect("checked current");
        goal.status = status;
        goal.reason = reason.map(str::to_owned);
        goal.updated_at_ms = self.ports.now_ms();
        self.commit(
            &mut state,
            next,
            action,
            Some(&id),
            json!({"reason": reason}),
        )
    }

    fn commit(
        &self,
        state: &mut GoalBoard,
        next: GoalBoard,
        action: &str,
        entity_id: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), GoalError> {
        let event = self
            .ports
            .persist(GOAL_SCOPE, action, entity_id, &next, detail)?;
        *state = next;
        self.ports.publish(event);
        Ok(())
    }
}

impl GoalBudgetPort for GoalOrchestrator {
    fn snapshot(&self) -> Result<GoalBudgetSnapshot, String> {
        Ok(self.budget_snapshot())
    }

    fn set_budget<'a>(
        &'a self,
        limits: GoalBudgetLimits,
    ) -> BuiltinPortFuture<'a, Result<GoalBudgetSnapshot, String>> {
        Box::pin(async move {
            self.set_budget_limits(limits)
                .map_err(|error| error.to_string())
        })
    }
}

fn budget_snapshot(state: &GoalBoard, now_ms: u64) -> GoalBudgetSnapshot {
    let Some(goal) = &state.current else {
        return GoalBudgetSnapshot::default();
    };
    let wall_clock_ms = now_ms.saturating_sub(goal.created_at_ms);
    GoalBudgetSnapshot {
        has_goal: true,
        turns_used: goal.budget.turns_used,
        tokens_used: goal.budget.tokens_used,
        wall_clock_ms,
        limits: GoalBudgetLimits {
            turn_budget: goal.budget.turn_budget,
            token_budget: goal.budget.token_budget,
            wall_clock_budget_ms: goal.budget.wall_clock_budget_ms,
        },
        over_budget: budget_reached(goal.budget, wall_clock_ms),
    }
}

fn budget_reached(budget: GoalBudgetState, wall_clock_ms: u64) -> bool {
    budget
        .turn_budget
        .is_some_and(|limit| budget.turns_used >= limit)
        || budget
            .token_budget
            .is_some_and(|limit| budget.tokens_used >= limit)
        || budget
            .wall_clock_budget_ms
            .is_some_and(|limit| wall_clock_ms >= limit)
}

fn validate_limits(limits: GoalBudgetLimits) -> Result<(), GoalError> {
    if limits.turn_budget == Some(0)
        || limits.token_budget == Some(0)
        || limits.wall_clock_budget_ms == Some(0)
        || (limits.turn_budget.is_none()
            && limits.token_budget.is_none()
            && limits.wall_clock_budget_ms.is_none())
    {
        return Err(GoalError::InvalidBudget);
    }
    Ok(())
}

fn contains_id(state: &GoalBoard, id: &str) -> bool {
    state.current.as_ref().is_some_and(|goal| goal.id == id)
        || state.queue.iter().any(|goal| goal.id == id)
}

fn validate_id(id: &str) -> Result<(), GoalError> {
    if id.trim().is_empty() || id.len() > 160 || id.chars().any(char::is_control) {
        return Err(GoalError::InvalidId);
    }
    Ok(())
}

fn normalize_objective(value: &str) -> Result<String, GoalError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GoalError::EmptyObjective);
    }
    if value.chars().count() > MAX_OBJECTIVE_CHARS {
        return Err(GoalError::ObjectiveTooLong);
    }
    Ok(value.to_owned())
}

fn normalize_reason(value: &str) -> Result<String, GoalError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(GoalError::EmptyReason);
    }
    Ok(value.to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    #[error("a goal already exists")]
    AlreadyExists,
    #[error("goal not found")]
    NotFound,
    #[error("goal queue is empty")]
    QueueEmpty,
    #[error("goal id is invalid")]
    InvalidId,
    #[error("goal id {0:?} already exists")]
    DuplicateId(String),
    #[error("goal objective must not be empty")]
    EmptyObjective,
    #[error("goal objective exceeds 4000 characters")]
    ObjectiveTooLong,
    #[error("goal reason must not be empty")]
    EmptyReason,
    #[error("invalid goal transition: {0}")]
    InvalidTransition(&'static str),
    #[error("goal budget must contain positive limits")]
    InvalidBudget,
    #[error("goal budget is already reached by current usage")]
    BudgetAlreadyReached,
    #[error("goal usage counter overflowed")]
    UsageOverflow,
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}
