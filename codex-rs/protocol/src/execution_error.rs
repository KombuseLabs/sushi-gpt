//! Payload-free failure context captured at existing execution boundaries.

use super::error::CodexErr;
use super::error::CodexErrorDetails;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// The boundary at which an otherwise unclassified execution error was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum ExecutionErrorStage {
    RequestPreparation,
    Transport,
    ProviderResponse,
    StreamProcessing,
}

/// A closed classification of the error variant, never derived from error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum ExecutionErrorCategory {
    InvalidRequest,
    Encoding,
    Connection,
    Timeout,
    Stream,
    Fatal,
    Io,
    Other,
}

/// Recognized structured provider validation codes; unknown values are not copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum ProviderValidationCode {
    InvalidRequestError,
    InvalidValue,
    InvalidType,
    MissingRequiredParameter,
    UnknownParameter,
    UnsupportedParameter,
    UnsupportedValue,
}

/// Root request parameter categories, never provider-supplied field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum ProviderValidationParameter {
    Model,
    ServiceTier,
    Reasoning,
    Tools,
    Input,
    Text,
    ToolChoice,
    ParallelToolCalls,
    Instructions,
    Include,
    Store,
    Stream,
    PreviousResponseId,
    ClientMetadata,
    PromptCacheKey,
}

/// Structural tool fields only; JSON Schema property names are never retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum ProviderToolField {
    Type,
    Name,
    Description,
    Parameters,
    Strict,
    Format,
}

/// A tool slot, optionally inside one namespace, with an optional structural field.
/// Indices are limited to 0..=65535; null `field` identifies the tool object itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ProviderToolLocation {
    #[schemars(range(min = 0, max = 65535))]
    pub tool_index: u16,
    #[schemars(range(min = 0, max = 65535))]
    pub nested_tool_index: Option<u16>,
    pub field: Option<ProviderToolField>,
}

/// Null fields explicitly represent unavailable or unrecognized validation detail.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderValidation {
    pub code: Option<ProviderValidationCode>,
    pub parameter: Option<ProviderValidationParameter>,
    /// Available only for recognized, bounded `tools` locations.
    pub tool_location: Option<ProviderToolLocation>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecutionErrorContext {
    pub(crate) stage: ExecutionErrorStage,
    pub(crate) http_status_code: Option<u16>,
    pub(crate) provider_validation: Option<ProviderValidation>,
}

impl CodexErr {
    /// Preserve the innermost known boundary without changing retry or display behavior.
    /// Validation details are typed and bounded; existing specific error codes still take priority.
    pub fn with_execution_context(
        mut self,
        stage: ExecutionErrorStage,
        http_status_code: Option<u16>,
        provider_validation: Option<ProviderValidation>,
    ) -> Self {
        if self.execution_context.is_none() {
            self.execution_context = Some(ExecutionErrorContext {
                stage,
                http_status_code,
                provider_validation,
            });
        }
        self
    }

    pub(crate) fn execution_category(&self) -> ExecutionErrorCategory {
        match self.details() {
            CodexErrorDetails::InvalidRequest(_) | CodexErrorDetails::ToolCollision(_) => {
                ExecutionErrorCategory::InvalidRequest
            }
            CodexErrorDetails::Json(_) => ExecutionErrorCategory::Encoding,
            CodexErrorDetails::ConnectionFailed(_) => ExecutionErrorCategory::Connection,
            CodexErrorDetails::Timeout | CodexErrorDetails::RequestTimeout => {
                ExecutionErrorCategory::Timeout
            }
            CodexErrorDetails::Stream(_) | CodexErrorDetails::ResponseStreamFailed(_) => {
                ExecutionErrorCategory::Stream
            }
            CodexErrorDetails::Fatal(_) => ExecutionErrorCategory::Fatal,
            CodexErrorDetails::Io(_) => ExecutionErrorCategory::Io,
            _ => ExecutionErrorCategory::Other,
        }
    }
}

#[cfg(test)]
#[path = "execution_error_tests.rs"]
mod tests;
