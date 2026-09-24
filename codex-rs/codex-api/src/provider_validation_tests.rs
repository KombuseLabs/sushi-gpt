use super::*;
use crate::ApiError;
use crate::TransportError;
use crate::api_bridge::map_api_error;
use codex_protocol::protocol::CodexErrorInfo;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn validation_extraction_rejects_untrusted_or_unbounded_detail() {
    let recognized = ProviderValidation {
        code: Some(Code::InvalidValue),
        parameter: Some(Parameter::Input),
        tool_location: None,
    };
    let body = json!({"error":{"code":"invalid_value","param":"input[0].tools[1].name","message":"SECRET"}}).to_string();
    assert_eq!(extract(&body), recognized);
    for value in [
        "input.SECRET",
        "input[secret]",
        "input[1234567]",
        "input[-1]",
        "input..name",
        "input[0][1]",
        "https://secret",
        "input\nsecret",
    ] {
        assert_eq!(parameter(value), None);
    }
    for body in [
        "not json".to_owned(),
        json!({"error":{"code":"SECRET", "param":"SECRET", "message":"service_tier", "type":"invalid_request_error"}}).to_string(),
        json!({"error":{"code":42,"param":["service_tier"]}}).to_string(),
        format!("{body}{}", " ".repeat(65536)),
    ] {
        assert_eq!(extract(&body), ProviderValidation::default());
    }
}

#[test]
fn validation_context_preserves_specific_mappings_and_retry_semantics() {
    for (status, code, expected) in [
        (
            http::StatusCode::BAD_REQUEST,
            "cyber_policy",
            CodexErrorInfo::CyberPolicy,
        ),
        (
            http::StatusCode::SERVICE_UNAVAILABLE,
            "server_is_overloaded",
            CodexErrorInfo::ServerOverloaded,
        ),
    ] {
        let error = map_api_error(ApiError::Transport(TransportError::Http {
            status,
            url: None,
            headers: None,
            body: Some(json!({"error":{"code":code,"param":"service_tier"}}).to_string()),
        }));
        assert_eq!(error.to_codex_protocol_error(), expected);
        assert_eq!(error.retry_delay(/*retry_count*/ 1), None);
    }
}
#[test]
fn validation_preserves_distinct_tool_locations_without_private_message() {
    let validation_for = |path: &str| {
        let body = json!({
            "error": {
                "code": "invalid_value",
                "param": path,
                "message": "PRIVATE_TEST_SENTINEL"
            }
        })
        .to_string();
        extract(&body)
    };

    let first = validation_for("tools[0].tools[0].format");
    let second = validation_for("tools[0].tools[1].format");

    for validation in [&first, &second] {
        let serialized = serde_json::to_string(validation).unwrap();
        assert!(!serialized.contains("PRIVATE_TEST_SENTINEL"));
    }

    assert_ne!(
        first, second,
        "Different tool error locations must remain distinguishable"
    );
}

#[test]
fn tool_locations_preserve_only_complete_structural_positions() {
    for (path, outer, nested, field) in [
        ("tools[0]", 0, None, None),
        ("tools[65535].tools[65535]", 65535, Some(65535), None),
        (
            "tools[12].format",
            12,
            None,
            Some(ProviderToolField::Format),
        ),
        (
            "tools[0].tools[1].format",
            0,
            Some(1),
            Some(ProviderToolField::Format),
        ),
        ("tools[2].type", 2, None, Some(ProviderToolField::Type)),
        ("tools[2].name", 2, None, Some(ProviderToolField::Name)),
        (
            "tools[2].description",
            2,
            None,
            Some(ProviderToolField::Description),
        ),
        (
            "tools[2].parameters",
            2,
            None,
            Some(ProviderToolField::Parameters),
        ),
        ("tools[2].strict", 2, None, Some(ProviderToolField::Strict)),
    ] {
        let body = json!({"error":{"param":path,"message":"PRIVATE_TEST_SENTINEL"}}).to_string();
        assert_eq!(
            extract(&body),
            ProviderValidation {
                code: None,
                parameter: Some(Parameter::Tools),
                tool_location: Some(ProviderToolLocation {
                    tool_index: outer,
                    nested_tool_index: nested,
                    field,
                }),
            }
        );
    }
}

#[test]
fn tool_locations_reject_private_malformed_and_unbounded_paths_without_truncation() {
    for path in [
        "tools",
        "tools[0].tools",
        "tools[0].tools[1].tools[2].format",
        "tools[0].parameters.properties.PRIVATE_TEST_SENTINEL",
        "tools[0].parameters.properties.name",
        "tools[0].format.type",
        "tools[0].function.name",
        "input[0].tools[1].name",
        "tools[-1].format",
        "tools[+1].format",
        "tools[1.0].format",
        "tools[].format",
        "tools[01].format",
        "tools[65536].format",
        "tools[9999999999999999999999].format",
        "tools[0].tools[65536].format",
        "tools[0][1].format",
        "tools[0].tools[01].format",
        "tools[0]..format",
        "tools[0].format.",
        "tools[0].format\n",
        "tools[0].PRIVATE_TEST_SENTINEL",
        "tools[０].format",
        " tools[0].format",
    ] {
        let body = json!({"error":{"code":"invalid_value","param":path,"message":"PRIVATE_TEST_SENTINEL"}}).to_string();
        assert_eq!(
            extract(&body),
            ProviderValidation {
                code: Some(Code::InvalidValue),
                parameter: parameter(path),
                tool_location: None,
            },
            "{path}"
        );
    }
    assert_eq!(tool_location(&"tools[0].".repeat(100)), None);
    for param in [
        json!(null),
        json!(42),
        json!(["tools[0].format"]),
        json!({"path":"tools[0].format"}),
    ] {
        assert_eq!(
            extract(&json!({"error":{"param":param}}).to_string()),
            ProviderValidation::default()
        );
    }
    let oversized =
        json!({"error":{"param":"tools[0].tools[1].format","message":"x".repeat(65536)}})
            .to_string();
    assert_eq!(extract(&oversized), ProviderValidation::default());
}

#[test]
fn tool_location_survives_http_error_mapping_without_private_payload() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "x-request-id",
        http::HeaderValue::from_static("PRIVATE_REQUEST_ID"),
    );
    let error = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::BAD_REQUEST,
        url: Some("https://PRIVATE.invalid/SECRET".into()),
        headers: Some(headers),
        body: Some(json!({"error":{"code":"invalid_value","param":"tools[0].tools[1].format","message":"PRIVATE_TEST_SENTINEL"},"PRIVATE_BODY":"SECRET"}).to_string()),
    }));
    assert_eq!(
        serde_json::to_value(error.to_codex_protocol_error()).unwrap(),
        json!({
            "execution_error": {
                "stage":"providerResponse", "category":"invalidRequest", "http_status_code":400,
                "provider_validation":{"code":"invalidValue","parameter":"tools","toolLocation":{"toolIndex":0,"nestedToolIndex":1,"field":"format"}}
            }
        })
    );
    assert_eq!(error.retry_delay(/*retry_count*/ 1), None);
}
