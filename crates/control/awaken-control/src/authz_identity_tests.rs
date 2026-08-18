use super::*;

#[test]
fn identity_modes_accept_product_names_and_legacy_aliases() {
    assert_eq!(
        ManagementIdentityMode::parse("no-login"),
        Some(ManagementIdentityMode::NoLogin)
    );
    assert_eq!(
        ManagementIdentityMode::parse("awaken-cloud"),
        Some(ManagementIdentityMode::AwakenCloud)
    );
    assert_eq!(
        ManagementIdentityMode::parse("self-managed"),
        Some(ManagementIdentityMode::SelfManaged)
    );
    assert_eq!(
        ManagementIdentityMode::parse("embedded"),
        Some(ManagementIdentityMode::SelfManaged)
    );
    assert_eq!(ManagementIdentityMode::parse("unknown"), None);
}

#[test]
fn embedded_iam_bootstrap_uses_the_platform_provisioned_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let iam = embedded_iam_for_workspace(dir.path(), "workspace_platform_owned");
    let token = std::fs::read_to_string(dir.path().join(ADMIN_TOKEN_FILE)).unwrap();
    let (_, workspace) = iam.authenticate(token.trim()).unwrap();
    assert_eq!(workspace.0, "workspace_platform_owned");
}

#[test]
fn embedded_iam_registers_only_org_to_workspace_scope() {
    let dir = tempfile::tempdir().unwrap();
    let iam = embedded_iam_for_tenant(dir.path(), "org_local", "workspace_local");
    let token = std::fs::read_to_string(dir.path().join(ADMIN_TOKEN_FILE)).unwrap();
    let (principal, _) = iam.authenticate(token.trim()).unwrap();
    assert_eq!(
        iam.authorize(
            principal,
            WORKSPACE_READ,
            ScopeRef::Workspace {
                workspace_id: WorkspaceId("workspace_local".into())
            },
        ),
        AuthorizationDecision::Allow
    );
}
