//! Both real adapters retain their original admission and compilation boundary.
use super::*;
use awaken_protocol_managed_testkit::{
    RehydrateFake, environment_fixture, managed_state_from_parts,
};
use awaken_session_configuration_contract::{CustomTool, SessionToolConfiguration};

#[tokio::test]
async fn requested_and_profiled_creation_persist_the_same_exact_inheritance_and_replacement() {
    // Causes: C1 Profile absent/present; C2 explicit replacement absent/present;
    // C3 replacement empty/nonempty. H1 !C1+!C2 => exact Runtime defaults;
    // H2 C1+!C2 => exact Profile clients, capability toolsets retained;
    // H5/H6 C2 => complete replacement, at BOTH C3 edges, for Requested and
    // Profiled HTTP. The pure owner's H3/H4 table supplies nonempty toolset and
    // empty Profile-client edges; these adapters use the original Profile and
    // Runtime fixtures. Each pair shares the same selected durable Store.
    for (rule, agent_id, replacement) in [
        ("H1", "a", None),
        ("H2", "client-tool-agent", None),
        ("H5-H6-full", "client-tool-agent", Some(false)),
        ("H5-H6-empty", "client-tool-agent", Some(true)),
    ] {
        let capability_client = ClientToolDescriptor {
            name: "host-client".into(),
            description: "host client description".into(),
            input_schema: json!({"type":"object","properties":{"host":{"type":"string"}}}),
        };
        let replacement_client = ClientToolDescriptor {
            name: "replacement-client".into(),
            description: "replacement description".into(),
            input_schema: json!({"type":"object","properties":{"replacement":{"type":"number"}}}),
        };
        let runtime = Arc::new(RehydrateFake::default());
        runtime.custom_tools.lock().unwrap().push(CustomTool {
            name: capability_client.name.clone(),
            description: capability_client.description.clone(),
            input_schema: capability_client.input_schema.clone(),
        });
        let sessions = support::session_repository("inheritance Store").await;
        let clock = awaken_time::system_clock();
        let state = Arc::new(
            managed_state_from_parts(
                runtime,
                None,
                sessions.clone(),
                environment_fixture(clock.clone()).execution,
                clock,
                Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .with_config_source(Arc::new(AgentWithClientTool)),
        );
        let expected_clients = match replacement {
            Some(true) => Vec::new(),
            Some(false) => vec![replacement_client.clone()],
            None => AgentWithClientTool
                .session_profile_in("default", agent_id)
                .map_or_else(|| vec![capability_client], |profile| profile.client_tools),
        };
        let expected = SessionToolConfiguration {
            toolsets: Vec::new(),
            client_tools: expected_clients,
        };
        let managed_agent = replacement.map_or_else(
            || json!(agent_id),
            |empty| {
                let tools = if empty {
                    json!([])
                } else {
                    json!([{
                        "type":"custom", "name":replacement_client.name,
                        "description":replacement_client.description,
                        "input_schema":replacement_client.input_schema,
                    }])
                };
                json!({"type":"agent_with_overrides", "id":agent_id, "tools":tools})
            },
        );
        let ordinary = call(
            &router(state.clone()),
            "POST",
            "/v1/sessions",
            Some(json!({"agent":managed_agent})),
        )
        .await
        .expect_for(StatusCode::OK, rule);
        let ordinary_root = sessions
            .get(&SessionId::from(ordinary["id"].as_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(
            ordinary_root.tools, expected,
            "{rule}: Requested durable policy"
        );
        assert_eq!(
            ordinary["agent"]["tools"].as_array().unwrap().len(),
            expected.client_tools.len(),
            "{rule}: public client projection"
        );
        let profiled_id = format!("tool-inheritance-{rule}");
        let mut profiled_body = json!({
            "session_id":profiled_id, "mode":"work_unit", "agent_id":agent_id,
            "environment_id":awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        });
        if replacement.is_some() {
            profiled_body["tools"] = serde_json::to_value(&expected).unwrap();
        }
        let profiled = call(
            &profiled_router(state, "default"),
            "POST",
            "/v1/awaken/sessions",
            Some(profiled_body),
        )
        .await
        .expect_for(StatusCode::ACCEPTED, rule);
        assert_eq!(
            profiled["id"], profiled_id,
            "{rule}: original operation identity"
        );
        let profiled_root = sessions.get(&SessionId::from(profiled_id)).await.unwrap();
        assert_eq!(
            profiled_root.tools, expected,
            "{rule}: Profiled durable policy"
        );
        assert_eq!(
            profiled_root.tools, ordinary_root.tools,
            "{rule}: both original callers select the same complete value"
        );
    }
}

#[tokio::test]
async fn requested_profile_and_override_cannot_skip_runtime_custom_tool_validation() {
    // E2: invalid Runtime custom name + valid Profile + explicit valid override
    // => original Managed capability validation rejects BEFORE Profile overlay,
    // Session root insertion or Runtime effects. The paired Profiled request
    // uses its original infallible neutral fold and accepts the exact override;
    // it must not acquire Managed-only validation as part of consolidation.
    let runtime = Arc::new(RehydrateFake::default());
    runtime.custom_tools.lock().unwrap().push(CustomTool {
        name: "bad.name".into(),
        description: "invalid Runtime name".into(),
        input_schema: json!({"type":"object"}),
    });
    let sessions = support::session_repository("validation Store").await;
    let clock = awaken_time::system_clock();
    let state = Arc::new(
        managed_state_from_parts(
            runtime.clone(),
            None,
            sessions.clone(),
            environment_fixture(clock.clone()).execution,
            clock,
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .with_config_source(Arc::new(AgentWithClientTool)),
    );
    let error = call(&router(state.clone()), "POST", "/v1/sessions", Some(json!({
        "agent":{"type":"agent_with_overrides", "id":"client-tool-agent", "tools":[{
            "type":"custom", "name":"valid_override", "description":"valid", "input_schema":{"type":"object"},
        }]},
    }))).await.expect_for(StatusCode::BAD_REQUEST, "E2");
    assert!(
        error
            .to_string()
            .contains("custom tool name `bad.name` has invalid characters"),
        "E2: {error}"
    );
    assert!(
        sessions
            .scan(awaken_session_contract::SessionScanQuery::Workspace {
                workspace_id: "default",
            })
            .await
            .unwrap()
            .into_sessions()
            .is_empty(),
        "E2: no durable root"
    );
    assert!(
        runtime.order.lock().unwrap().is_empty(),
        "E2: no Runtime effect"
    );
    let expected = SessionToolConfiguration {
        toolsets: Vec::new(),
        client_tools: vec![ClientToolDescriptor {
            name: "valid_override".into(),
            description: "valid".into(),
            input_schema: json!({"type":"object"}),
        }],
    };
    call(&profiled_router(state, "default"), "POST", "/v1/awaken/sessions", Some(json!({
        "session_id":"profiled-neutral-tool-validation", "mode":"work_unit", "agent_id":"client-tool-agent",
        "environment_id":awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        "tools":expected,
    }))).await.expect_for(StatusCode::ACCEPTED, "E2 paired neutral boundary");
    assert_eq!(
        sessions
            .get(&SessionId::from("profiled-neutral-tool-validation"))
            .await
            .unwrap()
            .tools,
        expected,
        "E2: neutral explicit replacement excludes the invalid inherited client"
    );
}

#[tokio::test]
async fn profiled_tool_compiler_rejection_preserves_repository_compensation_order() {
    // E4 graph: explicit neutral Web override is invalid; a valid Repository
    // participant is configured by the ORIGINAL Profiled preflight/finalizer;
    // the ORIGINAL late Web compiler then rejects => exact first error, no
    // Session root/Runtime install, and that real Registry participant Deleted.
    // The existing AcceptingFake supplies the pure Repository-layout preflight;
    // its prepared/applied/staged records must all stay empty on this rejection.
    // This is deliberately not Requested compiler-failure coverage: Requested
    // still compiles earlier; its unchanged tail/order and the existing direct
    // create/accept compiler-before-effects tests cover that separate boundary.
    let runtime = Arc::new(AcceptingFake::default());
    let sessions = support::session_repository("compiler Store").await;
    let catalog = resource_registry().await;
    let clock = awaken_time::system_clock();
    let state = Arc::new(
        managed_state_from_parts(
            runtime.clone(),
            None,
            sessions.clone(),
            environment_fixture(clock.clone()).execution,
            clock,
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .with_config_source(Arc::new(AgentWithClientTool))
        .with_resource_registry(catalog.clone()),
    );
    let invalid = SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides: vec![
                awaken_agent_contract::ToolPolicyOverride::with_optional_configuration(
                    "web_fetch",
                    ToolExecutionPolicy::default(),
                    Some(json!({"type":"web_fetch","domains":{"type":"allow","domains":[]}})),
                ),
            ],
        }],
        client_tools: Vec::new(),
    };
    let id = "profiled-late-tool-compiler";
    let error = call(&profiled_router(state, "default"), "POST", "/v1/awaken/sessions", Some(json!({
        "session_id":id, "mode":"work_unit", "agent_id":"client-tool-agent",
        "environment_id":awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        "repositories":[{"remote_url":"https://github.com/awaken/tools.git","mount_path":"/workspace/tools"}],
        "tools":invalid,
    }))).await.expect_for(StatusCode::BAD_REQUEST, "E4");
    assert!(
        error.to_string().contains("invalid_web_domain_count"),
        "E4: first compiler error: {error}"
    );
    assert!(
        matches!(
            sessions.get(&SessionId::from(id)).await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "E4: exact absence, never a storage failure"
    );
    assert!(
        runtime.prepared.lock().unwrap().is_empty()
            && runtime.applied.lock().unwrap().is_empty()
            && runtime.staged.lock().unwrap().is_empty(),
        "E4: no Runtime realization"
    );
    let participant = catalog
        .find_repository("default", &format!("profiled:{id}:repository:0"))
        .await
        .unwrap()
        .expect("E4: original preparation reached the real Registry");
    assert_eq!(
        participant.state,
        ResourceState::Deleted,
        "E4: original compensation retired its participant"
    );
}
