use super::*;
impl Session {
    pub(super) async fn start(
        provider: &ModelProviderInfo,
        prompt: &Prompt,
        model: &str,
        observer: Option<Arc<dyn ModelRequestObserver>>,
    ) -> Result<Self> {
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
            observer,
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
                .control(frame, &mut BTreeMap::new(), None, false)
                .await?;
        }
        Ok(session)
    }

    pub(super) async fn stop(&mut self) {
        let _ = tokio::time::timeout(Duration::from_millis(250), self.write(&json!({
            "type":"control_request","request_id":"native-interrupt","request":{"subtype":"interrupt","cancel_queued":true}
        }))).await;
        // Hard termination also covers CLIs without queued-interrupt support.
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }

    pub(super) async fn write(&mut self, value: &Value) -> Result<()> {
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

    pub(super) async fn read(&mut self, wait: ReadWait) -> Result<Value> {
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
        let parsed = serde_json::from_slice(&self.frame).map_err(|_| failure("invalid CLI JSON"));
        self.frame.clear();
        parsed
    }

    pub(super) fn remember(&mut self, item: &ResponseItem) -> Result<()> {
        let item =
            serde_json::to_value(item).map_err(|_| failure("cannot mirror native history"))?;
        self.remember_value(item)
    }

    /// Mirrors one already-serialized native item into the CLI-side history.
    pub(super) fn remember_value(&mut self, mut item: Value) -> Result<()> {
        self.adapter
            .prepare_item(&mut item)
            .map_err(|_| failure("cannot mirror native history"))?;
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
        }
        self.history.push(item);
        Ok(())
    }

    pub(super) async fn emit(
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

    pub(super) async fn control(
        &mut self,
        frame: Value,
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
                        let matches: Vec<String> = match tool_use_id {
                            Some(id) => {
                                let call =
                                    calls.get(id).ok_or_else(|| failure("missing tool call"))?;
                                if name != &call.name || arguments != &call.arguments {
                                    return Err(failure(
                                        "MCP tool call changed native tool arguments",
                                    ));
                                }
                                vec![id.to_owned()]
                            }
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
                            self.pending_tool_calls.push(frame);
                            return Ok(());
                        }
                        if matches.len() != 1 {
                            return Err(failure("uncorrelated or ambiguous MCP tool call"));
                        }
                        let call = calls
                            .get_mut(&matches[0])
                            .ok_or_else(|| failure("missing tool call"))?;
                        let item = call.item.take().ok_or_else(|| {
                            failure("duplicate MCP request for a native tool call")
                        })?;
                        call.request = Some(frame);
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
}
