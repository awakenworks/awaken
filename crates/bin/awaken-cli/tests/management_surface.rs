//! One server, the whole management plane: the admin config CRUD and the Managed
//! vault/credential front door are mounted together (`build_all_in_one_router`),
//! and a credential entered through the Managed vault surface is visible to
//! resolution because both share one store.

use awaken_cli::build_ephemeral_all_in_one_router as build_all_in_one_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// Multi-thread flavor matches production `awaken all-in-one`; model publication reads
// the catalog and credential repositories asynchronously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_and_vault_surfaces_are_served_together() {
    let app = build_all_in_one_router().await;

    // Admin config CRUD: author model metadata, read the catalog back.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/model-attributes/test-model",
        Some(json!({ "context_window": 4096 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        catalog["model_attributes"]["test-model"]["context_window"],
        4096
    );

    // Managed vault front door on the same server: create a vault + credential.
    let (s, vault) = call(
        &app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "Prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let (s, cred) = call(
        &app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": "ANTHROPIC_API_KEY",
            "secret_value": "sk-mgmt-secret", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cred["type"], "vault_credential");
    assert!(
        !serde_json::to_string(&cred)
            .unwrap()
            .contains("sk-mgmt-secret")
    );

    // Session surface is still mounted (the managed sessions route responds).
    let (s, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "default",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The product composition, rather than only the protocol fixture, wires the
    // local Vault as the write ingress for Managed repository credentials.  A
    // cloud Environment keeps this assertion at the management boundary: no
    // local Git checkout is needed to prove sealing, pinning, and rotation.
    let (s, environment) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "repository ingress", "config": { "type": "cloud" } })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{environment}");
    let (s, repository_session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "assistant",
            "environment_id": environment["id"],
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/managed-compat.git",
                "authorization_token": "initial-repository-token" // awaken-allow: secret
            }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{repository_session}");
    assert!(
        !repository_session
            .to_string()
            .contains("initial-repository-token")
    );
    let session_id = repository_session["id"].as_str().unwrap();
    let resource_id = repository_session["resources"][0]["id"].as_str().unwrap();
    let (s, rotated) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({ "authorization_token": "rotated-repository-token" })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{rotated}");
    assert!(!rotated.to_string().contains("rotated-repository-token"));
}
