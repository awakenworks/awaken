use std::sync::Arc;

use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_protocol_managed::VaultState;
use awaken_tenancy::WorkspaceScope;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::application_mcp_credentials_router;

async fn call(
    app: &axum::Router,
    workspace_id: &str,
    idempotency_key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/config/application-mcp-credentials")
        .header("content-type", "application/json");
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    let mut request = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope(workspace_id.to_owned()));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Hosted application command cause/effect graph: C1 trusted Workspace is
/// stamped, C2 authority/target are valid, C3 replay key exists, C4 key and
/// bearer match the prior command, C5 a new key carries replacement material.
/// Effects: E1 one stable Vault/source at revision 1, E2 exact replay, E3
/// conflict without mutation, E4 same ids at revision 2, E5 secret-free output.
/// Constraints: the same key cannot represent different bearer material.
///
/// | rule | C1-C3 | prior | key/bearer | effect |
/// |---|---|---|---|---|
/// | H1 | true | none | new | E1 + E5 |
/// | H2 | true | yes | same/same | E2 + E5 |
/// | H3 | true | yes | same/different | E3/409 |
/// | H4 | true | yes | new/new | E4 + E5 |
/// | H5 | C3 false | - | - | 400/no source |
#[tokio::test]
async fn hosted_application_bearer_http_contract_is_stable_and_rotatable() {
    let state = Arc::new(VaultState::new(
        Arc::new(InMemorySecretStore::new()),
        Arc::new(InMemoryCredentialRepo::new()),
    ));
    let app = application_mcp_credentials_router(state);
    let body = |url: &str, token: &str| {
        json!({
            "application_authority_id": "awaken-flow",
            "mcp_server_url": url,
            "token": token // awaken-allow: secret
        })
    };

    assert_eq!(
        call(
            &app,
            "workspace-a",
            None,
            body("https://flow.example.test/mcp", "token-1")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST,
        "H5"
    );
    let (status, created) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-1"),
        body("HTTPS://FLOW.EXAMPLE.TEST:443/mcp/", "token-1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H1");
    assert_eq!(created["revision"], 1);
    assert!(!created.to_string().contains("token-1"), "H1/E5");

    let (_, replay) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-1"),
        body("https://flow.example.test/mcp", "token-1"),
    )
    .await;
    assert_eq!(replay, created, "H2");
    assert_eq!(
        call(
            &app,
            "workspace-a",
            Some("flow-bearer-1"),
            body("https://flow.example.test/mcp", "different-token")
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "H3"
    );
    let (status, rotated) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-2"),
        body("https://flow.example.test/mcp", "token-2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H4");
    assert_eq!(rotated["vault_id"], created["vault_id"]);
    assert_eq!(
        rotated["credential_source_id"],
        created["credential_source_id"]
    );
    assert_eq!(rotated["revision"], 2);
    assert!(!rotated.to_string().contains("token-2"), "H4/E5");
}
