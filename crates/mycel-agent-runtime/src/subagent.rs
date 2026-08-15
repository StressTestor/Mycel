use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{OrchestrationError, OrchestrationPorts};

const SUBAGENT_SCOPE: &str = "subagent";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySet {
    pub tools: BTreeSet<String>,
    pub filesystem_roots: BTreeSet<String>,
    pub network: bool,
    pub can_spawn_subagents: bool,
    pub can_swarm: bool,
    pub can_workflow: bool,
}

impl CapabilitySet {
    pub fn is_subset_of(&self, parent: &Self) -> bool {
        self.tools.is_subset(&parent.tools)
            && self
                .filesystem_roots
                .iter()
                .all(|child| root_is_allowed(child, &parent.filesystem_roots))
            && (!self.network || parent.network)
            && (!self.can_spawn_subagents || parent.can_spawn_subagents)
            && (!self.can_swarm || parent.can_swarm)
            && (!self.can_workflow || parent.can_workflow)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerProfile {
    pub name: String,
    pub capabilities: CapabilitySet,
    /// Explicit opt-in. Workflow worker profiles must leave this false.
    pub allow_delegation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Lost,
}

impl SubagentStatus {
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentState {
    pub id: String,
    pub parent_id: Option<String>,
    pub profile_name: String,
    pub capabilities: CapabilitySet,
    pub allow_delegation: bool,
    pub detached: bool,
    pub status: SubagentStatus,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentBoard {
    pub agents: BTreeMap<String, SubagentState>,
}

pub struct SubagentRegistry {
    ports: OrchestrationPorts,
    state: Mutex<SubagentBoard>,
}

impl SubagentRegistry {
    pub fn open(ports: OrchestrationPorts) -> Result<Self, SubagentError> {
        let state = ports.restore(SUBAGENT_SCOPE)?;
        Ok(Self {
            ports,
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> SubagentBoard {
        lock(&self.state).clone()
    }

    pub fn get(&self, id: &str) -> Option<SubagentState> {
        lock(&self.state).agents.get(id).cloned()
    }

    pub fn register_root(
        &self,
        id: &str,
        capabilities: CapabilitySet,
    ) -> Result<SubagentState, SubagentError> {
        validate_agent_id(id)?;
        let mut state = lock(&self.state);
        if state.agents.contains_key(id) {
            return Err(SubagentError::Duplicate(id.to_owned()));
        }
        let agent = SubagentState {
            id: id.to_owned(),
            parent_id: None,
            profile_name: "root".to_owned(),
            capabilities,
            allow_delegation: true,
            detached: true,
            status: SubagentStatus::Running,
            started_at_ms: self.ports.now_ms(),
            ended_at_ms: None,
            reason: None,
        };
        let mut next = state.clone();
        next.agents.insert(id.to_owned(), agent.clone());
        self.commit(&mut state, next, "root_registered", Some(id), json!({}))?;
        Ok(agent)
    }

    pub fn spawn(
        &self,
        id: &str,
        parent_id: &str,
        profile: WorkerProfile,
        detached: bool,
    ) -> Result<SubagentState, SubagentError> {
        validate_agent_id(id)?;
        validate_profile(&profile)?;
        let mut state = lock(&self.state);
        if state.agents.contains_key(id) {
            return Err(SubagentError::Duplicate(id.to_owned()));
        }
        let parent = state
            .agents
            .get(parent_id)
            .ok_or_else(|| SubagentError::ParentNotFound(parent_id.to_owned()))?;
        if parent.status != SubagentStatus::Running {
            return Err(SubagentError::ParentNotRunning(parent_id.to_owned()));
        }
        if !parent.capabilities.can_spawn_subagents || !parent.allow_delegation {
            return Err(SubagentError::RecursionDenied(parent_id.to_owned()));
        }
        if !profile.capabilities.is_subset_of(&parent.capabilities) {
            return Err(SubagentError::CapabilityEscalation);
        }
        let agent = SubagentState {
            id: id.to_owned(),
            parent_id: Some(parent_id.to_owned()),
            profile_name: profile.name,
            capabilities: profile.capabilities,
            allow_delegation: profile.allow_delegation,
            detached,
            status: SubagentStatus::Running,
            started_at_ms: self.ports.now_ms(),
            ended_at_ms: None,
            reason: None,
        };
        let mut next = state.clone();
        next.agents.insert(id.to_owned(), agent.clone());
        self.commit(
            &mut state,
            next,
            "spawned",
            Some(id),
            json!({"parentId": parent_id, "detached": detached}),
        )?;
        Ok(agent)
    }

    pub fn finish(
        &self,
        id: &str,
        status: SubagentStatus,
        reason: Option<&str>,
    ) -> Result<SubagentState, SubagentError> {
        if !matches!(status, SubagentStatus::Completed | SubagentStatus::Failed) {
            return Err(SubagentError::InvalidSettlement);
        }
        self.set_terminal(id, status, reason, "finished")
    }

    /// Mark a running foreground child as detached from its parent turn.
    ///
    /// The executor release is owned by the background registry; this record
    /// keeps restart and parent-cancellation policy aligned with that durable
    /// task transition.
    pub fn detach(&self, id: &str) -> Result<bool, SubagentError> {
        let mut state = lock(&self.state);
        let current = state.agents.get(id).ok_or(SubagentError::NotFound)?;
        if current.status.is_terminal() {
            return Err(SubagentError::AlreadyTerminal);
        }
        if current.detached {
            return Ok(false);
        }
        let mut next = state.clone();
        next.agents.get_mut(id).expect("checked agent").detached = true;
        self.commit(&mut state, next, "detached", Some(id), json!({}))?;
        Ok(true)
    }

    /// Mark one running child cancelled after its executor has acknowledged
    /// cancellation. This is deliberately separate from `finish`, whose
    /// callers represent normal completion or failure.
    pub fn cancel(&self, id: &str, reason: &str) -> Result<SubagentState, SubagentError> {
        self.set_terminal(id, SubagentStatus::Cancelled, Some(reason), "cancelled")
    }

    pub fn resume(&self, id: &str) -> Result<SubagentState, SubagentError> {
        let mut state = lock(&self.state);
        let current = state.agents.get(id).ok_or(SubagentError::NotFound)?;
        if current.parent_id.is_none() || !current.status.is_terminal() {
            return Err(SubagentError::InvalidResume);
        }
        let mut next = state.clone();
        let agent = next.agents.get_mut(id).expect("checked agent");
        agent.status = SubagentStatus::Running;
        agent.started_at_ms = self.ports.now_ms();
        agent.ended_at_ms = None;
        agent.reason = None;
        let resumed = agent.clone();
        self.commit(&mut state, next, "resumed", Some(id), json!({}))?;
        Ok(resumed)
    }

    pub fn cancel_children(
        &self,
        parent_id: &str,
        reason: &str,
    ) -> Result<Vec<String>, SubagentError> {
        let reason = normalize_reason(reason)?;
        let mut state = lock(&self.state);
        if !state.agents.contains_key(parent_id) {
            return Err(SubagentError::ParentNotFound(parent_id.to_owned()));
        }
        let ids = state
            .agents
            .values()
            .filter(|agent| {
                agent.parent_id.as_deref() == Some(parent_id)
                    && agent.status == SubagentStatus::Running
                    && !agent.detached
            })
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(ids);
        }
        let now = self.ports.now_ms();
        let mut next = state.clone();
        for id in &ids {
            let agent = next.agents.get_mut(id).expect("selected agent");
            agent.status = SubagentStatus::Cancelled;
            agent.ended_at_ms = Some(now);
            agent.reason = Some(reason.clone());
        }
        self.commit(
            &mut state,
            next,
            "children_cancelled",
            Some(parent_id),
            json!({"childIds": ids, "reason": reason}),
        )?;
        Ok(ids)
    }

    pub fn reconcile(
        &self,
        active_agent_ids: &BTreeSet<String>,
    ) -> Result<Vec<SubagentState>, SubagentError> {
        let mut state = lock(&self.state);
        let ids = state
            .agents
            .values()
            .filter(|agent| {
                agent.parent_id.is_some()
                    && agent.status == SubagentStatus::Running
                    && !active_agent_ids.contains(&agent.id)
            })
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.ports.now_ms();
        let mut next = state.clone();
        for id in &ids {
            let agent = next.agents.get_mut(id).expect("selected agent");
            agent.status = SubagentStatus::Lost;
            agent.ended_at_ms = Some(now);
            agent.reason = Some("agent was not active after restart".to_owned());
        }
        let lost = ids
            .iter()
            .filter_map(|id| next.agents.get(id).cloned())
            .collect::<Vec<_>>();
        self.commit(
            &mut state,
            next,
            "reconciled_lost",
            None,
            json!({"agentIds": ids}),
        )?;
        Ok(lost)
    }

    fn set_terminal(
        &self,
        id: &str,
        status: SubagentStatus,
        reason: Option<&str>,
        action: &str,
    ) -> Result<SubagentState, SubagentError> {
        let mut state = lock(&self.state);
        let current = state.agents.get(id).ok_or(SubagentError::NotFound)?;
        if current.status.is_terminal() || current.parent_id.is_none() {
            return Err(SubagentError::AlreadyTerminal);
        }
        let reason = reason.map(normalize_reason).transpose()?;
        let mut next = state.clone();
        let agent = next.agents.get_mut(id).expect("checked agent");
        agent.status = status;
        agent.ended_at_ms = Some(self.ports.now_ms());
        agent.reason.clone_from(&reason);
        let finished = agent.clone();
        self.commit(
            &mut state,
            next,
            action,
            Some(id),
            json!({"status": status, "reason": reason}),
        )?;
        Ok(finished)
    }

    fn commit(
        &self,
        state: &mut SubagentBoard,
        next: SubagentBoard,
        action: &str,
        entity_id: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<(), SubagentError> {
        let event = self
            .ports
            .persist(SUBAGENT_SCOPE, action, entity_id, &next, detail)?;
        *state = next;
        self.ports.publish(event);
        Ok(())
    }
}

fn root_is_allowed(child: &str, parents: &BTreeSet<String>) -> bool {
    let Some(child) = normalized_absolute_path(child) else {
        return false;
    };
    parents.iter().any(|parent| {
        normalized_absolute_path(parent).is_some_and(|parent| child.starts_with(parent))
    })
}

fn normalized_absolute_path(value: &str) -> Option<&Path> {
    let path = Path::new(value);
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        None
    } else {
        Some(path)
    }
}

fn validate_profile(profile: &WorkerProfile) -> Result<(), SubagentError> {
    if profile.name.trim().is_empty() || profile.name.len() > 64 {
        return Err(SubagentError::InvalidProfile);
    }
    if profile.allow_delegation && !profile.capabilities.can_spawn_subagents {
        return Err(SubagentError::InvalidProfile);
    }
    Ok(())
}

fn validate_agent_id(id: &str) -> Result<(), SubagentError> {
    if id.trim().is_empty() || id.len() > 160 || id.chars().any(char::is_control) {
        return Err(SubagentError::InvalidId);
    }
    Ok(())
}

fn normalize_reason(reason: &str) -> Result<String, SubagentError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(SubagentError::EmptyReason);
    }
    Ok(reason.to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("invalid agent id")]
    InvalidId,
    #[error("invalid worker profile")]
    InvalidProfile,
    #[error("agent {0:?} already exists")]
    Duplicate(String),
    #[error("agent not found")]
    NotFound,
    #[error("parent agent {0:?} not found")]
    ParentNotFound(String),
    #[error("parent agent {0:?} is not running")]
    ParentNotRunning(String),
    #[error("parent agent {0:?} may not delegate recursively")]
    RecursionDenied(String),
    #[error("child capabilities exceed the parent capability set")]
    CapabilityEscalation,
    #[error("subagent is already terminal")]
    AlreadyTerminal,
    #[error("subagent cannot be resumed")]
    InvalidResume,
    #[error("invalid subagent settlement")]
    InvalidSettlement,
    #[error("subagent reason must not be empty")]
    EmptyReason,
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}
