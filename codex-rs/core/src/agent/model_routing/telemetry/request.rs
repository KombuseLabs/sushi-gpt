use super::Record;
use super::identifier;
use super::writer;
use crate::responses_metadata::CodexResponsesMetadata;
use codex_api::ResponseEvent;
use codex_protocol::protocol::TokenUsage;
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsageRecord {
    session_id: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    parent_thread_id: Option<codex_protocol::ThreadId>,
    parent_turn_id: Option<String>,
    root_turn_id: Option<String>,
    decision_id: Option<Uuid>,
    request_id: Option<String>,
    response_id: Option<String>,
    attempt_id: Uuid,
    requested_model: Option<String>,
    executed_model: Option<String>,
    usage: Option<Usage>,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cli_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Completed,
    Incomplete,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_write_input_tokens: Option<i64>,
    reasoning_output_tokens: Option<i64>,
    total_tokens: Option<i64>,
}

impl From<&TokenUsage> for Usage {
    fn from(u: &TokenUsage) -> Self {
        Self {
            input_tokens: (u.input_tokens >= 0).then_some(u.input_tokens),
            output_tokens: (u.output_tokens >= 0).then_some(u.output_tokens),
            total_tokens: (u.total_tokens >= 0).then_some(u.total_tokens),
            // The native parser collapses absent details to zero. Preserve that ambiguity.
            cached_input_tokens: (u.cached_input_tokens > 0).then_some(u.cached_input_tokens),
            cache_write_input_tokens: (u.cache_write_input_tokens > 0)
                .then_some(u.cache_write_input_tokens),
            reasoning_output_tokens: (u.reasoning_output_tokens > 0)
                .then_some(u.reasoning_output_tokens),
        }
    }
}

pub(crate) struct RequestAttempt {
    emitter: writer::Emitter,
    record: Option<UsageRecord>,
}

impl RequestAttempt {
    pub(crate) fn start(metadata: &CodexResponsesMetadata, model: &str) -> Option<Self> {
        Self::new(writer::emitter()?.clone(), metadata, model).into()
    }

    fn new(emitter: writer::Emitter, m: &CodexResponsesMetadata, model: &str) -> Self {
        Self {
            emitter,
            record: Some(UsageRecord {
                session_id: identifier(&m.session_id),
                thread_id: identifier(&m.thread_id),
                turn_id: m.turn_id.as_deref().and_then(identifier),
                parent_thread_id: m.parent_thread_id,
                parent_turn_id: m.parent_turn_id.as_deref().and_then(identifier),
                root_turn_id: m.root_turn_id.as_deref().and_then(identifier),
                decision_id: None,
                request_id: None,
                response_id: None,
                attempt_id: Uuid::new_v4(),
                requested_model: identifier(model),
                executed_model: None,
                usage: None,
                status: Status::Incomplete,
                transport: None,
                transport_instance_id: None,
                cli_version: None,
            }),
        }
    }

    pub(crate) fn set_request_id(&mut self, request_id: Option<&str>) {
        if let Some(record) = &mut self.record {
            record.request_id = request_id.and_then(identifier);
        }
    }

    pub(crate) fn set_cli_transport(&mut self, instance: &str, version: &str) {
        if let Some(record) = &mut self.record {
            record.transport = Some("claude_cli");
            record.transport_instance_id = identifier(instance);
            record.cli_version = identifier(version);
        }
    }

    pub(crate) fn observe(&mut self, event: &ResponseEvent) {
        let Some(record) = &mut self.record else {
            return;
        };
        match event {
            ResponseEvent::Created { response_id } => {
                record.response_id = response_id.as_deref().and_then(identifier)
            }
            ResponseEvent::ServerModel(model) => record.executed_model = identifier(model),
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                record.response_id = identifier(response_id);
                record.usage = token_usage.as_ref().map(Usage::from);
                record.status = Status::Completed;
                if let Some(record) = self.record.take() {
                    self.emitter.emit(Record::ResponseUsage(Box::new(record)));
                }
            }
            _ => {}
        }
    }
}

impl Drop for RequestAttempt {
    fn drop(&mut self) {
        if let Some(record) = self.record.take() {
            self.emitter.emit(Record::ResponseUsage(Box::new(record)));
        }
    }
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
