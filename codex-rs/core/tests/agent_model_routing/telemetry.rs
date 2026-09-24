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
        for backend in ["v1", "v2"] {
            let home = tempfile::tempdir()?;
            let status = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "telemetry::sushi_telemetry_correlates_native_children",
                    "--nocapture",
                ])
                .env(MARKER, backend)
                .env("SUSHIGPT_TELEMETRY", "1")
                .env("CODEX_HOME", home.path())
                .status()?;
            assert!(status.success());
        }
        return Ok(());
    };
    let home = std::env::var_os("CODEX_HOME").expect("isolated profile");
    tokio::runtime::Runtime::new()?.block_on(async {
        run_routing(if backend == "v1" { Backend::V1 } else { Backend::V2 }, RoutingCase::RoleModel, /*jev*/ None).await?;
        let observed = timeout(Duration::from_secs(5), async {
            loop {
                let records = records(Path::new(&home));
                if records.iter().any(|r| r["kind"] == "routing_decision") && records.iter().any(|r| r["responseId"] == "routing-child") { break records; }
                sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        let decision = observed.iter().find(|r| r["kind"] == "routing_decision").expect("decision");
        let child = observed.iter().find(|r| r["responseId"] == "routing-child").expect("child response");
        assert_eq!(json!([decision["source"],decision["reasonCode"],decision["requestedModel"],decision["policyModel"],decision["selectedModel"]]), json!(["rule","rule_matched",null,ROUTED_MODEL,PARENT_MODEL]));
        assert_eq!(decision["childThreadId"], child["threadId"]);
        assert_eq!(decision["parentThreadId"], child["parentThreadId"]);
        assert_eq!(child["requestedModel"], json!(PARENT_MODEL));
        assert!(child["executedModel"].is_null()); // No provider model header in this SSE fixture.
        assert_eq!(child["usage"], json!({"inputTokens":12,"outputTokens":3,"totalTokens":15,"cachedInputTokens":4,"cacheWriteInputTokens":null,"reasoningOutputTokens":null}));
        assert!(!serde_json::to_string(&observed)?.contains(TASK));
        assert!(!serde_json::to_string(&observed)?.contains(ENCRYPTED_TASK));
        Ok(())
    })
}
