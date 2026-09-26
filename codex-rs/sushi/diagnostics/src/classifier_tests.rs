use super::ClassifierAnswer as JevDecision;
use super::*;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[test]
fn classifier_lifecycle_is_bounded_and_has_one_terminal_event() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let (emitter, worker) = writer::open(home.path())?;
    let thread = ThreadId::new();
    let answer = |choice: &str, confidence: f64| JevDecision {
        choice: choice.to_string(),
        confidence,
        probabilities: [
            ("small".to_string(), 0.6),
            ("abstain".to_string(), 0.4),
            ("bad label\nprivate content".to_string(), 0.0),
        ]
        .into(),
    };
    for scenario in [
        "skip",
        "success",
        "http",
        "uncertain",
        "cancel_before",
        "cancel_after",
    ] {
        let mut attempt = ClassifierAttempt::new(thread, "turn-1");
        attempt.emitter = Some(emitter.clone());
        match scenario {
            "skip" => attempt.finish(Reason::RuleMatched),
            "success" => {
                attempt.request_started();
                attempt.answered(&answer("small", 0.9), /*min_confidence*/ 0.8);
                attempt.recommended("unsafe model\nprivate content");
                attempt.finish(Reason::InvalidTargetSettings);
            }
            "http" => {
                attempt.request_started();
                attempt.http_status(400);
                attempt.finish(Reason::Http);
            }
            "uncertain" => {
                attempt.request_started();
                attempt.answered(&answer("abstain", 0.3), /*min_confidence*/ 0.8);
                attempt.finish(Reason::Uncertain);
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
                r["httpStatusCode"],
                r["choice"],
                r["confidence"],
                r["probabilities"],
                r["minConfidence"]
            ])
        })
        .collect::<Vec<_>>();
    let probabilities = json!({"small": 0.6, "abstain": 0.4});
    assert_eq!(
        summaries,
        vec![
            json!([
                "skipped",
                "rule_matched",
                false,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
            json!([
                "request_started",
                "request_started",
                true,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
            json!([
                "succeeded",
                "jev_selected",
                true,
                null,
                null,
                "small",
                0.9,
                probabilities,
                0.8
            ]),
            json!([
                "request_started",
                "request_started",
                true,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
            json!(["failed", "http", true, null, 400, null, null, null, null]),
            json!([
                "request_started",
                "request_started",
                true,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
            json!([
                "failed",
                "uncertain",
                true,
                null,
                null,
                "abstain",
                0.3,
                probabilities,
                0.8
            ]),
            json!([
                "cancelled",
                "cancelled",
                false,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
            json!([
                "request_started",
                "request_started",
                true,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
            json!([
                "cancelled",
                "cancelled",
                true,
                null,
                null,
                null,
                null,
                null,
                null
            ]),
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
    for pair in [(1, 2), (3, 4), (5, 6), (8, 9)] {
        assert_eq!(rows[pair.0]["attemptId"], rows[pair.1]["attemptId"]);
    }
    assert!(!records.contains("private content"));
    Ok(())
}
