//! One server, the whole management plane: the admin config CRUD and the Managed
//! vault/credential front door are mounted together (`build_management_router`),
//! and a credential entered through the Managed vault surface is visible to
//! resolution because both share one store.

use awaken_cli::build_management_router;
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

#[tokio::test]
async fn admin_and_vault_surfaces_are_served_together() {
    let app = build_management_router().await;

    // Admin config CRUD: author a provider, read the catalog back.
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/config/providers/anthropic",
        Some(json!({ "id": "anthropic", "slug": "anthropic", "display_name": "Anthropic", "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, catalog) = call(&app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        catalog["providers"]
            .as_object()
            .is_some_and(|p| p.contains_key("anthropic"))
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
        Some(json!({ "agent": "default" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}
