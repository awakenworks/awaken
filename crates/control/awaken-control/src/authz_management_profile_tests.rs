use super::*;

#[test]
fn workspace_profile_is_one_deterministic_workspace_scoped_contract() {
    // Cause/effect decision table:
    // | Cause | Effect |
    // | repeated construction | byte-identical profile |
    // | each product-owned family | one registered Workspace-scoped pattern |
    // | agent publisher authors configuration | allow workspace.* |
    // | agent publisher discovers executable model supply | allow exact model_supply.read |
    // | agent publisher accesses credentials or mutates supply | no matching grant |
    // Deployment input cannot change the namespace, vocabulary, scope, or grants.
    let first = workspace_authorization_profile();
    let second = workspace_authorization_profile();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert_eq!(first.namespace.0, AWAKEN_WORKSPACE_POLICY_NAMESPACE);
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
            "awaken.workspace::workspace.*",
            "awaken.workspace::apikey.*",
            "awaken.workspace::model_supply.*",
            "awaken.workspace::file.*",
            "awaken.workspace::skill.*",
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
        grant.action_pattern.starts_with("awaken.workspace::")
            && matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id.starts_with("awaken.workspace:")
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
                    if role_id == AWAKEN_WORKSPACE_PUBLISHER_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        publisher_grants,
        [
            "awaken.workspace::workspace.*",
            "awaken.workspace::model_supply.read",
            "awaken.workspace::skill.*",
        ],
        "the publisher may discover models and author configuration, without credential or model-supply administration"
    );
    assert!(publisher_grants.iter().all(|grant| {
        !grant.contains("apikey")
            && !grant.ends_with("model_supply.*")
            && !grant.ends_with("model_supply.connect")
            && !grant.ends_with("model_supply.write")
    }));

    let credential_ingress_grants = first
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id == AWAKEN_WORKSPACE_CREDENTIAL_INGRESS_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        credential_ingress_grants,
        ["awaken.workspace::apikey.*"],
        "credential ingress must not inherit any non-credential authority"
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
    let current = workspace_authorization_profile();
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

#[test]
fn workspace_cutover_retires_both_legacy_active_heads_without_deleting_history() {
    use awaken_iam_contract::{AuthorizationProfileDocument, NamespaceId, ProfileLifecycle};
    use awaken_iam_server::InMemoryStore;

    // Cause-effect graph: either legacy namespace may have an active immutable
    // revision; once the canonical Workspace profile is active, exact CAS
    // retirement removes both heads from PDP composition while preserving each
    // revision as retired evidence. An absent legacy head is a no-op.
    //
    // | management head | resources head | terminal active profiles |
    // | active | active | awaken.workspace only |
    // | absent | active | awaken.workspace only |
    let profiles = AuthorizationProfileAdmin::new(Arc::new(InMemoryStore::new()));
    let mut engine = AuthzApi::new();
    for namespace in [
        LEGACY_MANAGEMENT_POLICY_NAMESPACE,
        LEGACY_RESOURCE_POLICY_NAMESPACE,
    ] {
        let draft = profiles
            .create_draft(CreateAuthorizationProfile {
                namespace: NamespaceId(namespace.to_owned()),
                document: AuthorizationProfileDocument::default(),
                created_at: awaken_iam_contract::Timestamp(AUTHORIZATION_PROFILE_EPOCH.to_owned()),
            })
            .unwrap();
        assert!(
            profiles
                .validate(&draft.namespace, draft.revision)
                .unwrap()
                .valid
        );
        profiles
            .activate(
                &mut engine,
                &PolicySnapshot::default(),
                &draft.namespace,
                draft.revision,
                ActivateAuthorizationProfile {
                    expected_active_revision: None,
                },
            )
            .unwrap();
    }

    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile());
    for namespace in [
        LEGACY_MANAGEMENT_POLICY_NAMESPACE,
        LEGACY_RESOURCE_POLICY_NAMESPACE,
    ] {
        retire_legacy_profile(&profiles, &mut engine, namespace);
        let namespace = NamespaceId(namespace.to_owned());
        assert!(profiles.active(&namespace).unwrap().is_none());
        assert_eq!(
            profiles.get(&namespace, 1).unwrap().unwrap().lifecycle,
            ProfileLifecycle::Retired
        );
    }
    assert_eq!(
        engine
            .snapshot()
            .active_profiles
            .iter()
            .map(|profile| profile.namespace.0.as_str())
            .collect::<Vec<_>>(),
        [AWAKEN_WORKSPACE_POLICY_NAMESPACE]
    );
}
