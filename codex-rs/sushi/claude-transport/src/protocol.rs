use super::*;
pub(super) fn prepare(prompt: &Prompt) -> Result<(Adapter, Value)> {
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

pub(super) fn text_content(item: &Value) -> Result<String> {
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

pub(super) fn tool_result_content(item: &ResponseItem, body: &Value) -> Result<(Vec<Value>, bool)> {
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
pub(super) struct MessageStream {
    message: Option<Value>,
    arguments: BTreeMap<usize, String>,
}

impl MessageStream {
    pub(super) fn push(&mut self, event: &Value) -> Result<Option<Value>> {
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
                                let delta = event["delta"]["text"]
                                    .as_str()
                                    .ok_or_else(|| failure("invalid text delta"))?;
                                match block.get_mut("text") {
                                    Some(Value::String(text)) => text.push_str(delta),
                                    _ => block["text"] = json!(delta),
                                }
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

pub(super) fn parse_usage(value: &Value) -> Option<TokenUsage> {
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
