//! Native spawn + host callback using two synthetic providers, never hosted inference.
use super::*;
use codex_config::config_toml::agent_model_routing::JevCapability;
use codex_core::StartThreadOptions;
use codex_model_provider_info::WireApi;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const KEY: &str = "SUSHI_HOSTED_SYNTHETIC_KEY";
const MODEL: &str = "synthetic-claude";
const ASSIGNMENT: &str = "Create the synthetic document through the authorized host tool";

fn response(events: Vec<serde_json::Value>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(events))
}

#[test]
fn native_hosted_child_uses_inherited_dynamic_tool_and_fails_closed() -> Result<()> {
    if std::env::var(KEY).as_deref() != Ok("synthetic-fixture") {
        let telemetry_home = tempfile::tempdir()?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "hosted::native_hosted_child_uses_inherited_dynamic_tool_and_fails_closed",
                "--nocapture",
            ])
            .env(KEY, "synthetic-fixture")
            .env("SUSHIGPT_TELEMETRY", "1")
            .env("CODEX_HOME", telemetry_home.path())
            .status()?;
        assert!(status.success());
        let records =
            std::fs::read_to_string(telemetry_home.path().join("sushigpt-telemetry.jsonl"))?;
        let events = records
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|row| row["kind"] == "classifier_transition")
            .collect::<Vec<_>>();
        for (phase, reason, status) in [
            ("skipped", "rule_matched", json!(null)),
            ("succeeded", "jev_selected", json!(null)),
            ("failed", "http", json!(503)),
        ] {
            let event = events
                .iter()
                .find(|row| row["phase"] == phase && row["reasonCode"] == reason)
                .expect("native routing emitted the expected classifier transition");
            assert_eq!(event["httpStatusCode"], status);
            if phase == "skipped" {
                assert_eq!(event["requestStarted"], false);
            } else {
                let start = events
                    .iter()
                    .find(|row| {
                        row["attemptId"] == event["attemptId"] && row["phase"] == "request_started"
                    })
                    .expect("terminal event has a matching send attempt");
                assert_eq!(
                    (&start["parentThreadId"], &start["parentTurnId"]),
                    (&event["parentThreadId"], &event["parentTurnId"])
                );
                assert!(event["parentTurnId"].is_string());
            }
            if phase == "succeeded" {
                assert_eq!(event["recommendedModel"], MODEL);
            }
        }
        assert!(!records.contains(ASSIGNMENT));
        assert!(!records.contains("synthetic-fixture"));
        return Ok(());
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        for scenario in [
            "success",
            "jev",
            "jev_outage",
            "encrypted",
            "unspecified",
            "fork",
            "missing_credential",
        ] {
            exercise(scenario).await?;
        }
        Ok(())
    })
}

async fn exercise(scenario: &str) -> Result<()> {
    let is_local_cli = matches!(
        scenario,
        "local_cli" | "local_cli_jev" | "missing_transport" | "local_cli_cancel"
    );
    let cli_directory = tempfile::tempdir()?;
    let cli_executable = cli_directory.path().join("success.py");
    #[cfg(unix)]
    let cli_pid = cli_executable.with_extension("py.pid");
    #[cfg(unix)]
    if is_local_cli {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            &cli_executable,
            include_str!("../../../sushi/claude-transport/tests/fixtures/peer.py"),
        )?;
        std::fs::set_permissions(&cli_executable, std::fs::Permissions::from_mode(0o700))?;
    }
    let root_server = start_mock_server().await;
    let child_server = start_mock_server().await;
    Mock::given(method("POST")).and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(if scenario == "jev_outage" {503} else {200}).set_body_json(json!({
            "model":"jev-1.13.0","answers":{"route":{"type":"choice","choice":"document","confidence":0.99,"probabilities":{"document":0.99,"abstain":0.01}}}
        }))).mount(&root_server).await;
    let mut spawn = ev_function_call_with_namespace(
        "hosted-spawn",
        "agent_router_agents",
        "spawn_agent",
        &json!({
            "task_name":"document_fixture", "message":ASSIGNMENT,
            "fork_turns":if scenario == "fork" {"1"} else {"none"}
        })
        .to_string(),
    );
    if scenario != "unspecified" {
        spawn["item"]["encrypted_function_args"] = if scenario == "encrypted" {
            json!(["message"])
        } else {
            json!([])
        };
    }
    let root_request = mount_sse_once_match(
        &root_server,
        |r: &wiremock::Request| !body_contains(r, "hosted-spawn"),
        sse(vec![
            ev_response_created("parent-first"),
            spawn,
            ev_completed("parent-first"),
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(response(vec![
            ev_response_created("parent-followup"),
            ev_assistant_message("done", "processed"),
            ev_completed("parent-followup"),
        ]))
        .with_priority(10)
        .mount(&root_server)
        .await;
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("synthetic request JSON");
            assert_eq!(body["model"], json!(MODEL));
            assert_eq!(body["store"], json!(false));
            if body["input"].as_array().expect("request input array").iter().any(|item| item["type"] == "function_call_output") {
                assert!(body.to_string().contains("document-fixture-created"));
                response(vec![ev_response_created("child-done"), ev_assistant_message("child-answer", "document completed"), ev_completed("child-done")])
            } else {
                let tool = body["tools"].as_array().expect("request tool array").iter().find(|t| t["description"] == "Synthetic document callback").expect("inherited dynamic tool");
                let call = json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"document-call","name":tool["name"],"arguments":"{\"title\":\"fixture\"}"}});
                response(vec![ev_response_created("child-tool"), call, ev_completed("child-tool")])
            }
        }).mount(&child_server).await;
    let child_url = format!("{}/v1", child_server.uri());
    let missing = scenario == "missing_credential";
    let jev_endpoint = format!("{}/v1/systemone", root_server.uri());
    let use_jev = matches!(scenario, "jev" | "jev_outage" | "local_cli_jev");
    let mut builder = test_codex()
        .with_model(PARENT_MODEL)
        .with_model_info_override(PARENT_MODEL, |model| {
            model.multi_agent_version = Some(MultiAgentVersion::V2);
            model.use_responses_lite = false;
        })
        .with_model_info_override(MODEL, |model| {
            model.multi_agent_version = Some(MultiAgentVersion::V2);
            model.tool_mode = Some(ToolMode::Direct);
            model.use_responses_lite = false;
            model.default_reasoning_level = None;
            model.supported_reasoning_levels.clear();
            model.supports_reasoning_summary_parameter = false;
            model.supports_search_tool = false;
            model.support_verbosity = false;
        })
        .with_config(move |config| {
            config.model = Some(PARENT_MODEL.into());
            // Plaintext messages reshape the agent tools; the backend rejects that under
            // its reserved `collaboration` namespace, so cross-provider children need a rename.
            config.multi_agent_v2.tool_namespace = Some("agent_router_agents".into());
            config
                .features
                .enable(Feature::Collab)
                .expect("enable collaboration");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable V2");
            config.agent_default_subagent_reasoning_effort = None;
            if is_local_cli {
                config.model_reasoning_effort = Some(ReasoningEffort::Low);
                config.service_tier = Some("default".into());
            }
            config.agent_model_routing = Some(AgentModelRouting {
                enabled: true,
                plaintext_messages: true,
                inherit_dynamic_tools: true,
                strict_candidates: true,
                rules: if use_jev {
                    vec![]
                } else {
                    vec![AgentModelRoute {
                        model_provider: Some("synthetic-hosted".into()),
                        model: MODEL.into(),
                        agent_type: Some("default".into()),
                        task_contains: vec![],
                        task_name_contains: vec![],
                        reasoning_effort: None,
                    }]
                },
                jev: Some(JevRouting {
                    enabled: !is_local_cli || use_jev,
                    api_key_env: KEY.into(),
                    endpoint: jev_endpoint.clone(),
                    classes: [(
                        "document".into(),
                        JevRoutingClass {
                            description: "A synthetic document task".into(),
                            model: MODEL.into(),
                            model_provider: Some("synthetic-hosted".into()),
                            reasoning_effort: None,
                            capabilities: vec![
                                JevCapability::Text,
                                JevCapability::Tools,
                                JevCapability::Streaming,
                                JevCapability::DynamicTools,
                            ],
                        },
                    )]
                    .into(),
                    ..Default::default()
                }),
            });
            let mut provider = config.model_provider.clone();
            provider.name = "Synthetic hosted fixture".into();
            provider.base_url = Some(child_url.clone());
            provider.wire_api = WireApi::OpenResponses;
            provider.requires_openai_auth = false;
            provider.supports_websockets = false;
            provider.env_key = Some(
                if missing {
                    "SUSHI_MISSING_HOSTED_FIXTURE_KEY"
                } else {
                    KEY
                }
                .into(),
            );
            provider.request_max_retries = Some(0);
            provider.stream_max_retries = Some(0);
            if is_local_cli {
                provider = codex_model_provider_info::ModelProviderInfo {
                    name: "Synthetic local CLI fixture".into(),
                    wire_api: WireApi::ClaudeCli,
                    cli_command: Some(cli_executable),
                    stream_idle_timeout_ms: Some(5000),
                    ..Default::default()
                };
            }
            config
                .model_providers
                .insert("synthetic-hosted".into(), provider);
        });
    if scenario == "missing_transport" {
        let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
        codex_sushi_routing::install(&mut extensions);
        builder = builder.with_extensions(std::sync::Arc::new(extensions.build()));
    }
    let mut test = builder.build_with_auto_env(&root_server).await?;
    let root = test.thread_manager.start_thread(StartThreadOptions {
        dynamic_tools:vec![DynamicToolSpec::Function(DynamicToolFunctionSpec { name:"document_fixture".into(), description:"Synthetic document callback".into(), input_schema:json!({"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}), defer_loading:false })],
        environments:Some(vec![test.executor_environment().selection().clone()]),
        ..StartThreadOptions::new(test.config.clone())
    }).await?;
    test.codex = root.thread;
    test.session_configured = root.session_configured;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_text_turn("Start the document fixture").await?;
    let root_body = root_request.single_request().body_json();
    assert!(
        !root_body["tools"]
            .to_string()
            .contains("\"encrypted\":true")
    );
    // "unspecified": the model omitted encrypted_function_args. Under the non-reserved namespace
    // the backend never encrypts, so the message is delivered as plaintext.
    if !matches!(
        scenario,
        "success" | "jev" | "local_cli" | "local_cli_jev" | "unspecified" | "local_cli_cancel"
    ) {
        assert!(created.try_recv().is_err(), "scenario {scenario}");
        if scenario == "missing_transport" {
            assert!(
                root_server
                    .received_requests()
                    .await
                    .expect("recorded native requests")
                    .iter()
                    .any(|request| body_contains(request, "no transport extension is registered"))
            );
        }
        assert!(
            child_server
                .received_requests()
                .await
                .expect("recorded mock requests")
                .is_empty()
        );
        return Ok(());
    }
    let id = timeout(Duration::from_secs(20), created.recv()).await??;
    let child = test.thread_manager.get_thread(id).await?;
    let call = timeout(
        Duration::from_secs(20),
        wait_for_event_match(&child, |event| match event {
            EventMsg::DynamicToolCallRequest(call) => Some(Ok(call.clone())),
            EventMsg::Error(error) => Some(Err(error.message.clone())),
            _ => None,
        }),
    )
    .await?
    .map_err(anyhow::Error::msg)?;
    assert_eq!(
        call.arguments,
        json!({"title":if is_local_cli {"fixture-0"} else {"fixture"}})
    );
    #[cfg(unix)]
    if scenario == "local_cli_cancel" {
        child.submit(Op::Interrupt).await?;
        let pid: i32 = std::fs::read_to_string(cli_pid)?.parse()?;
        timeout(Duration::from_secs(20), async {
            loop {
                // Signal zero observes only the synthetic peer; native cancellation must reap it.
                let gone = unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                if gone && child.agent_status().await == AgentStatus::Interrupted {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(child.agent_status().await, AgentStatus::Interrupted);
        assert!(
            child_server
                .received_requests()
                .await
                .expect("recorded mock requests")
                .is_empty()
        );
        return Ok(());
    }
    child
        .submit(Op::DynamicToolResponse {
            id: call.call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "document-fixture-created".into(),
                }],
                success: true,
            },
        })
        .await?;
    timeout(Duration::from_secs(20), async {
        loop {
            if matches!(child.agent_status().await, AgentStatus::Completed(_)) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let snapshot = child.config_snapshot().await;
    assert_eq!(
        (snapshot.model, snapshot.model_provider_id),
        (MODEL.into(), "synthetic-hosted".into())
    );
    assert_eq!(
        snapshot.permission_profile,
        test.codex.config_snapshot().await.permission_profile
    );
    if is_local_cli {
        assert_eq!(snapshot.reasoning_effort, None);
        assert_eq!(snapshot.service_tier, None);
        assert_eq!(
            child.agent_status().await,
            AgentStatus::Completed(Some("document completed".into()))
        );
        assert_eq!(
            root_server
                .received_requests()
                .await
                .expect("recorded parent requests")
                .iter()
                .filter(|request| request.url.path() == "/v1/systemone")
                .count(),
            usize::from(use_jev),
        );
        assert!(
            child_server
                .received_requests()
                .await
                .expect("recorded requests")
                .is_empty()
        );
        // Completion status is published before the controller queues parent mail.
        timeout(Duration::from_secs(20), async {
            loop {
                test.submit_text_turn("Read the completed child result")
                    .await?;
                if root_server
                    .received_requests()
                    .await
                    .expect("recorded parent requests")
                    .iter()
                    .any(|request| body_contains(request, "document completed"))
                {
                    return Ok::<_, anyhow::Error>(());
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        return Ok(());
    }
    assert_eq!(
        child_server
            .received_requests()
            .await
            .expect("recorded mock requests")
            .len(),
        2
    );
    for (phase, encrypted) in [
        ("reject encrypted followup", true),
        ("deliver plaintext followup", false),
    ] {
        let send_id = format!("{phase}-send");
        let followup_id = format!("{phase}-followup");
        let mut events = vec![ev_response_created("parent-message")];
        for (name, call_id, message) in [
            (
                "send_message",
                send_id.as_str(),
                "queued fixture instruction",
            ),
            (
                "followup_task",
                followup_id.as_str(),
                "followup fixture instruction",
            ),
        ] {
            let mut event = ev_function_call_with_namespace(
                call_id,
                "agent_router_agents",
                name,
                &json!({"target":"document_fixture", "message":message}).to_string(),
            );
            event["item"]["encrypted_function_args"] = if encrypted {
                json!(["message"])
            } else {
                json!([])
            };
            events.push(event);
        }
        events.push(ev_completed("parent-message"));
        let marker = followup_id.clone();
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(move |request: &wiremock::Request| {
                body_contains(request, phase) && !body_contains(request, &marker)
            })
            .respond_with(response(events))
            .with_priority(1)
            .mount(&root_server)
            .await;
        test.submit_text_turn(phase).await?;
        if encrypted {
            assert_eq!(
                child_server
                    .received_requests()
                    .await
                    .expect("recorded mock requests")
                    .len(),
                2
            );
        } else {
            timeout(Duration::from_secs(20), async {
                loop {
                    if child_server
                        .received_requests()
                        .await
                        .expect("recorded mock requests")
                        .len()
                        >= 3
                        && matches!(child.agent_status().await, AgentStatus::Completed(_))
                    {
                        break;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            let requests = child_server
                .received_requests()
                .await
                .expect("recorded mock requests");
            assert!(body_contains(
                requests.last().expect("child followup request"),
                "queued fixture instruction"
            ));
            assert!(body_contains(
                requests.last().expect("child followup request"),
                "followup fixture instruction"
            ));
        }
    }
    test.submit_text_turn("Read the completed child result")
        .await?;
    let requests = root_server
        .received_requests()
        .await
        .expect("recorded mock requests");
    assert!(
        requests
            .iter()
            .any(|request| body_contains(request, "document completed")),
        "native child result delivered to parent"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn native_local_claude_child_uses_inherited_host_callback() -> Result<()> {
    exercise("local_cli").await
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jev_can_select_local_claude_with_inherited_host_callback() -> Result<()> {
    // Provide only the synthetic classifier credential in a child test process.
    if std::env::var(KEY).as_deref() != Ok("synthetic-fixture") {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "hosted::jev_can_select_local_claude_with_inherited_host_callback",
                "--nocapture",
            ])
            .env(KEY, "synthetic-fixture")
            .status()?;
        assert!(status.success());
        return Ok(());
    }
    exercise("local_cli_jev").await
}

#[cfg(unix)]
#[tokio::test]
async fn native_local_transport_requires_registered_factory() -> Result<()> {
    exercise("missing_transport").await
}

#[cfg(unix)]
#[tokio::test]
async fn native_interrupt_reaps_local_transport_while_host_tool_is_pending() -> Result<()> {
    exercise("local_cli_cancel").await
}
