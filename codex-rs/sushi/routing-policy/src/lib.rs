//! Opt-in model selection rules evaluated against a delegated task, before native spawning.

#[path = "jev_routing.rs"]
mod jev;
pub use jev::JevCapability;
pub use jev::JevRouting;
pub use jev::JevRoutingClass;

use codex_protocol::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Routing inputs available without decrypting an agent's task description.
#[derive(Clone, Copy, Debug)]
pub enum AgentModelRoutingTask<'a> {
    /// The V1 spawn API exposes its task message as plaintext.
    V1Message(&'a str),
    /// V2 exposes a task name; its message may be encrypted and is never inspected.
    V2TaskName(&'a str),
    /// A V2 spawn whose message the model explicitly declared plaintext. Fixed rules still
    /// match only the task name; the message is offered to the optional classifier alone.
    V2PlaintextTask {
        task_name: &'a str,
        message: &'a str,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentModelRouting {
    /// Require every selected child to match a Jev class; classification failures stop spawning.
    #[serde(default)]
    pub strict_candidates: bool,
    /// Explicitly share host-defined dynamic tools with fresh native children.
    #[serde(default)]
    pub inherit_dynamic_tools: bool,
    /// Advertise plaintext V2 messages and require explicit plaintext response metadata.
    /// Existing encrypted messages are never reinterpreted. Disabled by default.
    #[serde(default)]
    pub plaintext_messages: bool,
    /// Enable configured rules. Omission preserves the existing spawn behavior.
    #[serde(default)]
    pub enabled: bool,
    /// Ordered rules; the first match supplies defaults for a fresh or partial-history child.
    #[serde(default)]
    pub rules: Vec<AgentModelRoute>,
    /// Optional V2 classifier, consulted only after enabled fixed rules miss.
    pub jev: Option<JevRouting>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentModelRoute {
    /// Optional provider for a fresh child, requiring an authoritative shared catalog.
    pub model_provider: Option<String>,
    /// Optional exact agent role. An omitted spawn role is matched as "default".
    pub agent_type: Option<String>,
    /// Case-insensitive substrings of the V1 task message. Never matches V2 messages.
    /// Any substring may match; an agent_type filter must also match when supplied.
    /// Cannot be combined with task_name_contains.
    #[serde(default)]
    pub task_contains: Vec<String>,
    /// Case-insensitive substrings of the V2 task_name, not its encrypted message.
    /// Any substring may match; an agent_type filter must also match when supplied.
    /// Cannot be combined with task_contains. Never matches V1 spawns.
    #[serde(default)]
    pub task_name_contains: Vec<String>,
    /// Model identifier validated against the current provider's existing model catalog.
    pub model: String,
    /// Optional default effort, subject to the selected model's existing validation.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl AgentModelRouting {
    pub fn validate(&self) -> Result<(), String> {
        if self.strict_candidates
            && (!self.enabled || self.jev.as_ref().is_none_or(|j| j.classes.is_empty()))
        {
            return Err(
                "strict_candidates requires enabled routing and nonempty candidate classes"
                    .to_string(),
            );
        }
        if self.strict_candidates
            && self.jev.as_ref().is_some_and(|j| {
                j.classes.values().any(|c| {
                    c.model_provider
                        .as_ref()
                        .is_none_or(|p| p.trim().is_empty() || p.len() > 256)
                        || [
                            JevCapability::Text,
                            JevCapability::Tools,
                            JevCapability::Streaming,
                        ]
                        .iter()
                        .any(|cap| !c.capabilities.contains(cap))
                        || (c.capabilities.contains(&JevCapability::DynamicTools)
                            != self.inherit_dynamic_tools)
                })
            })
        {
            return Err("Strict candidates require an explicit model_provider and text/tools/streaming capabilities; dynamic_tools must match inherit_dynamic_tools.".to_string());
        }
        if let Some(jev) = &self.jev {
            jev.validate()?;
        }
        if self.rules.len() > 32 {
            return Err("agent_model_routing supports at most 32 rules".to_string());
        }
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.model.trim().is_empty()
                || rule.model.len() > 256
                || rule
                    .model_provider
                    .as_ref()
                    .is_some_and(|provider| provider.trim().is_empty() || provider.len() > 256)
                || rule
                    .agent_type
                    .as_ref()
                    .is_some_and(|role| role.trim().is_empty() || role.len() > 256)
                || [&rule.task_contains, &rule.task_name_contains]
                    .iter()
                    .any(|matchers| {
                        matchers.len() > 16
                            || matchers
                                .iter()
                                .any(|needle| needle.trim().is_empty() || needle.len() > 256)
                    })
                || (!rule.task_contains.is_empty() && !rule.task_name_contains.is_empty())
                || (rule.agent_type.is_none()
                    && rule.task_contains.is_empty()
                    && rule.task_name_contains.is_empty())
            {
                return Err(format!(
                    "Invalid agent_model_routing rule {index}: provide a model and a non-empty role or task matcher (at most 16 task matchers, 256 bytes per value); task_contains and task_name_contains are mutually exclusive"
                ));
            }
        }
        Ok(())
    }

    /// Selects defaults from the role and the task field exposed by this spawn API.
    pub fn select(
        &self,
        agent_type: &str,
        task: AgentModelRoutingTask<'_>,
    ) -> Option<&AgentModelRoute> {
        let text = match task {
            AgentModelRoutingTask::V1Message(message) => message,
            AgentModelRoutingTask::V2TaskName(name)
            | AgentModelRoutingTask::V2PlaintextTask {
                task_name: name, ..
            } => name,
        };
        if !self.enabled {
            return None;
        }
        let text = text.to_lowercase();
        self.rules.iter().find(|rule| {
            let matchers = match task {
                AgentModelRoutingTask::V1Message(_) => {
                    if !rule.task_name_contains.is_empty() {
                        return false;
                    }
                    &rule.task_contains
                }
                AgentModelRoutingTask::V2TaskName(_)
                | AgentModelRoutingTask::V2PlaintextTask { .. } => {
                    if !rule.task_contains.is_empty() {
                        return false;
                    }
                    &rule.task_name_contains
                }
            };
            rule.agent_type
                .as_deref()
                .is_none_or(|role| role == agent_type)
                && (matchers.is_empty()
                    || matchers
                        .iter()
                        .any(|needle| text.contains(&needle.to_lowercase())))
        })
    }
}

#[cfg(test)]
#[path = "agent_model_routing_tests.rs"]
mod tests;
