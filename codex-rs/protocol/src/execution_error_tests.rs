use super::*;
use crate::protocol::CodexErrorInfo;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;

#[test]
fn context_is_payload_free_and_preserves_retry_and_known_codes() -> serde_json::Result<()> {
    let error = CodexErr::Stream("SECRET_PROVIDER_BODY".into())
        .with_retry_delay(Duration::from_secs(7))
        .with_execution_context(
            ExecutionErrorStage::StreamProcessing,
            /*http_status_code*/ None,
        )
        .with_execution_context(ExecutionErrorStage::RequestPreparation, Some(400));
    assert_eq!(
        error.retry_delay(/*retry_count*/ 1),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        serde_json::to_value(error.to_codex_protocol_error())?,
        json!({
            "execution_error": {"stage":"streamProcessing","category":"stream","http_status_code":null,"provider_validation":null}
        })
    );
    assert_eq!(
        CodexErr::ServerOverloaded
            .with_execution_context(ExecutionErrorStage::ProviderResponse, Some(503))
            .to_codex_protocol_error(),
        CodexErrorInfo::ServerOverloaded,
    );
    Ok(())
}

#[test]
fn execution_context_roundtrips_as_a_known_classification() -> serde_json::Result<()> {
    let info = CodexErr::Fatal("SECRET_LOCAL_PATH".into())
        .with_execution_context(
            ExecutionErrorStage::RequestPreparation,
            /*http_status_code*/ None,
        )
        .to_codex_protocol_error();
    let wire = serde_json::to_value(&info)?;
    assert_eq!(
        wire,
        json!({"execution_error":{"stage":"requestPreparation","category":"fatal","http_status_code":null,"provider_validation":null}})
    );
    assert_eq!(serde_json::from_value::<CodexErrorInfo>(wire)?, info);
    assert!(info.affects_turn_status());
    Ok(())
}

#[test]
fn tool_location_wire_bounds_and_older_validation_remain_compatible() -> serde_json::Result<()> {
    let older = json!({"code":"invalidValue","parameter":"tools"});
    let validation = serde_json::from_value::<ProviderValidation>(older)?;
    assert_eq!(
        validation,
        ProviderValidation {
            code: Some(ProviderValidationCode::InvalidValue),
            parameter: Some(ProviderValidationParameter::Tools),
            tool_location: None,
        }
    );
    for location in [
        json!({"toolIndex":65536,"nestedToolIndex":null,"field":"format"}),
        json!({"toolIndex":0,"nestedToolIndex":-1,"field":"format"}),
        json!({"toolIndex":0,"nestedToolIndex":null,"field":"PRIVATE"}),
        json!({"toolIndex":0.5,"nestedToolIndex":null,"field":null}),
        json!({"toolIndex":0,"nestedToolIndex":null,"field":"name","message":"PRIVATE"}),
    ] {
        assert!(serde_json::from_value::<ProviderToolLocation>(location).is_err());
    }
    Ok(())
}
