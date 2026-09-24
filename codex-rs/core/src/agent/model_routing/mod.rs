//! Optional routing policy. Native child preparation retains validation and lifecycle ownership.

mod control;
mod jev;
pub(crate) mod telemetry;
mod transport;
use telemetry::Reason;

use super::SpawnConfigOptions;
use super::apply_requested_spawn_agent_model_overrides;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_protocol::openai_models::ReasoningEffort;
use control::Mode;

/// The backend owns the tool definitions under this namespace and rejects any request whose
/// `spawn_agent`/`send_message`/`followup_task` schema differs from its own (HTTP 400). The
/// plaintext transport changes that schema, so it can only be exposed under another name.
pub(crate) const RESERVED_AGENT_TOOL_NAMESPACE: &str =
    crate::config::DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE;

#[derive(Default)]
pub(super) struct Selection {
    pub(super) model_provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) reasoning_effort: Option<ReasoningEffort>,
    source: Option<&'static str>,
    reason: Reason,
}

impl Selection {
    pub(super) fn apply_provider(&self, config: &mut Config) -> Result<(), String> {
        let Some(id) = self
            .model_provider
            .as_deref()
            .filter(|id| *id != config.model_provider_id)
        else {
            return Ok(());
        };
        if !config
            .agent_model_routing
            .as_ref()
            .is_some_and(|r| r.plaintext_messages)
            || config
                .multi_agent_v2
                .tool_namespace
                .as_deref()
                .is_none_or(|namespace| namespace == RESERVED_AGENT_TOOL_NAMESPACE)
            || config.model_catalog.is_none()
        {
            return Err("Cross-provider routing requires plaintext_messages, a multi_agent_v2.tool_namespace other than the backend-reserved `collaboration`, and an authoritative model_catalog_json containing both providers' models.".to_string());
        }
        if config
            .config_layer_stack
            .required_model_provider()
            .is_some_and(|required| required != id)
        {
            return Err(
                "Cross-provider routing conflicts with the required model provider.".to_string(),
            );
        }
        let provider = config
            .model_providers
            .get(id)
            .ok_or_else(|| format!("Unknown routed provider `{id}`"))?;
        if !matches!(
            provider.wire_api,
            codex_model_provider_info::WireApi::OpenResponses
                | codex_model_provider_info::WireApi::ClaudeCli
        ) {
            return Err(
                "Cross-provider targets require wire_api=openresponses or claude_cli.".to_string(),
            );
        }
        if provider.wire_api == codex_model_provider_info::WireApi::OpenResponses
            && (provider.env_key.is_none() || provider.api_key().ok().flatten().is_none())
        {
            return Err("The routed provider requires a nonempty credential in its configured env_key before child startup.".to_string());
        }
        if provider.wire_api == codex_model_provider_info::WireApi::ClaudeCli {
            crate::claude_cli::validate_provider(provider)?;
        }
        config.model_provider = provider.clone();
        config.model_provider_id = id.to_string();
        config.model_reasoning_effort = None;
        config.service_tier = None;
        Ok(())
    }

    pub(super) fn validate_selection(&self, config: &Config) -> Result<(), String> {
        let Some(routing) = config
            .agent_model_routing
            .as_ref()
            .filter(|r| r.strict_candidates)
        else {
            return Ok(());
        };
        if self.source == Some("default") || self.source == Some("disabled") {
            return Err(
                "Strict candidate selection failed; no replacement child was started.".to_string(),
            );
        }
        let allowed = routing.jev.as_ref().is_some_and(|j| {
            j.classes.values().any(|c| {
                Some(&c.model) == config.model.as_ref()
                    && c.model_provider.as_deref() == Some(config.model_provider_id.as_str())
                    && c.reasoning_effort == config.model_reasoning_effort
            })
        });
        if !allowed {
            return Err(
                "The selected provider, model, and effort are not an allowed Jev candidate."
                    .to_string(),
            );
        }
        if config.model_provider.env_key.is_some() && config.model_provider.api_key().is_err() {
            return Err("The selected candidate's provider credential is missing.".to_string());
        }
        Ok(())
    }
    pub(super) fn decision(
        &self,
        session: &Session,
        step: &StepContext,
        requested: Option<&str>,
        config: &Config,
    ) -> Option<telemetry::Decision> {
        telemetry::Decision::new(
            session.thread_id,
            &step.turn.sub_id,
            requested,
            self.model.as_deref(),
            config.model.as_deref(),
            self.reason,
        )
    }

    pub(super) fn log_resolved(&self, config: &Config) {
        if let Some(source) = self.source {
            tracing::info!(target: "agent_model_routing", source, policy_provider = ?self.model_provider, selected_provider = %config.model_provider_id, selected_model = ?config.model, effort = ?config.model_reasoning_effort, "native child configuration resolved after role overrides; execution not confirmed");
        }
    }
}

pub(super) async fn select(
    session: &Session,
    step: &StepContext,
    config: &Config,
    options: &SpawnConfigOptions<'_>,
) -> Selection {
    let mut attempt = telemetry::ClassifierAttempt::new(session.thread_id, &step.turn.sub_id);
    let selection = select_inner(session, step, config, options, &mut attempt).await;
    attempt.finish(selection.reason);
    selection
}

async fn select_inner(
    session: &Session,
    step: &StepContext,
    config: &Config,
    options: &SpawnConfigOptions<'_>,
    attempt: &mut telemetry::ClassifierAttempt,
) -> Selection {
    let Some(routing) = step
        .turn
        .config
        .agent_model_routing
        .as_ref()
        .filter(|r| r.enabled)
    else {
        return Selection {
            reason: if step.turn.config.agent_model_routing.is_some() {
                Reason::RoutingOff
            } else {
                Reason::Native
            },
            ..Selection::default()
        };
    };
    // One snapshot per decision, shared through CODEX_HOME; never modifies existing children.
    let mut reason = Reason::NoRule;
    let mode = match control::read(config.codex_home.as_path()).await {
        Ok(mode) => mode,
        Err(error) => {
            tracing::warn!(target: "agent_model_routing", reason = ?error.kind(), "invalid routing control; using native defaults");
            reason = Reason::ControlInvalid;
            Mode::Off
        }
    };
    let mut selection = Selection {
        source: Some("default"),
        reason,
        ..Selection::default()
    };
    if options.model.is_some() {
        selection.source = Some("explicit");
        selection.reason = Reason::Explicit;
        return selection;
    }
    if options.full_history_fork {
        selection.source = Some("full_history");
        selection.reason = Reason::FullHistory;
        return selection;
    }
    if mode == Mode::Off {
        selection.source = Some("disabled");
        if !matches!(selection.reason, Reason::ControlInvalid) {
            selection.reason = Reason::RoutingOff;
        }
        return selection;
    }
    let role = options.role_name.unwrap_or(DEFAULT_ROLE_NAME);
    if let Some(route) = routing.select(role, options.task) {
        return Selection {
            model_provider: route.model_provider.clone(),
            model: Some(route.model.clone()),
            reasoning_effort: route.reasoning_effort.clone(),
            source: Some("rule"),
            reason: Reason::RuleMatched,
        };
    }
    if mode == Mode::RulesOnly {
        selection.reason = Reason::RulesOnly;
        return selection;
    }
    let Some(settings) = routing.jev.as_ref().filter(|jev| jev.enabled) else {
        selection.reason = Reason::JevDisabled;
        return selection;
    };
    match jev::select(session, step, settings, role, options.task, attempt).await {
        Ok(class) => {
            attempt.recommended(&class.model);
            if class.model_provider.is_some() {
                selection.model = Some(class.model.clone());
                selection.model_provider = class.model_provider.clone();
                selection.reasoning_effort = class.reasoning_effort.clone();
                selection.source = Some("jev");
                selection.reason = Reason::JevSelected;
                return selection;
            }
            // Only classifier candidate failures fall back. Native caller validation still
            // propagates errors for explicit models, fixed rules, defaults, and roles.
            let mut candidate = config.clone();
            if apply_requested_spawn_agent_model_overrides(
                session,
                step,
                &mut candidate,
                Some(&class.model),
                options
                    .reasoning_effort
                    .clone()
                    .or_else(|| class.reasoning_effort.clone()),
            )
            .await
            .is_ok()
            {
                selection.model = Some(class.model.clone());
                selection.model_provider = class.model_provider.clone();
                selection.reasoning_effort = class.reasoning_effort.clone();
                selection.source = Some("jev");
                selection.reason = Reason::JevSelected;
            } else {
                selection.reason = Reason::InvalidTargetSettings;
                tracing::info!(target: "agent_model_routing", reason = "invalid_target_settings", "Jev routing fell back to native defaults");
            }
        }
        Err(jev::JevRouteFallback::Classifier(reason)) => {
            if let transport::JevFallback::Http(status) = reason {
                attempt.http_status(status);
            }
            selection.reason = match reason {
                transport::JevFallback::MissingKey => Reason::MissingKey,
                transport::JevFallback::Transport => Reason::Transport,
                transport::JevFallback::Timeout => Reason::Timeout,
                transport::JevFallback::Http(_) => Reason::Http,
                transport::JevFallback::OversizedResponse => Reason::OversizedResponse,
                transport::JevFallback::InvalidResponse => Reason::InvalidResponse,
                transport::JevFallback::Uncertain => Reason::Uncertain,
            };
            tracing::info!(target: "agent_model_routing", ?reason, "Jev routing fell back to native defaults");
        }
        Err(reason @ jev::JevRouteFallback::UnsupportedInput) => {
            selection.reason = Reason::UnsupportedInput;
            tracing::info!(target: "agent_model_routing", ?reason, "Jev routing fell back to native defaults");
        }
        Err(reason @ jev::JevRouteFallback::UnavailableModel) => {
            selection.reason = Reason::UnavailableModel;
            tracing::info!(target: "agent_model_routing", ?reason, "Jev routing fell back to native defaults");
        }
    }
    selection
}
