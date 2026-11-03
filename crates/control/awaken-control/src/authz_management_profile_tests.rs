// @formal entrypoint authz.credential_ingress_role_is_exact_workspace_apikey_and_provider_connect_only
// @formal entrypoint authz.hosted_workspace_and_runtime_member_grants_are_exact
use super::*;

#[test]
fn workspace_profile_is_one_deterministic_workspace_scoped_contract() {
    // Cause/effect decision table:
    // | Cause | Effect |
    // | repeated construction | byte-identical profile |
    // | each product-owned family | original registered Workspace-scoped pattern |
    // | exact existing apikey.write | registered Org + Workspace scope |
    // | any other action | Workspace only; declaring a scope grants no role |
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

    let expected_scopes = [
        ("awaken.workspace::workspace.*", vec![ScopeKind::Workspace]),
        ("awaken.workspace::apikey.*", vec![ScopeKind::Workspace]),
        (
            "awaken.workspace::model_supply.*",
            vec![ScopeKind::Workspace],
        ),
        ("awaken.workspace::file.*", vec![ScopeKind::Workspace]),
        ("awaken.workspace::skill.*", vec![ScopeKind::Workspace]),
        (
            "awaken.workspace::tunnel.manage",
            vec![ScopeKind::Workspace],
        ),
        (
            "awaken.workspace::apikey.write",
            vec![ScopeKind::Org, ScopeKind::Workspace],
        ),
    ];

    let actions = first
        .document
        .resource_model
        .actions
        .iter()
        .map(|action| action.0.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        expected_scopes
            .iter()
            .map(|(action, _)| *action)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        first
            .document
            .action_scope_rules
            .iter()
            .map(|rule| (
                rule.action_pattern.as_str(),
                rule.allowed_scope_kinds.clone()
            ))
            .collect::<Vec<_>>(),
        expected_scopes
    );
    assert!(!first.document.grants.is_empty());
    // Multiple authorities per role must compile to distinct durable grant IDs;
    // repeated construction above proves those IDs are also deterministic.
    assert_eq!(
        first
            .document
            .grants
            .iter()
            .map(|grant| &grant.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        first.document.grants.len()
    );
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
        [
            "awaken.workspace::apikey.*",
            "awaken.workspace::model_supply.connect"
        ],
        "credential ingress admits exact connection, not general model-supply administration"
    );

    let member_grants = first
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id == AWAKEN_WORKSPACE_USER_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        member_grants,
        [
            "awaken.workspace::file.read",
            "awaken.workspace::skill.read",
            "awaken.workspace::workspace.read",
            "awaken.workspace::model_supply.read",
        ],
        "the Workspace member is read-only across every hosted product family"
    );
    let builder_grants = first
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id == AWAKEN_WORKSPACE_HOSTED_BUILDER_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        builder_grants,
        [
            "awaken.workspace::workspace.*",
            "awaken.workspace::model_supply.read",
            "awaken.workspace::file.*",
            "awaken.workspace::skill.*",
        ],
        "Builder writes product content but cannot administer credentials or model supply"
    );
    assert!(
        builder_grants
            .iter()
            .all(|grant| !grant.contains("apikey") && !grant.ends_with("model_supply.*"))
    );
    let tunnel_manager_grants = first
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                GrantSubjectRef::Role { role_id }
                    if role_id == AWAKEN_WORKSPACE_TUNNEL_MANAGER_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        tunnel_manager_grants,
        ["awaken.workspace::tunnel.manage"],
        "WIF Tunnel authority must not inherit broader Workspace actions"
    );
    assert!(
        member_grants
            .iter()
            .all(|grant| !grant.contains("tunnel.manage")),
        "ordinary read-only members do not carry workspace:manage_tunnels"
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
    // C1 retains the original missing-model-supply case. C2 represents the
    // previous profile with its original six registrations and grants, but no
    // exact Org credential-write declaration. Both take R1-R4 through the same
    // PAP/CAS reconciliation; equal restarts never append another revision.
    for missing in ["model_supply", "awaken.workspace::apikey.write"] {
        let profiles = AuthorizationProfileAdmin::new(Arc::new(InMemoryStore::new()));
        let mut engine = AuthzApi::new();
        let current = workspace_authorization_profile();
        let mut legacy = current.clone();
        legacy
            .document
            .resource_model
            .actions
            .retain(|action| !action.0.contains(missing));
        legacy
            .document
            .action_scope_rules
            .retain(|rule| !rule.action_pattern.contains(missing));
        if missing == "model_supply" {
            legacy
                .document
                .grants
                .retain(|grant| !grant.action_pattern.contains(missing));
        } else {
            assert_eq!(legacy.document.grants, current.document.grants);
        }

        reconcile_builtin_profile(&profiles, &mut engine, legacy.clone()).unwrap();
        assert_eq!(
            profiles
                .active(&legacy.namespace)
                .unwrap()
                .unwrap()
                .revision,
            1,
            "R1"
        );
        reconcile_builtin_profile(&profiles, &mut engine, legacy.clone()).unwrap();
        assert_eq!(profiles.list(&legacy.namespace).unwrap().len(), 1, "R2");

        reconcile_builtin_profile(&profiles, &mut engine, current.clone()).unwrap();
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

        reconcile_builtin_profile(&profiles, &mut engine, current.clone()).unwrap();
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

    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile()).unwrap();
    for namespace in [
        LEGACY_MANAGEMENT_POLICY_NAMESPACE,
        LEGACY_RESOURCE_POLICY_NAMESPACE,
    ] {
        retire_legacy_profile(&profiles, &mut engine, namespace).unwrap();
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
