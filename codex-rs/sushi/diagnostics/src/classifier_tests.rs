use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[test]
fn classifier_lifecycle_is_bounded_and_has_one_terminal_event() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (emitter, worker) = writer::open(home.path())?;
    let thread = ThreadId::new();
    for scenario in ["skip", "success", "http", "cancel_before", "cancel_after"] {
        let mut attempt = ClassifierAttempt::new(thread, "turn-1");
        attempt.emitter = Some(emitter.clone());
        match scenario {
            "skip" => attempt.finish(Reason::RuleMatched),
            "success" => {
                attempt.request_started();
                attempt.recommended("unsafe model\nprivate content");
                attempt.finish(Reason::InvalidTargetSettings);
            }
            "http" => {
                attempt.request_started();
                attempt.http_status(400);
                attempt.finish(Reason::Http);
            }
            "cancel_after" => attempt.request_started(),
            _ => {}
        }
    }
    drop(emitter);
    worker.join().expect("writer completes");
    let records = std::fs::read_to_string(home.path().join("sushigpt-telemetry.jsonl"))?;
    let rows = records
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let summaries = rows
        .iter()
        .map(|r| {
            json!([
                r["phase"],
                r["reasonCode"],
                r["requestStarted"],
                r["recommendedModel"],
                r["httpStatusCode"]
            ])
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summaries,
        vec![
            json!(["skipped", "rule_matched", false, null, null]),
            json!(["request_started", "request_started", true, null, null]),
            json!(["succeeded", "jev_selected", true, null, null]),
            json!(["request_started", "request_started", true, null, null]),
            json!(["failed", "http", true, null, 400]),
            json!(["cancelled", "cancelled", false, null, null]),
            json!(["request_started", "request_started", true, null, null]),
            json!(["cancelled", "cancelled", true, null, null]),
        ]
    );
    for row in &rows {
        assert_eq!(
            (&row["kind"], &row["parentThreadId"], &row["parentTurnId"]),
            (
                &json!("classifier_transition"),
                &json!(thread),
                &json!("turn-1")
            )
        );
    }
    for pair in [(1, 2), (3, 4), (6, 7)] {
        assert_eq!(rows[pair.0]["attemptId"], rows[pair.1]["attemptId"]);
    }
    assert!(!records.contains("private content"));
    Ok(())
}
