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
};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_model_catalog::repo::InMemoryCatalogRepo;
use awaken_model_catalog::{
    ModelApiCompat, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_protocol_managed::{McpProbe, McpProbeStatus};
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
async fn mcp_refresh_for_source_exposes_only_public_client_refresh() {
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

    // A confidential-client scheme yields none: its client_secret was consumed
    // at create, so the exchange could never authenticate.
    let confidential = create_mcp_oauth(
        &h,
        &vault_id,
        url,
        Some(json!({
            "client_id": "cli_conf",
            "refresh_token": "rt-secret-token", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "client_secret_basic", "client_secret": "cs" } // awaken-allow: secret
        })),
    )
    .await;
    let confidential_id = confidential["id"].as_str().unwrap().to_string();
    let confidential_source = h
        .state
        .credential_source_id(&vault_id, &confidential_id)
        .unwrap();
    assert!(
        h.state
            .mcp_refresh_for_source(&confidential_source)
            .is_none()
    );

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
