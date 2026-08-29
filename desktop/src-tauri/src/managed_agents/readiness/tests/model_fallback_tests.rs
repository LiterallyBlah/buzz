use super::*;

#[test]
fn resolve_effective_agent_env_user_env_wins_over_structured_fields() {
    // User env_vars must win over baked defaults; in OSS builds baked map is empty,
    // so this validates the user-env layer is present in the output.
    let mut env_vars = BTreeMap::new();
    env_vars.insert("BUZZ_AGENT_PROVIDER".to_string(), "anthropic".to_string());
    env_vars.insert(
        "BUZZ_AGENT_MODEL".to_string(),
        "claude-opus-4-5".to_string(),
    );

    // Minimal record: only the fields resolve_effective_agent_env reads.
    let record = crate::managed_agents::types::ManagedAgentRecord {
        pubkey: "test-pubkey".to_string(),
        name: "test-agent".to_string(),
        persona_id: None,
        private_key_nsec: String::new(),
        auth_tag: None,
        relay_url: String::new(),
        avatar_url: None,
        acp_command: "buzz-acp".to_string(),
        agent_command: "buzz-agent".to_string(),
        agent_command_override: None,
        agent_args: vec![],
        mcp_command: String::new(),
        turn_timeout_seconds: 320,
        idle_timeout_seconds: None,
        max_turn_duration_seconds: None,
        parallelism: 1,
        system_prompt: None,
        model: None,
        provider: None,
        persona_source_version: None,
        env_vars,
        start_on_app_launch: false,
        auto_restart_on_config_change: true,
        runtime_pid: None,
        backend: Default::default(),
        backend_agent_id: None,
        provider_policy_pending: false,
        provider_binary_path: None,
        team_id: None,
        persona_team_dir: None,
        persona_name_in_team: None,
        created_at: String::new(),
        updated_at: String::new(),
        last_started_at: None,
        last_stopped_at: None,
        last_exit_code: None,
        last_error: None,
        last_error_code: None,
        respond_to: Default::default(),
        respond_to_allowlist: vec![],
        display_name: None,
        slug: None,
        runtime: None,
        name_pool: Vec::new(),
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        team_catalog_source: None,
        definition_respond_to: None,
        definition_respond_to_allowlist: Vec::new(),
        definition_parallelism: None,
        relay_mesh: None,
        effort_level: None,
    };

    let runtime = known_acp_runtime_exact("buzz-agent");
    let effective = resolve_effective_agent_env(&record, &[], runtime, &Default::default());

    // User env_vars must be present in the output (last-write-wins).
    assert_eq!(
        effective.env.get("BUZZ_AGENT_PROVIDER").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        effective.env.get("BUZZ_AGENT_MODEL").map(String::as_str),
        Some("claude-opus-4-5")
    );
}

#[test]
fn buzz_agent_databricks_v2_with_databricks_model_but_no_buzz_agent_model_is_ready() {
    // The baked buzz-releases env sets DATABRICKS_MODEL but not BUZZ_AGENT_MODEL.
    // An agent with only DATABRICKS_MODEL must pass the readiness gate.
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "databricks_v2"),
            ("DATABRICKS_MODEL", "goose-claude-4-6-sonnet"),
            ("DATABRICKS_HOST", "https://dbc.example.com"),
        ]),
    );
    assert!(
        agent_readiness(&env).is_ready(),
        "DATABRICKS_MODEL must satisfy the model requirement for databricks_v2"
    );
}

#[test]
fn buzz_agent_databricks_v2_hyphen_alias_with_databricks_model_is_ready() {
    // buzz-agent accepts both "databricks_v2" and "databricks-v2". The
    // readiness gate must recognize the hyphen alias and accept DATABRICKS_MODEL.
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "databricks-v2"),
            ("DATABRICKS_MODEL", "goose-claude-4-6-sonnet"),
            ("DATABRICKS_HOST", "https://dbc.example.com"),
        ]),
    );
    assert!(
        agent_readiness(&env).is_ready(),
        "databricks-v2 alias with DATABRICKS_MODEL must be Ready"
    );
}

#[test]
fn buzz_agent_databricks_hyphen_alias_missing_host_returns_not_ready() {
    // The hyphen alias "databricks-v2" requires DATABRICKS_HOST just like
    // the underscore variants. Without it the agent cannot reach the endpoint.
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "databricks-v2"),
            ("DATABRICKS_MODEL", "goose-claude-4-6-sonnet"),
            // DATABRICKS_HOST intentionally absent
        ]),
    );
    let result = agent_readiness(&env);
    assert!(
        !result.is_ready(),
        "databricks-v2 without DATABRICKS_HOST must be NotReady"
    );
    let reqs = result.requirements();
    assert!(
        reqs.iter()
            .any(|r| matches!(r, Requirement::EnvKey { key } if key == "DATABRICKS_HOST")),
        "missing requirements must include DATABRICKS_HOST; got {reqs:?}"
    );
}

#[test]
fn buzz_agent_databricks_v1_with_databricks_model_but_no_buzz_agent_model_is_ready() {
    // V1 (Model Serving) also resolves DATABRICKS_MODEL — same fallback applies.
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "databricks"),
            ("DATABRICKS_MODEL", "dbrx-instruct"),
            ("DATABRICKS_HOST", "https://dbc.example.com"),
        ]),
    );
    assert!(
        agent_readiness(&env).is_ready(),
        "DATABRICKS_MODEL must satisfy the model requirement for databricks (V1)"
    );
}

#[test]
fn buzz_agent_anthropic_with_anthropic_model_but_no_buzz_agent_model_is_ready() {
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "anthropic"),
            ("ANTHROPIC_MODEL", "claude-opus-4-5"),
            ("ANTHROPIC_API_KEY", "sk-test"),
        ]),
    );
    assert!(
        agent_readiness(&env).is_ready(),
        "ANTHROPIC_MODEL must satisfy the model requirement for anthropic"
    );
}

#[test]
fn buzz_agent_openai_with_openai_compat_model_but_no_buzz_agent_model_is_ready() {
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "openai"),
            ("OPENAI_COMPAT_MODEL", "gpt-4o"),
            ("OPENAI_COMPAT_API_KEY", "sk-test"),
        ]),
    );
    assert!(
        agent_readiness(&env).is_ready(),
        "OPENAI_COMPAT_MODEL must satisfy the model requirement for openai"
    );
}

#[test]
fn buzz_agent_empty_provider_model_fallback_key_is_not_ready() {
    // An empty DATABRICKS_MODEL with no BUZZ_AGENT_MODEL must still be NotReady.
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "databricks_v2"),
            ("DATABRICKS_MODEL", ""),
            ("DATABRICKS_HOST", "https://dbc.example.com"),
        ]),
    );
    let result = agent_readiness(&env);
    assert!(
        !result.is_ready(),
        "empty DATABRICKS_MODEL with no BUZZ_AGENT_MODEL must be NotReady"
    );
    assert!(result
        .requirements()
        .contains(&Requirement::NormalizedField {
            field: "model".to_string()
        }));
}

// ── OpenRouter readiness ─────────────────────────────────────────────

#[test]
fn buzz_agent_openrouter_with_all_fields_is_ready() {
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "openrouter"),
            ("BUZZ_AGENT_MODEL", "anthropic/claude-sonnet-4"),
            ("OPENROUTER_API_KEY", "sk-or-test-key"),
        ]),
    );
    let result = agent_readiness(&env);
    assert!(
        result.is_ready(),
        "openrouter with all fields should be ready"
    );
}

#[test]
fn buzz_agent_openrouter_missing_key_returns_not_ready() {
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "openrouter"),
            ("BUZZ_AGENT_MODEL", "anthropic/claude-sonnet-4"),
        ]),
    );
    let result = agent_readiness(&env);
    assert!(!result.is_ready());
    assert!(result.requirements().contains(&Requirement::EnvKey {
        key: "OPENROUTER_API_KEY".to_string()
    }));
}
#[test]
fn buzz_agent_openrouter_with_provider_model_fallback_is_ready() {
    let env = make_env(
        "buzz-agent",
        env_with(&[
            ("BUZZ_AGENT_PROVIDER", "openrouter"),
            ("OPENROUTER_MODEL", "google/gemini-2.5-flash"),
            ("OPENROUTER_API_KEY", "sk-or-test-key"),
        ]),
    );
    let result = agent_readiness(&env);
    assert!(
        result.is_ready(),
        "OPENROUTER_MODEL fallback should satisfy model requirement"
    );
}
