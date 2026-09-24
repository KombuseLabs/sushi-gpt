//! Adapts native child state to the registered routing policy; retains native validation.
use super::SpawnConfigOptions;
use super::model_supports_multi_agent_backend;
use super::resolve_requested_spawn_agent_model;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_config::config_toml::agent_model_routing::JevRouting;
use codex_config::config_toml::agent_model_routing::JevRoutingClass;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::RoutingHost;
use codex_extension_api::RoutingObserver;
use codex_extension_api::RoutingRequest;
use codex_extension_api::RoutingSelection;
use codex_extension_api::RoutingSource;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_models_manager::manager::RefreshStrategy;

/// The backend owns tool definitions under this namespace and rejects changed schemas.
/// Plaintext agent messages therefore require a separate configured namespace.
pub(crate) const RESERVED_AGENT_TOOL_NAMESPACE: &str =
    crate::config::DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE;

/// The one definition of a usable cross-provider target. Candidate availability and the
/// applied selection must agree, so both go through here.
fn cross_provider_target<'c>(
    config: &'c Config,
    extensions: &ExtensionRegistry<Config>,
    id: &str,
) -> Result<&'c ModelProviderInfo, String> {
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
    match provider.wire_api {
        WireApi::OpenResponses => {
            if provider.env_key.is_none() || provider.api_key().ok().flatten().is_none() {
                return Err("The routed provider requires a nonempty credential in its configured env_key before child startup.".to_string());
            }
        }
        WireApi::ClaudeCli => extensions
            .model_transport()
            .ok_or("Local model transport is configured but no transport extension is registered.")?
            .validate(provider)?,
        WireApi::Responses => {
            return Err(
                "Cross-provider targets require wire_api=openresponses or claude_cli.".to_string(),
            );
        }
    }
    Ok(provider)
}

pub(super) fn apply_provider(
    selection: &RoutingSelection,
    config: &mut Config,
    extensions: &ExtensionRegistry<Config>,
) -> Result<(), String> {
    let Some(id) = selection
        .model_provider
        .as_deref()
        .filter(|id| *id != config.model_provider_id)
    else {
        return Ok(());
    };
    let provider = cross_provider_target(config, extensions, id)?.clone();
    config.model_provider = provider;
    config.model_provider_id = id.to_string();
    config.model_reasoning_effort = None;
    config.service_tier = None;
    Ok(())
}

pub(super) fn validate_selection(
    selection: &RoutingSelection,
    config: &Config,
) -> Result<(), String> {
    let Some(routing) = config
        .agent_model_routing
        .as_ref()
        .filter(|r| r.strict_candidates)
    else {
        return Ok(());
    };
    if matches!(
        selection.source,
        Some(RoutingSource::Default | RoutingSource::Disabled)
    ) {
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

/// Logs the resolved child configuration and hands the observer over for the spawn record.
pub(super) fn finish(
    mut selection: RoutingSelection,
    config: &Config,
) -> Option<Box<dyn RoutingObserver>> {
    if let Some(source) = selection.source {
        tracing::info!(target: "agent_model_routing", source = source.as_str(), policy_provider = ?selection.model_provider, selected_provider = %config.model_provider_id, selected_model = ?config.model, effort = ?config.model_reasoning_effort, "native child configuration resolved after role overrides; execution not confirmed");
    }
    if let Some(observer) = &mut selection.observer {
        observer.resolved(config.model.as_deref());
    }
    selection.observer
}

struct Host<'a> {
    session: &'a Session,
    step: &'a StepContext,
    config: &'a Config,
    options: &'a SpawnConfigOptions<'a>,
}
impl RoutingHost for Host<'_> {
    fn candidates_available<'a>(&'a self, settings: &'a JevRouting) -> ExtensionFuture<'a, bool> {
        Box::pin(async move {
            let config = &self.step.turn.config;
            let extensions = &self.session.services.extensions;
            let provider_unavailable = |id: &str| {
                if id == config.model_provider_id {
                    config.model_providers.get(id).is_none_or(|provider| {
                        provider.env_key.is_some() && provider.api_key().is_err()
                    })
                } else {
                    cross_provider_target(config, extensions, id).is_err()
                }
            };
            if settings.classes.values().any(|class| {
                class
                    .model_provider
                    .as_deref()
                    .is_some_and(provider_unavailable)
            }) {
                return false;
            }
            let available = self
                .session
                .services
                .models_manager
                .list_models(RefreshStrategy::Offline, config.http_client_factory())
                .await;
            settings.classes.values().all(|class| {
                available.iter().any(|model| {
                    model.model == class.model
                        && model_supports_multi_agent_backend(
                            model,
                            self.step.turn.multi_agent_version,
                        )
                })
            })
        })
    }
    fn validate_candidate<'a>(&'a self, class: &'a JevRoutingClass) -> ExtensionFuture<'a, bool> {
        Box::pin(async move {
            resolve_requested_spawn_agent_model(
                self.session,
                self.step,
                self.config,
                Some(&class.model),
                self.options
                    .reasoning_effort
                    .clone()
                    .or_else(|| class.reasoning_effort.clone()),
            )
            .await
            .is_ok()
        })
    }
}

pub(super) async fn select(
    session: &Session,
    step: &StepContext,
    config: &Config,
    options: &SpawnConfigOptions<'_>,
) -> Result<RoutingSelection, String> {
    let Some(router) = session.services.extensions.agent_routing() else {
        if step
            .turn
            .config
            .agent_model_routing
            .as_ref()
            .is_some_and(|r| r.enabled)
        {
            return Err(
                "Agent model routing is configured but no routing extension is registered.".into(),
            );
        }
        return Ok(RoutingSelection::default());
    };
    let host = Host {
        session,
        step,
        config,
        options,
    };
    Ok(router
        .select(
            RoutingRequest {
                settings: step.turn.config.agent_model_routing.as_ref(),
                codex_home: config.codex_home.as_path(),
                role: options.role_name.unwrap_or(DEFAULT_ROLE_NAME),
                task: options.task,
                explicit_model: options.model,
                full_history: options.full_history_fork,
                thread_id: session.thread_id,
                turn_id: &step.turn.sub_id,
                http_client: step.turn.config.http_client_factory(),
            },
            &host,
        )
        .await)
}
