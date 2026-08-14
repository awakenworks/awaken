use super::*;

#[test]
fn hosted_member_bundle_reads_workspace_resources_and_runs_only_at_its_workspace() {
    // Cause/effect graph: active Workspace+Runtime profiles and the two
    // namespace-confined Member bindings at one exact Workspace -> read
    // Workspace/File/Skill/Run; mutations and credentials stay denied; a
    // sibling Workspace stays denied. This is the internal permission bundle
    // Cloud replaces atomically for the one user-facing Member level.
    //
    // | action | exact Workspace | sibling Workspace |
    // | read workspace/file/skill/run | allow | deny |
    // | write workspace/file/skill/run or apikey.read | deny | deny |
    let profiles =
        AuthorizationProfileAdmin::new(Arc::new(awaken_iam_server::InMemoryStore::new()));
    let mut engine = AuthzApi::new();
    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile());
    reconcile_builtin_profile(
        &profiles,
        &mut engine,
        hosted_runtime_authorization_profile(),
    );
    let principal = PrincipalRef::Account {
        account_id: awaken_iam_contract::AccountId("member".into()),
    };
    let workspace = WorkspaceId("workspace-a".into());
    for role in [
        AWAKEN_WORKSPACE_USER_ROLE,
        HOSTED_RUNTIME_WORKSPACE_USER_ROLE,
    ] {
        engine.policy_mut().bind_role(RoleBinding {
            principal: principal.clone(),
            role: RoleId(role.into()),
            scope: ScopeRef::Workspace {
                workspace_id: workspace.clone(),
            },
        });
    }
    let decide = |action, target: &str| {
        engine
            .authorize(&AuthorizationRequest::direct(
                principal.clone(),
                action,
                ScopeRef::Workspace {
                    workspace_id: WorkspaceId(target.into()),
                },
            ))
            .decision
    };
    for action in ["workspace.read", "file.read", "skill.read"] {
        assert_eq!(
            decide(qualify_action(action), "workspace-a"),
            AuthorizationDecision::Allow
        );
        assert_eq!(
            decide(qualify_action(action), "workspace-b"),
            AuthorizationDecision::Deny
        );
    }
    assert_eq!(
        decide(qualify_hosted_runtime_action(RUN_READ), "workspace-a"),
        AuthorizationDecision::Allow
    );
    for action in [
        "workspace.write",
        "file.write",
        "skill.write",
        "apikey.read",
    ] {
        assert_eq!(
            decide(qualify_action(action), "workspace-a"),
            AuthorizationDecision::Deny
        );
    }
    assert_eq!(
        decide(qualify_hosted_runtime_action(RUN_CREATE), "workspace-a"),
        AuthorizationDecision::Deny
    );
}

#[test]
fn agent_executor_is_exact_workspace_and_hosted_run_only() {
    // Cause/effect graph: active canonical profiles (C1) + the existing
    // agent-executor role bound at one Workspace (C2) + action namespace and
    // target Workspace (C3) -> Hosted Session read/create authority at the
    // exact Workspace (E1); foreign Workspace and Management/Resource actions
    // remain default-deny (E2).
    //
    // Decision table:
    // | Namespace/action | Bound Workspace | Result |
    // | hosted run.read/run.create | yes | allow |
    // | hosted run.read/run.create | no | deny |
    // | management apikey/model/workspace | yes | deny |
    // | resource file/skill | yes | deny |
    let profiles =
        AuthorizationProfileAdmin::new(Arc::new(awaken_iam_server::InMemoryStore::new()));
    let mut engine = AuthzApi::new();
    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile());
    reconcile_builtin_profile(
        &profiles,
        &mut engine,
        hosted_runtime_authorization_profile(),
    );

    let principal = PrincipalRef::Service {
        service_id: "flow-agent-executor".to_owned(),
    };
    let workspace = WorkspaceId("workspace-flow".to_owned());
    engine.policy_mut().bind_role(RoleBinding {
        principal: principal.clone(),
        role: RoleId(HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE.to_owned()),
        scope: ScopeRef::Workspace {
            workspace_id: workspace.clone(),
        },
    });

    let decide = |action, workspace_id| {
        engine
            .authorize(&AuthorizationRequest::direct(
                principal.clone(),
                qualify_hosted_runtime_action(action),
                ScopeRef::Workspace { workspace_id },
            ))
            .decision
    };
    for action in [RUN_READ, RUN_CREATE] {
        assert_eq!(
            decide(action, workspace.clone()),
            AuthorizationDecision::Allow,
            "{action} exact Workspace"
        );
        assert_eq!(
            decide(action, WorkspaceId("workspace-other".to_owned())),
            AuthorizationDecision::Deny,
            "{action} foreign Workspace"
        );
    }

    for action in [
        "workspace.read",
        "workspace.write",
        "apikey.read",
        "apikey.write",
        "model_supply.read",
        "model_supply.write",
    ] {
        assert_eq!(
            engine
                .authorize(&AuthorizationRequest::direct(
                    principal.clone(),
                    qualify_action(action),
                    ScopeRef::Workspace {
                        workspace_id: workspace.clone(),
                    },
                ))
                .decision,
            AuthorizationDecision::Deny,
            "management {action}"
        );
    }
    for action in ["file.read", "file.write", "skill.read", "skill.write"] {
        assert_eq!(
            engine
                .authorize(&AuthorizationRequest::direct(
                    principal.clone(),
                    qualify_action(action),
                    ScopeRef::Workspace {
                        workspace_id: workspace.clone(),
                    },
                ))
                .decision,
            AuthorizationDecision::Deny,
            "resource {action}"
        );
    }
}

#[test]
fn workspace_publisher_can_discover_but_cannot_administer_model_supply() {
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
    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile());

    let principal = PrincipalRef::Service {
        service_id: "flow-publisher".to_owned(),
    };
    let workspace = WorkspaceId("workspace-flow".to_owned());
    engine.policy_mut().bind_role(RoleBinding {
        principal: principal.clone(),
        role: RoleId(AWAKEN_WORKSPACE_PUBLISHER_ROLE.to_owned()),
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
                qualify_action(action),
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
    // | hosted_admin | allow | allow | allow | deny/absent |
    // | local workspace_admin | allow | allow | allow | allow |
    let profile = workspace_authorization_profile();
    let hosted_grants = profile
        .document
        .grants
        .iter()
        .filter(|grant| {
            matches!(
                &grant.subject,
                awaken_iam_contract::GrantSubjectRef::Role { role_id }
                    if role_id == AWAKEN_WORKSPACE_HOSTED_ADMIN_ROLE
            )
        })
        .map(|grant| grant.action_pattern.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        hosted_grants,
        [
            "awaken.workspace::workspace.*",
            "awaken.workspace::apikey.*",
            "awaken.workspace::model_supply.read",
            "awaken.workspace::file.*",
            "awaken.workspace::skill.*",
        ]
    );
    assert!(
        hosted_grants
            .iter()
            .all(|grant| !grant.ends_with("model_supply.*"))
    );
}

#[test]
fn credential_ingress_is_exact_workspace_and_apikey_only() {
    // Cause/effect graph: canonical Workspace profile (C1) + independent
    // credential-ingress binding (C2) + action and target Workspace (C3)
    // -> credential reads/writes at the bound Workspace (E1), while foreign
    // Workspace access and every non-credential family remain denied (E2).
    //
    // Decision table:
    // | Action | Bound Workspace | Result |
    // | apikey.read/write | yes | allow |
    // | apikey.read/write | no | deny |
    // | workspace/model_supply action | yes | deny |
    let profiles =
        AuthorizationProfileAdmin::new(Arc::new(awaken_iam_server::InMemoryStore::new()));
    let mut engine = AuthzApi::new();
    reconcile_builtin_profile(&profiles, &mut engine, workspace_authorization_profile());
    reconcile_builtin_profile(
        &profiles,
        &mut engine,
        hosted_runtime_authorization_profile(),
    );

    let principal = PrincipalRef::Service {
        service_id: "flow-credential-ingress".to_owned(),
    };
    let workspace = WorkspaceId("workspace-flow".to_owned());
    engine.policy_mut().bind_role(RoleBinding {
        principal: principal.clone(),
        role: RoleId(AWAKEN_WORKSPACE_CREDENTIAL_INGRESS_ROLE.to_owned()),
        scope: ScopeRef::Workspace {
            workspace_id: workspace.clone(),
        },
    });

    let decide = |action: &str, workspace_id: WorkspaceId| {
        engine
            .authorize(&AuthorizationRequest::direct(
                principal.clone(),
                qualify_action(action),
                ScopeRef::Workspace { workspace_id },
            ))
            .decision
    };
    for action in ["apikey.read", "apikey.write"] {
        assert_eq!(
            decide(action, workspace.clone()),
            AuthorizationDecision::Allow,
            "{action}"
        );
        assert_eq!(
            decide(action, WorkspaceId("workspace-other".to_owned())),
            AuthorizationDecision::Deny,
            "{action} foreign Workspace"
        );
    }
    for action in [
        "workspace.read",
        "workspace.write",
        "model_supply.read",
        "model_supply.connect",
        "model_supply.write",
    ] {
        assert_eq!(
            decide(action, workspace.clone()),
            AuthorizationDecision::Deny,
            "{action}"
        );
    }
    for action in ["file.read", "skill.write"] {
        assert_eq!(
            engine
                .authorize(&AuthorizationRequest::direct(
                    principal.clone(),
                    qualify_action(action),
                    ScopeRef::Workspace {
                        workspace_id: workspace.clone(),
                    },
                ))
                .decision,
            AuthorizationDecision::Deny,
            "{action}"
        );
    }
    assert_eq!(
        engine
            .authorize(&AuthorizationRequest::direct(
                principal,
                awaken_iam_contract::ActionKey::in_namespace(
                    &awaken_iam_contract::NamespaceId(HOSTED_RUNTIME_POLICY_NAMESPACE.to_owned(),),
                    "run.create",
                ),
                ScopeRef::Workspace {
                    workspace_id: workspace,
                },
            ))
            .decision,
        AuthorizationDecision::Deny,
        "run.create"
    );
}
