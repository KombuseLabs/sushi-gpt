//! Optional routing implementation. Native spawning and validation remain in the host.
mod control;
mod transport;
use codex_extension_api::AgentRouting;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::RoutingHost;
use codex_extension_api::RoutingRequest;
use codex_extension_api::RoutingSelection;
use codex_extension_api::RoutingSource;
use codex_sushi_diagnostics::ClassifierAttempt;
use codex_sushi_diagnostics::Decision;
use codex_sushi_diagnostics::Reason;
use codex_sushi_routing_policy::AgentModelRoutingTask;
use control::Mode;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use transport::JevFallback;
use transport::JevRequest;

struct Router;
impl AgentRouting for Router {
    fn select<'a>(
        &'a self,
        request: RoutingRequest<'a>,
        host: &'a dyn RoutingHost,
    ) -> ExtensionFuture<'a, RoutingSelection> {
        Box::pin(async move {
            let mut attempt = ClassifierAttempt::new(request.thread_id, request.turn_id);
            let (mut selection, reason) = select(&request, host, &mut attempt).await;
            attempt.finish(reason);
            selection.observer = Decision::new(
                request.thread_id,
                request.turn_id,
                request.explicit_model,
                selection.model.as_deref(),
                None,
                reason,
            )
            .map(|decision| Box::new(decision) as _);
            selection
        })
    }
}

async fn select(
    request: &RoutingRequest<'_>,
    host: &dyn RoutingHost,
    attempt: &mut ClassifierAttempt,
) -> (RoutingSelection, Reason) {
    let Some(routing) = request.settings.filter(|r| r.enabled) else {
        return (
            RoutingSelection::default(),
            if request.settings.is_some() {
                Reason::RoutingOff
            } else {
                Reason::Native
            },
        );
    };
    let mut selection = RoutingSelection {
        source: Some(RoutingSource::Default),
        ..Default::default()
    };
    if request.explicit_model.is_some() {
        selection.source = Some(RoutingSource::Explicit);
        return (selection, Reason::Explicit);
    }
    if request.full_history {
        selection.source = Some(RoutingSource::FullHistory);
        return (selection, Reason::FullHistory);
    }
    // The control file is only consulted once the cheap native precedence rules have passed.
    let (mode, off_reason) = match control::read(request.codex_home).await {
        Ok(mode) => (mode, Reason::RoutingOff),
        Err(error) => {
            tracing::warn!(target: "agent_model_routing", reason = ?error.kind(), "invalid routing control; using native defaults");
            (Mode::Off, Reason::ControlInvalid)
        }
    };
    if mode == Mode::Off {
        selection.source = Some(RoutingSource::Disabled);
        return (selection, off_reason);
    }
    if let Some(route) = routing.select(request.role, request.task) {
        return (
            RoutingSelection {
                model_provider: route.model_provider.clone(),
                model: Some(route.model.clone()),
                reasoning_effort: route.reasoning_effort.clone(),
                source: Some(RoutingSource::Rule),
                observer: None,
            },
            Reason::RuleMatched,
        );
    }
    if mode == Mode::RulesOnly {
        return (selection, Reason::RulesOnly);
    }
    let Some(settings) = routing.jev.as_ref().filter(|j| j.enabled) else {
        return (selection, Reason::JevDisabled);
    };
    let AgentModelRoutingTask::V2TaskName(task_name) = request.task else {
        return (selection, Reason::UnsupportedInput);
    };
    if task_name.trim().is_empty() || task_name.len() > 256 || request.role.len() > 256 {
        return (selection, Reason::UnsupportedInput);
    }
    if !host.candidates_available(settings).await {
        return (selection, Reason::UnavailableModel);
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
    let api_key = std::env::var(&settings.api_key_env).ok();
    let decision = transport::classify_jev(
        &request.http_client,
        JevRequest {
            endpoint: &settings.endpoint,
            model: &settings.model,
            state: json!({"task_name":task_name,"agent_type":request.role}),
            instructions: &settings.instructions,
            criteria,
            timeout: Duration::from_millis(settings.timeout_ms),
            min_confidence: settings.min_confidence,
        },
        api_key.as_deref(),
        || attempt.request_started(),
    )
    .await;
    let decision = match decision {
        Ok(decision) => decision,
        Err(fallback) => {
            if let JevFallback::Http(status) = fallback {
                attempt.http_status(status);
            }
            tracing::info!(target: "agent_model_routing", ?fallback, "Jev routing fell back to native defaults");
            return (
                selection,
                match fallback {
                    JevFallback::MissingKey => Reason::MissingKey,
                    JevFallback::Transport => Reason::Transport,
                    JevFallback::Timeout => Reason::Timeout,
                    JevFallback::Http(_) => Reason::Http,
                    JevFallback::OversizedResponse => Reason::OversizedResponse,
                    JevFallback::InvalidResponse => Reason::InvalidResponse,
                    JevFallback::Uncertain => Reason::Uncertain,
                },
            );
        }
    };
    tracing::info!(target: "agent_model_routing", source = "jev", class = %decision.choice, confidence = decision.confidence, classifier_model = %settings.model, "classifier selected a configured class");
    let Some(class) = settings.classes.get(&decision.choice) else {
        return (selection, Reason::InvalidResponse);
    };
    attempt.recommended(&class.model);
    if class.model_provider.is_none() && !host.validate_candidate(class).await {
        tracing::info!(target: "agent_model_routing", reason = "invalid_target_settings", "Jev routing fell back to native defaults");
        return (selection, Reason::InvalidTargetSettings);
    }
    selection.model = Some(class.model.clone());
    selection.model_provider = class.model_provider.clone();
    selection.reasoning_effort = class.reasoning_effort.clone();
    selection.source = Some(RoutingSource::Jev);
    (selection, Reason::JevSelected)
}

pub fn install<C: Sync>(registry: &mut codex_extension_api::ExtensionRegistryBuilder<C>) {
    registry.agent_routing(Arc::new(Router));
}
