use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn sushi_telemetry_records_only_observed_usage_and_server_model() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (emitter, worker) = writer::open(home.path())?;
    let mut metadata = CodexResponsesMetadata {
        session_id: "session".into(),
        thread_id: "child".into(),
        ..Default::default()
    };
    metadata.turn_id = Some("turn".into());
    metadata.parent_thread_id = Some(codex_protocol::ThreadId::new());
    let mut attempt = RequestAttempt::new(emitter.clone(), &metadata, "requested-model");
    attempt.set_request_id(Some("upstream-request"));
    attempt.observe(&ResponseEvent::ServerModel("executed-model".into()));
    attempt.observe(&ResponseEvent::OutputTextDelta("PRIVATE_SENTINEL".into()));
    let completed = ResponseEvent::Completed {
        response_id: "response".into(),
        token_usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 3,
            cached_input_tokens: 4,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 13,
            ..Default::default()
        }),
        usage_metadata: None,
        end_turn: Some(true),
    };
    attempt.observe(&completed);
    attempt.observe(&completed); // A repeated terminal event does not produce another record.
    drop(attempt);
    let mut failed = RequestAttempt::new(emitter.clone(), &metadata, "requested-model");
    failed.observe(&ResponseEvent::Created {
        response_id: Some("failed-response".into()),
    });
    drop(failed);
    let mut retry = RequestAttempt::new(emitter.clone(), &metadata, "requested-model");
    retry.observe(&ResponseEvent::Completed {
        response_id: "retry-response".into(),
        token_usage: None,
        usage_metadata: None,
        end_turn: None,
    });
    drop(retry);
    drop(emitter);
    worker.join().expect("writer finishes");
    let text = std::fs::read_to_string(home.path().join("sushigpt-telemetry.jsonl"))?;
    assert!(!text.contains("PRIVATE_SENTINEL"));
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(records.len(), 3);
    assert_eq!(
        records[0]["usage"],
        json!({"inputTokens":10,"outputTokens":3,"cachedInputTokens":4,"cacheWriteInputTokens":null,"reasoningOutputTokens":null,"totalTokens":13})
    );
    assert_eq!(
        records
            .iter()
            .map(|r| json!([
                r["requestedModel"],
                r["executedModel"],
                r["status"],
                r["responseId"]
            ]))
            .collect::<Vec<_>>(),
        vec![
            json!(["requested-model", "executed-model", "completed", "response"]),
            json!(["requested-model", null, "incomplete", "failed-response"]),
            json!(["requested-model", null, "completed", "retry-response"]),
        ]
    );
    assert!(records[1]["usage"].is_null() && records[2]["usage"].is_null());
    assert_ne!(records[0]["attemptId"], records[1]["attemptId"]);
    assert_eq!(
        records[0]["parentThreadId"],
        json!(metadata.parent_thread_id)
    );
    Ok(())
}

#[test]
fn sushi_telemetry_preserves_unknown_details_and_rejects_invalid_identifiers() -> anyhow::Result<()>
{
    assert_eq!(
        serde_json::to_value(Usage::from(&TokenUsage::default()))?,
        json!({"inputTokens":0,"outputTokens":0,"totalTokens":0,"cachedInputTokens":null,"cacheWriteInputTokens":null,"reasoningOutputTokens":null})
    );
    assert_eq!(identifier("private content\n"), None);
    assert_eq!(identifier(&"x".repeat(257)), None);
    assert_eq!(
        identifier("provider/model-1.0"),
        Some("provider/model-1.0".into())
    );
    Ok(())
}

#[test]
fn sushi_telemetry_local_transport_does_not_infer_model_or_completion() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (emitter, worker) = writer::open(home.path())?;
    let metadata = CodexResponsesMetadata {
        session_id: "session".into(),
        thread_id: "child".into(),
        ..Default::default()
    };
    let mut attempt = RequestAttempt::new(emitter.clone(), &metadata, "preferred-model");
    let instance = Uuid::new_v4().to_string();
    attempt.set_cli_transport(&instance, "2.1.263");
    drop(attempt);
    drop(emitter);
    worker.join().expect("writer finishes");
    let record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        home.path().join("sushigpt-telemetry.jsonl"),
    )?)?;
    assert_eq!(
        json!([
            record["transport"],
            record["transportInstanceId"],
            record["cliVersion"],
            record["executedModel"],
            record["status"]
        ]),
        json!(["claude_cli", instance, "2.1.263", null, "incomplete"])
    );
    Ok(())
}
