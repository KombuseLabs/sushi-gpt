//! A native-child-owned Claude process. Tools execute only in the native ToolRouter.
use crate::agent::child_config::telemetry::RequestAttempt;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;
use crate::responses_metadata::CodexResponsesMetadata;
use codex_api::ResponseEvent;
use codex_api::openresponses::Adapter;
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
pub(crate) struct Transport {
    state: Arc<Mutex<State>>,
    shutdown: CancellationToken,
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
                    session = Some(Session::start(&provider, &prompt, &model).await?);
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
}

fn prepare(prompt: &Prompt) -> Result<(Adapter, Value)> {
    let mut body = json!({"input":prompt.input,"tools":prompt.tools});
    let adapter =
        Adapter::prepare(&mut body).map_err(|_| failure("unsupported history or tool schema"))?;
    for item in body["input"]
        .as_array_mut()
        .ok_or_else(|| failure("invalid input"))?
    {
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
        }
    }
    Ok((adapter, body))
}

fn text_content(item: &Value) -> Result<String> {
    let mut text = String::new();
    for block in item["content"]
        .as_array()
        .ok_or_else(|| failure("text content required"))?
    {
        if !matches!(block["type"].as_str(), Some("input_text" | "output_text")) {
            return Err(failure("only plaintext messages are currently supported"));
        }
        text.push_str(
            block["text"]
                .as_str()
                .ok_or_else(|| failure("invalid text"))?,
        );
        text.push('\n');
    }
    Ok(text)
}

impl Session {
    async fn start(provider: &ModelProviderInfo, prompt: &Prompt, model: &str) -> Result<Self> {
        let (adapter, body) = prepare(prompt)?;
        let raw_tools =
            serde_json::to_value(&prompt.tools).map_err(|_| failure("invalid tools"))?;
        let tools = body["tools"].as_array().ok_or_else(|| failure("invalid tools"))?.iter().map(|tool| {
            json!({"name":tool["name"],"description":tool["description"],"inputSchema":tool["parameters"]})
        }).collect();
        let directory =
            tempfile::tempdir().map_err(|_| failure("cannot create isolated working directory"))?;
        let executable = provider
            .cli_command
            .as_ref()
            .ok_or_else(|| failure("missing executable"))?;
        let version_output = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(executable)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| failure("version probe timed out"))?
        .map_err(|_| failure("cannot execute configured CLI"))?;
        let version = String::from_utf8_lossy(&version_output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        if !version_output.status.success()
            || version.is_empty()
            || version.len() > 40
            || !version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err(failure("unrecognized CLI version"));
        }
        let mut child = Command::new(executable)
            .args([
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--tools",
                "",
                "--strict-mcp-config",
                "--mcp-config",
            ])
            .arg(json!({"mcpServers":{SERVER:{"type":"sdk","name":SERVER}}}).to_string())
            .args([
                "--setting-sources=",
                "--no-session-persistence",
                "--safe-mode",
                "--permission-prompts",
                "host",
                // A live session showed the CLI denying the native tools on its own
                // ("you haven't granted it yet") instead of asking the host. Grant the whole
                // native server up front; ToolRouter still decides what executes.
                "--allowedTools",
                &format!("mcp__{SERVER}"),
                "--max-thinking-tokens",
                "0",
                "--model",
            ])
            .arg(model)
            .current_dir(directory.path())
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| failure("cannot start configured CLI"))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| failure("missing CLI input"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| failure("missing CLI output"))?,
        );
        let mut session = Self {
            child,
            input,
            output,
            _directory: directory,
            adapter,
            raw_tools,
            tools,
            model: model.to_owned(),
            instructions: prompt.base_instructions.text.clone(),
            history: vec![],
            held_reply: None,
            instance_id: Uuid::new_v4().to_string(),
            version,
            timeout: provider.stream_idle_timeout(),
            in_turn: false,
            turn_id: None,
            frame: vec![],
            pending_tool_calls: vec![],
        };
        let mut system = prompt.base_instructions.text.clone();
        for item in body["input"]
            .as_array()
            .ok_or_else(|| failure("invalid input"))?
        {
            if item["type"] != "message" {
                return Err(failure("fresh plaintext history required"));
            }
            match item["role"].as_str() {
                Some("system" | "developer") => {
                    system.push('\n');
                    system.push_str(&text_content(item)?);
                }
                Some("user") => {}
                _ => return Err(failure("initial assistant history is unsupported")),
            }
        }
        session.write(&json!({"type":"control_request","request_id":"native-initialize","request":{
            "subtype":"initialize","sdkMcpServers":[SERVER],"systemPrompt":[system],"agents":{},"skills":[],
            "promptSuggestions":false,"agentProgressSummaries":false
        }})).await?;
        loop {
            let frame = session.read(ReadWait::ProviderActivity).await?;
            if frame["type"] == "control_response" {
                if frame["response"]["request_id"] != "native-initialize"
                    || frame["response"]["subtype"] != "success"
                {
                    return Err(failure("CLI initialization failed"));
                }
                // Account metadata is deliberately neither stored nor logged.
                break;
            }
            session
                .control(&frame, &mut BTreeMap::new(), None, false)
                .await?;
        }
        Ok(session)
    }

    async fn stop(&mut self) {
        let _ = tokio::time::timeout(Duration::from_millis(250), self.write(&json!({
            "type":"control_request","request_id":"native-interrupt","request":{"subtype":"interrupt","cancel_queued":true}
        }))).await;
        // Hard termination also covers CLIs without queued-interrupt support.
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    async fn write(&mut self, value: &Value) -> Result<()> {
        let mut line =
            serde_json::to_vec(value).map_err(|_| failure("cannot encode control frame"))?;
        line.push(b'\n');
        self.input
            .write_all(&line)
            .await
            .map_err(|_| failure("CLI input closed"))?;
        self.input
            .flush()
            .await
            .map_err(|_| failure("CLI input closed"))
    }

    async fn read(&mut self, wait: ReadWait) -> Result<Value> {
        // Bound a frame before allocation, including a peer that never sends a newline.
        loop {
            let available = match wait {
                ReadWait::ProviderActivity => {
                    tokio::time::timeout(self.timeout, self.output.fill_buf())
                        .await
                        .map_err(|_| failure("CLI stream timed out"))?
                }
                // Human approval and native tool execution are governed by native cancellation.
                ReadWait::NativeToolResult => self.output.fill_buf().await,
            }
            .map_err(|_| failure("CLI stream read failed"))?;
            if available.is_empty() {
                return Err(failure("CLI exited before completion"));
            }
            let length = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            if self.frame.len() + length > MAX_FRAME {
                return Err(failure("oversized CLI frame"));
            }
            self.frame.extend_from_slice(&available[..length]);
            self.output.consume(length);
            if self.frame.last() == Some(&b'\n') {
                break;
            }
        }
        serde_json::from_slice(&std::mem::take(&mut self.frame))
            .map_err(|_| failure("invalid CLI JSON"))
    }

    fn remember(&mut self, item: &ResponseItem) -> Result<()> {
        let mut body = json!({"input":[item],"tools":self.raw_tools});
        Adapter::prepare(&mut body).map_err(|_| failure("cannot mirror native history"))?;
        let mut item = body["input"][0].take();
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
        }
        self.history.push(item);
        Ok(())
    }

    async fn emit(
        &mut self,
        sender: &mpsc::Sender<Result<ResponseEvent>>,
        item: ResponseItem,
    ) -> Result<()> {
        self.remember(&item)?;
        sender
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .map_err(|_| CodexErr::TurnAborted)
    }

    async fn control(
        &mut self,
        frame: &Value,
        calls: &mut BTreeMap<String, ToolCall>,
        sender: Option<&mpsc::Sender<Result<ResponseEvent>>>,
        defer_uncorrelated: bool,
    ) -> Result<()> {
        if frame["type"] != "control_request" {
            return Err(failure("unexpected control frame"));
        }
        let request = &frame["request"];
        let response = match request["subtype"].as_str() {
            Some("can_use_tool") => {
                let id = request["tool_use_id"]
                    .as_str()
                    .ok_or_else(|| failure("permission request without tool ID"))?;
                let call = calls
                    .get(id)
                    .ok_or_else(|| failure("permission request for an unknown native tool"))?;
                if request["tool_name"] != format!("{PREFIX}{}", call.name)
                    || request["input"] != call.arguments
                {
                    return Err(failure("permission request changed native tool arguments"));
                }
                // This permits transport into MCP only. ToolRouter decides whether to execute.
                json!({"behavior":"allow","updatedInput":call.arguments})
            }
            Some("mcp_message") if request["server_name"] == SERVER => {
                let message = &request["message"];
                let result = match message["method"].as_str() {
                    Some("initialize") => {
                        json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"codex-native","version":"1"}})
                    }
                    Some("notifications/initialized") => json!({}),
                    Some("tools/list") => json!({"tools":self.tools}),
                    Some("tools/call") => {
                        let name = &message["params"]["name"];
                        let arguments = &message["params"]["arguments"];
                        // Claude Code 2.1.x names the originating tool_use block in _meta.
                        let tool_use_id = message["params"]["_meta"]["claudecode/toolUseId"]
                            .as_str()
                            .filter(|id| calls.get(*id).is_some_and(|call| call.request.is_none()));
                        let matches: Vec<_> = match tool_use_id {
                            Some(id) => vec![id.to_owned()],
                            None => calls
                                .iter()
                                .filter(|(_, call)| {
                                    call.request.is_none()
                                        && name == &call.name
                                        && arguments == &call.arguments
                                })
                                .map(|(id, _)| id.clone())
                                .collect(),
                        };
                        if matches.is_empty() && defer_uncorrelated {
                            // The CLI issues tools/call before message_delta/message_stop; the
                            // model message that declares this call is still streaming.
                            self.pending_tool_calls.push(frame.clone());
                            return Ok(());
                        }
                        if matches.len() != 1 {
                            return Err(failure("uncorrelated or ambiguous MCP tool call"));
                        }
                        let call = calls
                            .get(&matches[0])
                            .ok_or_else(|| failure("missing tool call"))?;
                        if name != &call.name || arguments != &call.arguments {
                            return Err(failure("MCP tool call changed native tool arguments"));
                        }
                        let call = calls
                            .get_mut(&matches[0])
                            .ok_or_else(|| failure("missing tool call"))?;
                        call.request = Some(frame.clone());
                        let item = call.item.clone();
                        self.emit(
                            sender.ok_or_else(|| failure("tool call during initialization"))?,
                            item,
                        )
                        .await?;
                        return Ok(());
                    }
                    _ => return Err(failure("unsupported MCP method")),
                };
                json!({"mcp_response":{"jsonrpc":"2.0","id":message.get("id").cloned().unwrap_or(json!(0)),"result":result}})
            }
            _ => return Err(failure("unsupported CLI control request")),
        };
        self.write(&json!({"type":"control_response","response":{"subtype":"success","request_id":frame["request_id"],"response":response}})).await
    }

    async fn sample(
        &mut self,
        prompt: &Prompt,
        model: &str,
        metadata: &CodexResponsesMetadata,
        sender: &mpsc::Sender<Result<ResponseEvent>>,
        results: &mut mpsc::Receiver<ResponseItem>,
    ) -> Result<ResponseEvent> {
        let (_, body) = prepare(prompt)?;
        if model != self.model
            || prompt.base_instructions.text != self.instructions
            || serde_json::to_value(&prompt.tools).ok().as_ref() != Some(&self.raw_tools)
        {
            return Err(failure(
                "changing models, tools or instructions requires a new child",
            ));
        }
        let input = body["input"]
            .as_array()
            .ok_or_else(|| failure("invalid input"))?;
        if !input.starts_with(&self.history) {
            return Err(failure(
                "native history changed; replay and compaction are unsupported",
            ));
        }
        let mut new_text = String::new();
        for item in &input[self.history.len()..] {
            if self.history.is_empty()
                && matches!(item["role"].as_str(), Some("system" | "developer"))
            {
                continue;
            }
            if item["type"] != "message" || item["role"] != "user" {
                return Err(failure("unsupported native continuation"));
            }
            new_text.push_str(&text_content(item)?);
        }
        self.history = input.clone();
        self.in_turn = true;
        if let Some(reply) = self.held_reply.take() {
            if !new_text.is_empty() {
                return Err(failure(
                    "mailbox input during a pending tool continuation is unsupported",
                ));
            }
            self.write(&reply).await?;
        } else {
            if new_text.is_empty() {
                return Err(failure("missing native user input"));
            }
            self.write(&json!({"type":"user","message":{"role":"user","content":new_text},"parent_tool_use_id":null,"uuid":Uuid::new_v4().to_string()})).await?;
        }
        let mut attempt = RequestAttempt::start(metadata, model);
        if let Some(attempt) = &mut attempt {
            attempt.set_cli_transport(&self.instance_id, &self.version);
        }
        self.pending_tool_calls.clear();
        let mut calls = BTreeMap::<String, ToolCall>::new();
        let mut response_id = None;
        let mut usage = None;
        let mut final_message = None;
        let mut message_stream = MessageStream::default();
        loop {
            let wait = if calls
                .values()
                .any(|call| call.request.is_some() && !call.finished)
            {
                ReadWait::NativeToolResult
            } else {
                ReadWait::ProviderActivity
            };
            let frame = tokio::select! {
                item = results.recv(), if !calls.is_empty() => {
                    let item = item.ok_or_else(|| failure("native tool result channel closed"))?;
                    let raw = serde_json::to_value(&item).map_err(|_| failure("invalid native result"))?;
                    let id = raw["call_id"].as_str().ok_or_else(|| failure("native result has no call ID"))?;
                    let call = calls.get_mut(id).ok_or_else(|| failure("result for unknown native call"))?;
                    if call.finished { return Err(failure("duplicate native result")); }
                    let request = call.request.as_ref().ok_or_else(|| failure("result before MCP request"))?;
                    let (content, is_error) = tool_result_content(&item, &raw["output"])?;
                    let reply = json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"mcp_response":{"jsonrpc":"2.0","id":request["request"]["message"]["id"],"result":{"content":content,"isError":is_error}}}}});
                    call.finished = true;
                    self.remember(&item)?;
                    if calls.values().all(|call| call.finished) {
                        // Native limits and cancellation run before releasing the final result.
                        self.held_reply = Some(reply);
                        break;
                    }
                    self.write(&reply).await?;
                    continue;
                }
                frame = self.read(wait) => frame?,
            };
            let frame = match frame["type"].as_str() {
                Some("assistant") => continue, // Complete content is assembled from the wire stream.
                Some("stream_event") => {
                    if !frame["parent_tool_use_id"].is_null() {
                        return Err(failure("unexpected CLI subagent"));
                    }
                    let Some(message) = message_stream.push(&frame["event"])? else {
                        continue;
                    };
                    json!({"type":"assistant","message":message})
                }
                _ => frame,
            };
            match frame["type"].as_str() {
                Some("control_request") => {
                    self.control(&frame, &mut calls, Some(sender), true).await?
                }
                Some("assistant") => {
                    if !frame["parent_tool_use_id"].is_null() || response_id.is_some() {
                        return Err(failure("unexpected CLI agent or model continuation"));
                    }
                    let message = &frame["message"];
                    let id = message["id"]
                        .as_str()
                        .ok_or_else(|| failure("assistant response without ID"))?
                        .to_owned();
                    let executed = message["model"]
                        .as_str()
                        .ok_or_else(|| failure("assistant response without model"))?
                        .to_owned();
                    for event in [
                        ResponseEvent::Created {
                            response_id: Some(id.clone()),
                        },
                        ResponseEvent::ServerModel(executed),
                    ] {
                        if let Some(attempt) = &mut attempt {
                            attempt.observe(&event);
                        }
                        sender
                            .send(Ok(event))
                            .await
                            .map_err(|_| CodexErr::TurnAborted)?;
                    }
                    response_id = Some(id.clone());
                    usage = parse_usage(&message["usage"]);
                    let mut text = String::new();
                    for block in message["content"]
                        .as_array()
                        .ok_or_else(|| failure("invalid assistant content"))?
                    {
                        match block["type"].as_str() {
                            Some("text") => text.push_str(
                                block["text"]
                                    .as_str()
                                    .ok_or_else(|| failure("invalid assistant text"))?,
                            ),
                            Some("tool_use") => {
                                let call_id = block["id"]
                                    .as_str()
                                    .ok_or_else(|| failure("tool call without ID"))?
                                    .to_owned();
                                let name = block["name"]
                                    .as_str()
                                    .and_then(|name| name.strip_prefix(PREFIX))
                                    .ok_or_else(|| failure("CLI attempted a non-native tool"))?
                                    .to_owned();
                                let arguments = block["input"].clone();
                                if calls.contains_key(&call_id)
                                    || calls.values().any(|call| {
                                        call.name == name && call.arguments == arguments
                                    })
                                {
                                    return Err(failure(
                                        "duplicate or ambiguous assistant tool call",
                                    ));
                                }
                                let item: ResponseItem = serde_json::from_value(json!({"type":"function_call","name":name,"call_id":call_id,"arguments":arguments.to_string(),"encrypted_function_args":[]})).map_err(|_| failure("invalid tool call"))?;
                                let item = self
                                    .adapter
                                    .restore(item, true)
                                    .map_err(|_| failure("unknown native tool"))?;
                                calls.insert(
                                    call_id,
                                    ToolCall {
                                        name,
                                        arguments,
                                        item,
                                        request: None,
                                        finished: false,
                                    },
                                );
                            }
                            // No reasoning is replayed as user-visible text or fabricated reasoning.
                            Some("thinking" | "redacted_thinking") => {
                                return Err(failure("reasoning blocks are not yet supported"));
                            }
                            _ => return Err(failure("unsupported assistant content")),
                        }
                    }
                    if calls.len() > 1 && !prompt.parallel_tool_calls {
                        return Err(failure(
                            "model returned multiple tool calls while parallel calls are disabled",
                        ));
                    }
                    if !text.is_empty() {
                        let item = serde_json::from_value(json!({"type":"message","role":"assistant","id":format!("msg_{id}"),"content":[{"type":"output_text","text":text}]})).map_err(|_| failure("invalid assistant message"))?;
                        if calls.is_empty() {
                            final_message = Some(item);
                        } else {
                            self.emit(sender, item).await?;
                        }
                    }
                    // Correlate tools/call requests that arrived before this message completed.
                    for pending in std::mem::take(&mut self.pending_tool_calls) {
                        self.control(&pending, &mut calls, Some(sender), false)
                            .await?;
                    }
                }
                Some("result") => {
                    if !calls.is_empty()
                        || response_id.is_none()
                        || frame["subtype"] != "success"
                        || frame["is_error"] != false
                        || frame["queued_turn_count"]
                            .as_u64()
                            .is_some_and(|count| count != 0)
                    {
                        return Err(failure("CLI turn did not complete successfully"));
                    }
                    if let Some(item) = final_message.take() {
                        self.emit(sender, item).await?;
                    }
                    self.in_turn = false;
                    break;
                }
                // Informational frames. Claude Code 2.1.x emits `command_lifecycle` (queued/
                // started/finished) after every user message and `rate_limit_event` after the
                // status frame; neither carries model content.
                Some(
                    "stream_event" | "user" | "keep_alive" | "rate_limit_event"
                    | "command_lifecycle",
                ) => {}
                Some("system") if matches!(frame["subtype"].as_str(), Some("init" | "status")) => {}
                Some("system") if frame["subtype"] == "permission_denied" => {
                    return Err(failure(
                        "CLI denied a native tool itself instead of asking the host",
                    ));
                }
                _ => {
                    return Err(failure(
                        "unsupported CLI event or unexpected background activity",
                    ));
                }
            }
        }
        let event = ResponseEvent::Completed {
            response_id: response_id.ok_or_else(|| failure("missing model response"))?,
            token_usage: usage,
            usage_metadata: None,
            end_turn: Some(!self.in_turn),
        };
        if let Some(attempt) = &mut attempt {
            attempt.observe(&event);
        }
        Ok(event)
    }
}

fn tool_result_content(item: &ResponseItem, body: &Value) -> Result<(Vec<Value>, bool)> {
    let success = match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.success,
        _ => return Err(failure("unexpected native tool result type")),
    };
    let content = match body {
        Value::String(text) => vec![json!({"type":"text","text":text})],
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                if block["type"] != "input_text" {
                    return Err(failure("non-text tool results are unsupported"));
                }
                let text = block["text"]
                    .as_str()
                    .ok_or_else(|| failure("invalid text tool result"))?;
                Ok(json!({"type":"text","text":text}))
            })
            .collect::<Result<Vec<_>>>()?,
        _ => return Err(failure("unsupported tool result body")),
    };
    Ok((content, success == Some(false)))
}

#[derive(Default)]
struct MessageStream {
    message: Option<Value>,
    arguments: BTreeMap<usize, String>,
}

impl MessageStream {
    fn push(&mut self, event: &Value) -> Result<Option<Value>> {
        match event["type"].as_str() {
            Some("message_start") => {
                if self.message.is_some() {
                    return Err(failure("overlapping model messages"));
                }
                let mut message = event["message"].clone();
                message["content"] = json!([]);
                self.message = Some(message);
            }
            Some("content_block_start" | "content_block_delta" | "content_block_stop") => {
                let index = event["index"]
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    .ok_or_else(|| failure("invalid block index"))?;
                let blocks = self
                    .message
                    .as_mut()
                    .and_then(|m| m["content"].as_array_mut())
                    .ok_or_else(|| failure("block outside model message"))?;
                match event["type"].as_str() {
                    Some("content_block_start") => {
                        if index != blocks.len() {
                            return Err(failure("nonsequential block index"));
                        }
                        let block = event["content_block"].clone();
                        if !matches!(block["type"].as_str(), Some("text" | "tool_use")) {
                            return Err(failure("unsupported model content block"));
                        }
                        blocks.push(block);
                    }
                    Some("content_block_delta") => {
                        let block = blocks
                            .get_mut(index)
                            .ok_or_else(|| failure("delta for unknown block"))?;
                        match event["delta"]["type"].as_str() {
                            Some("text_delta") if block["type"] == "text" => {
                                let text = format!(
                                    "{}{}",
                                    block["text"].as_str().unwrap_or_default(),
                                    event["delta"]["text"]
                                        .as_str()
                                        .ok_or_else(|| failure("invalid text delta"))?
                                );
                                block["text"] = json!(text);
                            }
                            Some("input_json_delta") if block["type"] == "tool_use" => {
                                self.arguments.entry(index).or_default().push_str(
                                    event["delta"]["partial_json"]
                                        .as_str()
                                        .ok_or_else(|| failure("invalid tool delta"))?,
                                )
                            }
                            _ => return Err(failure("unsupported model delta")),
                        }
                    }
                    _ => {
                        if let Some(arguments) = self.arguments.remove(&index) {
                            let block = blocks
                                .get_mut(index)
                                .ok_or_else(|| failure("stop for unknown block"))?;
                            block["input"] = serde_json::from_str(&arguments)
                                .map_err(|_| failure("invalid tool arguments"))?;
                        }
                    }
                }
            }
            Some("message_delta") => {
                let message = self
                    .message
                    .as_mut()
                    .ok_or_else(|| failure("delta outside model message"))?;
                message["stop_reason"] = event["delta"]["stop_reason"].clone();
                if let Some(usage) = event["usage"].as_object() {
                    for (key, value) in usage {
                        message["usage"][key] = value.clone();
                    }
                }
            }
            Some("message_stop") => {
                if !self.arguments.is_empty() {
                    return Err(failure("unfinished tool arguments"));
                }
                return self
                    .message
                    .take()
                    .map(Some)
                    .ok_or_else(|| failure("stop outside model message"));
            }
            _ => return Err(failure("unsupported model stream event")),
        }
        Ok(None)
    }
}

fn parse_usage(value: &Value) -> Option<TokenUsage> {
    let input = value["input_tokens"].as_i64()?;
    let output = value["output_tokens"].as_i64()?;
    let cached = value["cache_read_input_tokens"].as_i64().unwrap_or(0);
    let written = value["cache_creation_input_tokens"].as_i64().unwrap_or(0);
    Some(TokenUsage {
        input_tokens: input + cached + written,
        output_tokens: output,
        cached_input_tokens: cached,
        cache_write_input_tokens: written,
        total_tokens: input + cached + written + output,
        ..Default::default()
    })
}

#[cfg(test)]
#[path = "claude_cli_tests.rs"]
mod tests;
