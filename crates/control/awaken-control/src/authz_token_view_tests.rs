use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn token_views_are_filtered_to_the_named_workspace() {
    // Cause/effect graph: the requested Workspace is the sole visibility fence;
    // tokens bound to it are returned and tokens bound elsewhere are hidden.
    //
    // | Rule | token scope equals requested scope | Effect |
    // | V1   | yes                                | include |
    // | V2   | no                                 | exclude |
    let (_dir, iam) = fresh_iam();
    mint(&iam, "tok_in_a", "wrkspc_a", "workspace_admin");
    mint(&iam, "tok_in_b", "wrkspc_b", "workspace_admin");

    let ids_a: Vec<String> = iam
        .token_views("wrkspc_a")
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids_a.contains(&"tok_in_a".to_string()), "V1: {ids_a:?}");
    assert!(
        !ids_a.contains(&"tok_in_b".to_string()),
        "V2: workspace A's view must not disclose workspace B's token: {ids_a:?}"
    );

    let ids_b: Vec<String> = iam
        .token_views("wrkspc_b")
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids_b.contains(&"tok_in_b".to_string()), "V1: {ids_b:?}");
    assert!(!ids_b.contains(&"tok_in_a".to_string()), "V2: {ids_b:?}");
}

/// The displayed role is derived from the binding at the token's **own**
/// workspace; no other workspace's binding may stand in for a missing one.
///
/// | Rule | binding at the token's workspace | binding elsewhere | displayed role |
/// | R1   | yes (`workspace_user`)           | yes (`admin`)     | `workspace_user` |
/// | R2   | no                               | yes (`admin`)     | none             |
///
/// R2 is the state a mint leaves behind when the token row commits and the
/// role-binding write does not (they are two separate, non-transactional
/// repository writes, and `principal_id` is caller-supplied so one principal
/// can legitimately span workspaces). Positional fallback then labels the
/// stranded token with the *other* workspace's role — here `admin` against a
/// token that grants no such thing. The view is operator-facing only (both
/// callers build JSON), so this is misinformation an operator acts on, not an
/// enforcement bypass; the enforcement path is the policy engine, which never
/// consults this function.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_never_displays_a_role_borrowed_from_another_workspace() {
    let (_dir, iam) = fresh_iam();
    let principal = PrincipalRef::Service {
        service_id: "svc-spans-workspaces".to_string(),
    };

    // The principal holds `admin` in workspace B, with a real token there.
    iam.mint_service_token(TokenSpec {
        token_id: "tok_in_b".to_string(),
        service_id: "svc-spans-workspaces".to_string(),
        workspace_id: "wrkspc_b".to_string(),
        role: "admin".to_string(),
        created_at: None,
        expires_at: None,
    })
    .expect("mint the workspace B token");

    // R1: a token in workspace A whose own binding exists shows that role.
    iam.mint_service_token(TokenSpec {
        token_id: "tok_in_a_bound".to_string(),
        service_id: "svc-spans-workspaces".to_string(),
        workspace_id: "wrkspc_a".to_string(),
        role: "workspace_user".to_string(),
        created_at: None,
        expires_at: None,
    })
    .expect("mint the workspace A token");

    // R2: a token row in workspace C whose binding write never landed.
    let stranded = ApiToken {
        id: ApiTokenId("tok_in_c_stranded".to_string()),
        prefix: awaken_iam_contract::ApiTokenPrefix("sk-awaken-stranded".to_string()),
        principal: principal.clone(),
        secret_hash: "$argon2id$unused-by-this-projection".to_string(),
        workspace: WorkspaceId("wrkspc_c".to_string()),
        created_at: Timestamp(now_rfc3339()),
        expires_at: None,
        revoked_at: None,
    };
    ApiTokenRepository::create(&iam.store, stranded).expect("persist the stranded token row");

    let role_at = |workspace: &str, token_id: &str| -> Option<String> {
        iam.token_views(workspace)
            .into_iter()
            .find(|view| view["id"] == serde_json::json!(token_id))
            .map(|view| view["role"].as_str().map(str::to_string))
            .expect("the token is visible in its own workspace's view")
    };

    assert_eq!(
        role_at("wrkspc_b", "tok_in_b"),
        Some("admin".to_string()),
        "R1: workspace B's own binding"
    );
    assert_eq!(
        role_at("wrkspc_a", "tok_in_a_bound"),
        Some("workspace_user".to_string()),
        "R1: workspace A's own binding, not B's `admin`"
    );
    assert_eq!(
        role_at("wrkspc_c", "tok_in_c_stranded"),
        None,
        "R2: no binding at wrkspc_c means no role, never `admin` borrowed from wrkspc_b"
    );
}
