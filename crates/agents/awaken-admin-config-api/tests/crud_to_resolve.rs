//! The admin config plane end-to-end over HTTP: author provider + endpoint +
//! offering and enter a credential through the routes, then resolve a run against
//! the *same* stores the router wrote to. Proves the L1 admin surface and the
//! resolver share one catalog/credential state (ADR-0043), and that a credential's
//! secret is write-only (never echoed on any response).

use std::collections::HashMap;
use std::sync::Arc;

use awaken_admin_config_api::{AdminState, admin_router};
use awaken_config_resolver::resolve_inference;
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
use awaken_credential_vault::{
    CredentialBinding, CredentialSource, CredentialSourceId, InMemorySecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    app: Router,
    catalog: Arc<InMemoryCatalogRepo>,
    credentials: Arc<InMemoryCredentialRepo>,
    secrets: Arc<InMemorySecretStore>,
}

fn harness() -> Harness {
    let catalog = Arc::new(InMemoryCatalogRepo::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: credentials.clone(),
        secrets: secrets.clone(),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        probe: None,
    });
    Harness {
        app,
        catalog,
        credentials,
        secrets,
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
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
async fn author_catalog_and_credential_then_resolve_a_run() {
    let h = harness();

    // Author provider → endpoint → offering through the HTTP surface.
    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/providers/anthropic",
        Some(json!({
            "id": "ignored-by-path",
            "slug": "anthropic",
            "display_name": "Anthropic",
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/endpoints/ep1",
        Some(json!({
            "id": "ep1",
            "provider_id": "anthropic",
            "flavor": "anthropic_messages",
            "base_url": "https://api.anthropic.com/v1/",
            "timeout_secs": 300,
            "display_name": "prod",
            "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/config/offerings",
        Some(json!({
            "model_id": "claude-opus-4-8",
            "provider_id": "anthropic",
            "protocol_endpoint_id": "ep1",
            "flavor": "anthropic_messages",
            "upstream_model": null
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Enter a credential; the response is secret-free (secret never echoed).
    let (s, cred) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "vault",
            "provider_id": "anthropic",
            "env_key": "ANTHROPIC_API_KEY",
            "secret": "sk-admin-secret" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let body_text = serde_json::to_string(&cred).unwrap();
    assert!(
        !body_text.contains("sk-admin-secret"),
        "secret leaked: {body_text}"
    );
    let cred_id = cred["id"].as_str().expect("credential id").to_string();

    // The dangling-reference guard is live: an offering for an unknown endpoint is
    // a 404 (the referenced endpoint is not found).
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/offerings",
        Some(json!({
            "model_id": "ghost",
            "provider_id": "anthropic",
            "protocol_endpoint_id": "does-not-exist",
            "flavor": "anthropic_messages",
            "upstream_model": null
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");

    // Resolve against the SAME stores the router wrote to — the admin plane and the
    // resolver share one catalog/credential state.
    let catalog = h.catalog.snapshot().await.unwrap();
    let source: CredentialSource = h
        .credentials
        .get(&CredentialSourceId(cred_id.clone()))
        .await
        .unwrap();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(source.id.0.clone(), source);

    let resolved = resolve_inference(
        &catalog,
        "claude-opus-4-8",
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(cred_id.clone()),
        },
        &sources,
        &*h.secrets,
    )
    .await
    .expect("resolve against authored catalog");

    assert_eq!(resolved.adapter_kind, "anthropic");
    assert_eq!(
        resolved.base_url.as_deref(),
        Some("https://api.anthropic.com/v1/")
    );
    // The secret materializes only here, at the resolver seam.
    assert_eq!(
        resolved.credential.as_ref().unwrap().expose_secret(),
        "sk-admin-secret"
    );
}

#[tokio::test]
async fn get_missing_provider_is_problem_json_404() {
    let h = harness();
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/config/providers/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.contains("problem+json"), "content-type was {ct}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let err: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["status"], 404);
}
