use super::*;
use AgentModelRoutingTask::V1Message;
use AgentModelRoutingTask::V2TaskName;
use pretty_assertions::assert_eq;

#[test]
fn strict_candidates_require_executable_declarations_and_explicit_opt_in() {
    let source = r#"
enabled = true
strict_candidates = true
plaintext_messages = true
inherit_dynamic_tools = true
[jev]
enabled = true
[jev.classes.document]
description = "A bounded document task through the authorized host callback"
model = "synthetic-hosted"
model_provider = "hosted"
capabilities = ["text", "tools", "streaming", "dynamic_tools"]
"#;
    let routing: AgentModelRouting = toml::from_str(source).unwrap();
    routing.validate().unwrap();
    let mut classifier_disabled = routing;
    classifier_disabled.jev.as_mut().unwrap().enabled = false;
    classifier_disabled.validate().unwrap();
    classifier_disabled.jev.as_mut().unwrap().classes.clear();
    assert!(classifier_disabled.validate().is_err());
    for modified in [
        source.replacen("enabled = true", "enabled = false", 1),
        source.replace(
            "inherit_dynamic_tools = true",
            "inherit_dynamic_tools = false",
        ),
        source.replace("model_provider = \"hosted\"", ""),
        source.replace("\"streaming\", ", ""),
    ] {
        let invalid: AgentModelRouting = toml::from_str(&modified).unwrap();
        assert!(invalid.validate().is_err());
    }
    assert!(
        toml::from_str::<AgentModelRouting>(&source.replace("\"text\"", "\"telepathy\"")).is_err()
    );
}

#[test]
fn parses_and_selects_ordered_rules_with_independent_role_and_task_filters() {
    let routing: AgentModelRouting = toml::from_str(
        r#"
        enabled = true
        [[rules]]
        agent_type = "reviewer"
        task_contains = ["security"]
        model = "review-model"
        reasoning_effort = "high"
        [[rules]]
        task_contains = ["summarize", "extract"]
        model = "analysis-model"
    "#,
    )
    .unwrap();
    routing.validate().unwrap();
    assert_eq!(
        routing.select("reviewer", V1Message("Summarize SECURITY findings")),
        routing.rules.first()
    );
    assert_eq!(
        routing.select("default", V1Message("Summarize SECURITY findings")),
        routing.rules.get(1)
    );
    assert_eq!(
        routing.select("reviewer", V1Message("write a parser")),
        None
    );
    let disabled = AgentModelRouting {
        plaintext_messages: false,
        strict_candidates: false,
        inherit_dynamic_tools: false,
        enabled: false,
        ..routing
    };
    assert_eq!(
        disabled.select("reviewer", V1Message("summarize security findings")),
        None
    );
}

#[test]
fn rejects_ambiguous_empty_or_unbounded_matchers_and_unknown_settings() {
    for contents in [
        "[[rules]]\nmodel = 'worker'",
        "[[rules]]\nmodel = 'worker'\ntask_contains = ['']",
        "[[rules]]\nmodel = 'worker'\ntask_name_contains = [' ']",
        "[[rules]]\nmodel = 'worker'\ntask_contains = ['x']\ntask_name_contains = ['x']",
        "[[rules]]\nmodel = ''\nagent_type = 'reviewer'",
    ] {
        let routing: AgentModelRouting = toml::from_str(contents).unwrap();
        assert!(routing.validate().is_err());
    }
    assert!(toml::from_str::<AgentModelRouting>("enabled = true\nunknown = 1").is_err());
    let rule = AgentModelRoute {
        model_provider: None,
        model: "worker".to_string(),
        agent_type: Some("reviewer".to_string()),
        task_contains: Vec::new(),
        task_name_contains: Vec::new(),
        reasoning_effort: None,
    };
    let routing = AgentModelRouting {
        plaintext_messages: false,
        strict_candidates: false,
        inherit_dynamic_tools: false,
        enabled: true,
        rules: vec![rule; 33],
        jev: None,
    };
    assert!(routing.validate().is_err());
}

#[test]
fn separates_message_and_name_matchers_and_preserves_role_filters_and_rule_order() {
    let routing: AgentModelRouting = toml::from_str(
        r#"
        enabled = true
        [[rules]]
        task_contains = ["summarize"]
        model = "v1-worker"
        [[rules]]
        agent_type = "reviewer"
        task_name_contains = ["summarize", "extract"]
        model = "v2-reviewer"
        [[rules]]
        task_name_contains = ["summarize", "extract"]
        model = "v2-worker"
        [[rules]]
        agent_type = "default"
        model = "role-worker"
        "#,
    )
    .unwrap();
    routing.validate().unwrap();
    assert_eq!(
        routing.select("reviewer", V1Message("SUMMARIZE findings")),
        routing.rules.first()
    );
    assert_eq!(
        routing.select("reviewer", V2TaskName("SUMMARIZE_findings")),
        routing.rules.get(1)
    );
    assert_eq!(
        routing.select("default", V2TaskName("extract_findings")),
        routing.rules.get(2)
    );
    assert_eq!(
        routing.select("default", V2TaskName("unmatched")),
        routing.rules.get(3)
    );
    assert_eq!(
        routing.select("default", V1Message("unmatched")),
        routing.rules.get(3)
    );
    assert_eq!(routing.select("reviewer", V2TaskName("unmatched")), None);
}

#[test]
fn bounds_both_task_matcher_lists() {
    for field in ["task_contains", "task_name_contains"] {
        for matchers in [vec!["x".to_string(); 17], vec!["x".repeat(257)]] {
            let contents = format!("[[rules]]\nmodel = 'worker'\n{field} = {matchers:?}");
            let routing: AgentModelRouting = toml::from_str(&contents).unwrap();
            assert!(routing.validate().is_err());
        }
    }
}

#[test]
fn validates_jev_settings_and_defaults_without_enabling_requests() {
    let valid = r#"
        [jev]
        enabled = true
        [jev.classes.small]
        description = "Simple bounded tasks"
        model = "worker"
    "#;
    let config: AgentModelRouting = toml::from_str(valid).unwrap();
    config.validate().unwrap();
    assert!(!config.enabled);
    assert_eq!(config.jev.as_ref().unwrap().api_key_env, "TYPESAFE_API_KEY");
    for setting in [
        "timeout_ms = 0",
        "timeout_ms = 10001",
        "min_confidence = nan",
        "min_confidence = 1.1",
        "api_key_env = 'KEY WITH SPACES'",
        "endpoint = 'http://api.typesafe.ai/v1/systemone'",
        "endpoint = 'https://api.typesafe.ai.evil/v1/systemone'",
        "endpoint = 'http://127.0.0.1:1234@evil/v1/systemone'",
        "endpoint = 'http://127.0.0.1:0/v1/systemone'",
    ] {
        let config: AgentModelRouting =
            toml::from_str(&valid.replace("enabled = true", &format!("enabled = true\n{setting}")))
                .unwrap();
        assert!(config.validate().is_err(), "{setting}");
    }
    for endpoint in [
        "http://127.0.0.1:1234/v1/systemone",
        "http://[::1]:1234/v1/systemone",
    ] {
        let mut config = config.clone();
        config.jev.as_mut().unwrap().endpoint = endpoint.to_string();
        config.validate().unwrap();
    }
    let mut config = config;
    let jev = config.jev.as_mut().unwrap();
    let class = jev.classes.remove("small").unwrap();
    assert!(jev.validate().is_err());
    jev.classes.insert("abstain".to_string(), class.clone());
    assert!(jev.validate().is_err());
    jev.classes = (0..17)
        .map(|i| (format!("class_{i}"), class.clone()))
        .collect();
    assert!(jev.validate().is_err());
    assert!(
        toml::from_str::<AgentModelRouting>(
            &valid.replace("enabled = true", "api_key = 'not-supported'")
        )
        .is_err()
    );
}
