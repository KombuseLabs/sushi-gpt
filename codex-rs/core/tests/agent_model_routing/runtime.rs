//! Consecutive native spawns in one parent session, with a control change between them.
use super::*;
use anyhow::Context;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::sse_response;
use pretty_assertions::assert_eq;

fn child_request(request: &wiremock::Request) -> bool {
    let body = if request
        .headers
        .get("content-encoding")
        .is_some_and(|h| h == "zstd")
    {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).expect("decode mock request")
    } else {
        request.body.clone()
    };
    let body: serde_json::Value = serde_json::from_slice(&body).expect("parse mock request");
    body["client_metadata"]["x-codex-parent-thread-id"].is_string()
}

pub(super) async fn verify_runtime_control(key: &str) -> Result<()> {
    let server = start_mock_server().await;
    let classifier = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/systemone"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer synthetic-fixture",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "answers": {"route": {"type":"choice", "choice":"small", "confidence":0.95,
                                  "probabilities":{"small":0.95,"abstain":0.05}}}
        })))
        .mount(&classifier)
        .await;
    let settings = JevRouting {
        enabled: true,
        api_key_env: key.to_string(),
        endpoint: format!("{}/v1/systemone", classifier.uri()),
        classes: [(
            "small".to_string(),
            JevRoutingClass {
                capabilities: vec![],
                model_provider: None,
                description: "Synthetic task".to_string(),
                model: ROUTED_MODEL.to_string(),
                reasoning_effort: Some(ReasoningEffort::Medium),
            },
        )]
        .into(),
        ..JevRouting::default()
    };
    let test = test_codex()
        .with_model_info_override(PARENT_MODEL, |model| {
            model.multi_agent_version = Some(MultiAgentVersion::V2)
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable native collaboration");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable native v2");
            config.agent_default_subagent_model = Some(PARENT_MODEL.to_string());
            config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::Low);
            config.agent_max_threads = Some(16);
            config.agent_model_routing = Some(AgentModelRouting {
                plaintext_messages: false,
                strict_candidates: false,
                inherit_dynamic_tools: false,
                enabled: true,
                rules: vec![AgentModelRoute {
                    model_provider: None,
                    agent_type: None,
                    task_contains: vec![],
                    task_name_contains: vec!["rule".to_string()],
                    model: ROUTED_MODEL.to_string(),
                    reasoning_effort: Some(ReasoningEffort::High),
                }],
                jev: Some(settings),
            });
        })
        .build_with_auto_env(&server)
        .await?;
    // Completion notifications and tool followups can generate extra parent requests.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| !child_request(request))
        .respond_with(sse_response(sse(vec![
            ev_response_created("runtime-followup"),
            ev_assistant_message("runtime-answer", "spawn processed"),
            ev_completed("runtime-followup"),
        ])))
        .with_priority(10)
        .mount(&server)
        .await;
    let mode_path = test.codex_home_path().join("agent-model-routing.mode");
    let mut created = test.thread_manager.subscribe_thread_created();
    let mut first_child = None;
    for (index, (mode, task, expected_model, expected_effort, calls)) in [
        (None, "rule_worker", ROUTED_MODEL, ReasoningEffort::High, 0),
        (
            Some("off\n"),
            "rule_worker",
            PARENT_MODEL,
            ReasoningEffort::Low,
            0,
        ),
        (
            Some("rules-only\n"),
            "rule_worker",
            ROUTED_MODEL,
            ReasoningEffort::High,
            0,
        ),
        (
            Some("rules-only\n"),
            "classify_worker",
            PARENT_MODEL,
            ReasoningEffort::Low,
            0,
        ),
        (
            Some("configured\n"),
            "classify_worker",
            ROUTED_MODEL,
            ReasoningEffort::Medium,
            1,
        ),
        (
            Some("off\n"),
            "classify_worker",
            PARENT_MODEL,
            ReasoningEffort::Low,
            1,
        ),
        (
            Some("corrupt\n"),
            "classify_worker",
            PARENT_MODEL,
            ReasoningEffort::Low,
            1,
        ),
        (
            Some("configured\n"),
            "rule_worker",
            ROUTED_MODEL,
            ReasoningEffort::High,
            1,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(mode) = mode {
            let temp = mode_path.with_extension("pending");
            tokio::fs::write(&temp, mode).await?;
            tokio::fs::rename(temp, &mode_path).await?;
        }
        let prompt = format!("runtime routing step {index}");
        let call = format!("runtime-spawn-{index}");
        let task_name = format!("{task}_{index}");
        let mut spawn = ev_function_call_with_namespace(
            &call,
            "collaboration",
            "spawn_agent",
            &json!({"message":ENCRYPTED_TASK,"task_name":task_name,"fork_turns":"none"})
                .to_string(),
        );
        spawn["item"]["encrypted_function_args"] = json!(["message"]);
        let match_prompt = prompt.clone();
        let match_call = call.clone();
        mount_sse_once_match(
            &server,
            move |r: &wiremock::Request| {
                !child_request(r)
                    && body_contains(r, &match_prompt)
                    && !body_contains(r, &match_call)
            },
            sse(vec![ev_response_created(&call), spawn, ev_completed(&call)]),
        )
        .await;
        let mut response = sse_response(sse(vec![
            ev_response_created("runtime-child"),
            ev_assistant_message("runtime-child-answer", "done"),
            ev_completed("runtime-child"),
        ]));
        if index == 0 {
            response = response.set_delay(Duration::from_secs(10));
        }
        let requests = mount_response_once_match(&server, child_request, response).await;
        test.submit_turn(&prompt).await?;
        let id = timeout(Duration::from_secs(10), created.recv())
            .await
            .with_context(|| format!("step {index}: child creation"))??;
        let child = test.thread_manager.get_thread(id).await?;
        timeout(Duration::from_secs(10), async {
            while !requests
                .requests()
                .iter()
                .any(|r| r.body_json()["client_metadata"]["thread_id"] == json!(id))
            {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .with_context(|| format!("step {index}: request for child {id}"))?;
        let snapshot = child.config_snapshot().await;
        assert_eq!(
            (snapshot.model.as_str(), snapshot.reasoning_effort),
            (expected_model, Some(expected_effort.clone()))
        );
        // The capture helper also observes requests rejected by its later matcher.
        // Assert only this native child, identified by its actual thread metadata.
        for request in requests
            .requests()
            .into_iter()
            .filter(|r| r.body_json()["client_metadata"]["thread_id"] == json!(id))
        {
            let body = request.body_json();
            assert_eq!(
                (body["model"].clone(), body["reasoning"]["effort"].clone()),
                (json!(expected_model), json!(expected_effort))
            );
            assert!(body["input"].as_array().expect("child request input").iter().any(|item| {
                item["type"] == "agent_message" && item["content"].as_array().is_some_and(|parts| {
                    parts.contains(&json!({"type":"encrypted_content","encrypted_content":ENCRYPTED_TASK}))
                })
            }));
        }
        assert_eq!(
            classifier
                .received_requests()
                .await
                .expect("record classifier requests")
                .len(),
            calls
        );
        if index == 0 {
            assert_eq!(child.agent_status().await, AgentStatus::Running);
            first_child = Some(child);
        } else {
            if index == 1 {
                let first = first_child.as_ref().expect("first child was created");
                assert_eq!(first.agent_status().await, AgentStatus::Running);
                let snapshot = first.config_snapshot().await;
                assert_eq!(
                    (snapshot.model.as_str(), snapshot.reasoning_effort),
                    (ROUTED_MODEL, Some(ReasoningEffort::High))
                );
            }
            timeout(Duration::from_secs(10), async {
                while !matches!(child.agent_status().await, AgentStatus::Completed(_)) {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .with_context(|| format!("step {index}: child completion"))?;
        }
    }
    let first = first_child.expect("first child was created");
    timeout(Duration::from_secs(15), async {
        while !matches!(first.agent_status().await, AgentStatus::Completed(_)) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(
        classifier
            .received_requests()
            .await
            .expect("record classifier requests")
            .len(),
        1
    );
    Ok(())
}
