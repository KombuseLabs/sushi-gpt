//! Narrow host-owned routing, sampling, and diagnostic boundaries.
use crate::ExtensionFuture;
use codex_api::ResponseEvent;
use codex_config::config_toml::agent_model_routing::AgentModelRouting;
use codex_config::config_toml::agent_model_routing::AgentModelRoutingTask;
use codex_config::config_toml::agent_model_routing::JevRouting;
use codex_config::config_toml::agent_model_routing::JevRoutingClass;
use codex_http_client::HttpClientFactory;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::ThreadId;
use codex_protocol::error::Result;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_tools::ToolSpec;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Read-only native validation. Implementations must use the captured spawn configuration.
/// Boxed futures keep this host capability object-safe without exposing Core runtime types.
pub trait RoutingHost: Sync {
    fn candidates_available<'a>(&'a self, settings: &'a JevRouting) -> ExtensionFuture<'a, bool>;
    fn validate_candidate<'a>(
        &'a self,
        candidate: &'a JevRoutingClass,
    ) -> ExtensionFuture<'a, bool>;
}

pub struct RoutingRequest<'a> {
    pub settings: Option<&'a AgentModelRouting>,
    pub codex_home: &'a Path,
    pub role: &'a str,
    pub task: AgentModelRoutingTask<'a>,
    pub explicit_model: Option<&'a str>,
    pub full_history: bool,
    pub thread_id: ThreadId,
    pub turn_id: &'a str,
    pub http_client: HttpClientFactory,
}

/// A proposal only; the host owns provider, model, role, and runtime validation.
#[derive(Default)]
pub struct RoutingSelection {
    pub model_provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub source: Option<&'static str>,
    pub observer: Option<Box<dyn RoutingObserver>>,
}

/// Tracks resolution and native spawn outcome without owning a child or its lifecycle.
pub trait RoutingObserver: Send + Sync {
    fn resolved(&mut self, selected_model: Option<&str>);
    fn record(self: Box<Self>, call_id: &str, child_thread_id: Option<ThreadId>);
}

/// Selects optional defaults before native child configuration is finalized.
/// Implementations must not spawn children or change permissions/history.
pub trait AgentRouting: Send + Sync {
    fn select<'a>(
        &'a self,
        request: RoutingRequest<'a>,
        host: &'a dyn RoutingHost,
    ) -> ExtensionFuture<'a, RoutingSelection>;
}

/// Content-free correlation supplied by the host, not arbitrary request metadata.
#[derive(Clone, Debug, Default)]
pub struct RequestMetadata {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub parent_thread_id: Option<ThreadId>,
    pub parent_turn_id: Option<String>,
    pub root_turn_id: Option<String>,
}

/// Observes a single request. Drop must represent an incomplete attempt when appropriate.
pub trait ModelRequestAttempt: Send {
    fn set_request_id(&mut self, request_id: Option<&str>);
    fn set_cli_transport(&mut self, instance: &str, version: &str);
    fn observe(&mut self, event: &ResponseEvent);
}

/// Creates optional passive diagnostics; no observation can change model execution.
pub trait ModelRequestObserver: std::fmt::Debug + Send + Sync {
    fn start(
        &self,
        metadata: &RequestMetadata,
        model: &str,
    ) -> Option<Box<dyn ModelRequestAttempt>>;
}

/// Snapshot of native sampling inputs. No session, tool executor, or permission authority.
#[derive(Clone, Debug, Default)]
pub struct SamplingPrompt {
    pub input: Vec<ResponseItem>,
    pub tools: Arc<[ToolSpec]>,
    pub parallel_tool_calls: bool,
    pub base_instructions: BaseInstructions,
    pub output_schema: Option<Value>,
    pub cyber_access_program: Option<codex_protocol::turn_input::CyberAccessProgram>,
}

/// Duplex event stream. The host alone executes tools and returns their native results.
pub struct ModelTransportStream {
    pub rx_event: mpsc::Receiver<Result<ResponseEvent>>,
    pub tool_result_tx: Option<mpsc::Sender<ResponseItem>>,
    pub consumer_dropped: CancellationToken,
}

/// Per-native-child transport session; cancellation and drop must stop owned subprocesses.
pub trait ModelTransport: std::fmt::Debug + Send + Sync {
    fn stream<'a>(
        &'a self,
        provider: &'a ModelProviderInfo,
        prompt: &'a SamplingPrompt,
        model: &'a str,
        metadata: &'a RequestMetadata,
        turn_cancel: CancellationToken,
    ) -> ExtensionFuture<'a, Result<ModelTransportStream>>;
}

/// Creates a transport only for supported configured providers. It does not create an agent.
pub trait ModelTransportFactory: Send + Sync {
    fn validate(&self, provider: &ModelProviderInfo) -> std::result::Result<(), String>;
    fn create(
        &self,
        provider: &ModelProviderInfo,
        observer: Option<Arc<dyn ModelRequestObserver>>,
    ) -> Option<Arc<dyn ModelTransport>>;
}

impl futures::Stream for ModelTransportStream {
    type Item = Result<ResponseEvent>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}
