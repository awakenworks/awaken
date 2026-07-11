//! The Managed vault/credential front door over HTTP: create a vault, enter
//! `environment_variable` / `static_bearer` / `mcp_oauth` credentials (secrets
//! write-only), retrieve them secret-free, and confirm an env-var credential
//! resolves an inference. Also covers the wire constraints (unknown vault 404,
//! duplicate key rejected, unknown auth type 400) and the vault→MCP URL-binding
//! seam (`mcp_credential_source_for_url`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::resolve_inference;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_credential_vault::{
    CredentialBinding, CredentialSource, CredentialSourceId, InMemorySecretStore, SecretRef,
    SecretStore,
};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_model_catalog::repo::InMemoryCatalogRepo;
use awaken_model_catalog::{
    ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_protocol_managed::{McpProbe, McpProbeStatus, TokenEndpointAuthBinding};
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

/// A fake live probe: answers a canned status and records every `(url, bearer)`
/// it is handed — the bearer arrives as the already-materialized secret (the
/// port signature takes a `RedactedString`, so a vault ref cannot cross it).
struct FakeProbe {
    status: McpProbeStatus,
    seen: Mutex<Vec<(String, String)>>,
}

impl FakeProbe {
    fn new(status: McpProbeStatus) -> Arc<Self> {
        Arc::new(Self {
            status,
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl McpProbe for FakeProbe {
    async fn probe(&self, mcp_server_url: &str, bearer: &RedactedString) -> McpProbeStatus {
        self.seen.lock().unwrap().push((
            mcp_server_url.to_string(),
            bearer.expose_secret().to_string(),
        ));
        self.status
    }
}

/// A harness whose vault surface wires the given live probe.
fn harness_with_probe(probe: Arc<FakeProbe>) -> Harness {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let state = Arc::new(VaultState::new(secrets.clone(), credentials.clone()).with_probe(probe));
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
        dialect: ApiDialect::AnthropicMessages,
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
        dialect: ApiDialect::AnthropicMessages,
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

    // An unknown auth type is a clean 400 (unknown-variant deserialize error).
    let (s3, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({ "type": "basic_auth", "token": "x" })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s3, StatusCode::BAD_REQUEST);

    // A known type with missing required fields 400s too (static_bearer needs
    // `token` + `mcp_server_url`, not the old guessed field name).
    let (s4, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({ "type": "static_bearer", "bearer_token": "x" })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s4, StatusCode::BAD_REQUEST);
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
async fn list_vaults_returns_one_full_page_sorted_by_id() {
    let h = harness();
    let a = create_vault(&h, "alpha").await;
    let b = create_vault(&h, "bravo").await;

    let (s, page) = call(&h.app, "GET", "/v1/vaults", None).await;
    assert_eq!(s, StatusCode::OK);
    // The SDK `PageCursor` shape: data + has_more + next_page (single page here).
    assert_eq!(page["has_more"], false);
    assert!(page["next_page"].is_null());
    let data = page["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["type"], "vault");
    // Deterministic ascending-id order (`vlt_` is zero-padded == creation order).
    assert_eq!(data[0]["id"], a);
    assert_eq!(data[1]["id"], b);
}

#[tokio::test]
async fn list_credentials_is_scoped_to_the_vault_and_404s_unknown() {
    let h = harness();
    let vault_a = create_vault(&h, "a").await;
    let vault_b = create_vault(&h, "b").await;
    let c1 = create_credential(&h, &vault_a, "K1").await;
    let c2 = create_credential(&h, &vault_a, "K2").await;
    create_credential(&h, &vault_b, "K3").await;

    let (s, page) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_a}/credentials"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page["has_more"], false);
    assert!(page["next_page"].is_null());
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![c1.as_str(), c2.as_str()]);
    // Every listed row is the secret-free projection.
    assert_eq!(page["data"][0]["type"], "vault_credential");

    // Vault B sees only its own credential (path scope holds).
    let (_, page_b) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_b}/credentials"),
        None,
    )
    .await;
    assert_eq!(page_b["data"].as_array().unwrap().len(), 1);

    // An unknown vault is a 404, not an empty page.
    let (s, _) = call(&h.app, "GET", "/v1/vaults/vlt_missing/credentials", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archive_vault_soft_deletes_and_hides_from_default_list() {
    let h = harness();
    let keep = create_vault(&h, "keep").await;
    let gone = create_vault(&h, "gone").await;

    // Archive stamps archived_at and returns the vault.
    let (s, archived) = call(&h.app, "POST", &format!("/v1/vaults/{gone}/archive"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(archived["type"], "vault");
    assert!(archived["archived_at"].is_string());

    // Default list excludes the archived vault; include_archived returns both.
    let (_, page) = call(&h.app, "GET", "/v1/vaults", None).await;
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![keep.as_str()]);
    let (_, all) = call(&h.app, "GET", "/v1/vaults?include_archived=true", None).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 2);

    // Soft-delete: retrieve still works and reports archived_at.
    let (s, got) = call(&h.app, "GET", &format!("/v1/vaults/{gone}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(got["archived_at"].is_string());

    // Archiving an unknown vault is a 404.
    let (s, _) = call(&h.app, "POST", "/v1/vaults/vlt_missing/archive", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archive_credential_soft_deletes_and_hides_from_list() {
    let h = harness();
    let vault_id = create_vault(&h, "v").await;
    let keep = create_credential(&h, &vault_id, "KEEP").await;
    let gone = create_credential(&h, &vault_id, "GONE").await;

    let (s, archived) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{gone}/archive"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(archived["type"], "vault_credential");
    assert!(archived["archived_at"].is_string());

    let (_, page) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_id}/credentials"),
        None,
    )
    .await;
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![keep.as_str()]);
    let (_, all) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_id}/credentials?include_archived=true"),
        None,
    )
    .await;
    assert_eq!(all["data"].as_array().unwrap().len(), 2);

    // Wrong vault + unknown credential both 404.
    let other = create_vault(&h, "other").await;
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{other}/credentials/{gone}/archive"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_credential_removes_one_and_scopes_by_vault() {
    let h = harness();
    let vault_a = create_vault(&h, "a").await;
    let vault_b = create_vault(&h, "b").await;
    let c1 = create_credential(&h, &vault_a, "K1").await;
    let c2 = create_credential(&h, &vault_a, "K2").await;

    // Deleting under the wrong vault 404s (path scope holds), does not remove.
    let (s, _) = call(
        &h.app,
        "DELETE",
        &format!("/v1/vaults/{vault_b}/credentials/{c1}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(h.state.credential_source_id(&vault_a, &c1).is_some());

    // Delete c1 under its own vault: the receipt, then it is gone from the list.
    let (s, deleted) = call(
        &h.app,
        "DELETE",
        &format!("/v1/vaults/{vault_a}/credentials/{c1}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(deleted["type"], "vault_credential_deleted");
    assert_eq!(deleted["id"], c1);
    assert!(h.state.credential_source_id(&vault_a, &c1).is_none());

    let (_, page) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_a}/credentials"),
        None,
    )
    .await;
    let ids: Vec<&str> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![c2.as_str()]);

    // Deleting an already-gone credential is a 404, not an idempotent 200.
    let (s, _) = call(
        &h.app,
        "DELETE",
        &format!("/v1/vaults/{vault_a}/credentials/{c1}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_vault_replaces_name_and_patches_metadata() {
    let h = harness();
    let (_, vault) = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "old", "metadata": { "a": "1", "keep": "x" } })),
    )
    .await;
    let vault_id = vault["id"].as_str().unwrap().to_string();

    // Rename + patch metadata: upsert `b`, delete `a` (null), preserve `keep`.
    let (s, updated) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}"),
        Some(json!({ "display_name": "new", "metadata": { "a": null, "b": "2" } })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(updated["display_name"], "new");
    assert_eq!(updated["metadata"]["b"], "2");
    assert_eq!(updated["metadata"]["keep"], "x");
    assert!(updated["metadata"].get("a").is_none());

    // An empty body is a no-op that echoes the (already-updated) vault.
    let (s, echo) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(echo["display_name"], "new");

    // A bad name is a 400; an unknown vault is a 404.
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}"),
        Some(json!({ "display_name": "x".repeat(256) })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/vaults/vlt_missing",
        Some(json!({ "display_name": "y" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_credential_patches_fields_reseals_secret_and_rejects_type_change() {
    let h = harness();
    let vault_id = create_vault(&h, "v").await;
    let cred_id = create_credential(&h, &vault_id, "ENVKEY").await;

    // Patch networking + re-seal the secret + set display_name + patch metadata.
    let (s, updated) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        Some(json!({
            "auth": {
                "type": "environment_variable",
                "secret_value": "rotated-secret", // awaken-allow: secret
                "networking": { "type": "limited", "allowed_hosts": ["api.example.com"] }
            },
            "display_name": "renamed",
            "metadata": { "team": "core" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(updated["auth"]["networking"]["type"], "limited");
    assert_eq!(
        updated["auth"]["networking"]["allowed_hosts"][0],
        "api.example.com"
    );
    assert_eq!(updated["display_name"], "renamed");
    assert_eq!(updated["metadata"]["team"], "core");
    // The rotated secret never appears on the wire.
    assert!(
        !serde_json::to_string(&updated)
            .unwrap()
            .contains("rotated-secret")
    );

    // The re-seal reached the domain: materialize yields the new secret.
    let source_id = h.state.credential_source_id(&vault_id, &cred_id).unwrap();
    let source = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&source_id).await.unwrap()
    };
    let secret = awaken_credential_vault::materialize(&source, &*h.secrets)
        .await
        .unwrap();
    assert_eq!(secret.expose_secret(), "rotated-secret");

    // Changing the credential's kind is rejected (kind is immutable).
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        Some(json!({ "auth": { "type": "static_bearer", "token": "x" } })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // An unknown credential is a 404.
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/crd_missing"),
        Some(json!({ "display_name": "z" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_credential_clears_display_name_with_explicit_null() {
    let h = harness();
    let vault_id = create_vault(&h, "v").await;
    let (_, cred) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "brr", // awaken-allow: secret
            "display_name": "to-clear"
        })),
    )
    .await;
    let cred_id = cred["id"].as_str().unwrap().to_string();
    assert_eq!(cred["display_name"], "to-clear");

    // display_name: null clears it (distinct from omitting the field).
    let (s, updated) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        Some(json!({ "display_name": null })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        updated.get("display_name").is_none(),
        "display_name is cleared"
    );
}

#[tokio::test]
async fn update_mcp_oauth_refresh_rotates_sealed_secrets() {
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;
    let url = "https://mcp.example.com/sse";
    let cred = create_mcp_oauth(
        &h,
        &vault_id,
        url,
        Some(json!({
            "client_id": "cli",
            "refresh_token": "rt-old", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "client_secret_basic", "client_secret": "cs-old" }, // awaken-allow: secret
            "scope": "old"
        })),
    )
    .await;
    let cred_id = cred["id"].as_str().unwrap().to_string();
    let source_id = h.state.credential_source_id(&vault_id, &cred_id).unwrap();

    // Rotate the refresh token + client secret and change the scheme + scope.
    let (s, updated) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        Some(json!({
            "auth": {
                "type": "mcp_oauth",
                "refresh": {
                    "refresh_token": "rt-new", // awaken-allow: secret
                    "scope": "new",
                    "token_endpoint_auth": { "type": "client_secret_post", "client_secret": "cs-new" } // awaken-allow: secret
                }
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(updated["auth"]["refresh"]["scope"], "new");
    assert_eq!(
        updated["auth"]["refresh"]["token_endpoint_auth"]["type"],
        "client_secret_post"
    );
    let raw = serde_json::to_string(&updated).unwrap();
    assert!(!raw.contains("rt-new") && !raw.contains("cs-new"));

    // Both rotated secrets are re-sealed under their existing sibling refs.
    let rt = h
        .secrets
        .get(&SecretRef(format!("sec:refresh:{}", source_id.0)))
        .await
        .unwrap();
    assert_eq!(rt.expose_secret(), "rt-new");
    let cs = h
        .secrets
        .get(&SecretRef(format!("sec:client:{}", source_id.0)))
        .await
        .unwrap();
    assert_eq!(cs.expose_secret(), "cs-new");
    // The binding the session refresher reads now reflects the new scheme.
    let binding = h.state.mcp_refresh_for_source(&source_id).unwrap();
    assert_eq!(
        binding.token_endpoint_auth,
        TokenEndpointAuthBinding::ClientSecretPost {
            secret_ref: SecretRef(format!("sec:client:{}", source_id.0))
        }
    );

    // Updating refresh on a credential that has none is a 400.
    let plain = create_mcp_oauth(&h, &vault_id, url, None).await;
    let plain_id = plain["id"].as_str().unwrap().to_string();
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{plain_id}"),
        Some(json!({ "auth": { "type": "mcp_oauth", "refresh": { "refresh_token": "x" } } })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_refresh_token_endpoint_auth_omitting_client_secret_keeps_the_sealed_one() {
    // The SDK's *update* token_endpoint_auth makes `client_secret` OPTIONAL: a
    // caller may switch/keep the scheme without resending the secret. This must
    // NOT 400 (the create shape requires the secret), and the previously sealed
    // secret must be preserved.
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;
    let url = "https://mcp.example.com/sse";
    let cred = create_mcp_oauth(
        &h,
        &vault_id,
        url,
        Some(json!({
            "client_id": "cli",
            "refresh_token": "rt", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "client_secret_basic", "client_secret": "cs-orig" } // awaken-allow: secret
        })),
    )
    .await;
    let cred_id = cred["id"].as_str().unwrap().to_string();
    let source_id = h.state.credential_source_id(&vault_id, &cred_id).unwrap();

    // Switch scheme to `post` WITHOUT a client_secret — a valid SDK payload.
    let (s, updated) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        Some(json!({
            "auth": {
                "type": "mcp_oauth",
                "refresh": { "token_endpoint_auth": { "type": "client_secret_post" } }
            }
        })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "omitting client_secret on update must not 400"
    );
    assert_eq!(
        updated["auth"]["refresh"]["token_endpoint_auth"]["type"],
        "client_secret_post"
    );
    // The original sealed client secret is preserved (not wiped), and the binding
    // still points at the stable ref for the new scheme.
    let cs = h
        .secrets
        .get(&SecretRef(format!("sec:client:{}", source_id.0)))
        .await
        .unwrap();
    assert_eq!(cs.expose_secret(), "cs-orig");
    let binding = h.state.mcp_refresh_for_source(&source_id).unwrap();
    assert_eq!(
        binding.token_endpoint_auth,
        TokenEndpointAuthBinding::ClientSecretPost {
            secret_ref: SecretRef(format!("sec:client:{}", source_id.0))
        }
    );
}

#[tokio::test]
async fn update_credential_covers_static_and_mcp_auth_branches() {
    let h = harness();
    let vault_id = create_vault(&h, "v").await;
    let url = "https://mcp.example.com/sse";

    // static_bearer: rotate the token (phase-2 primary re-seal for a bearer).
    let (_, bearer) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({ "type": "static_bearer", "mcp_server_url": url, "token": "brr-old" })), // awaken-allow: secret
    )
    .await;
    let bearer_id = bearer["id"].as_str().unwrap().to_string();
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{bearer_id}"),
        Some(json!({ "auth": { "type": "static_bearer", "token": "brr-new" } })), // awaken-allow: secret
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let bearer_source = h.state.credential_source_id(&vault_id, &bearer_id).unwrap();
    let bearer_row = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&bearer_source).await.unwrap()
    };
    assert_eq!(
        awaken_credential_vault::materialize(&bearer_row, &*h.secrets)
            .await
            .unwrap()
            .expose_secret(),
        "brr-new"
    );

    // mcp_oauth WITH refresh: rotate access_token + expires_at, and drive the
    // `none` then `client_secret_basic` refresh-scheme update branches (the
    // SDK-driven test only reached `client_secret_post`).
    let oauth = create_mcp_oauth(
        &h,
        &vault_id,
        url,
        Some(json!({
            "client_id": "cli",
            "refresh_token": "rt", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "none" }
        })),
    )
    .await;
    let oauth_id = oauth["id"].as_str().unwrap().to_string();

    let (s, u1) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{oauth_id}"),
        Some(json!({
            "auth": {
                "type": "mcp_oauth",
                "access_token": "at-new", // awaken-allow: secret
                "expires_at": "2030-01-01T00:00:00Z",
                "refresh": { "scope": "s2", "token_endpoint_auth": { "type": "none" } }
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(u1["auth"]["expires_at"], "2030-01-01T00:00:00Z");
    assert_eq!(u1["auth"]["refresh"]["scope"], "s2");
    assert_eq!(u1["auth"]["refresh"]["token_endpoint_auth"]["type"], "none");

    let (s, u2) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{oauth_id}"),
        Some(json!({
            "auth": {
                "type": "mcp_oauth",
                "refresh": { "token_endpoint_auth": { "type": "client_secret_basic", "client_secret": "cs-b" } } // awaken-allow: secret
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        u2["auth"]["refresh"]["token_endpoint_auth"]["type"],
        "client_secret_basic"
    );

    // The rotated access token re-sealed under the row's material_ref.
    let oauth_source = h.state.credential_source_id(&vault_id, &oauth_id).unwrap();
    let oauth_row = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&oauth_source).await.unwrap()
    };
    assert_eq!(
        awaken_credential_vault::materialize(&oauth_row, &*h.secrets)
            .await
            .unwrap()
            .expose_secret(),
        "at-new"
    );
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

    // The cap spans all credential types, not just env-vars.
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "brr" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a static_bearer must count against the same 20-cap"
    );
}

/// Enter an mcp_oauth credential against `url` (optionally with a refresh object)
/// and return the full response body.
async fn create_mcp_oauth(h: &Harness, vault_id: &str, url: &str, refresh: Option<Value>) -> Value {
    let mut body = json!({
        "type": "mcp_oauth",
        "mcp_server_url": url,
        "access_token": "at-secret-token" // awaken-allow: secret
    });
    if let Some(r) = refresh {
        body["refresh"] = r;
    }
    let (s, cred) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    cred
}

#[tokio::test]
async fn static_bearer_credential_is_secret_free_and_round_trips() {
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;

    let (s, cred) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "brr-bearer-secret", // awaken-allow: secret
            "display_name": "linear bearer"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(cred["type"], "vault_credential");
    assert_eq!(cred["auth"]["type"], "static_bearer");
    assert_eq!(
        cred["auth"]["mcp_server_url"],
        "https://mcp.example.com/sse"
    );
    // The auth projection is URL-only per the SDK response shape: no token field
    // at all, let alone the value.
    let raw = serde_json::to_string(&cred).unwrap();
    assert!(!raw.contains("brr-bearer-secret"));
    assert!(!raw.contains("\"token\""));
    let cred_id = cred["id"].as_str().unwrap().to_string();

    // Retrieve round-trips the same secret-free projection.
    let (s, got) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["auth"]["type"], "static_bearer");
    assert_eq!(got["auth"]["mcp_server_url"], "https://mcp.example.com/sse");
    assert_eq!(got["display_name"], "linear bearer");
    assert!(
        !serde_json::to_string(&got)
            .unwrap()
            .contains("brr-bearer-secret")
    );

    // The token is sealed in the domain: the row is secret-free but materializes.
    let source_id = h.state.credential_source_id(&vault_id, &cred_id).unwrap();
    let source = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&source_id).await.unwrap()
    };
    assert!(
        !serde_json::to_string(&source)
            .unwrap()
            .contains("brr-bearer-secret")
    );
    let secret = awaken_credential_vault::materialize(&source, &*h.secrets)
        .await
        .unwrap();
    assert_eq!(secret.expose_secret(), "brr-bearer-secret");

    // No refresh token to report; no live probe yet, so status stays unknown.
    let (s, validation) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(validation["has_refresh_token"], false);
    assert_eq!(validation["status"], "unknown");
}

#[tokio::test]
async fn mcp_oauth_credential_with_refresh_never_leaks_secrets() {
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;

    let cred = create_mcp_oauth(
        &h,
        &vault_id,
        "https://mcp.example.com/sse",
        Some(json!({
            "client_id": "cli_1",
            "refresh_token": "rt-secret-token", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "client_secret_basic", "client_secret": "cs-secret" }, // awaken-allow: secret
            "scope": "mcp:read"
        })),
    )
    .await;
    assert_eq!(cred["auth"]["type"], "mcp_oauth");
    assert_eq!(
        cred["auth"]["mcp_server_url"],
        "https://mcp.example.com/sse"
    );
    // The refresh projection is configuration-only: scheme tag, no secrets.
    assert_eq!(cred["auth"]["refresh"]["client_id"], "cli_1");
    assert_eq!(
        cred["auth"]["refresh"]["token_endpoint"],
        "https://auth.example.com/token"
    );
    assert_eq!(
        cred["auth"]["refresh"]["token_endpoint_auth"]["type"],
        "client_secret_basic"
    );
    assert_eq!(cred["auth"]["refresh"]["scope"], "mcp:read");
    // None of the three wire secrets appears anywhere in the raw body.
    let raw = serde_json::to_string(&cred).unwrap();
    assert!(!raw.contains("at-secret-token"));
    assert!(!raw.contains("rt-secret-token"));
    assert!(!raw.contains("cs-secret"));
    let cred_id = cred["id"].as_str().unwrap().to_string();

    // Retrieve is equally secret-free.
    let (s, got) = call(
        &h.app,
        "GET",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let raw = serde_json::to_string(&got).unwrap();
    assert!(!raw.contains("at-secret-token"));
    assert!(!raw.contains("rt-secret-token"));
    assert!(!raw.contains("cs-secret"));

    // Validate reports the stored refresh-token fact; status stays unknown (no
    // live probe in this slice) — and leaks nothing either.
    let (s, validation) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(validation["has_refresh_token"], true);
    assert_eq!(validation["status"], "unknown");
    let raw = serde_json::to_string(&validation).unwrap();
    assert!(!raw.contains("at-secret-token"));
    assert!(!raw.contains("rt-secret-token"));
    assert!(!raw.contains("cs-secret"));

    // The client secret was SEALED (not dropped): it materializes back through
    // the store under the deterministic `sec:client:{source_id}` ref, so the
    // confidential-client refresh grant can authenticate later.
    let source_id = h.state.credential_source_id(&vault_id, &cred_id).unwrap();
    let sealed = h
        .secrets
        .get(&SecretRef(format!("sec:client:{}", source_id.0)))
        .await
        .expect("the client secret is sealed under sec:client:{source_id}");
    assert_eq!(sealed.expose_secret(), "cs-secret");
}

#[tokio::test]
async fn mcp_oauth_credential_without_refresh_reports_no_refresh_token() {
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;

    let cred = create_mcp_oauth(&h, &vault_id, "https://mcp.example.com/sse", None).await;
    // No refresh object in -> no refresh projection out (omitted, not null-ish).
    assert!(cred["auth"].get("refresh").is_none());
    assert!(
        !serde_json::to_string(&cred)
            .unwrap()
            .contains("at-secret-token")
    );
    let cred_id = cred["id"].as_str().unwrap().to_string();

    let (s, validation) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(validation["has_refresh_token"], false);
    assert_eq!(validation["status"], "unknown");
}

#[tokio::test]
async fn mcp_credential_source_for_url_binds_by_vault_and_exact_url() {
    let h = harness();
    let vault_a = create_vault(&h, "a").await;
    let vault_b = create_vault(&h, "b").await;
    let url = "https://mcp.example.com/sse";

    // Vault A holds an mcp_oauth credential for `url`, plus two decoys that must
    // never match: an env-var credential and a static_bearer against the same URL.
    let oauth = create_mcp_oauth(&h, &vault_a, url, None).await;
    let oauth_id = oauth["id"].as_str().unwrap().to_string();
    create_credential(&h, &vault_a, "K").await;
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_a}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": url,
            "token": "brr" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The seam returns the oauth credential's domain source id, not a decoy's.
    let expected = h.state.credential_source_id(&vault_a, &oauth_id).unwrap();
    let got = h
        .state
        .mcp_credential_source_for_url(std::slice::from_ref(&vault_a), url)
        .expect("mcp_oauth credential binds by URL");
    assert_eq!(got, expected);

    // Wrong vault, wrong url, or a session bound to no vaults -> no binding.
    assert!(
        h.state
            .mcp_credential_source_for_url(std::slice::from_ref(&vault_b), url)
            .is_none()
    );
    assert!(
        h.state
            .mcp_credential_source_for_url(
                std::slice::from_ref(&vault_a),
                "https://other.example.com/sse"
            )
            .is_none()
    );
    assert!(h.state.mcp_credential_source_for_url(&[], url).is_none());

    // A vault list spanning both vaults still finds it (vault B contributes none).
    let both = vec![vault_b, vault_a];
    assert_eq!(
        h.state.mcp_credential_source_for_url(&both, url),
        Some(expected)
    );
}

#[tokio::test]
async fn validate_with_probe_reports_valid_and_the_probe_sees_the_materialized_token() {
    let probe = FakeProbe::new(McpProbeStatus::Valid);
    let h = harness_with_probe(probe.clone());
    let vault_id = create_vault(&h, "mcp").await;
    let url = "https://mcp.example.com/sse";
    let cred = create_mcp_oauth(&h, &vault_id, url, None).await;
    let cred_id = cred["id"].as_str().unwrap().to_string();

    let (s, validation) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(validation["status"], "valid");
    assert_eq!(validation["mcp_probe"], json!({ "handshake": "ok" }));
    // The probe was handed the server URL and the MATERIALIZED access token —
    // the resolved secret, never a `sec:` vault ref.
    let seen = probe.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, url);
    assert_eq!(seen[0].1, "at-secret-token");
    // The validation body itself stays secret-free.
    assert!(
        !serde_json::to_string(&validation)
            .unwrap()
            .contains("at-secret-token")
    );
}

#[tokio::test]
async fn validate_with_probe_reports_invalid_with_the_http_status() {
    let probe = FakeProbe::new(McpProbeStatus::Invalid { http_status: 401 });
    let h = harness_with_probe(probe);
    let vault_id = create_vault(&h, "mcp").await;
    let cred = create_mcp_oauth(&h, &vault_id, "https://mcp.example.com/sse", None).await;
    let cred_id = cred["id"].as_str().unwrap().to_string();

    let (s, validation) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(validation["status"], "invalid");
    assert_eq!(validation["mcp_probe"], json!({ "http_status": 401 }));
}

#[tokio::test]
async fn validate_never_probes_env_var_or_static_bearer_credentials() {
    // Even with a probe wired and answering Valid, only `mcp_oauth` is probed:
    // env-var / static_bearer have no MCP handshake, so they stay `unknown`.
    let probe = FakeProbe::new(McpProbeStatus::Valid);
    let h = harness_with_probe(probe.clone());
    let vault_id = create_vault(&h, "mixed").await;
    let env_id = create_credential(&h, &vault_id, "K").await;
    let (s, bearer) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "brr" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let bearer_id = bearer["id"].as_str().unwrap().to_string();

    for cred_id in [env_id, bearer_id] {
        let (s, validation) = call(
            &h.app,
            "POST",
            &format!("/v1/vaults/{vault_id}/credentials/{cred_id}/mcp_oauth_validate"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(validation["status"], "unknown");
        assert_eq!(validation["mcp_probe"], Value::Null);
    }
    assert!(
        probe.seen.lock().unwrap().is_empty(),
        "the probe must never run for env-var / static_bearer credentials"
    );
}

#[tokio::test]
async fn mcp_refresh_for_source_exposes_public_and_confidential_refresh() {
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;
    let url = "https://mcp.example.com/sse";

    // A public-client refresh (`token_endpoint_auth: none`) is exposed in full.
    let refreshable = create_mcp_oauth(
        &h,
        &vault_id,
        url,
        Some(json!({
            "client_id": "cli_pub",
            "refresh_token": "rt-secret-token", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "none" },
            "scope": "mcp:read",
            "resource": "https://mcp.example.com"
        })),
    )
    .await;
    let refreshable_id = refreshable["id"].as_str().unwrap().to_string();
    let source_id = h
        .state
        .credential_source_id(&vault_id, &refreshable_id)
        .unwrap();
    let binding = h
        .state
        .mcp_refresh_for_source(&source_id)
        .expect("a public-client refresh is exposed");
    assert_eq!(binding.token_endpoint, "https://auth.example.com/token");
    assert_eq!(binding.client_id, "cli_pub");
    assert_eq!(binding.scope.as_deref(), Some("mcp:read"));
    assert_eq!(binding.resource.as_deref(), Some("https://mcp.example.com"));
    assert_eq!(binding.token_endpoint_auth, TokenEndpointAuthBinding::None);
    // The refresh token itself stays sealed: the binding carries only its ref.
    assert_eq!(
        binding.refresh_token_ref,
        SecretRef(format!("sec:refresh:{}", source_id.0))
    );

    // An mcp_oauth credential entered WITHOUT a refresh object yields none.
    let plain = create_mcp_oauth(&h, &vault_id, url, None).await;
    let plain_id = plain["id"].as_str().unwrap().to_string();
    let plain_source = h.state.credential_source_id(&vault_id, &plain_id).unwrap();
    assert!(h.state.mcp_refresh_for_source(&plain_source).is_none());

    // A confidential-client scheme is exposed too: its client_secret was sealed
    // at create, so the binding carries the sealed ref for the grant's client
    // authentication (basic → the Basic header, post → the form field).
    for auth_type in ["client_secret_basic", "client_secret_post"] {
        let confidential = create_mcp_oauth(
            &h,
            &vault_id,
            url,
            Some(json!({
                "client_id": "cli_conf",
                "refresh_token": "rt-secret-token", // awaken-allow: secret
                "token_endpoint": "https://auth.example.com/token",
                "token_endpoint_auth": { "type": auth_type, "client_secret": "cs" } // awaken-allow: secret
            })),
        )
        .await;
        let confidential_id = confidential["id"].as_str().unwrap().to_string();
        let confidential_source = h
            .state
            .credential_source_id(&vault_id, &confidential_id)
            .unwrap();
        let binding = h
            .state
            .mcp_refresh_for_source(&confidential_source)
            .expect("a confidential-client refresh is exposed");
        assert_eq!(binding.client_id, "cli_conf");
        // The auth binding carries the sealed client secret's ref — the
        // deterministic `sec:client:{source_id}` shape, never material.
        let secret_ref = SecretRef(format!("sec:client:{}", confidential_source.0));
        let expected = match auth_type {
            "client_secret_basic" => TokenEndpointAuthBinding::ClientSecretBasic { secret_ref },
            _ => TokenEndpointAuthBinding::ClientSecretPost { secret_ref },
        };
        assert_eq!(binding.token_endpoint_auth, expected, "{auth_type}");
    }

    // Env-var and static_bearer rows never carry a refresh configuration.
    let env_id = create_credential(&h, &vault_id, "K").await;
    let env_source = h.state.credential_source_id(&vault_id, &env_id).unwrap();
    assert!(h.state.mcp_refresh_for_source(&env_source).is_none());
    let (s, bearer) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": url,
            "token": "brr" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let bearer_id = bearer["id"].as_str().unwrap().to_string();
    let bearer_source = h.state.credential_source_id(&vault_id, &bearer_id).unwrap();
    assert!(h.state.mcp_refresh_for_source(&bearer_source).is_none());
}
