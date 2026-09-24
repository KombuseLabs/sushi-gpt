#![allow(clippy::expect_used)]

//! Exercises routing through native spawn tools and inspects the resulting child thread.

use anyhow::Result;
use codex_config::config_toml::agent_model_routing::AgentModelRoute;
use codex_config::config_toml::agent_model_routing::AgentModelRouting;
use codex_config::config_toml::agent_model_routing::JevRouting;
use codex_config::config_toml::agent_model_routing::JevRoutingClass;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::time::sleep;
use tokio::time::timeout;

#[path = "agent_model_routing/hosted.rs"]
mod hosted;
#[path = "agent_model_routing/runtime.rs"]
mod runtime;
#[path = "agent_model_routing/telemetry.rs"]
mod telemetry;

const ROOT: &str = "start the configured routing worker";
const TASK: &str = "summarize the routing fixture";
// Opaque Fernet ciphertext generated for TASK; the mock provider does not decrypt it.
const ENCRYPTED_TASK: &str = "gAAAAABqscU1yhIIgPz6LnZEA7Tmt5DndJRdbhqu0s0j_vfFNefNMkmIaiUIP7JWAjyZGsaXAwJpmaD9dyCcAIbgAQ3kdguM2UPmDFDJeWDy7cAPu_tdU40=";
const TASK_NAME: &str = "summarize_routing_worker";
const CALL: &str = "native-routing-spawn";
const PARENT_MODEL: &str = "gpt-5.6-sol";
const ROUTED_MODEL: &str = "gpt-5.6-terra";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    V1,
    V2,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoutingCase {
    Match,
    NoConfiguration,
    Unregistered,
    UnregisteredNoConfiguration,
    ExplicitEffort,
    RoleModel,
    PartialHistory,
    Disabled,
    ExplicitModel,
    NoMatch,
    FullHistory,
    UnavailableModel,
    WrongTaskField,
    RoleOnly,
    RoleMismatch,
    FirstMatch,
    PlaintextV2,
}

fn body_contains(request: &wiremock::Request, needle: &str) -> bool {
    let body = match request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
    {
        Some(encoding) if encoding.eq_ignore_ascii_case("zstd") => {
            zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
        }
        _ => Some(request.body.clone()),
    };
    body.and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(needle))
}

#[test_case(Backend::V1, RoutingCase::Unregistered; "v1 configured routing requires registration")]
#[test_case(Backend::V2, RoutingCase::Unregistered; "v2 configured routing requires registration")]
#[test_case(Backend::V1, RoutingCase::UnregisteredNoConfiguration; "v1 unregistered native defaults")]
#[test_case(Backend::V2, RoutingCase::UnregisteredNoConfiguration; "v2 unregistered native defaults")]
#[test_case(Backend::V1, RoutingCase::NoConfiguration; "v1 preserves behavior without configuration")]
#[test_case(Backend::V2, RoutingCase::NoConfiguration; "v2 preserves behavior without configuration")]
#[test_case(Backend::V1, RoutingCase::ExplicitEffort; "v1 preserves an explicit reasoning effort")]
#[test_case(Backend::V2, RoutingCase::ExplicitEffort; "v2 preserves an explicit reasoning effort")]
#[test_case(Backend::V1, RoutingCase::RoleModel; "v1 preserves role model precedence")]
#[test_case(Backend::V2, RoutingCase::RoleModel; "v2 preserves role model precedence")]
#[test_case(Backend::V2, RoutingCase::PartialHistory; "v2 routes a partial history child")]
#[test_case(Backend::V1, RoutingCase::Match; "v1 selects a configured model")]
#[test_case(Backend::V2, RoutingCase::Match; "v2 selects a configured model")]
#[test_case(Backend::V1, RoutingCase::Disabled; "v1 preserves behavior when disabled")]
#[test_case(Backend::V2, RoutingCase::Disabled; "v2 preserves behavior when disabled")]
#[test_case(Backend::V1, RoutingCase::ExplicitModel; "v1 preserves an explicit model")]
#[test_case(Backend::V2, RoutingCase::ExplicitModel; "v2 preserves an explicit model")]
#[test_case(Backend::V1, RoutingCase::NoMatch; "v1 preserves inheritance without a match")]
#[test_case(Backend::V2, RoutingCase::NoMatch; "v2 preserves inheritance without a match")]
#[test_case(Backend::V1, RoutingCase::FullHistory; "v1 does not reroute a full history fork")]
#[test_case(Backend::V2, RoutingCase::FullHistory; "v2 does not reroute a full history fork")]
#[test_case(Backend::V1, RoutingCase::UnavailableModel; "v1 rejects an unavailable routed model")]
#[test_case(Backend::V2, RoutingCase::UnavailableModel; "v2 rejects an unavailable routed model")]
#[test_case(Backend::V1, RoutingCase::WrongTaskField; "v1 never interprets a message as a task name")]
#[test_case(Backend::V2, RoutingCase::WrongTaskField; "v2 never matches ciphertext with task_contains")]
#[test_case(Backend::V1, RoutingCase::RoleOnly; "v1 routes by role only")]
#[test_case(Backend::V2, RoutingCase::RoleOnly; "v2 routes encrypted tasks by role only")]
#[test_case(Backend::V1, RoutingCase::RoleMismatch; "v1 requires matching role and message")]
#[test_case(Backend::V2, RoutingCase::RoleMismatch; "v2 requires matching role and task name")]
#[test_case(Backend::V1, RoutingCase::FirstMatch; "v1 uses the first matching rule")]
#[test_case(Backend::V2, RoutingCase::FirstMatch; "v2 uses the first matching name rule")]
#[test_case(Backend::V2, RoutingCase::PlaintextV2; "v2 plaintext transport still routes by task name")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_uses_native_spawn_and_completion(
    backend: Backend,
    case: RoutingCase,
) -> Result<()> {
    run_routing(backend, case, None).await
}

async fn run_routing(backend: Backend, case: RoutingCase, jev: Option<JevRouting>) -> Result<()> {
    let server = start_mock_server().await;
    let message = if backend == Backend::V2 && case != RoutingCase::PlaintextV2 {
        ENCRYPTED_TASK
    } else {
        TASK
    };
    let mut arguments = json!({ "message": message });
    let namespace = match backend {
        Backend::V1 => {
            arguments["fork_context"] = json!(case == RoutingCase::FullHistory);
            "multi_agent_v1"
        }
        Backend::V2 => {
            arguments["task_name"] = json!(TASK_NAME);
            arguments["fork_turns"] = json!(match case {
                RoutingCase::FullHistory => "all",
                RoutingCase::PartialHistory => "1",
                _ => "none",
            });
            "collaboration"
        }
    };
    if case == RoutingCase::ExplicitModel {
        arguments["model"] = json!(PARENT_MODEL);
    }
    if case == RoutingCase::ExplicitEffort {
        arguments["reasoning_effort"] = json!("medium");
    }
    if case == RoutingCase::RoleModel {
        arguments["agent_type"] = json!("routing_role");
    }
    let mut spawn_event =
        ev_function_call_with_namespace(CALL, namespace, "spawn_agent", &arguments.to_string());
    if backend == Backend::V2 {
        spawn_event["item"]["encrypted_function_args"] = if case == RoutingCase::PlaintextV2 {
            json!([])
        } else {
            json!(["message"])
        };
    }
    let root_requests = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, ROOT)
                && !body_contains(request, CALL)
                && !body_contains(request, message)
        },
        sse(vec![
            ev_response_created("routing-root"),
            spawn_event,
            telemetry::completed("routing-root"),
        ]),
    )
    .await;
    let parent_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CALL),
        sse(vec![
            ev_response_created("routing-root-followup"),
            ev_assistant_message("routing-root-answer", "spawn processed"),
            telemetry::completed("routing-root-followup"),
        ]),
    )
    .await;
    let child_requests = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, message) && !body_contains(request, CALL)
        },
        sse(vec![
            ev_response_created("routing-child"),
            ev_assistant_message("routing-child-answer", "routing fixture completed"),
            telemetry::completed("routing-child"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model_info_override(PARENT_MODEL, move |model| {
            // Model metadata also selects the tool surface, independently of the feature flag.
            model.multi_agent_version = Some(match backend {
                Backend::V1 => MultiAgentVersion::V1,
                Backend::V2 => MultiAgentVersion::V2,
            });
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable native collaboration");
            if backend == Backend::V2 {
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("enable native v2");
            } else {
                config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("disable native v2");
            }
            if matches!(
                case,
                RoutingCase::NoConfiguration | RoutingCase::UnregisteredNoConfiguration
            ) {
                return;
            }
            if case == RoutingCase::RoleModel {
                let role_path = config.codex_home.join("routing-role.toml");
                std::fs::write(
                    &role_path,
                    format!("model = \"{PARENT_MODEL}\"\nmodel_reasoning_effort = \"medium\"\n"),
                )
                .expect("write routing role");
                config.agent_roles.insert(
                    "routing_role".to_string(),
                    AgentRoleConfig {
                        description: Some("Routing test role".to_string()),
                        config_file: Some(role_path.to_path_buf()),
                        nickname_candidates: None,
                    },
                );
            }
            let matcher = if case == RoutingCase::NoMatch {
                "unmatched"
            } else {
                "SUMMARIZE"
            };
            let mut rule = AgentModelRoute {
                model_provider: None,
                agent_type: None,
                task_contains: match backend {
                    Backend::V1 => vec![matcher.to_string()],
                    Backend::V2 => Vec::new(),
                },
                task_name_contains: match backend {
                    Backend::V1 => Vec::new(),
                    Backend::V2 => vec![matcher.to_string()],
                },
                model: if case == RoutingCase::UnavailableModel {
                    "not-an-available-model"
                } else {
                    ROUTED_MODEL
                }
                .to_string(),
                reasoning_effort: Some(ReasoningEffort::High),
            };
            if case == RoutingCase::WrongTaskField {
                std::mem::swap(&mut rule.task_contains, &mut rule.task_name_contains);
                if backend == Backend::V2 {
                    // Even a literal substring of the ciphertext must never be matched.
                    rule.task_contains = vec![ENCRYPTED_TASK[..8].to_string()];
                }
            }
            if case == RoutingCase::RoleMismatch {
                rule.agent_type = Some("reviewer".to_string());
            }
            if case == RoutingCase::RoleOnly {
                rule.agent_type = Some("default".to_string());
                rule.task_contains.clear();
                rule.task_name_contains.clear();
            }
            let mut rules = vec![rule.clone()];
            if case == RoutingCase::FirstMatch {
                rule.model = PARENT_MODEL.to_string();
                rules.push(rule);
            }
            config.agent_model_routing = Some(AgentModelRouting {
                plaintext_messages: false,
                strict_candidates: false,
                inherit_dynamic_tools: false,
                enabled: case != RoutingCase::Disabled,
                rules,
                jev: None,
            });
        });
    if let Some(jev) = jev {
        builder = builder.with_config(move |config| {
            if let Some(routing) = &mut config.agent_model_routing {
                if case != RoutingCase::FirstMatch {
                    routing.rules.clear();
                }
                routing.jev = Some(jev);
            }
        });
    }
    if matches!(
        case,
        RoutingCase::Unregistered | RoutingCase::UnregisteredNoConfiguration
    ) {
        builder = builder.with_extensions(codex_extension_api::empty_extension_registry());
    }
    let test = builder.build_with_auto_env(&server).await?;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_turn(ROOT).await?;
    let root_requests = root_requests.requests();
    assert!(!root_requests.is_empty());
    for request in root_requests {
        assert_eq!(request.body_json()["model"], json!(PARENT_MODEL));
    }
    if matches!(
        case,
        RoutingCase::UnavailableModel | RoutingCase::Unregistered
    ) {
        let expected = if case == RoutingCase::Unregistered {
            "no routing extension is registered"
        } else {
            "Unknown model"
        };
        assert!(
            parent_followup
                .function_call_output_text(CALL)
                .is_some_and(|output| output.contains(expected))
        );
        assert!(created.try_recv().is_err());
        assert!(child_requests.requests().is_empty());
        return Ok(());
    }
    let child_id = timeout(Duration::from_secs(10), created.recv()).await??;
    let child = test.thread_manager.get_thread(child_id).await?;
    timeout(Duration::from_secs(10), async {
        loop {
            if let AgentStatus::Completed(_) = child.agent_status().await {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let expected_model = match case {
        RoutingCase::Match
        | RoutingCase::ExplicitEffort
        | RoutingCase::PartialHistory
        | RoutingCase::RoleOnly
        | RoutingCase::FirstMatch
        | RoutingCase::PlaintextV2 => ROUTED_MODEL,
        _ => PARENT_MODEL,
    };
    let snapshot = child.config_snapshot().await;
    assert_eq!(snapshot.model, expected_model);
    let child_requests = child_requests.requests();
    assert!(!child_requests.is_empty());
    for request in &child_requests {
        let body = request.body_json();
        assert_eq!(body["model"], json!(expected_model));
        if backend == Backend::V2 && case != RoutingCase::PlaintextV2 {
            assert!(
                body["input"]
                    .as_array()
                    .expect("child request input array")
                    .iter()
                    .any(|item| {
                        item["type"] == "agent_message"
                            && item["content"].as_array().is_some_and(|content| {
                                content.contains(&json!({
                                    "type": "encrypted_content",
                                    "encrypted_content": ENCRYPTED_TASK,
                                }))
                            })
                    })
            );
        }
    }
    let expected_effort = match case {
        RoutingCase::Match
        | RoutingCase::PartialHistory
        | RoutingCase::RoleOnly
        | RoutingCase::FirstMatch
        | RoutingCase::PlaintextV2 => Some(ReasoningEffort::High),
        RoutingCase::ExplicitEffort | RoutingCase::RoleModel => Some(ReasoningEffort::Medium),
        _ => None,
    };
    if let Some(expected_effort) = expected_effort {
        assert_eq!(snapshot.reasoning_effort, Some(expected_effort.clone()));
        for request in &child_requests {
            assert_eq!(
                request.body_json()["reasoning"]["effort"],
                json!(expected_effort)
            );
        }
    }
    assert_eq!(
        child.agent_status().await,
        AgentStatus::Completed(Some("routing fixture completed".to_string()))
    );
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id, ..
    }) = &snapshot.session_source
    else {
        panic!("expected a native child thread");
    };
    assert_eq!(*parent_thread_id, test.session_configured.thread_id);
    assert_eq!(
        snapshot.session_source.get_agent_path().is_some(),
        backend == Backend::V2
    );
    // Completion notifications can trigger another parent request after the spawn response.
    assert!(
        backend == Backend::V1
            || parent_followup
                .requests()
                .iter()
                .any(|request| request.body_contains_text(TASK_NAME))
    );
    Ok(())
}

// Each test process receives only a synthetic credential. Do not mutate the environment
// of a process running async tests or require developers to export a real API key.
#[test]
fn jev_routing_uses_native_spawn_and_encrypted_context() -> Result<()> {
    const KEY: &str = "SUSHI_JEV_SYNTHETIC_TEST_KEY";
    if std::env::var(KEY).as_deref() != Ok("synthetic-fixture") {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "jev_routing_uses_native_spawn_and_encrypted_context",
                "--nocapture",
            ])
            .env(KEY, "synthetic-fixture")
            .status()?;
        assert!(status.success());
        return Ok(());
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        for (backend, case, scenario, requests) in [
            (Backend::V2, RoutingCase::Match, "selected", 1),
            (Backend::V2, RoutingCase::ExplicitEffort, "explicit_effort", 1),
            (Backend::V2, RoutingCase::PartialHistory, "partial", 1),
            (Backend::V2, RoutingCase::RoleModel, "role_precedence", 1),
            (Backend::V2, RoutingCase::FirstMatch, "rule_precedence", 0),
            (Backend::V2, RoutingCase::FirstMatch, "disabled_with_rules", 0),
            (Backend::V2, RoutingCase::ExplicitModel, "explicit_model", 0),
            (Backend::V2, RoutingCase::FullHistory, "full_history", 0),
            (Backend::V2, RoutingCase::Disabled, "routing_disabled", 0),
            (Backend::V2, RoutingCase::NoConfiguration, "unconfigured", 0),
            (Backend::V1, RoutingCase::NoMatch, "v1", 0),
            (Backend::V2, RoutingCase::NoMatch, "disabled", 0),
            (Backend::V2, RoutingCase::NoMatch, "missing_key", 0),
            (Backend::V2, RoutingCase::NoMatch, "unavailable", 0),
            (Backend::V2, RoutingCase::NoMatch, "uncertain", 1),
            (Backend::V2, RoutingCase::NoMatch, "outage", 1),
            (Backend::V2, RoutingCase::NoMatch, "timeout", 1),
        ] {
            let server = wiremock::MockServer::start().await;
            let mut response = wiremock::ResponseTemplate::new(if scenario == "outage" { 529 } else { 200 }).set_body_json(json!({
                "model": "jev-1.13.0", "answers": {"route": {"type": "choice", "choice": "small", "confidence": if scenario == "uncertain" {0.2} else {0.95}, "probabilities": {"small": 0.99, "abstain": 0.01}}}
            }));
            if scenario == "timeout" { response = response.set_delay(Duration::from_secs(1)); }
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v1/systemone"))
                .and(wiremock::matchers::header("authorization", "Bearer synthetic-fixture"))
                .respond_with(response).expect(requests).mount(&server).await;
            let settings = JevRouting {
                enabled: !matches!(scenario, "disabled" | "disabled_with_rules"), endpoint: format!("{}/v1/systemone", server.uri()),
                api_key_env: if scenario == "missing_key" {"SUSHI_JEV_MISSING_TEST_KEY"} else {KEY}.to_string(),
                timeout_ms: if scenario == "timeout" {100} else {1500},
                classes: [("small".to_string(), JevRoutingClass { capabilities: vec![], model_provider: None, description: "Synthetic bounded task".to_string(), model: if scenario == "unavailable" {"unavailable-fixture"} else {ROUTED_MODEL}.to_string(), reasoning_effort: Some(ReasoningEffort::High) })].into(),
                ..JevRouting::default()
            };
            run_routing(backend, case, Some(settings)).await?;
            for request in server.received_requests().await.unwrap_or_default() {
                let body: serde_json::Value = serde_json::from_slice(&request.body)?;
                assert_eq!(body["state"], json!({"task_name": TASK_NAME, "agent_type": if case == RoutingCase::RoleModel {"routing_role"} else {"default"}}));
                assert!(!String::from_utf8_lossy(&request.body).contains(ENCRYPTED_TASK));
                assert!(!String::from_utf8_lossy(&request.body).contains(TASK));
            }
            server.verify().await;
        }
        runtime::verify_runtime_control(KEY).await?;
        Ok(())
    })
}
