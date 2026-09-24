//! Native turn failures must resolve a voice handoff without leaking provider diagnostics.

use anyhow::Context;
use anyhow::Result;
use codex_config::config_toml::RealtimeWsVersion;
use codex_protocol::protocol::CodexResponseHandoffMode;
use codex_protocol::protocol::ConversationStartParams;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeOutputModality;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::ResponseTemplate;

#[derive(Clone, Copy)]
enum Scenario {
    ImmediateFailure,
    RetryThenFailure,
    ProgressThenFailure,
    ClientManaged,
}

#[test_case(RealtimeWsVersion::V1, Scenario::ImmediateFailure; "v1 terminal failure")]
#[test_case(RealtimeWsVersion::V2, Scenario::ImmediateFailure; "v2 terminal failure")]
#[test_case(RealtimeWsVersion::V2, Scenario::ProgressThenFailure; "v2 failure replaces progress")]
#[test_case(RealtimeWsVersion::V3, Scenario::ImmediateFailure; "v3 terminal failure")]
#[test_case(RealtimeWsVersion::V3, Scenario::RetryThenFailure; "retry is not terminal")]
#[test_case(RealtimeWsVersion::V3, Scenario::ProgressThenFailure; "failure replaces progress")]
#[test_case(RealtimeWsVersion::V3, Scenario::ClientManaged; "client retains ownership")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_native_turn_resolves_handoff(
    version: RealtimeWsVersion,
    scenario: Scenario,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let api_server = responses::start_mock_server().await;
    let preliminary_response = match scenario {
        Scenario::RetryThenFailure => Some(responses::sse(vec![responses::ev_response_created(
            "incomplete_response",
        )])),
        Scenario::ProgressThenFailure => Some(responses::sse(vec![
            responses::ev_response_created("progress_response"),
            responses::ev_assistant_message("progress", "I am working on it."),
            // An unknown tool triggers a native follow-up without running any command.
            responses::ev_function_call("unavailable", "unavailable_test_tool", "{}"),
            responses::ev_completed("progress_response"),
        ])),
        Scenario::ImmediateFailure | Scenario::ClientManaged => None,
    };
    let preliminary = if let Some(body) = preliminary_response {
        Some(responses::mount_sse_once(&api_server, body).await)
    } else {
        None
    };
    let failure = responses::mount_response_once(
        &api_server,
        ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "PRIVATE_PROVIDER_DIAGNOSTIC_MUST_NOT_LEAK", "type": "invalid_request_error"}
        })),
    ).await;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let realtime_url = format!("ws://{}", listener.local_addr()?);
    let (finish_tx, mut finish_rx) = oneshot::channel();
    let sideband = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
        // Wait for native session.update before admitting the handoff.
        websocket
            .next()
            .await
            .transpose()?
            .context("session update")?;
        let events = match version {
            RealtimeWsVersion::V1 => vec![
                json!({"type": "session.updated", "session": {"id": "failure_session", "instructions": "backend prompt"}}),
                json!({"type": "conversation.handoff.requested", "handoff_id": "failure_handoff", "item_id": "failure_handoff", "input_transcript": "synthetic delegated task"}),
            ],
            RealtimeWsVersion::V3 => vec![
                json!({"type": "session.started", "session": {"id": "failure_session", "instructions": "backend prompt"}}),
                json!({"type": "delegation.created", "item": {"id": "failure_handoff", "type": "delegation", "target": "client", "content": [{"type": "input_text", "text": "synthetic delegated task"}]}}),
            ],
            RealtimeWsVersion::V2 => vec![
                json!({"type": "session.updated", "session": {"id": "failure_session", "instructions": "backend prompt"}}),
                json!({"type": "conversation.item.done", "item": {"id": "failure_handoff", "type": "function_call", "name": "background_agent", "call_id": "failure_handoff", "arguments": "{\"input\":\"synthetic delegated task\"}"}}),
            ],
        };
        for event in events {
            websocket
                .send(Message::Text(event.to_string().into()))
                .await?;
        }
        let mut outputs = Vec::<Value>::new();
        loop {
            tokio::select! {
                _ = &mut finish_rx => break,
                message = websocket.next() => {
                    match message.transpose()? {
                        Some(Message::Text(text)) => outputs.push(serde_json::from_str(&text)?),
                        Some(_) => {},
                        None => break,
                    }
                }
            }
        }
        websocket.close(None).await?;
        Ok::<_, anyhow::Error>(outputs)
    });

    let mut builder = test_codex().with_config(move |config| {
        config.experimental_realtime_ws_base_url = Some(realtime_url);
        config.realtime.version = version;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(1);
    });
    let test = builder.build_with_auto_env(&api_server).await?;
    test.codex
        .submit(Op::RealtimeConversationStart(ConversationStartParams {
            client_managed_handoffs: matches!(scenario, Scenario::ClientManaged),
            delegation_ack_filler: None,
            flush_transcript_tail_on_session_end: false,
            codex_responses_as_items: false,
            codex_response_item_prefix: None,
            // Failure must use the thinking channel even when normal output is speakable.
            codex_response_handoff_mode: CodexResponseHandoffMode::BemTags,
            codex_response_handoff_channel_prefixes: None,
            model: None,
            output_modality: RealtimeOutputModality::Audio,
            include_startup_context: true,
            initial_items: Vec::new(),
            realtime_start_instructions: None,
            realtime_end_instructions: None,
            prompt: Some(Some("backend prompt".to_string())),
            realtime_session_id: None,
            transport: None,
            version: None,
            voice: None,
        }))
        .await?;

    let (terminal_errors, stream_errors) = tokio::time::timeout(Duration::from_secs(15), async {
        let mut terminal_errors = 0;
        let mut stream_errors = 0;
        loop {
            match test.codex.next_event().await?.msg {
                EventMsg::Error(error) if error.affects_turn_status() => terminal_errors += 1,
                EventMsg::StreamError(_) => stream_errors += 1,
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>((terminal_errors, stream_errors))
    })
    .await??;
    assert_eq!(
        (terminal_errors, stream_errors),
        (
            1,
            usize::from(matches!(scenario, Scenario::RetryThenFailure))
        )
    );
    assert_eq!(failure.requests().len(), 1);
    if let Some(preliminary) = preliminary {
        assert_eq!(preliminary.requests().len(), 1);
    }
    // Let the existing 200ms stream flush timer run after terminal completion.
    tokio::time::sleep(Duration::from_millis(350)).await;
    finish_tx.send(()).ok().context("sideband still open")?;
    let outputs = sideband.await??;
    let notices: Vec<_> = outputs
        .iter()
        .filter(|output| output.to_string().contains("<realtime_task_failure>"))
        .collect();
    assert_eq!(
        notices.len(),
        usize::from(!matches!(scenario, Scenario::ClientManaged)),
        "{outputs:#?}"
    );
    if let Some(notice) = notices.first() {
        let text = match version {
            RealtimeWsVersion::V1 => {
                assert_eq!(notice["handoff_id"], "failure_handoff");
                assert_eq!(notice["type"], "conversation.handoff.append");
                notice["output_text"].as_str().context("v1 output text")?
            }
            RealtimeWsVersion::V3 => {
                assert_eq!(notice["delegation_item_id"], "failure_handoff");
                assert_eq!(notice["type"], "delegation.context.append");
                assert!(
                    notice.get("channel").is_none(),
                    "failure is reasoning context"
                );
                notice["content"][0]["text"]
                    .as_str()
                    .context("v3 output text")?
            }
            RealtimeWsVersion::V2 => {
                assert_eq!(notice["type"], "conversation.item.create");
                assert_eq!(notice["item"]["type"], "function_call_output");
                assert_eq!(notice["item"]["call_id"], "failure_handoff");
                notice["item"]["output"]
                    .as_str()
                    .context("v2 output text")?
            }
        };
        assert!(text.contains("The delegated task failed."));
        assert!(text.len() < 600);
        assert!(!text.contains("PRIVATE_PROVIDER_DIAGNOSTIC"));
        let last_handoff_output = outputs
            .iter()
            .rfind(|output| output["type"] != "response.create");
        assert_eq!(
            last_handoff_output,
            Some(*notice),
            "no stale progress after failure"
        );
        if version == RealtimeWsVersion::V2 {
            assert_eq!(
                outputs.last().context("response after failure")?["type"],
                "response.create"
            );
        }
    }
    Ok(())
}
