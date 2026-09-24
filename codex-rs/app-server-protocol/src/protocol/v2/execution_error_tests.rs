use super::CodexErrorInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::execution_error::ExecutionErrorStage;
use codex_protocol::execution_error::ProviderValidation;
use codex_protocol::execution_error::ProviderValidationCode;
use codex_protocol::execution_error::ProviderValidationParameter;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn execution_diagnostic_wire_contract_contains_only_typed_context() -> serde_json::Result<()> {
    let core = CodexErr::InvalidRequest("SECRET_PROVIDER_BODY".into())
        .with_execution_context(
            ExecutionErrorStage::ProviderResponse,
            Some(400),
            Some(ProviderValidation {
                code: Some(ProviderValidationCode::InvalidValue),
                parameter: Some(ProviderValidationParameter::ServiceTier),
                tool_location: None,
            }),
        )
        .to_codex_protocol_error();
    let public = CodexErrorInfo::from(core);
    assert_eq!(
        serde_json::to_value(&public)?,
        json!({
            "executionError":{"stage":"providerResponse","category":"invalidRequest","httpStatusCode":400,"providerValidation":{"code":"invalidValue","parameter":"serviceTier","toolLocation":null}}
        })
    );
    assert_eq!(
        serde_json::from_value::<CodexErrorInfo>(serde_json::to_value(&public)?)?,
        public
    );
    Ok(())
}

#[test]
fn tool_location_survives_public_error_conversion() -> serde_json::Result<()> {
    use codex_protocol::execution_error::ProviderToolField;
    use codex_protocol::execution_error::ProviderToolLocation;

    let core = CodexErr::InvalidRequest("PRIVATE_TEST_SENTINEL".into())
        .with_execution_context(
            ExecutionErrorStage::ProviderResponse,
            Some(400),
            Some(ProviderValidation {
                code: Some(ProviderValidationCode::InvalidValue),
                parameter: Some(ProviderValidationParameter::Tools),
                tool_location: Some(ProviderToolLocation {
                    tool_index: 0,
                    nested_tool_index: Some(1),
                    field: Some(ProviderToolField::Format),
                }),
            }),
        )
        .to_codex_protocol_error();
    let public = CodexErrorInfo::from(core);
    let wire = serde_json::to_value(&public)?;
    assert_eq!(
        wire,
        json!({"executionError":{
            "stage":"providerResponse", "category":"invalidRequest", "httpStatusCode":400,
            "providerValidation":{"code":"invalidValue","parameter":"tools","toolLocation":{"toolIndex":0,"nestedToolIndex":1,"field":"format"}}
        }})
    );
    assert_eq!(serde_json::from_value::<CodexErrorInfo>(wire)?, public);
    Ok(())
}
