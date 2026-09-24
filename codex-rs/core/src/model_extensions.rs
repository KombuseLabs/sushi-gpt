//! Core-owned projections for optional native model execution extensions.
use crate::client_common::Prompt;
use crate::responses_metadata::CodexResponsesMetadata;
use codex_extension_api::RequestMetadata;
use codex_extension_api::SamplingPrompt;
impl From<&Prompt> for SamplingPrompt {
    fn from(p: &Prompt) -> Self {
        Self {
            input: p.input.clone(),
            tools: p.tools.clone(),
            parallel_tool_calls: p.parallel_tool_calls,
            base_instructions: p.base_instructions.clone(),
            output_schema: p.output_schema.clone(),
            cyber_access_program: p.cyber_access_program,
        }
    }
}
impl From<&CodexResponsesMetadata> for RequestMetadata {
    fn from(m: &CodexResponsesMetadata) -> Self {
        Self {
            session_id: m.session_id.clone(),
            thread_id: m.thread_id.clone(),
            turn_id: m.turn_id.clone(),
            parent_thread_id: m.parent_thread_id,
            parent_turn_id: m.parent_turn_id.clone(),
            root_turn_id: m.root_turn_id.clone(),
        }
    }
}
