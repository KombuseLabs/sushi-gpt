use super::*;
use pretty_assertions::assert_eq;

#[test]
fn reconstructs_tool_arguments_only_at_model_message_boundary() -> anyhow::Result<()> {
    let mut stream = MessageStream::default();
    for event in [
        json!({"type":"message_start","message":{"id":"response","model":"observed","usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call","name":"tool","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"title\":"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"fixture\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
    ] {
        assert_eq!(stream.push(&event)?, None);
    }
    assert_eq!(
        stream.push(&json!({"type":"message_stop"}))?,
        Some(json!({
            "id":"response","model":"observed","usage":{"input_tokens":3,"output_tokens":2},"stop_reason":"tool_use",
            "content":[{"type":"tool_use","id":"call","name":"tool","input":{"title":"fixture"}}]
        }))
    );
    Ok(())
}

#[cfg(unix)]
async fn exercise(mode: &str) -> anyhow::Result<()> {
    use codex_tools::ResponsesApiTool;
    use codex_tools::ToolSpec;
    use futures::StreamExt;
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let executable = directory.path().join(format!("{mode}.py"));
    std::fs::write(&executable, include_str!("../tests/fixtures/peer.py"))?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
    let provider = ModelProviderInfo {
        wire_api: codex_model_provider_info::WireApi::ClaudeCli,
        cli_command: Some(executable.clone()),
        stream_idle_timeout_ms: Some(if mode == "slow_tool" { 1000 } else { 3000 }),
        ..Default::default()
    };
    let mut prompt = Prompt {
        parallel_tool_calls: true,
        input: vec![serde_json::from_value(
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"create document"}]}),
        )?],
        tools: vec![ToolSpec::Function(ResponsesApiTool {
            name: "document_fixture".into(),
            description: "Synthetic document callback".into(),
            strict: false,
            parameters: serde_json::from_value(json!({"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}))?,
            defer_loading: None,
            output_schema: None,
        })]
        .into(),
        ..Default::default()
    };
    let transport = Transport::default();
    let mut metadata = CodexResponsesMetadata {
        session_id: "session".into(),
        thread_id: "child".into(),
        ..Default::default()
    };
    metadata.turn_id = Some("turn".into());
    let turn = CancellationToken::new();
    let mut call_count = 0;
    let mut completed = false;
    let mut failed = false;
    for _ in 0..3 {
        let mut stream = transport
            .stream(
                &provider,
                &prompt,
                "requested-model",
                &metadata,
                turn.clone(),
            )
            .await?;
        while let Some(event) = stream.next().await {
            match event {
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    prompt.input.push(item.clone());
                    if let ResponseItem::FunctionCall { call_id, .. } = item {
                        call_count += 1;
                        if mode == "cancel" {
                            turn.cancel();
                            continue;
                        }
                        if mode == "slow_tool" {
                            tokio::time::sleep(Duration::from_millis(1200)).await;
                        }
                        let result: ResponseItem = serde_json::from_value(
                            json!({"type":"function_call_output","call_id":call_id,"output":"document-fixture-created"}),
                        )?;
                        prompt.input.push(result.clone());
                        stream
                            .tool_result_tx
                            .as_ref()
                            .expect("duplex channel")
                            .send(result)
                            .await?;
                    }
                }
                Ok(ResponseEvent::Completed { end_turn, .. }) => {
                    completed = end_turn == Some(true);
                    break;
                }
                Err(_) => {
                    failed = true;
                    break;
                }
                _ => {}
            }
        }
        if completed || failed {
            break;
        }
    }
    assert_eq!(call_count, if mode == "sequential" { 2 } else { 1 });
    assert_eq!(
        completed,
        !matches!(mode, "early_exit" | "error" | "cancel")
    );
    assert_eq!(failed, matches!(mode, "early_exit" | "error" | "cancel"));
    if mode == "cancel" {
        let state = transport.state.lock().await;
        assert!(state.failed && state.session.is_none());
        let pid: i32 = std::fs::read_to_string(executable.with_extension("py.pid"))?.parse()?;
        // Signal zero only checks the fixture process, after the adapter has reaped it.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    turn.cancel();
    if mode == "success" {
        metadata.turn_id = Some("followup".into());
        prompt.input.push(serde_json::from_value(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"report the result"}]}))?);
        let mut followup = transport
            .stream(
                &provider,
                &prompt,
                "requested-model",
                &metadata,
                CancellationToken::new(),
            )
            .await?;
        let mut final_response = None;
        while let Some(event) = followup.next().await {
            if let ResponseEvent::Completed {
                response_id,
                end_turn,
                ..
            } = event?
            {
                final_response = Some((response_id, end_turn));
                break;
            }
        }
        assert_eq!(final_response, Some(("model-final-2".into(), Some(true))));
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn native_result_bridge_handles_sequential_calls_and_terminal_failures() -> anyhow::Result<()>
{
    for mode in [
        "success",
        "sequential",
        "early_exit",
        "error",
        "cancel",
        "slow_tool",
    ] {
        exercise(mode).await?;
    }
    Ok(())
}

#[test]
fn native_tool_failure_is_preserved_and_images_are_not_silently_stringified() -> anyhow::Result<()>
{
    let mut item: ResponseItem = serde_json::from_value(
        json!({"type":"function_call_output","call_id":"denied","output":"Denied by native policy"}),
    )?;
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut item {
        output.success = Some(false);
    }
    assert_eq!(
        tool_result_content(&item, &json!("Denied by native policy"))?,
        (
            vec![json!({"type":"text","text":"Denied by native policy"})],
            true
        )
    );
    assert!(
        tool_result_content(
            &item,
            &json!([{"type":"input_image","image_url":"fixture"}])
        )
        .is_err()
    );
    Ok(())
}
