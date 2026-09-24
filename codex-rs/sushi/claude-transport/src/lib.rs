//! A native-child-owned Claude process. Tools execute only in the native ToolRouter.
use codex_api::ResponseEvent;
use codex_api::openresponses::Adapter;
use codex_extension_api::ModelRequestObserver;
use codex_extension_api::ModelTransportStream as ResponseStream;
use codex_extension_api::RequestMetadata as CodexResponsesMetadata;
use codex_extension_api::SamplingPrompt as Prompt;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod process;
mod protocol;
mod sampling;
use protocol::MessageStream;
use protocol::parse_usage;
use protocol::prepare;
use protocol::text_content;
use protocol::tool_result_content;

const SERVER: &str = "native";
const PREFIX: &str = "mcp__native__";
const MAX_FRAME: usize = 16 * 1024 * 1024;

enum ReadWait {
    ProviderActivity,
    NativeToolResult,
}

fn failure(message: &str) -> CodexErr {
    CodexErr::Fatal(format!("Local Claude transport: {message}"))
}

pub(crate) fn validate_provider(provider: &ModelProviderInfo) -> std::result::Result<(), String> {
    if provider
        .cli_command
        .as_deref()
        .is_none_or(|command| command.as_os_str().is_empty())
    {
        return Err("Local Claude requires an explicit cli_command executable.".into());
    }
    if provider.base_url.is_some()
        || provider.env_key.is_some()
        || provider.auth.is_some()
        || provider.gateway_oauth.is_some()
        || provider.aws.is_some()
        || provider.experimental_bearer_token.is_some()
        || provider.requires_openai_auth
        || provider.supports_websockets
    {
        return Err(
            "Local Claude uses CLI-managed authentication, without HTTP provider settings.".into(),
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
struct Transport {
    state: Arc<Mutex<State>>,
    shutdown: CancellationToken,
    observer: Option<Arc<dyn ModelRequestObserver>>,
}

#[derive(Debug, Default)]
struct State {
    session: Option<Session>,
    failed: bool,
    active: bool,
}

impl State {
    fn take_session(&mut self) -> Result<Option<Session>> {
        if self.failed {
            return Err(failure("session failed; replay is disabled"));
        }
        if self.active {
            return Err(failure("overlapping native sampling requests"));
        }
        self.active = true;
        Ok(self.session.take())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = Arc::clone(&self.state);
            runtime.spawn(async move {
                let session = { state.lock().await.session.take() };
                if let Some(mut session) = session {
                    session.stop().await;
                }
            });
        }
    }
}

impl Transport {
    pub(crate) async fn stream(
        &self,
        provider: &ModelProviderInfo,
        prompt: &Prompt,
        model: &str,
        metadata: &CodexResponsesMetadata,
        turn_cancel: CancellationToken,
    ) -> Result<ResponseStream> {
        validate_provider(provider).map_err(|error| failure(&error))?;
        if prompt.output_schema.is_some() || prompt.cyber_access_program.is_some() {
            return Err(failure(
                "output schemas and access programs are unsupported",
            ));
        }
        let (tx, rx_event) = mpsc::channel(32);
        let (tool_result_tx, mut results) = mpsc::channel(32);
        let consumer_dropped = CancellationToken::new();
        let cancel = consumer_dropped.clone();
        let shutdown = self.shutdown.clone();
        let state = Arc::clone(&self.state);
        let provider = provider.clone();
        let prompt = prompt.clone();
        let model = model.to_owned();
        let metadata = metadata.clone();
        let observer = self.observer.clone();
        let turn_id = metadata.turn_id.clone();
        // A native turn can be interrupted while parked in a tool, between streams.
        let weak = Arc::downgrade(&self.state);
        let turn_watch = turn_cancel.clone();
        tokio::spawn(async move {
            turn_watch.cancelled().await;
            if let Some(state) = weak.upgrade() {
                let session = {
                    let mut state = state.lock().await;
                    if state
                        .session
                        .as_ref()
                        .is_some_and(|s| s.in_turn && s.turn_id == turn_id)
                    {
                        state.failed = true;
                        state.session.take()
                    } else {
                        None
                    }
                };
                if let Some(mut session) = session {
                    session.stop().await;
                }
            }
        });
        tokio::spawn(async move {
            let lease = { state.lock().await.take_session() };
            let mut session = match lease {
                Ok(session) => session,
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            };
            let operation = async {
                if session.is_none() {
                    session =
                        Some(Session::start(&provider, &prompt, &model, observer.clone()).await?);
                }
                let session = session
                    .as_mut()
                    .ok_or_else(|| failure("missing initialized session"))?;
                session.turn_id = metadata.turn_id.clone();
                session
                    .sample(&prompt, &model, &metadata, &tx, &mut results)
                    .await
            };
            let mut outcome = tokio::select! {
                outcome = operation => outcome,
                _ = cancel.cancelled() => Err(CodexErr::TurnAborted),
                _ = tx.closed() => Err(CodexErr::TurnAborted),
                _ = turn_cancel.cancelled() => Err(CodexErr::TurnAborted),
                _ = shutdown.cancelled() => Err(CodexErr::TurnAborted),
            };
            {
                let mut state = state.lock().await;
                state.active = false;
                if cancel.is_cancelled()
                    || shutdown.is_cancelled()
                    || (turn_cancel.is_cancelled() && session.as_ref().is_some_and(|s| s.in_turn))
                {
                    outcome = Err(CodexErr::TurnAborted);
                }
                if outcome.is_ok() {
                    // Publish ownership before completion can make the native turn drop.
                    state.session = session.take();
                } else {
                    state.failed = true;
                }
            }
            if let Some(mut session) = session {
                session.stop().await;
            }
            let _ = tx.send(outcome).await;
        });
        Ok(ResponseStream {
            rx_event,
            consumer_dropped,
            tool_result_tx: Some(tool_result_tx),
        })
    }
}

#[derive(Debug)]
struct ToolCall {
    name: String,
    arguments: Value,
    item: ResponseItem,
    request: Option<Value>,
    finished: bool,
}

#[derive(Debug)]
struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    _directory: tempfile::TempDir,
    adapter: Adapter,
    raw_tools: Value,
    tools: Vec<Value>,
    model: String,
    instructions: String,
    history: Vec<Value>,
    held_reply: Option<Value>,
    instance_id: String,
    version: String,
    timeout: Duration,
    in_turn: bool,
    turn_id: Option<String>,
    frame: Vec<u8>,
    /// MCP tools/call requests that arrived before their model message completed.
    pending_tool_calls: Vec<Value>,
    observer: Option<Arc<dyn ModelRequestObserver>>,
}

#[cfg(test)]
#[path = "claude_cli_tests.rs"]
mod tests;

impl codex_extension_api::ModelTransport for Transport {
    fn stream<'a>(
        &'a self,
        provider: &'a ModelProviderInfo,
        prompt: &'a Prompt,
        model: &'a str,
        metadata: &'a CodexResponsesMetadata,
        cancel: CancellationToken,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<ResponseStream>> {
        Box::pin(self.stream(provider, prompt, model, metadata, cancel))
    }
}
struct Factory;
impl codex_extension_api::ModelTransportFactory for Factory {
    fn validate(&self, provider: &ModelProviderInfo) -> std::result::Result<(), String> {
        validate_provider(provider)
    }
    fn create(
        &self,
        provider: &ModelProviderInfo,
        observer: Option<Arc<dyn ModelRequestObserver>>,
    ) -> Option<Arc<dyn codex_extension_api::ModelTransport>> {
        (provider.wire_api == codex_model_provider_info::WireApi::ClaudeCli).then(|| {
            Arc::new(Transport {
                observer,
                state: Default::default(),
                shutdown: CancellationToken::new(),
            }) as _
        })
    }
}
pub fn install<C: Sync>(registry: &mut codex_extension_api::ExtensionRegistryBuilder<C>) {
    registry.model_transport(Arc::new(Factory));
}
