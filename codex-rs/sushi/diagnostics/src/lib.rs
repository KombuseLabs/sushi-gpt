//! Opt-in, content-free diagnostics; never a routing input or a billing ledger.
mod classifier;
mod request;
mod writer;

pub use classifier::ClassifierAttempt;
use codex_protocol::ThreadId;
use request::RequestAttempt;
use serde::Serialize;
use uuid::Uuid;

#[derive(Clone, Copy, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    #[default]
    Native,
    Explicit,
    FullHistory,
    RoutingOff,
    ControlInvalid,
    NoRule,
    RuleMatched,
    JevSelected,
    InvalidTargetSettings,
    UnsupportedInput,
    UnavailableModel,
    MissingKey,
    Transport,
    Timeout,
    Http,
    OversizedResponse,
    InvalidResponse,
    Uncertain,
    RulesOnly,
    JevDisabled,
    RequestStarted,
    Cancelled,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Source {
    Native,
    Rule,
    Jev,
    Fallback,
}

#[derive(Serialize)]
#[serde(tag = "kind")]
enum Record {
    #[serde(rename = "routing_decision")]
    RoutingDecision(Decision),
    #[serde(rename = "classifier_transition")]
    ClassifierTransition(classifier::Transition),
    #[serde(rename = "response_usage")]
    ResponseUsage(Box<request::UsageRecord>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    decision_id: Uuid,
    parent_thread_id: ThreadId,
    parent_turn_id: Option<String>,
    call_id: Option<String>,
    child_thread_id: Option<ThreadId>,
    requested_model: Option<String>,
    policy_model: Option<String>,
    selected_model: Option<String>,
    source: Source,
    reason_code: Reason,
}

impl Decision {
    pub fn new(
        parent_thread_id: ThreadId,
        parent_turn_id: &str,
        requested_model: Option<&str>,
        policy_model: Option<&str>,
        selected_model: Option<&str>,
        reason_code: Reason,
    ) -> Option<Self> {
        writer::emitter()?;
        let source = match reason_code {
            Reason::RuleMatched => Source::Rule,
            Reason::JevSelected => Source::Jev,
            Reason::Native
            | Reason::Explicit
            | Reason::FullHistory
            | Reason::RoutingOff
            | Reason::NoRule
            | Reason::RulesOnly
            | Reason::JevDisabled => Source::Native,
            _ => Source::Fallback,
        };
        Some(Self {
            decision_id: Uuid::new_v4(),
            parent_thread_id,
            parent_turn_id: identifier(parent_turn_id),
            call_id: None,
            child_thread_id: None,
            requested_model: requested_model.and_then(identifier),
            policy_model: policy_model.and_then(identifier),
            selected_model: selected_model.and_then(identifier),
            source,
            reason_code,
        })
    }

    pub fn record(mut self, call_id: &str, child_thread_id: Option<ThreadId>) {
        self.call_id = identifier(call_id);
        self.child_thread_id = child_thread_id;
        if let Some(emitter) = writer::emitter() {
            emitter.emit(Record::RoutingDecision(self));
        }
    }
}

// Only bounded opaque identifiers/model slugs cross this boundary. Never serialize metadata blobs.
fn identifier(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/@".contains(&b)))
    .then(|| value.to_owned())
}

#[derive(Debug)]
struct Observer;
impl codex_extension_api::ModelRequestObserver for Observer {
    fn start(
        &self,
        metadata: &codex_extension_api::RequestMetadata,
        model: &str,
    ) -> Option<Box<dyn codex_extension_api::ModelRequestAttempt>> {
        RequestAttempt::start(metadata, model).map(|attempt| Box::new(attempt) as _)
    }
}
impl codex_extension_api::ModelRequestAttempt for RequestAttempt {
    fn set_request_id(&mut self, id: Option<&str>) {
        self.set_request_id(id);
    }
    fn set_cli_transport(&mut self, instance: &str, version: &str) {
        self.set_cli_transport(instance, version);
    }
    fn observe(&mut self, event: &codex_api::ResponseEvent) {
        self.observe(event);
    }
}
pub fn install<C: Sync>(registry: &mut codex_extension_api::ExtensionRegistryBuilder<C>) {
    registry.model_request_observer(std::sync::Arc::new(Observer));
}

impl codex_extension_api::RoutingObserver for Decision {
    fn resolved(&mut self, model: Option<&str>) {
        self.selected_model = model.and_then(identifier);
    }
    fn record(self: Box<Self>, call_id: &str, child: Option<ThreadId>) {
        (*self).record(call_id, child);
    }
}
