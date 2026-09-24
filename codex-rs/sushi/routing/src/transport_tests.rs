//! TypeSafe HTTP contract and failure boundaries, using synthetic inputs and credentials only.
use super::JevDecision;
use super::JevFallback;
use super::JevRequest;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[test_case("valid", None)]
#[test_case("missing_key", Some(JevFallback::MissingKey))]
#[test_case("empty_key", Some(JevFallback::MissingKey))]
#[test_case("low_confidence", Some(JevFallback::Uncertain))]
#[test_case("abstain", Some(JevFallback::Uncertain))]
#[test_case("tie", Some(JevFallback::Uncertain))]
#[test_case("unknown_class", Some(JevFallback::InvalidResponse))]
#[test_case("not_highest", Some(JevFallback::InvalidResponse))]
#[test_case("bad_sum", Some(JevFallback::InvalidResponse))]
#[test_case("negative", Some(JevFallback::InvalidResponse))]
#[test_case("missing_class", Some(JevFallback::InvalidResponse))]
#[test_case("extra_class", Some(JevFallback::InvalidResponse))]
#[test_case("missing_answer", Some(JevFallback::InvalidResponse))]
#[test_case("wrong_type", Some(JevFallback::InvalidResponse))]
#[test_case("malformed", Some(JevFallback::InvalidResponse))]
#[test_case("too_large", Some(JevFallback::OversizedResponse))]
#[test_case("redirect", Some(JevFallback::Http(302)))]
#[test_case("unauthorized", Some(JevFallback::Http(401)))]
#[test_case("limited", Some(JevFallback::Http(429)))]
#[test_case("outage", Some(JevFallback::Http(529)))]
#[test_case("timeout", Some(JevFallback::Timeout))]
#[tokio::test]
async fn jev_http_contract(scenario: &str, expected: Option<JevFallback>) -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let redirect = MockServer::start().await;
    let mut body = json!({"model":"jev-1.13.0", "answers":{"route":{"type":"choice", "choice":"small", "confidence":0.9, "probabilities":{"small":0.9,"abstain":0.1}}},"usage":{"input_tokens":50,"output_tokens":0}});
    match scenario {
        "low_confidence" => body["answers"]["route"]["confidence"] = json!(0.1),
        "unknown_class" => body["answers"]["route"]["choice"] = json!("unknown"),
        "not_highest" => body["answers"]["route"]["choice"] = json!("abstain"),
        "abstain" => {
            body["answers"]["route"]["choice"] = json!("abstain");
            body["answers"]["route"]["probabilities"] = json!({"small":0.1,"abstain":0.9});
        }
        "tie" => body["answers"]["route"]["probabilities"] = json!({"small":0.5,"abstain":0.5}),
        "bad_sum" => body["answers"]["route"]["probabilities"]["small"] = json!(0.3),
        "negative" => {
            body["answers"]["route"]["probabilities"] = json!({"small":1.1,"abstain":-0.1})
        }
        "missing_class" => body["answers"]["route"]["probabilities"] = json!({"small":1.0}),
        "extra_class" => body["answers"]["route"]["probabilities"]["unknown"] = json!(0.0),
        "missing_answer" => body["answers"] = json!({}),
        "wrong_type" => body["answers"]["route"]["type"] = json!("noul"),
        _ => {}
    }
    let status = match scenario {
        "redirect" => 302,
        "unauthorized" => 401,
        "limited" => 429,
        "outage" => 529,
        _ => 200,
    };
    let mut response = ResponseTemplate::new(status).set_body_json(body);
    match scenario {
        "redirect" => response = response.insert_header("location", redirect.uri()),
        "malformed" => response = response.set_body_string("not JSON"),
        "too_large" => response = response.set_body_string("x".repeat(32769)),
        "timeout" => response = response.set_delay(Duration::from_secs(3)),
        _ => {}
    }
    let no_key = matches!(scenario, "missing_key" | "empty_key");
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer synthetic-key"))
        .respond_with(response)
        .expect(if no_key { 0 } else { 1 })
        .mount(&server)
        .await;
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let endpoint = format!("{}/v1/systemone", server.uri());
    let sent = std::cell::Cell::new(false);
    let result = super::classify_jev(
        &factory,
        JevRequest {
            endpoint: &endpoint,
            model: "jev-1.13.0",
            state: json!({"task_name":"fixture_task","agent_type":"default"}),
            instructions: "Classify the synthetic task",
            criteria: [("small", "Small task"), ("abstain", "Unknown")].into(),
            timeout: Duration::from_millis(1500),
            min_confidence: 0.8,
        },
        match scenario {
            "missing_key" => None,
            "empty_key" => Some(" "),
            _ => Some("synthetic-key"),
        },
        || sent.set(true),
    )
    .await;
    assert_eq!(sent.get(), !no_key);
    assert_eq!(
        result,
        match expected {
            Some(reason) => Err(reason),
            None => Ok(JevDecision {
                choice: "small".to_string(),
                confidence: 0.9
            }),
        }
    );
    if !no_key {
        let requests = server.received_requests().await.expect("recorded requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[0].body)?,
            json!({"model":"jev-1.13.0","state":{"task_name":"fixture_task","agent_type":"default"},"questions":{"route":{"type":"choice","instructions":"Classify the synthetic task","criteria":{"small":"Small task","abstain":"Unknown"}}}})
        );
    }
    assert!(
        redirect
            .received_requests()
            .await
            .expect("redirect requests")
            .is_empty()
    );
    Ok(())
}
