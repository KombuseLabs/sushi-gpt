//! Explicit stateless adaptation. Native execution and tool validation stay in core.
use crate::ApiError;
use crate::ResponseEvent;
use crate::ResponseStream;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

#[derive(Debug)]
struct Tool {
    name: String,
    namespace: Option<String>,
    custom: bool,
}

#[derive(Debug)]
pub struct Adapter {
    tools: BTreeMap<String, Tool>,
    /// (namespace, name) -> alias, the reverse of `tools`, for O(log n) history rewrites.
    aliases: BTreeMap<(Option<String>, String), String>,
}

fn unsupported() -> ApiError {
    ApiError::Stream(
        "Unsupported OpenResponses input, tool, or response; compatibility processing stopped"
            .to_string(),
    )
}

fn contains_encryption(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (key == "encrypted_content" && !value.is_null())
                || (key == "encrypted" && value == true)
                || (key == "encrypted_function_args"
                    && value.as_array().is_some_and(|v| !v.is_empty()))
                || contains_encryption(value)
        }),
        Value::Array(array) => array.iter().any(contains_encryption),
        _ => false,
    }
}

impl Adapter {
    pub fn prepare(body: &mut Value) -> Result<Self, ApiError> {
        if contains_encryption(body) {
            return Err(unsupported());
        }
        let mut adapter = Self {
            tools: BTreeMap::new(),
            aliases: BTreeMap::new(),
        };
        let mut flattened = Vec::new();
        for tool in body["tools"].as_array().into_iter().flatten() {
            let (namespace, definitions) = if tool["type"] == "namespace" {
                (
                    Some(tool["name"].as_str().ok_or_else(unsupported)?.to_string()),
                    tool["tools"].as_array().ok_or_else(unsupported)?.clone(),
                )
            } else {
                (None, vec![tool.clone()])
            };
            for mut definition in definitions {
                let custom = match definition["type"].as_str() {
                    Some("function") => false,
                    Some("custom") => true,
                    _ => return Err(unsupported()),
                };
                let name = definition["name"]
                    .as_str()
                    .ok_or_else(unsupported)?
                    .to_string();
                let key = (namespace.clone(), name.clone());
                if adapter.aliases.contains_key(&key) {
                    return Err(unsupported());
                }
                let alias = format!("codex_tool_{}", adapter.tools.len());
                adapter.aliases.insert(key, alias.clone());
                if custom {
                    definition["parameters"] = json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false});
                }
                definition["type"] = json!("function");
                definition["name"] = json!(alias);
                definition
                    .as_object_mut()
                    .ok_or_else(unsupported)?
                    .retain(|k, _| {
                        matches!(
                            k.as_str(),
                            "type" | "name" | "description" | "parameters" | "strict"
                        )
                    });
                adapter.tools.insert(
                    alias,
                    Tool {
                        name,
                        namespace: namespace.clone(),
                        custom,
                    },
                );
                flattened.push(definition);
            }
        }
        body["tools"] = Value::Array(flattened);
        for item in body["input"].as_array_mut().ok_or_else(unsupported)? {
            adapter.prepare_item(item)?;
        }
        body["store"] = json!(false);
        body["stream"] = json!(true);
        // No reasoning, provider-native server tools, persistence, or service-tier translation.
        if body.get("reasoning").is_some_and(|r| {
            !r.is_null()
                && r.as_object()
                    .is_none_or(|o| o.values().any(|v| !v.is_null()))
        }) || body.get("access_programs").is_some_and(|v| !v.is_null())
        {
            return Err(unsupported());
        }
        let object = body.as_object_mut().ok_or_else(unsupported)?;
        object.retain(|key, _| {
            matches!(
                key.as_str(),
                "model"
                    | "instructions"
                    | "input"
                    | "tools"
                    | "tool_choice"
                    | "parallel_tool_calls"
                    | "store"
                    | "stream"
                    | "text"
            )
        });
        Ok(adapter)
    }

    /// Rewrites one native input item in place into the flattened tool vocabulary.
    pub fn prepare_item(&self, item: &mut Value) -> Result<(), ApiError> {
        if contains_encryption(item) {
            return Err(unsupported());
        }
        match item["type"].as_str() {
            Some("agent_message") => {
                let content = item["content"].as_array().ok_or_else(unsupported)?;
                if content.iter().any(|c| c["type"] != "input_text") {
                    return Err(unsupported());
                }
                *item = json!({"type":"message","role":"user","content":content});
            }
            Some("function_call" | "custom_tool_call") => {
                let key = (
                    item["namespace"].as_str().map(str::to_owned),
                    item["name"].as_str().ok_or_else(unsupported)?.to_owned(),
                );
                let alias = self.aliases.get(&key).ok_or_else(unsupported)?;
                let tool = self.tools.get(alias).ok_or_else(unsupported)?;
                if tool.custom {
                    item["arguments"] = json!(json!({"input":item["input"]}).to_string());
                }
                item["type"] = json!("function_call");
                item["name"] = json!(alias);
                let object = item.as_object_mut().ok_or_else(unsupported)?;
                object.remove("namespace");
                object.remove("input");
                object.remove("encrypted_function_args");
            }
            Some("custom_tool_call_output") => {
                item["type"] = json!("function_call_output");
            }
            Some("message" | "function_call_output") => {}
            _ => return Err(unsupported()),
        }
        let kind = item["type"].as_str().ok_or_else(unsupported)?.to_string();
        item.as_object_mut()
            .ok_or_else(unsupported)?
            .retain(|key, _| match kind.as_str() {
                "message" => matches!(key.as_str(), "type" | "role" | "content" | "id"),
                "function_call" => matches!(
                    key.as_str(),
                    "type" | "name" | "arguments" | "call_id" | "id"
                ),
                "function_call_output" => matches!(key.as_str(), "type" | "call_id" | "output"),
                _ => false,
            });
        Ok(())
    }

    pub fn restore(&self, item: ResponseItem, done: bool) -> Result<ResponseItem, ApiError> {
        let mut item = serde_json::to_value(item).map_err(|_| unsupported())?;
        if contains_encryption(&item) {
            return Err(unsupported());
        }
        if !matches!(item["type"].as_str(), Some("function_call" | "message")) {
            return Err(unsupported());
        }
        if item["type"] == "function_call" {
            let alias = item["name"].as_str().ok_or_else(unsupported)?;
            let tool = self.tools.get(alias).ok_or_else(unsupported)?;
            item["name"] = json!(tool.name);
            item["namespace"] = json!(tool.namespace);
            if tool.custom {
                let input = if done {
                    let args: Value =
                        serde_json::from_str(item["arguments"].as_str().ok_or_else(unsupported)?)
                            .map_err(|_| unsupported())?;
                    args["input"].as_str().ok_or_else(unsupported)?.to_string()
                } else {
                    String::new()
                };
                item["type"] = json!("custom_tool_call");
                item["input"] = json!(input);
                item.as_object_mut()
                    .ok_or_else(unsupported)?
                    .remove("arguments");
            } else {
                item["encrypted_function_args"] = json!([]);
            }
        }
        serde_json::from_value(item).map_err(|_| unsupported())
    }

    pub(crate) fn wrap(self, mut stream: ResponseStream) -> ResponseStream {
        let (tx, rx_event) = mpsc::channel(32);
        let upstream_request_id = stream.upstream_request_id.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = tx.closed() => break,
                    event = stream.next() => match event { Some(event) => event, None => break },
                };
                let event = match event {
                    Ok(ResponseEvent::OutputItemAdded(item)) => self
                        .restore(item, /*done*/ false)
                        .map(ResponseEvent::OutputItemAdded),
                    Ok(ResponseEvent::OutputItemDone(item)) => {
                        self.restore(item, /*done*/ true)
                            .map(ResponseEvent::OutputItemDone)
                    }
                    // Native tools execute completed items; wrapped JSON is not raw custom input.
                    Ok(ResponseEvent::ToolCallInputDelta { .. }) => continue,
                    event => event,
                };
                let failed = event.is_err();
                if tx.send(event).await.is_err() || failed {
                    break;
                }
            }
        });
        ResponseStream {
            rx_event,
            upstream_request_id,
        }
    }
}

#[cfg(test)]
#[path = "openresponses_tests.rs"]
mod tests;
