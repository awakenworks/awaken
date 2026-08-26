//! Agents anti-corruption adapter for IAM Directory placement.

use awaken_agent_contract::stable_fingerprint;
use awaken_iam_contract::{
    CreateDirectoryNode, DirectoryNodeDto, DirectoryNodeId, OrgId, PrincipalRef,
    ProductSpaceBinding, ProductSpaceRef, Timestamp,
};
use awaken_iam_core::Organization;
use awaken_iam_server::{DirectoryApi, PolicyAdminApi, SqlStore, SqliteBackend};

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
        product: "agents".into(),
        space_id: workspace_id.to_owned(),
    };
    if let Some(binding) = directory
        .product_space_binding(&product_space)
        .expect("read Agents Directory placement")
    {
        assert_eq!(
            binding.org_id, org,
            "an Agents product space cannot cross its immutable Org partition"
        );
        return;
    }

    let node_id = DirectoryNodeId(format!("agents.workspace.{workspace_id}"));
    directory
        .create_node(CreateDirectoryNode {
            node: DirectoryNodeDto {
                id: node_id.clone(),
                org_id: org.clone(),
                parent_id: None,
                name: workspace_id.to_owned(),
                slug: format!(
                    "agents-{}",
                    stable_fingerprint(&workspace_id).replace(':', "-")
                ),
                description: Some("Agents workspace".into()),
                archived: false,
                created_at: at.clone(),
                updated_at: at.clone(),
            },
            binding: Some(ProductSpaceBinding {
                product_space,
                org_id: org,
                node_id,
            }),
        })
        .expect("create Agents Directory placement");
}
