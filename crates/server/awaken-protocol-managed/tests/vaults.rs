//! The Managed vault/credential front door over HTTP: create a vault, enter an
//! `environment_variable` credential (secret write-only), retrieve it secret-free,
//! and confirm the same credential resolves an inference. Also covers the wire
//! constraints (unknown vault 404, duplicate key rejected, unknown auth type 400).

use std::collections::HashMap;
use std::sync::Arc;

use awaken_config_resolver::resolve_inference;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_credential_vault::{
    CredentialBinding, CredentialSource, CredentialSourceId, InMemorySecretStore,
};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_model_catalog::repo::InMemoryCatalogRepo;
use awaken_model_catalog::{
    ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_protocol_managed::{VaultState, vault_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    app: Router,
    state: Arc<VaultState>,
    secrets: Arc<InMemorySecretStore>,
    credentials: Arc<InMemoryCredentialRepo>,
}

fn harness() -> Harness {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let state = Arc::new(VaultState::new(secrets.clone(), credentials.clone()));
    let app = vault_router(state.clone());
    Harness {
        app,
        state,
        secrets,
        credentials,
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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
async fn vault_credential_lifecycle_and_resolution() {
    let h = harness();

    // Create a vault.
    let (s, vault) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "Prod keys", "metadata": { "team": "core" } })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(vault["type"], "vault");
    let vault_id = vault["id"].as_str().unwrap().to_string();

    // Enter an environment_variable credential — the secret is write-only.
    let (s, cred) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": "ANTHROPIC_API_KEY",
            "secret_value": "sk-vault-secret", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cred["type"], "vault_credential");
    assert_eq!(cred["auth"]["type"], "environment_variable");
    assert_eq!(cred["auth"]["secret_name"], "ANTHROPIC_API_KEY");
    assert_eq!(cred["auth"]["networking"]["type"], "unrestricted");
    let cred_id = cred["id"].as_str().unwrap().to_string();
    // The secret never appears on the wire projection.
    assert!(
        !serde_json::to_string(&cred)
            .unwrap()
            .contains("sk-vault-secret")
    );

    // Retrieve is secret-free too.
    let (s, got) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["id"], cred_id);

    // Validate: an env-var credential has no upstream to probe -> `unknown`.
    let (s, validation) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(validation["type"], "vault_credential_validation");
    assert_eq!(validation["status"], "unknown");

    // The vault credential is a real resolvable domain row: bind it and resolve.
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .expect("vault credential maps to a domain source");
    let catalog = seed_catalog(&h).await;
    let source: CredentialSource = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&source_id).await.unwrap()
    };
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(source.id.0.clone(), source);
    let resolved = resolve_inference(
        &catalog,
        "claude-opus-4-8",
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(source_id.0.clone()),
        },
        &sources,
        &*h.secrets,
    )
    .await
    .expect("resolve the vault credential");
    assert_eq!(
        resolved.credential.as_ref().unwrap().expose_secret(),
        "sk-vault-secret"
    );
}

async fn seed_catalog(_h: &Harness) -> awaken_model_catalog::ProviderCatalog {
    let repo = InMemoryCatalogRepo::new();
    repo.put_provider(Provider {
        id: ProviderId::new("anthropic"),
        slug: "anthropic".into(),
        display_name: "Anthropic".into(),
        version: 1,
    })
    .await
    .unwrap();
    repo.put_endpoint(ProtocolEndpoint {
        id: ProtocolEndpointId::new("ep1"),
        provider_id: ProviderId::new("anthropic"),
        flavor: ModelApiCompat::AnthropicMessages,
        base_url: Some("https://api.anthropic.com/v1/".into()),
        timeout_secs: 300,
        display_name: "prod".into(),
        version: 1,
    })
    .await
    .unwrap();
    repo.put_offering(Offering {
        model_id: "claude-opus-4-8".into(),
        provider_id: ProviderId::new("anthropic"),
        protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
        flavor: ModelApiCompat::AnthropicMessages,
        upstream_model: None,
    })
    .await
    .unwrap();
    repo.snapshot().await.unwrap()
}

#[tokio::test]
async fn wire_constraints_fail_closed() {
    let h = harness();

    // Credential under an unknown vault -> 404.
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/vaults/vlt_missing/credentials",
        Some(json!({
            "type": "environment_variable",
            "secret_name": "K",
            "secret_value": "v", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Create a vault, then reject a duplicate key and an unknown auth type.
    let (_, vault) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "v" })),
    )
    .await;
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let body = json!({
        "type": "environment_variable",
        "secret_name": "DUP",
        "secret_value": "v", // awaken-allow: secret
        "networking": { "type": "unrestricted" }
    });
    let (s1, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(body.clone()),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(body),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::BAD_REQUEST,
        "duplicate key must be rejected"
    );

    // An unsupported auth type is a clean 400 (unknown-variant deserialize error).
    let (s3, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({ "type": "static_bearer", "bearer_token": "x" })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s3, StatusCode::BAD_REQUEST);
}

/// Create a vault named `display_name` and return its id.
async fn create_vault(h: &Harness, display_name: &str) -> String {
    let (s, vault) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": display_name })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    vault["id"].as_str().unwrap().to_string()
}

/// Enter an env-var credential named `secret_name` and return its id.
async fn create_credential(h: &Harness, vault_id: &str, secret_name: &str) -> String {
    let (s, cred) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": secret_name,
            "secret_value": "sk-v", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    cred["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn delete_vault_cascades_credentials() {
    let h = harness();
    let vault_id = create_vault(&h, "doomed").await;
    let cred_id = create_credential(&h, &vault_id, "K").await;

    let (s, deleted) = call(&h.app, "DELETE", &format!("/v1/vaults/{vault_id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(deleted["type"], "vault_deleted");
    assert_eq!(deleted["id"], vault_id);

    // The vault and its credential bookkeeping are gone.
    let (s, _) = call(&h.app, "GET", &format!("/v1/vaults/{vault_id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(h.state.credential_source_id(&vault_id, &cred_id).is_none());

    // Deleting an unknown vault is a 404, not an idempotent 200.
    let (s, _) = call(&h.app, "DELETE", "/v1/vaults/vlt_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retrieve_vault_unknown_is_404() {
    let h = harness();
    let (s, _) = call(&h.app, "GET", "/v1/vaults/vlt_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn retrieve_credential_wrong_vault_is_404() {
    let h = harness();
    let vault_a = create_vault(&h, "a").await;
    let vault_b = create_vault(&h, "b").await;
    let cred_id = create_credential(&h, &vault_a, "K").await;

    // The credential exists, but not under vault B — the path scope must hold.
    let (s, _) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_b}/credentials/{cred_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_b}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // An outright unknown credential 404s under its own vault too.
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_a}/credentials/crd_missing/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_vault_rejects_bad_display_name() {
    let h = harness();
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "empty display_name");
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "x".repeat(256) })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "over-long display_name");
    // The 255-char boundary itself is accepted.
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "x".repeat(255) })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn too_many_credentials_is_rejected() {
    let h = harness();
    let vault_id = create_vault(&h, "full").await;
    for i in 0..20 {
        create_credential(&h, &vault_id, &format!("KEY_{i}")).await;
    }
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": "KEY_20",
            "secret_value": "sk-v", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "21st credential must be rejected"
    );
}
