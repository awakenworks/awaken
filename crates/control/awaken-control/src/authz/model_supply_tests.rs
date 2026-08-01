use super::*;

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
