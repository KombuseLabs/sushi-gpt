use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::path::Path;

pub(super) fn completed(id: &str) -> Value {
    let mut event = ev_completed(id);
    event["response"]["usage"] = json!({"input_tokens":12,"output_tokens":3,"total_tokens":15,"input_tokens_details":{"cached_tokens":4}});
    event
}

fn records(home: &Path) -> Vec<Value> {
    std::fs::read_to_string(home.join("sushigpt-telemetry.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn sushi_telemetry_correlates_native_children() -> Result<()> {
    const MARKER: &str = "SUSHI_TELEMETRY_TEST_BACKEND";
    let Ok(backend) = std::env::var(MARKER) else {
        for backend in ["v1", "v2", "v2_fallback_class"] {
            let home = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "telemetry::sushi_telemetry_correlates_native_children",
                    "--nocapture",
                ])
                .env(MARKER, backend)
                .env(JEV_KEY, "synthetic-fixture")
                .env("SUSHIGPT_TELEMETRY", "1")
                .env("CODEX_HOME", home.path())
                .status()?;
            assert!(status.success());
        }
        return Ok(());
    };
    let home = std::env::var_os("CODEX_HOME").expect("isolated profile");
    tokio::runtime::Runtime::new()?.block_on(async {
        // The fallback-class variant answers with low confidence so the configured class applies.
        let fallback_class = backend == "v2_fallback_class";
        let classifier = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"route": {"type": "choice", "choice": "small", "confidence": 0.2,
                                      "probabilities": {"small": 0.6, "cheap": 0.39, "abstain": 0.01}}}
            })))
            .expect(u64::from(fallback_class))
            .mount(&classifier)
            .await;
        let class = |description: &str, effort| JevRoutingClass { capabilities: vec![], model_provider: None, description: description.to_string(), model: ROUTED_MODEL.to_string(), reasoning_effort: Some(effort) };
        let jev = fallback_class.then(|| JevRouting {
            enabled: true,
            api_key_env: JEV_KEY.to_string(),
            endpoint: format!("{}/v1/systemone", classifier.uri()),
            fallback_class: Some("cheap".to_string()),
            classes: [("small".to_string(), class("Synthetic bounded task", ReasoningEffort::High)), ("cheap".to_string(), class("Cheapest class", ReasoningEffort::Low))].into(),
            ..JevRouting::default()
        });
        let case = if fallback_class { RoutingCase::JevFallbackClass } else { RoutingCase::RoleModel };
        run_routing(if backend == "v1" { Backend::V1 } else { Backend::V2 }, case, jev).await?;
        let observed = timeout(Duration::from_secs(5), async {
            loop {
                let records = records(Path::new(&home));
                if records.iter().any(|r| r["kind"] == "routing_decision") && records.iter().any(|r| r["responseId"] == "routing-child") { break records; }
                sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        let decision = observed.iter().find(|r| r["kind"] == "routing_decision").expect("decision");
        let child = observed.iter().find(|r| r["responseId"] == "routing-child").expect("child response");
        let expected = if fallback_class {
            json!(["fallback_class", "uncertain", null, ROUTED_MODEL, ROUTED_MODEL])
        } else {
            json!(["rule", "rule_matched", null, ROUTED_MODEL, PARENT_MODEL])
        };
        assert_eq!(json!([decision["source"],decision["reasonCode"],decision["requestedModel"],decision["policyModel"],decision["selectedModel"]]), expected);
        // The classifier lifecycle keeps its own outcome; the fallback class is a routing decision.
        if fallback_class {
            let transition = observed.iter().rev().find(|r| r["kind"] == "classifier_transition").expect("classifier transition");
            assert_eq!(json!([transition["phase"], transition["reasonCode"]]), json!(["failed", "uncertain"]));
            assert_eq!(
                json!([transition["choice"], transition["confidence"], transition["probabilities"], transition["minConfidence"]]),
                json!(["small", 0.2, {"small": 0.6, "cheap": 0.39, "abstain": 0.01}, 0.8])
            );
        }
        classifier.verify().await;
        assert_eq!(decision["childThreadId"], child["threadId"]);
        assert_eq!(decision["parentThreadId"], child["parentThreadId"]);
        assert_eq!(child["requestedModel"], json!(if fallback_class { ROUTED_MODEL } else { PARENT_MODEL }));
        assert!(child["executedModel"].is_null()); // No provider model header in this SSE fixture.
        assert_eq!(child["usage"], json!({"inputTokens":12,"outputTokens":3,"totalTokens":15,"cachedInputTokens":4,"cacheWriteInputTokens":null,"reasoningOutputTokens":null}));
        assert!(!serde_json::to_string(&observed)?.contains(TASK));
        assert!(!serde_json::to_string(&observed)?.contains(ENCRYPTED_TASK));
        Ok(())
    })
}
