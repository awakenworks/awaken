//! Agents anti-corruption adapter for IAM Directory placement.

use awaken_iam_contract::{
    EnsureProductSpacePlacement, OrgId, PrincipalRef, ProductId, ProductSpaceRef, Timestamp,
};
use awaken_iam_core::Organization;
use awaken_iam_server::{
    DirectoryApi, DirectoryCommandContext, PolicyAdminApi, SqlStore, SqliteBackend,
};

use super::BOOTSTRAP_PRINCIPAL;

/// Reconcile the local Agents product space into IAM's one Directory authority.
/// Hosted deployments perform the same command in Cloud provisioning; the
/// runtime and its opaque `ScopeId` never interpret hierarchy placement.
pub(super) fn reconcile_agents_directory(
    store: &SqlStore<SqliteBackend>,
    org_id: &str,
    workspace_id: &str,
    at: &Timestamp,
) {
    let mut pap = PolicyAdminApi::new(store.clone());
    let directory = DirectoryApi::new(store.clone());
    let org = OrgId(org_id.to_owned());
    if pap
        .get_org(&org)
        .expect("read local IAM organization")
        .is_none()
    {
        pap.create_org(
            Organization {
                id: org.clone(),
                display_name: None,
                owner: PrincipalRef::Service {
                    service_id: BOOTSTRAP_PRINCIPAL.to_owned(),
                },
                created_at: at.clone(),
                updated_at: at.clone(),
            },
            at.clone(),
        )
        .expect("create local IAM organization");
    }

    let product_space = ProductSpaceRef {
        product_id: ProductId::new("agents").expect("static product id is canonical"),
        space_id: format!("workspace/{workspace_id}"),
    };
    directory
        .ensure_product_space_placement(
            EnsureProductSpacePlacement {
                product_space,
                org_id: org,
                parent_product_space: None,
                name: workspace_id.to_owned(),
                preferred_slug: workspace_id.to_owned(),
                description: Some("Agents workspace".into()),
            },
            DirectoryCommandContext::product_service(
                ProductId::new("agents").expect("static product id is canonical"),
                "agents",
                at.clone(),
            ),
        )
        .expect("ensure Agents Directory placement");
}
