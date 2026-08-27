use super::super::embedded_iam_for_tenant;
use super::*;
use awaken_iam_contract::{
    CreateDirectoryNode, MoveDirectoryNode, ProductId, ProductSpaceRef, UpdateDirectoryNode,
    WorkspaceId,
};

#[test]
fn embedded_agents_workspace_has_one_idempotent_iam_directory_placement() {
    // Cause/effect decision table:
    // C1 first boot for Org + Workspace -> E1 create the IAM Org, then create
    // one Directory node + `agents` product-space placement atomically; C2 restart
    // with the exact same coordinates -> E2 reuse the placement and preserve
    // Directory revision; C3 product id contains non-slug punctuation -> E3 a
    // valid deterministic display slug; C4 runtime scope -> E4 remains the
    // opaque Workspace id, not a node id; C5 user moves/renames the placement
    // before restart -> E5 bootstrap replay restores neither old hierarchy nor
    // old metadata and creates no second command path; C6 Directory mutation
    // -> E6 IAM policy version remains unchanged.
    let data = tempfile::tempdir().unwrap();
    let first = embedded_iam_for_tenant(data.path(), "org-directory", "workspace_directory");
    let first_directory = DirectoryApi::new(first.store.clone());
    let policy_version = PolicyAdminApi::new(first.store.clone())
        .store_version()
        .unwrap();
    let product_space = ProductSpaceRef {
        product_id: ProductId::new("agents").unwrap(),
        space_id: "workspace/workspace_directory".into(),
    };
    let placement = first_directory
        .product_space_placement(&OrgId("org-directory".into()), &product_space)
        .unwrap()
        .expect("Agents workspace is placed");
    assert_eq!(placement.org_id, OrgId("org-directory".into()));
    assert_eq!(
        first.workspace_id,
        WorkspaceId("workspace_directory".into())
    );
    let node = first_directory
        .node(&placement.node_id)
        .unwrap()
        .expect("Agents Directory node exists");
    assert!(
        node.slug
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
        "E3: slug is accepted by the Directory contract"
    );
    let user_context = |second| {
        DirectoryCommandContext::service(
            "directory-user",
            Timestamp(format!("2026-08-27T00:00:{second:02}Z")),
        )
    };
    let folder = first_directory
        .create_node(
            CreateDirectoryNode {
                org_id: OrgId("org-directory".into()),
                parent_id: None,
                name: "Customer folder".into(),
                preferred_slug: "customer-folder".into(),
                description: None,
            },
            user_context(1),
        )
        .unwrap()
        .node;
    first_directory
        .move_node(
            &placement.node_id,
            MoveDirectoryNode {
                parent_id: Some(folder.id),
            },
            user_context(2),
        )
        .unwrap();
    first_directory
        .update_node(
            &placement.node_id,
            UpdateDirectoryNode {
                name: "Customer Agents workspace".into(),
                slug: "customer-agents-workspace".into(),
                description: Some("user-owned presentation".into()),
            },
            user_context(3),
        )
        .unwrap();
    let user_edited_node = first_directory
        .node(&placement.node_id)
        .unwrap()
        .expect("user-edited Agents Directory node exists");
    let revision = first_directory
        .revision(&OrgId("org-directory".into()))
        .unwrap();
    assert_eq!(
        PolicyAdminApi::new(first.store.clone())
            .store_version()
            .unwrap(),
        policy_version,
        "E6: presentation changes do not mutate authorization policy"
    );
    drop(first);

    let restarted = embedded_iam_for_tenant(data.path(), "org-directory", "workspace_directory");
    let restarted_directory = DirectoryApi::new(restarted.store.clone());
    assert_eq!(
        restarted_directory
            .revision(&OrgId("org-directory".into()))
            .unwrap(),
        revision
    );
    assert_eq!(
        restarted_directory
            .product_space_placement(&OrgId("org-directory".into()), &product_space)
            .unwrap(),
        Some(placement)
    );
    assert_eq!(
        restarted_directory
            .node(&user_edited_node.id)
            .unwrap()
            .unwrap(),
        user_edited_node,
        "E5: bootstrap replay preserves user hierarchy and metadata"
    );
    assert_eq!(
        PolicyAdminApi::new(restarted.store.clone())
            .store_version()
            .unwrap(),
        policy_version,
        "E6: restart reconciliation does not couple Directory to policy"
    );
}
