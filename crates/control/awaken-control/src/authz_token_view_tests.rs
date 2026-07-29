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
