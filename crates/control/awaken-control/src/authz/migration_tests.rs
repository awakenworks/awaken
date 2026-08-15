use super::*;

#[test]
fn embedded_upgrade_consolidates_legacy_companion_bindings_once() {
    // Cause-effect graph: legacy split-profile bindings sharing one principal,
    // local intent and scope converge to one canonical Workspace binding before
    // old rows are removed; replay is idempotent.
    //
    // | legacy role(s) | canonical result |
    // | both `admin` / both `agent_publisher` | one `admin` / `publisher` |
    // | `hosted_workspace_admin` | one exact-authority compatibility role |
    // | absent after migration | unchanged on replay |
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite_migrated_store(
        SqliteBackend::open_path(dir.path().join("iam.sqlite")).unwrap(),
        "iam",
    )
    .unwrap();
    let principal = PrincipalRef::Service {
        service_id: "legacy-local-admin".to_owned(),
    };
    let scope = ScopeRef::Workspace {
        workspace_id: WorkspaceId("workspace-local".to_owned()),
    };
    for legacy_role in [
        "awaken.runtime.management:admin",
        "awaken.runtime.resources:admin",
        "awaken.runtime.management:agent_publisher",
        "awaken.runtime.resources:agent_publisher",
        "awaken.runtime.management:hosted_workspace_admin",
    ] {
        RoleBindingRepo::add(
            &store,
            RoleBinding {
                principal: principal.clone(),
                role: RoleId(legacy_role.to_owned()),
                scope: scope.clone(),
            },
        )
        .unwrap();
    }

    assert!(migrate_legacy_workspace_bindings(&store));
    assert!(migrate_legacy_workspace_bindings(&store));
    let bindings = RoleBindingRepo::list_for_principal(&store, &principal).unwrap();
    assert_eq!(bindings.len(), 3);
    for expected in ["admin", "legacy_hosted_admin", "publisher"] {
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.role == qualify_role(expected))
                .count(),
            1
        );
    }
    assert!(bindings.iter().all(|binding| binding.scope == scope));
}

#[test]
fn embedded_upgrade_retains_incomplete_or_unknown_legacy_authority() {
    // A lone half of a formerly split role is not equivalent to the unified
    // role. Keeping the row and legacy profiles active preserves authority and
    // prevents a silent privilege increase. Unknown custom roles fail closed
    // through the same path. Replaying the migration changes nothing.
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite_migrated_store(
        SqliteBackend::open_path(dir.path().join("iam.sqlite")).unwrap(),
        "iam",
    )
    .unwrap();
    let principal = PrincipalRef::Service {
        service_id: "incomplete-legacy-admin".to_owned(),
    };
    let scope = ScopeRef::Workspace {
        workspace_id: WorkspaceId("workspace-local".to_owned()),
    };
    for role in [
        "awaken.runtime.management:admin",
        "awaken.runtime.resources:custom_operator",
    ] {
        RoleBindingRepo::add(
            &store,
            RoleBinding {
                principal: principal.clone(),
                role: RoleId(role.to_owned()),
                scope: scope.clone(),
            },
        )
        .unwrap();
    }

    assert!(!migrate_legacy_workspace_bindings(&store));
    let first = RoleBindingRepo::list_for_principal(&store, &principal).unwrap();
    assert!(!migrate_legacy_workspace_bindings(&store));
    assert_eq!(
        RoleBindingRepo::list_for_principal(&store, &principal).unwrap(),
        first
    );
    assert_eq!(first.len(), 2);
    assert!(first.iter().all(|binding| binding.scope == scope));
}
