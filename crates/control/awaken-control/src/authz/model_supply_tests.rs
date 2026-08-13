use super::*;

#[test]
fn agent_publisher_can_discover_but_cannot_administer_model_supply() {
    // Cause/effect graph: active canonical profile (C1) + exact publisher role
    // binding at one Workspace (C2) + requested action/namespace (C3) -> allow
    // Agent authoring, model discovery, and exact Skill materialization (E1),
    // while File, credential, and model-supply administration remain
    // default-deny (E2). A different Workspace remains denied (E3).
    //
    // Decision table:
    // | Action | Bound Workspace | Result |
    // | workspace.write | yes | allow |
    // | model_supply.read | yes | allow |
    // | apikey.read | yes | deny |
    // | model_supply.connect/write | yes | deny |
    // | resource skill.read/write | yes | allow |
    // | resource file.read/write | yes | deny |
    // | model_supply.read | different Workspace | deny |
    let profiles =
        AuthorizationProfileAdmin::new(Arc::new(awaken_iam_server::InMemoryStore::new()));
    let mut engine = AuthzApi::new();
    reconcile_builtin_profile(&profiles, &mut engine, management_authorization_profile());
    reconcile_builtin_profile(
        &profiles,
        &mut engine,
        management_resource_authorization_profile(),
    );

    let principal = PrincipalRef::Service {
        service_id: "flow-publisher".to_owned(),
    };
    let workspace = WorkspaceId("workspace-flow".to_owned());
    engine.policy_mut().bind_role(RoleBinding {
        principal: principal.clone(),
        role: RoleId(MANAGEMENT_AGENT_PUBLISHER_ROLE.to_owned()),
        scope: ScopeRef::Workspace {
            workspace_id: workspace.clone(),
        },
    });
    engine.policy_mut().bind_role(RoleBinding {
        principal: principal.clone(),
        role: RoleId(RESOURCE_AGENT_PUBLISHER_ROLE.to_owned()),
        scope: ScopeRef::Workspace {
            workspace_id: workspace.clone(),
        },
    });

    let decide = |engine: &AuthzApi, action: &str, workspace_id: WorkspaceId| {
        engine
            .authorize(&AuthorizationRequest::direct(
                principal.clone(),
                qualify_action(action),
                ScopeRef::Workspace { workspace_id },
            ))
            .decision
    };
    for action in ["workspace.write", "model_supply.read"] {
        assert_eq!(
            decide(&engine, action, workspace.clone()),
            AuthorizationDecision::Allow,
            "{action}"
        );
    }
    for action in ["apikey.read", "model_supply.connect", "model_supply.write"] {
        assert_eq!(
            decide(&engine, action, workspace.clone()),
            AuthorizationDecision::Deny,
            "{action}"
        );
    }
    assert_eq!(
        decide(
            &engine,
            "model_supply.read",
            WorkspaceId("workspace-other".to_owned())
        ),
        AuthorizationDecision::Deny
    );

    let decide_resource = |engine: &AuthzApi, action: &str, workspace_id: WorkspaceId| {
        engine
            .authorize(&AuthorizationRequest::direct(
                principal.clone(),
                qualify_resource_action(action),
                ScopeRef::Workspace { workspace_id },
            ))
            .decision
    };
    for action in ["skill.read", "skill.write"] {
        assert_eq!(
            decide_resource(&engine, action, workspace.clone()),
            AuthorizationDecision::Allow,
            "{action}"
        );
    }
    for action in ["file.read", "file.write"] {
        assert_eq!(
            decide_resource(&engine, action, workspace.clone()),
            AuthorizationDecision::Deny,
            "{action}"
        );
    }
    assert_eq!(
        decide_resource(
            &engine,
            "skill.read",
            WorkspaceId("workspace-other".to_owned())
        ),
        AuthorizationDecision::Deny
    );
}

#[test]
fn hosted_workspace_role_can_read_but_cannot_author_model_supply() {
    // Cause-effect graph: one product-owned profile (C1) + hosted Workspace
    // role (C2) -> ordinary Workspace/API-key administration and model read
    // (E1), with no connect/write model-supply grant (E2).
    //
    // Decision table:
    // | Role | workspace | apikey | model read | model connect/write |
    // | hosted_workspace_admin | allow | allow | allow | deny/absent |
    // | local workspace_admin | allow | allow | allow | allow |
    let profile = management_authorization_profile();
    let hosted_grants = profile
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                awaken_iam_contract::GrantSubjectRef::Role { role_id }
                    if role_id == MANAGEMENT_HOSTED_WORKSPACE_ADMIN_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        hosted_grants,
        [
            "awaken.runtime.management::workspace.*",
            "awaken.runtime.management::apikey.*",
            "awaken.runtime.management::model_supply.read",
        ]
    );
    assert!(
        hosted_grants
            .iter()
            .all(|grant| !grant.ends_with("model_supply.*"))
    );
}
