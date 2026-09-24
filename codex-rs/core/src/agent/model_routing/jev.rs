//! V2-only data boundary between native spawning and the optional TypeSafe classifier.
use super::super::model_supports_multi_agent_backend;
use super::transport::JevFallback;
use super::transport::JevRequest;
use super::transport::classify_jev;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_config::config_toml::agent_model_routing::AgentModelRoutingTask;
use codex_config::config_toml::agent_model_routing::JevRouting;
use codex_config::config_toml::agent_model_routing::JevRoutingClass;
use codex_model_provider_info::WireApi;
use codex_models_manager::manager::RefreshStrategy;
use serde_json::json;
use std::time::Duration;

#[derive(Debug)]
pub(super) enum JevRouteFallback {
    UnsupportedInput,
    UnavailableModel,
    Classifier(JevFallback),
}

pub(super) async fn select<'a>(
    session: &Session,
    step: &StepContext,
    settings: &'a JevRouting,
    role: &str,
    task: AgentModelRoutingTask<'_>,
    attempt: &mut super::telemetry::ClassifierAttempt,
) -> Result<&'a JevRoutingClass, JevRouteFallback> {
    let AgentModelRoutingTask::V2TaskName(task_name) = task else {
        return Err(JevRouteFallback::UnsupportedInput);
    };
    if task_name.trim().is_empty() || task_name.len() > 256 || role.len() > 256 {
        return Err(JevRouteFallback::UnsupportedInput);
    }
    let factory = step.turn.config.http_client_factory();
    if settings.classes.values().any(|class| {
        class.model_provider.as_ref().is_some_and(|id| {
            step.turn
                .config
                .model_providers
                .get(id)
                .is_none_or(|provider| {
                    let unsupported_cross_provider = match provider.wire_api {
                        WireApi::OpenResponses => provider.env_key.is_none(),
                        WireApi::ClaudeCli => {
                            crate::claude_cli::validate_provider(provider).is_err()
                        }
                        WireApi::Responses => true,
                    };
                    (provider.env_key.is_some() && provider.api_key().is_err())
                        || (id != &step.turn.config.model_provider_id
                            && (unsupported_cross_provider
                                || step.turn.config.model_catalog.is_none()
                                || !step
                                    .turn
                                    .config
                                    .agent_model_routing
                                    .as_ref()
                                    .is_some_and(|r| r.plaintext_messages)))
                })
        })
    }) {
        return Err(JevRouteFallback::UnavailableModel);
    }
    let available = session
        .services
        .models_manager
        .list_models(RefreshStrategy::Offline, factory.clone())
        .await;
    if settings.classes.values().any(|class| {
        !available.iter().any(|model| {
            model.model == class.model
                && model_supports_multi_agent_backend(model, step.turn.multi_agent_version)
        })
    }) {
        return Err(JevRouteFallback::UnavailableModel);
    }
    let mut criteria = settings
        .classes
        .iter()
        .map(|(label, class)| (label.as_str(), class.description.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    criteria.insert(
        "abstain",
        "Insufficient information, ambiguous task name, or no suitable class.",
    );
    // Credentials are read only at an enabled, eligible routing decision; never from files.
    let api_key = std::env::var(&settings.api_key_env).ok();
    let decision = classify_jev(
        &factory,
        JevRequest {
            endpoint: &settings.endpoint,
            model: &settings.model,
            state: json!({"task_name": task_name, "agent_type": role}),
            instructions: &settings.instructions,
            criteria,
            timeout: Duration::from_millis(settings.timeout_ms),
            min_confidence: settings.min_confidence,
        },
        api_key.as_deref(),
        || attempt.request_started(),
    )
    .await
    .map_err(JevRouteFallback::Classifier)?;
    tracing::info!(target: "agent_model_routing", source = "jev", class = %decision.choice, confidence = decision.confidence, classifier_model = %settings.model, "classifier selected a configured class");
    settings
        .classes
        .get(&decision.choice)
        .ok_or(JevRouteFallback::Classifier(JevFallback::InvalidResponse))
}
