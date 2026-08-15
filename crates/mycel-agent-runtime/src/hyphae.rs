use std::sync::{Mutex, MutexGuard};

use mycel_agent_protocol::ThinkingEffort;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{OrchestrationError, OrchestrationPorts};

const HYPHAE_SCOPE: &str = "hyphae";
const MAX_TASK_CHARS: usize = 100_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HyphaeSwarmMode {
    #[default]
    Off,
    Standing,
    Task,
}

/// Session-local mode only. Persisting this snapshot means replaying the
/// current session; adapters must never copy it into user or project config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HyphaeState {
    pub thinking_effort: Option<ThinkingEffort>,
    pub swarm_mode: HyphaeSwarmMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HyphaeTransition {
    pub state: HyphaeState,
    pub submit_prompt: Option<String>,
    /// The session adapter should update its provider request template when
    /// this is true. This is never a persistent model-config mutation.
    pub effort_changed: bool,
}

pub struct HyphaeReducer {
    ports: OrchestrationPorts,
    state: Mutex<HyphaeState>,
}

impl HyphaeReducer {
    pub fn open(
        ports: OrchestrationPorts,
        current_effort: Option<ThinkingEffort>,
    ) -> Result<Self, HyphaeError> {
        let mut state: HyphaeState = ports.restore(HYPHAE_SCOPE)?;
        if state.thinking_effort.is_none() {
            state.thinking_effort = current_effort;
        }
        if state
            .thinking_effort
            .as_ref()
            .is_some_and(|effort| effort.as_str().trim().is_empty())
        {
            return Err(HyphaeError::InvalidRestoredState);
        }
        Ok(Self {
            ports,
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> HyphaeState {
        lock(&self.state).clone()
    }

    /// Reduce `/hyphae` arguments. `xhigh_supported` must come from the
    /// selected model's validated capability catalog. An already-xhigh
    /// session can still turn the mode on when the catalog is unavailable,
    /// because no effort switch is required.
    pub fn apply(
        &self,
        arguments: &str,
        xhigh_supported: bool,
    ) -> Result<HyphaeTransition, HyphaeError> {
        let mut state = lock(&self.state);
        let command = HyphaeCommand::parse(arguments, state.swarm_mode)?;
        let mut next = state.clone();
        let mut submit_prompt = None;
        let action;

        match command {
            HyphaeCommand::Off => {
                next.swarm_mode = HyphaeSwarmMode::Off;
                action = "disabled";
            }
            HyphaeCommand::On => {
                select_xhigh(&mut next, xhigh_supported)?;
                next.swarm_mode = HyphaeSwarmMode::Standing;
                action = "enabled";
            }
            HyphaeCommand::Task(prompt) => {
                select_xhigh(&mut next, xhigh_supported)?;
                next.swarm_mode = HyphaeSwarmMode::Task;
                submit_prompt = Some(prompt);
                action = "task_enabled";
            }
        }

        let effort_changed = state.thinking_effort != next.thinking_effort;
        if *state != next || submit_prompt.is_some() {
            let event = self.ports.persist(
                HYPHAE_SCOPE,
                action,
                None,
                &next,
                json!({
                    "swarmMode": next.swarm_mode,
                    "effortChanged": effort_changed,
                    "submitPrompt": submit_prompt,
                }),
            )?;
            *state = next.clone();
            self.ports.publish(event);
        }
        Ok(HyphaeTransition {
            state: next,
            submit_prompt,
            effort_changed,
        })
    }

    /// Clear the one-shot authorization after its submitted turn is accepted.
    /// Standing mode is intentionally unaffected.
    pub fn finish_task(&self) -> Result<HyphaeState, HyphaeError> {
        let mut state = lock(&self.state);
        if state.swarm_mode != HyphaeSwarmMode::Task {
            return Err(HyphaeError::NoTaskMode);
        }
        let mut next = state.clone();
        next.swarm_mode = HyphaeSwarmMode::Off;
        let event = self.ports.persist(
            HYPHAE_SCOPE,
            "task_finished",
            None,
            &next,
            json!({"thinkingEffortUnchanged": true}),
        )?;
        *state = next.clone();
        self.ports.publish(event);
        Ok(next)
    }
}

enum HyphaeCommand {
    On,
    Off,
    Task(String),
}

impl HyphaeCommand {
    fn parse(arguments: &str, current_mode: HyphaeSwarmMode) -> Result<Self, HyphaeError> {
        let prompt = arguments.trim();
        if prompt.is_empty() {
            return Ok(if current_mode == HyphaeSwarmMode::Off {
                Self::On
            } else {
                Self::Off
            });
        }
        if prompt.eq_ignore_ascii_case("on") {
            return Ok(Self::On);
        }
        if prompt.eq_ignore_ascii_case("off") {
            return Ok(Self::Off);
        }
        if prompt.chars().count() > MAX_TASK_CHARS {
            return Err(HyphaeError::TaskTooLarge);
        }
        Ok(Self::Task(prompt.to_owned()))
    }
}

fn select_xhigh(state: &mut HyphaeState, xhigh_supported: bool) -> Result<(), HyphaeError> {
    if state
        .thinking_effort
        .as_ref()
        .is_some_and(|effort| effort.as_str() == "xhigh")
    {
        return Ok(());
    }
    if !xhigh_supported {
        return Err(HyphaeError::XhighUnsupported);
    }
    state.thinking_effort =
        Some(ThinkingEffort::new("xhigh").expect("the static xhigh effort is non-empty"));
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum HyphaeError {
    #[error("the selected model does not support xhigh thinking effort")]
    XhighUnsupported,
    #[error("Hyphae task prompt is too large")]
    TaskTooLarge,
    #[error("Hyphae is not in one-shot task mode")]
    NoTaskMode,
    #[error("restored Hyphae state contains an invalid thinking effort")]
    InvalidRestoredState,
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}
