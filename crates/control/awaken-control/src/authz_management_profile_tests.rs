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

#[test]
fn built_in_profile_reconciles_changed_contract_once_and_hydrates_replays() {
    use awaken_iam_server::InMemoryStore;

    /* Built-in profile recovery decision table:
     * R1 no active revision + canonical request -> create, validate, activate V1;
     * R2 equal active document + canonical request -> hydrate V1, append nothing;
     * R3 stale active document + canonical request -> append and CAS-activate V2;
     * R4 equal V2 restart -> hydrate V2, append nothing.
     * Effects: the durable PAP and live PDP select the same current document;
     * immutable older revisions remain audit history rather than runtime policy.
     */
    let profiles = AuthorizationProfileAdmin::new(Arc::new(InMemoryStore::new()));
    let mut engine = AuthzApi::new();
    let current = management_authorization_profile();
    let mut legacy = current.clone();
    legacy
        .document
        .resource_model
        .actions
        .retain(|action| !action.0.contains("model_supply"));
    legacy
        .document
        .action_scope_rules
        .retain(|rule| !rule.action_pattern.contains("model_supply"));
    legacy
        .document
        .grants
        .retain(|grant| !grant.action_pattern.contains("model_supply"));

    reconcile_builtin_profile(&profiles, &mut engine, legacy.clone());
    assert_eq!(
        profiles
            .active(&legacy.namespace)
            .unwrap()
            .unwrap()
            .revision,
        1,
        "R1"
    );
    reconcile_builtin_profile(&profiles, &mut engine, legacy.clone());
    assert_eq!(profiles.list(&legacy.namespace).unwrap().len(), 1, "R2");

    reconcile_builtin_profile(&profiles, &mut engine, current.clone());
    let active = profiles.active(&current.namespace).unwrap().unwrap();
    assert_eq!(active.revision, 2, "R3");
    assert_eq!(active.document, current.document);
    assert_eq!(
        engine
            .snapshot()
            .active_profiles
            .iter()
            .find(|profile| profile.namespace == current.namespace)
            .expect("live PDP contains the reconciled profile")
            .revision,
        2
    );

    reconcile_builtin_profile(&profiles, &mut engine, current.clone());
    assert_eq!(profiles.list(&current.namespace).unwrap().len(), 2, "R4");
    assert_eq!(
        profiles
            .active(&current.namespace)
            .unwrap()
            .unwrap()
            .revision,
        2
    );
}
