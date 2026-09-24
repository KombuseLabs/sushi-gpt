use super::Config;
use super::ConfigOverrides;
use super::ConfigToml;
use core_test_support::TempDirExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn routing_configuration_reaches_native_runtime_and_rejects_empty_matchers()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let cfg: ConfigToml = toml::from_str(
        r#"
        [agent_model_routing]
        enabled = true
        [[agent_model_routing.rules]]
        task_name_contains = ["summarize"]
        model = "configured-worker-model"
        [agent_model_routing.jev]
        enabled = true
        [agent_model_routing.jev.classes.small]
        description = "Small fixture task"
        model = "configured-worker-model"
    "#,
    )?;
    let expected = cfg.agent_model_routing.clone();
    let config =
        Config::load_from_base_config_with_overrides(cfg, ConfigOverrides::default(), home.abs())
            .await?;
    assert_eq!(config.agent_model_routing, expected);
    let invalid: ConfigToml = toml::from_str(
        r#"
        [agent_model_routing]
        enabled = true
        [[agent_model_routing.rules]]
        model = "configured-worker-model"
    "#,
    )?;
    let error = Config::load_from_base_config_with_overrides(
        invalid,
        ConfigOverrides::default(),
        home.abs(),
    )
    .await
    .expect_err("empty matchers must fail config loading");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("agent_model_routing rule 0"));
    Ok(())
}

#[tokio::test]
async fn plaintext_messages_require_a_non_reserved_agent_tool_namespace() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    for source in [
        // Omitted namespace resolves to the reserved default.
        r#"
        [agent_model_routing]
        enabled = true
        plaintext_messages = true
    "#,
        r#"
        [features]
        multi_agent_v2 = { enabled = true, tool_namespace = "collaboration" }
        [agent_model_routing]
        enabled = true
        plaintext_messages = true
    "#,
    ] {
        let reserved: ConfigToml = toml::from_str(source)?;
        let error = Config::load_from_base_config_with_overrides(
            reserved,
            ConfigOverrides::default(),
            home.abs(),
        )
        .await
        .expect_err("plaintext under the reserved namespace must fail config loading");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("tool_namespace"), "{error}");
    }
    let renamed: ConfigToml = toml::from_str(
        r#"
        [features]
        multi_agent_v2 = { enabled = true, tool_namespace = "agent_router_agents" }
        [agent_model_routing]
        enabled = true
        plaintext_messages = true
    "#,
    )?;
    let config = Config::load_from_base_config_with_overrides(
        renamed,
        ConfigOverrides::default(),
        home.abs(),
    )
    .await?;
    assert_eq!(
        config.multi_agent_v2.tool_namespace.as_deref(),
        Some("agent_router_agents")
    );
    assert!(
        config
            .agent_model_routing
            .is_some_and(|routing| routing.plaintext_messages)
    );
    Ok(())
}
