use super::*;

#[test]
fn management_profile_is_one_deterministic_workspace_scoped_contract() {
    // Cause/effect decision table: repeated construction -> byte-identical
    // profile; each product-owned family (workspace, apikey, model_supply) ->
    // one registered Workspace-scoped action pattern; deployment input cannot
    // change the namespace, vocabulary, scope, or grants.
    let first = management_authorization_profile();
    let second = management_authorization_profile();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert_eq!(first.namespace.0, MANAGEMENT_POLICY_NAMESPACE);
    assert_eq!(first.created_at.0, AUTHORIZATION_PROFILE_EPOCH);

    let actions = first
        .document
        .resource_model
        .actions
        .iter()
        .map(|action| action.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            "awaken.runtime.management::workspace.*",
            "awaken.runtime.management::apikey.*",
            "awaken.runtime.management::model_supply.*",
        ]
    );
    assert!(
        first
            .document
            .action_scope_rules
            .iter()
            .all(|rule| rule.allowed_scope_kinds == [ScopeKind::Workspace])
    );
    assert!(!first.document.grants.is_empty());
    assert!(first.document.grants.iter().all(|grant| {
        grant
            .action_pattern
            .starts_with("awaken.runtime.management::")
            && matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id.starts_with("awaken.runtime.management:")
            )
    }));
    let publisher_grants = first
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id == MANAGEMENT_AGENT_PUBLISHER_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        publisher_grants,
        ["awaken.runtime.management::workspace.*"],
        "the cross-product publisher may author configuration but must not receive apikey.*"
    );
}
