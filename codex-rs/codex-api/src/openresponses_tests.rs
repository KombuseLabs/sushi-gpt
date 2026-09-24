use super::*;
use pretty_assertions::assert_eq;

fn request() -> Value {
    json!({"model":"synthetic-claude","store":false,"stream":true,
    "input":[{"type":"agent_message","content":[{"type":"input_text","text":"Synthetic assignment"}]}],
    "tools":[{"type":"namespace","name":"functions","tools":[
        {"type":"function","name":"document","parameters":{"type":"object"}},
        {"type":"custom","name":"apply_patch","description":"Apply a patch"}
    ]}]})
}

#[test]
fn preserves_assignment_and_maps_tools_without_executing_them() {
    let mut body = request();
    let adapter = Adapter::prepare(&mut body).unwrap();
    assert_eq!(
        body["input"],
        json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"Synthetic assignment"}]}])
    );
    let native =
        json!({"type":"function_call","call_id":"call-1","name":"codex_tool_0","arguments":"{}"});
    let item = adapter
        .restore(serde_json::from_value(native).unwrap(), /*done*/ true)
        .unwrap();
    let item = serde_json::to_value(item).unwrap();
    assert_eq!(
        (
            item["namespace"].clone(),
            item["name"].clone(),
            item["encrypted_function_args"].clone()
        ),
        (json!("functions"), json!("document"), json!([]))
    );
}

#[test]
fn custom_tool_round_trip_keeps_native_name_input_and_call_id() {
    let mut body = request();
    let adapter = Adapter::prepare(&mut body).unwrap();
    let call = json!({"type":"function_call","call_id":"patch-1","name":"codex_tool_1","arguments":json!({"input":"*** Begin Patch\n*** End Patch"}).to_string()});
    let restored = adapter
        .restore(serde_json::from_value(call).unwrap(), /*done*/ true)
        .unwrap();
    let native = serde_json::to_value(restored).unwrap();
    assert_eq!(native["type"], json!("custom_tool_call"));
    assert_eq!(native["call_id"], json!("patch-1"));
    let mut next = request();
    next["input"].as_array_mut().unwrap().extend([
        native,
        json!({"type":"custom_tool_call_output","call_id":"patch-1","output":"applied"}),
    ]);
    Adapter::prepare(&mut next).unwrap();
    assert_eq!(
        next["input"][2],
        json!({"type":"function_call_output","call_id":"patch-1","output":"applied"})
    );
    assert_eq!(
        next["input"][1]["arguments"],
        json!(json!({"input":"*** Begin Patch\n*** End Patch"}).to_string())
    );
}

#[test]
fn rejects_encrypted_assignments_history_and_tool_schemas() {
    for item in [
        json!({"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"opaque"}]}),
        json!({"type":"compaction","encrypted_content":"opaque"}),
        json!({"type":"function_call","name":"document","namespace":"functions","arguments":"{}","encrypted_function_args":["message"]}),
        json!({"type":"reasoning","summary":[]}),
    ] {
        let mut body = request();
        body["input"] = json!([item]);
        assert!(Adapter::prepare(&mut body).is_err());
    }
    let mut body = request();
    body["tools"][0]["tools"][0]["parameters"]["encrypted"] = json!(true);
    assert!(Adapter::prepare(&mut body).is_err());
}

#[test]
fn rejects_unknown_returned_tools_and_malformed_custom_arguments() {
    let adapter = Adapter::prepare(&mut request()).unwrap();
    for (name, arguments) in [
        ("not_advertised", "{}"),
        ("codex_tool_1", "not json"),
        ("codex_tool_1", "{}"),
    ] {
        let item = serde_json::from_value(
            json!({"type":"function_call","call_id":"call-1","name":name,"arguments":arguments}),
        )
        .unwrap();
        assert!(adapter.restore(item, /*done*/ true).is_err());
    }
}

#[tokio::test]
async fn stream_preserves_text_completion_and_errors_without_fabricating_model_evidence() {
    let adapter = Adapter::prepare(&mut request()).unwrap();
    let (tx, rx_event) = mpsc::channel(4);
    tx.send(Ok(ResponseEvent::OutputTextDelta("answer".into())))
        .await
        .unwrap();
    tx.send(Ok(ResponseEvent::Completed {
        response_id: "fixture".into(),
        token_usage: None,
        usage_metadata: None,
        end_turn: Some(true),
    }))
    .await
    .unwrap();
    tx.send(Err(ApiError::Stream("synthetic failure".into())))
        .await
        .unwrap();
    drop(tx);
    let stream = adapter.wrap(ResponseStream {
        rx_event,
        upstream_request_id: Some("req-fixture".into()),
    });
    assert_eq!(stream.upstream_request_id.as_deref(), Some("req-fixture"));
    let events = stream.collect::<Vec<_>>().await;
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0], Ok(ResponseEvent::OutputTextDelta(text)) if text == "answer"));
    assert!(
        matches!(&events[1], Ok(ResponseEvent::Completed { response_id, end_turn:Some(true), .. }) if response_id == "fixture")
    );
    assert!(matches!(&events[2], Err(ApiError::Stream(message)) if message == "synthetic failure"));
}

#[tokio::test]
async fn dropping_consumer_cancels_upstream_receiver() {
    let adapter = Adapter::prepare(&mut request()).unwrap();
    let (tx, rx_event) = mpsc::channel(1);
    drop(adapter.wrap(ResponseStream {
        rx_event,
        upstream_request_id: None,
    }));
    tokio::time::timeout(std::time::Duration::from_secs(1), tx.closed())
        .await
        .unwrap();
}
