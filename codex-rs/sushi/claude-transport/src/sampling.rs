use super::*;
impl Session {
    pub(super) async fn sample(
        &mut self,
        prompt: &Prompt,
        model: &str,
        metadata: &CodexResponsesMetadata,
        sender: &mpsc::Sender<Result<ResponseEvent>>,
        results: &mut mpsc::Receiver<ResponseItem>,
    ) -> Result<ResponseEvent> {
        let (_, mut body) = prepare(prompt)?;
        if model != self.model
            || prompt.base_instructions.text != self.instructions
            || serde_json::to_value(&prompt.tools).ok().as_ref() != Some(&self.raw_tools)
        {
            return Err(failure(
                "changing models, tools or instructions requires a new child",
            ));
        }
        let Value::Array(input) = body["input"].take() else {
            return Err(failure("invalid input"));
        };
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
        self.history = input;
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
        let mut attempt = self
            .observer
            .as_ref()
            .and_then(|observer| observer.start(metadata, model));
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
                    self.remember_value(raw)?;
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
                    self.control(frame, &mut calls, Some(sender), true).await?
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
                                        item: Some(item),
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
                        self.control(pending, &mut calls, Some(sender), false)
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
