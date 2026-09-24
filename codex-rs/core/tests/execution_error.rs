//! Native execution failures retain a bounded stage instead of collapsing to Other.
use anyhow::Result;
use codex_protocol::execution_error::ExecutionErrorCategory;
use codex_protocol::execution_error::ExecutionErrorStage;
use codex_protocol::execution_error::ProviderValidation;
use codex_protocol::execution_error::ProviderValidationCode;
use codex_protocol::execution_error::ProviderValidationParameter;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;
use wiremock::ResponseTemplate;

#[derive(Clone, Copy)]
enum Failure {
    Preparation,
    Provider,
    ToolLocation,
    Unknown,
    Oversized,
    Stream,
}

#[test_case(Failure::Preparation; "local request preparation")]
#[test_case(Failure::Provider; "provider rejects HTTP request")]
#[test_case(Failure::ToolLocation; "exact tool location survives native turn")]
#[test_case(Failure::Unknown; "unknown provider validation omitted")]
#[test_case(Failure::Oversized; "oversized provider validation omitted")]
#[test_case(Failure::Stream; "stream ends before completion")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_failure_context_survives_native_turn(failure: Failure) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response =
        match failure {
            Failure::Provider => ResponseTemplate::new(400)
                .set_body_json(json!({"error":{"message":"SECRET_PROVIDER_BODY","code":"invalid_value","param":"input[0].tools[1].name"}})),
            Failure::ToolLocation => ResponseTemplate::new(400)
                .set_body_json(json!({"error":{"message":"PRIVATE_TEST_SENTINEL","code":"invalid_value","param":"tools[0].tools[1].format"}})),
            Failure::Unknown => ResponseTemplate::new(400).set_body_json(json!({"error":{"code":"SECRET_CODE", "param":"input.SECRET_FIELD", "message":"service_tier"}})),
            Failure::Oversized => ResponseTemplate::new(400).set_body_json(json!({"error":{"code":"invalid_value", "param":"service_tier", "message":"x".repeat(65536)}})),
            Failure::Preparation | Failure::Stream => responses::sse_response(responses::sse(
                vec![responses::ev_response_created("incomplete")],
            )),
        };
    let requests = responses::mount_response_once(&server, response).await;
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
        if matches!(failure, Failure::Preparation) {
            config.model_provider.wire_api = codex_model_provider_info::WireApi::ClaudeCli;
            // Root use is rejected before any CLI process or network request.
            config.model_reasoning_effort = None;
            config.service_tier = None;
        }
    });
    let test = builder.build_with_auto_env(&server).await?;
    test.codex
        .start_or_steer_turn(codex_core::TurnInputRequest::user_input(vec![
            codex_protocol::user_input::UserInput::Text {
                text: "Synthetic failure fixture".into(),
                text_elements: vec![],
            },
        ]))
        .await?;
    let error = core_test_support::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::Error(error) => Some(error.clone()),
        _ => None,
    })
    .await;
    core_test_support::wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let (stage, category, status, count) = match failure {
        Failure::Preparation => (
            ExecutionErrorStage::RequestPreparation,
            ExecutionErrorCategory::Fatal,
            None,
            0,
        ),
        Failure::Provider | Failure::ToolLocation | Failure::Unknown | Failure::Oversized => (
            ExecutionErrorStage::ProviderResponse,
            ExecutionErrorCategory::InvalidRequest,
            Some(400),
            1,
        ),
        Failure::Stream => (
            ExecutionErrorStage::StreamProcessing,
            ExecutionErrorCategory::Stream,
            None,
            1,
        ),
    };
    assert_eq!(
        error.codex_error_info,
        Some(CodexErrorInfo::ExecutionError {
            stage,
            category,
            http_status_code: status,
            provider_validation: match failure {
                Failure::Provider => Some(ProviderValidation {
                    code: Some(ProviderValidationCode::InvalidValue),
                    parameter: Some(ProviderValidationParameter::Input),
                    tool_location: None,
                }),
                Failure::ToolLocation => Some(ProviderValidation {
                    code: Some(ProviderValidationCode::InvalidValue),
                    parameter: Some(ProviderValidationParameter::Tools),
                    tool_location: Some(codex_protocol::execution_error::ProviderToolLocation {
                        tool_index: 0,
                        nested_tool_index: Some(1),
                        field: Some(codex_protocol::execution_error::ProviderToolField::Format),
                    }),
                }),
                Failure::Unknown | Failure::Oversized => Some(ProviderValidation::default()),
                Failure::Preparation | Failure::Stream => None,
            },
        })
    );
    assert_eq!(requests.requests().len(), count);
    Ok(())
}
