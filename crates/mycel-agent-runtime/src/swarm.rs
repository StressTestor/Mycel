use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const MAX_SWARM_FAN_OUT: usize = 128;
pub const SWARM_ITEM_PLACEHOLDER: &str = "{{item}}";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SwarmMemberKind {
    Spawn { item: String, profile_name: String },
    Resume { agent_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmMemberSpec {
    pub index: usize,
    pub prompt: String,
    pub kind: SwarmMemberKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwarmPlan {
    pub description: String,
    pub max_concurrency: usize,
    pub members: Vec<SwarmMemberSpec>,
}

impl SwarmPlan {
    pub fn waves(&self) -> Vec<Vec<SwarmMemberSpec>> {
        self.members
            .chunks(self.max_concurrency)
            .map(<[SwarmMemberSpec]>::to_vec)
            .collect()
    }
}

pub struct SwarmPlanner {
    max_fan_out: usize,
    max_concurrency: usize,
}

impl SwarmPlanner {
    pub fn new(max_fan_out: usize, max_concurrency: usize) -> Result<Self, SwarmError> {
        if !(1..=MAX_SWARM_FAN_OUT).contains(&max_fan_out) {
            return Err(SwarmError::FanOutLimit);
        }
        if max_concurrency == 0 || max_concurrency > max_fan_out {
            return Err(SwarmError::ConcurrencyLimit);
        }
        Ok(Self {
            max_fan_out,
            max_concurrency,
        })
    }

    pub fn plan(
        &self,
        description: &str,
        profile_name: &str,
        items: &[String],
        prompt_template: &str,
        resumes: &BTreeMap<String, String>,
    ) -> Result<SwarmPlan, SwarmError> {
        let description = description.trim();
        if description.is_empty() {
            return Err(SwarmError::EmptyDescription);
        }
        let total = items.len().saturating_add(resumes.len());
        if total == 0 || (resumes.is_empty() && items.len() < 2) {
            return Err(SwarmError::InsufficientMembers);
        }
        if total > self.max_fan_out {
            return Err(SwarmError::FanOutLimit);
        }
        if !items.is_empty() && !prompt_template.contains(SWARM_ITEM_PLACEHOLDER) {
            return Err(SwarmError::MissingPlaceholder);
        }
        let profile_name = profile_name.trim();
        if !items.is_empty() && profile_name.is_empty() {
            return Err(SwarmError::EmptyProfile);
        }
        let mut members = Vec::with_capacity(total);
        let mut prompts = BTreeSet::new();
        for (agent_id, prompt) in resumes {
            let prompt = prompt.trim();
            if agent_id.trim().is_empty() || prompt.is_empty() {
                return Err(SwarmError::InvalidResume);
            }
            if !prompts.insert(prompt.to_owned()) {
                return Err(SwarmError::DuplicatePrompt);
            }
            members.push(SwarmMemberSpec {
                index: members.len() + 1,
                prompt: prompt.to_owned(),
                kind: SwarmMemberKind::Resume {
                    agent_id: agent_id.to_owned(),
                },
            });
        }
        for item in items {
            let item = item.trim();
            if item.is_empty() {
                return Err(SwarmError::EmptyItem);
            }
            let prompt = prompt_template.replace(SWARM_ITEM_PLACEHOLDER, item);
            if !prompts.insert(prompt.clone()) {
                return Err(SwarmError::DuplicatePrompt);
            }
            members.push(SwarmMemberSpec {
                index: members.len() + 1,
                prompt,
                kind: SwarmMemberKind::Spawn {
                    item: item.to_owned(),
                    profile_name: profile_name.to_owned(),
                },
            });
        }
        Ok(SwarmPlan {
            description: description.to_owned(),
            max_concurrency: self.max_concurrency,
            members,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SwarmError {
    #[error("swarm fan-out exceeds its configured ceiling")]
    FanOutLimit,
    #[error("swarm concurrency is outside its configured ceiling")]
    ConcurrencyLimit,
    #[error("swarm description must not be empty")]
    EmptyDescription,
    #[error("swarm requires at least two new members or one resumed member")]
    InsufficientMembers,
    #[error("swarm prompt template must include {{item}}")]
    MissingPlaceholder,
    #[error("swarm profile must not be empty")]
    EmptyProfile,
    #[error("swarm item must not be empty")]
    EmptyItem,
    #[error("swarm resume entry is invalid")]
    InvalidResume,
    #[error("swarm members must have distinct prompts")]
    DuplicatePrompt,
}
