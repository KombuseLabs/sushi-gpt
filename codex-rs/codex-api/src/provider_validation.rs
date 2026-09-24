use codex_protocol::execution_error::ProviderToolField;
use codex_protocol::execution_error::ProviderToolLocation;
use codex_protocol::execution_error::ProviderValidation;
use codex_protocol::execution_error::ProviderValidationCode as Code;
use codex_protocol::execution_error::ProviderValidationParameter as Parameter;

pub(crate) fn extract(body: &str) -> ProviderValidation {
    if body.len() > 64 * 1024 {
        return ProviderValidation::default();
    }
    let Ok(body) = serde_json::from_str::<serde_json::Value>(body) else {
        return ProviderValidation::default();
    };
    let code = match body["error"]["code"].as_str() {
        Some("invalid_request_error") => Some(Code::InvalidRequestError),
        Some("invalid_value") => Some(Code::InvalidValue),
        Some("invalid_type") => Some(Code::InvalidType),
        Some("missing_required_parameter") => Some(Code::MissingRequiredParameter),
        Some("unknown_parameter") => Some(Code::UnknownParameter),
        Some("unsupported_parameter") => Some(Code::UnsupportedParameter),
        Some("unsupported_value") => Some(Code::UnsupportedValue),
        _ => None,
    };
    ProviderValidation {
        code,
        parameter: body["error"]["param"].as_str().and_then(parameter),
        tool_location: body["error"]["param"].as_str().and_then(tool_location),
    }
}

// Accept only tools[N](.tools[M])?(.FIELD)?; never truncate a longer path.
fn tool_location(path: &str) -> Option<ProviderToolLocation> {
    if path.len() > 160 {
        return None;
    }
    let mut segments = path.split('.');
    let index = tool_index(segments.next()?)?;
    let mut field = segments.next();
    let nested_tool_index = if let Some(segment) = field
        && segment.starts_with("tools[")
    {
        let index = tool_index(segment)?;
        field = segments.next();
        Some(index)
    } else {
        None
    };
    let field = match field {
        None => None,
        Some("type") => Some(ProviderToolField::Type),
        Some("name") => Some(ProviderToolField::Name),
        Some("description") => Some(ProviderToolField::Description),
        Some("parameters") => Some(ProviderToolField::Parameters),
        Some("strict") => Some(ProviderToolField::Strict),
        Some("format") => Some(ProviderToolField::Format),
        Some(_) => return None,
    };
    if segments.next().is_some() {
        return None;
    }
    Some(ProviderToolLocation {
        tool_index: index,
        nested_tool_index,
        field,
    })
}

fn tool_index(segment: &str) -> Option<u16> {
    let index = segment.strip_prefix("tools[")?.strip_suffix(']')?;
    if index.is_empty()
        || index.len() > 5
        || (index.len() > 1 && index.starts_with('0'))
        || !index.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    index.parse().ok()
}

fn parameter(path: &str) -> Option<Parameter> {
    if path.is_empty() || path.len() > 160 || path.split('.').count() > 12 {
        return None;
    }
    let mut root = None;
    for segment in path.split('.') {
        let name = match segment.split_once('[') {
            Some((name, index)) => {
                let index = index.strip_suffix(']')?;
                if index.is_empty() || index.len() > 6 || !index.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                name
            }
            None => segment,
        };
        if !matches!(
            name,
            "model"
                | "service_tier"
                | "reasoning"
                | "tools"
                | "input"
                | "text"
                | "tool_choice"
                | "parallel_tool_calls"
                | "instructions"
                | "include"
                | "store"
                | "stream"
                | "previous_response_id"
                | "client_metadata"
                | "prompt_cache_key"
                | "type"
                | "role"
                | "content"
                | "name"
                | "description"
                | "parameters"
                | "properties"
                | "required"
                | "additionalProperties"
                | "strict"
                | "format"
                | "effort"
                | "summary"
                | "verbosity"
                | "enabled"
                | "id"
                | "status"
                | "arguments"
                | "call_id"
                | "namespace"
                | "defer_loading"
        ) {
            return None;
        }
        root.get_or_insert(name);
    }
    match root? {
        "model" => Some(Parameter::Model),
        "service_tier" => Some(Parameter::ServiceTier),
        "reasoning" => Some(Parameter::Reasoning),
        "tools" => Some(Parameter::Tools),
        "input" => Some(Parameter::Input),
        "text" => Some(Parameter::Text),
        "tool_choice" => Some(Parameter::ToolChoice),
        "parallel_tool_calls" => Some(Parameter::ParallelToolCalls),
        "instructions" => Some(Parameter::Instructions),
        "include" => Some(Parameter::Include),
        "store" => Some(Parameter::Store),
        "stream" => Some(Parameter::Stream),
        "previous_response_id" => Some(Parameter::PreviousResponseId),
        "client_metadata" => Some(Parameter::ClientMetadata),
        "prompt_cache_key" => Some(Parameter::PromptCacheKey),
        _ => None,
    }
}

#[cfg(test)]
#[path = "provider_validation_tests.rs"]
mod tests;
