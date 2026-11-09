// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: A hosted execution context does not prove a human management request;
// @formal reason: the regression checks refusal before catalog, definition or audit effects.
use super::*;

#[tokio::test]
async fn a_hosted_execution_scope_cannot_reveal_stored_definitions_or_catalog_secrets() {
    let plane = ConfigPlane::new(
        Arc::new(test_config_service()),
        Arc::new(SqliteConfigStore::open_in_memory().await.unwrap()),
        Arc::new(StaticToolCatalog(Vec::new())),
    );
    let scope = ScopeId::from("private-definitions");
    let hidden = AgentConfig {
        id: "hidden-agent".into(),
        instructions: "PRIVATE SYSTEM INSTRUCTIONS".into(),
        ..Default::default()
    };
    plane.put(&scope, &hidden).await.unwrap();
    let resources = Arc::new(InMemoryAgentInputBindingRepository::new());
    let store = ConfigServiceDraftStore::request_scoped(plane.clone(), resources.clone());
    let validator = ConfigServiceDraftValidator::request_scoped(plane.clone());
    let reader =
        CatalogCapabilityReader::request_scoped(repo("m-1").await, &[], &[], plane.clone(), None);
    let context = awaken_tool_contract::tool::ToolOperationContext {
        run_id: Some(awaken_agent_contract::agent::run::Id("run".into())),
        thread_id: None,
        operation_id: "read-definitions".into(),
        call_id: None,
        execution_scope: Some(awaken_agent_contract::ExecutionScopeRef(scope.clone())),
    };
    awaken_tool_contract::tool::with_tool_operation_context(context, async {
        assert!(store.get_versioned(&hidden.id).await.is_err());
        assert!(validator.validate(&hidden).await.is_err());
        assert!(reader.available().is_err());
        assert!(reader.capabilities().await.agents.is_empty());
    })
    .await;
    // A fixed native operator's separate local contract remains usable.
    let operator = ConfigServiceDraftStore::new(plane, scope, resources);
    let stored = operator.get_versioned(&hidden.id).await.unwrap().unwrap();
    assert_eq!(stored.config.instructions, hidden.instructions);
}
