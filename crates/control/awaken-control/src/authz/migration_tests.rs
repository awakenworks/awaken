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

    migrate_legacy_workspace_bindings(&store).unwrap();
    migrate_legacy_workspace_bindings(&store).unwrap();
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
fn embedded_upgrade_rejects_incomplete_legacy_authority_without_writes() {
    // Cause/effect graph: C1 a lone half of a formerly split role is not
    // equivalent to the unified role. E1 reject the release migration before
    // hydration; E2 preserve every old row without a partial canonical write.
    //
    // Decision table:
    // | complete authority | known role | effect |
    // | false              | true       | E1+E2  |
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
    RoleBindingRepo::add(
        &store,
        RoleBinding {
            principal: principal.clone(),
            role: RoleId("awaken.runtime.management:admin".to_owned()),
            scope: scope.clone(),
        },
    )
    .unwrap();

    let error = migrate_legacy_workspace_bindings(&store).unwrap_err();
    assert!(error.contains("no authority-equivalent canonical binding"));
    let first = RoleBindingRepo::list_for_principal(&store, &principal).unwrap();
    assert_eq!(
        RoleBindingRepo::list_for_principal(&store, &principal).unwrap(),
        first
    );
    assert_eq!(first.len(), 1);
    assert!(first.iter().all(|binding| binding.scope == scope));
}

#[test]
fn embedded_upgrade_rejects_an_unknown_legacy_role_without_writes() {
    // Cause/effect graph: C1 the row uses a retired profile namespace; C2 its
    // local role has no canonical authority definition. E1 reject rather than
    // infer a role; E2 preserve the exact row for explicit operator repair.
    //
    // Decision table:
    // | legacy namespace | known role | effect |
    // | true             | false      | E1+E2  |
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite_migrated_store(
        SqliteBackend::open_path(dir.path().join("iam.sqlite")).unwrap(),
        "iam",
    )
    .unwrap();
    let binding = RoleBinding {
        principal: PrincipalRef::Service {
            service_id: "unknown-legacy-role".to_owned(),
        },
        role: RoleId("awaken.runtime.resources:custom_operator".to_owned()),
        scope: ScopeRef::Workspace {
            workspace_id: WorkspaceId("workspace-local".to_owned()),
        },
    };
    RoleBindingRepo::add(&store, binding.clone()).unwrap();

    let error = migrate_legacy_workspace_bindings(&store).unwrap_err();
    assert!(error.contains("unsupported legacy role"));
    assert_eq!(RoleBindingRepo::list(&store).unwrap(), [binding]);
}

#[test]
fn embedded_upgrade_resumes_cleanup_after_canonical_binding_commit() {
    // Cause/effect graph: C1 the canonical binding was durably added; C2 the
    // process crashed before its last legacy row was removed. E1 the canonical
    // row proves the exact intended authority; E2 retry deletes the remnant and
    // converges idempotently without requiring the former companion row.
    //
    // Decision table:
    // | complete legacy pair | canonical binding | effect |
    // | false                | true              | E1+E2  |
    let dir = tempfile::tempdir().unwrap();
    let store = sqlite_migrated_store(
        SqliteBackend::open_path(dir.path().join("iam.sqlite")).unwrap(),
        "iam",
    )
    .unwrap();
    let principal = PrincipalRef::Service {
        service_id: "legacy-interrupted-admin".to_owned(),
    };
    let scope = ScopeRef::Workspace {
        workspace_id: WorkspaceId("workspace-local".to_owned()),
    };
    for role in [
        "awaken.runtime.resources:admin".to_owned(),
        qualify_role("admin").0,
    ] {
        RoleBindingRepo::add(
            &store,
            RoleBinding {
                principal: principal.clone(),
                role: RoleId(role),
                scope: scope.clone(),
            },
        )
        .unwrap();
    }

    migrate_legacy_workspace_bindings(&store).unwrap();
    migrate_legacy_workspace_bindings(&store).unwrap();

    let bindings = RoleBindingRepo::list_for_principal(&store, &principal).unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].role, qualify_role("admin"));
    assert_eq!(bindings[0].scope, scope);
}
