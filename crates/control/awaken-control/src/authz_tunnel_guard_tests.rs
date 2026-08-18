use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_routes_enter_the_workspace_pep_and_stamp_its_scope() {
    // Cause/effect graph: C1=Cloud-only Tunnel family, C2=self-managed API
    // token or browser credential. Effect: every C2 is rejected before the
    // application handler. A role grant cannot upgrade an API key into WIF.
    async fn scope_echo(
        axum::Extension(scope): axum::Extension<awaken_tenancy::WorkspaceScope>,
    ) -> Response {
        (StatusCode::OK, Json(json!({ "workspace": scope.0 }))).into_response()
    }

    let (_dir, iam) = fresh_iam();
    let member = mint(&iam, "tok_tunnel_member", "wrkspc_tunnel", "workspace_user");
    let admin = mint(&iam, "tok_tunnel_admin", "wrkspc_tunnel", "workspace_admin");
    let app = Router::new()
        .route("/v1/tunnels", axum::routing::get(scope_echo))
        .route(
            "/v1/tunnels/{tunnel_id}/rotate_token",
            axum::routing::post(scope_echo),
        )
        .layer(axum::middleware::from_fn_with_state(iam, management_guard));

    let (missing, error) = call(&app, "GET", "/v1/tunnels", None, None).await;
    assert_eq!(missing, StatusCode::UNAUTHORIZED, "missing: {error}");
    let (denied, error) = call(&app, "GET", "/v1/tunnels", Some(&member), None).await;
    assert_eq!(denied, StatusCode::UNAUTHORIZED, "member: {error}");
    let (admin_denied, error) = call(&app, "GET", "/v1/tunnels", Some(&admin), None).await;
    assert_eq!(admin_denied, StatusCode::UNAUTHORIZED, "admin: {error}");
    let (rotated, error) = call(
        &app,
        "POST",
        "/v1/tunnels/tnl_1/rotate_token",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(rotated, StatusCode::UNAUTHORIZED, "rotate: {error}");
}
