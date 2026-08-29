//! The Managed vault/credential front door over HTTP: create a vault, enter
//! `environment_variable` / `static_bearer` / `mcp_oauth` credentials (secrets
//! write-only), retrieve them secret-free, and confirm an unscoped env-var
//! credential cannot be misused for provider inference. Also covers the wire constraints (unknown vault 404,
//! duplicate key rejected, unknown auth type 400) and the vault→MCP URL-binding
//! seam (`mcp_credential_source_for_url`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::resolve_inference;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_contract::TokenEndpointAuth;
use awaken_credential_vault::catalog::{ManagedVaultDeletionPhase, ManagedVaultRepo};
use awaken_credential_vault::repo::{
    InMemoryCredentialRepo, ManagedCredentialAdoptionProgress, ManagedCredentialRepository,
    reconcile_managed_vault_deletions,
};
use awaken_credential_vault::{
    CredentialBinding, CredentialSource, InMemorySecretStore, SecretStore,
};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_model_catalog::repo::InMemoryCatalogRepo;
use awaken_model_catalog::{
    ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
};
use awaken_protocol_managed::{VaultState, vault_router};
use awaken_session_application::SessionCredentialSource;
use awaken_session_contract::{McpProbe, McpProbeStatus};
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

#[derive(Default)]
struct RecordingEnvelopeIssuer {
    issued: Mutex<Vec<(String, u64, String)>>,
}

#[derive(Default)]
struct RecordingCustodian {
    published: Mutex<Vec<(String, u64, String)>>,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialCustodian for RecordingCustodian {
    fn handles(
        &self,
        _selected_holder: &awaken_credential_contract::PlaintextHolder,
        _usage: &awaken_credential_contract::CredentialUsage,
    ) -> bool {
        true
    }

    async fn publish(
        &self,
        publication: awaken_credential_contract::CredentialCustodyPublication,
    ) -> Result<(), String> {
        self.published.lock().unwrap().push((
            publication.access.credential.id,
            publication.access.credential.revision,
            publication.material.expose_secret().to_owned(),
        ));
        Ok(())
    }
}

struct SelectiveCustodian {
    holder: awaken_credential_contract::PlaintextHolder,
    usage: awaken_credential_contract::CredentialUsage,
    published: Mutex<usize>,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialCustodian for SelectiveCustodian {
    fn handles(
        &self,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        usage: &awaken_credential_contract::CredentialUsage,
    ) -> bool {
        selected_holder == &self.holder && usage == &self.usage
    }

    async fn publish(
        &self,
        _publication: awaken_credential_contract::CredentialCustodyPublication,
    ) -> Result<(), String> {
        *self.published.lock().unwrap() += 1;
        Ok(())
    }
}

enum AdversarialEnvelopeResponse {
    SubstitutedPayload,
    Failure,
}

struct AdversarialEnvelopeIssuer(AdversarialEnvelopeResponse);

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialEnvelopeIssuer for RecordingEnvelopeIssuer {
    async fn issue(
        &self,
        request: awaken_credential_contract::CredentialEnvelopeIssuance,
    ) -> Result<awaken_credential_contract::CredentialEnvelope, String> {
        self.issued.lock().unwrap().push((
            request.access.credential.id.clone(),
            request.access.credential.revision,
            request.material.expose_secret().to_string(),
        ));
        Ok(
            awaken_credential_contract::CredentialEnvelope::SealedForWorker {
                envelope_ref: awaken_credential_contract::SealedCredentialEnvelopeRef {
                    id: "test-envelope".into(),
                    payload_fingerprint:
                        awaken_credential_contract::credential_envelope_payload_fingerprint(
                            &request.access,
                            &request.selected_holder,
                            &request.binding,
                        ),
                },
                recipient: request.selected_holder.trust_domain,
                expires_at_unix_ms: u64::MAX,
            },
        )
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialEnvelopeIssuer for AdversarialEnvelopeIssuer {
    async fn issue(
        &self,
        request: awaken_credential_contract::CredentialEnvelopeIssuance,
    ) -> Result<awaken_credential_contract::CredentialEnvelope, String> {
        match self.0 {
            AdversarialEnvelopeResponse::Failure => Err("issuer unavailable".into()),
            AdversarialEnvelopeResponse::SubstitutedPayload => Ok(
                awaken_credential_contract::CredentialEnvelope::SealedForWorker {
                    envelope_ref: awaken_credential_contract::SealedCredentialEnvelopeRef {
                        id: "substituted-envelope".into(),
                        payload_fingerprint: "substituted-payload".into(),
                    },
                    recipient: request.selected_holder.trust_domain,
                    expires_at_unix_ms: u64::MAX,
                },
            ),
        }
    }
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
    call_in_workspace(app, Some("default"), method, uri, body).await
}

async fn call_in_workspace(
    app: &Router,
    workspace_id: Option<&str>,
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
    let mut request = b.body(body).unwrap();
    if let Some(workspace_id) = workspace_id {
        request
            .extensions_mut()
            .insert(awaken_tenancy::WorkspaceScope(workspace_id.to_string()));
    }
    let resp = app.clone().oneshot(request).await.unwrap();
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
async fn every_vault_and_credential_route_hides_another_workspaces_ids() {
    let h = harness();
    let create = json!({"display_name":"owned","metadata":{}});
    let (status, vault) = call_in_workspace(
        &h.app,
        Some("workspace-a"),
        "POST",
        "/v1/vaults",
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap();
    let create_credential = json!({
        "type":"environment_variable",
        "secret_name":"TOKEN",
        "secret_value":"secret",
        "networking":{"type":"unrestricted"},
        "metadata":{}
    });
    let credential_uri = format!("/v1/vaults/{vault_id}/credentials");
    let (status, credential) = call_in_workspace(
        &h.app,
        Some("workspace-a"),
        "POST",
        &credential_uri,
        Some(create_credential.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let credential_id = credential["id"].as_str().unwrap();
    let item_uri = format!("{credential_uri}/{credential_id}");

    for (method, uri, body) in [
        ("GET", format!("/v1/vaults/{vault_id}"), None),
        ("POST", format!("/v1/vaults/{vault_id}"), Some(json!({}))),
        ("POST", format!("/v1/vaults/{vault_id}/archive"), None),
        ("DELETE", format!("/v1/vaults/{vault_id}"), None),
        ("GET", credential_uri.clone(), None),
        ("POST", credential_uri.clone(), Some(create_credential)),
        ("GET", item_uri.clone(), None),
        ("POST", item_uri.clone(), Some(json!({}))),
        ("POST", format!("{item_uri}/archive"), None),
        ("DELETE", item_uri.clone(), None),
        ("POST", format!("{item_uri}/mcp_oauth_validate"), None),
    ] {
        let (status, _) = call_in_workspace(&h.app, Some("workspace-b"), method, &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }

    let (status, _) = call_in_workspace(
        &h.app,
        Some("workspace-a"),
        "GET",
        &format!("/v1/vaults/{vault_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "cross-workspace attempts mutated owner data"
    );
}

/// Session-selection decision rules after Control admission: C1 the requested
/// Workspace owns the stable Vault and normalized target; C2 another Workspace
/// presents those opaque ids; C3 a positive generation creates, exactly
/// replays, or monotonically rotates while no rollout target is installed.
/// Effects: E1 the owner selects and pins the exact revision, E2 the other
/// Workspace observes neither Vault nor source, E3 create/replay have no
/// predecessor and are converged, E4 a newer generation advances once and
/// remains pending. Rules: V1 C1+C3 generation 1 create/exact replay -> E1+E3;
/// V2 C1+C3 generation 2 -> E1+E4; V3 C2 -> E2. The Control HTTP matrix covers
/// zero/stale/conflicting generations, subsequent target states, and exact
/// acknowledgement.
#[tokio::test]
async fn hosted_application_bearer_is_stable_rotatable_and_session_selectable() {
    let h = harness();
    let (vault_id, source_id, revision, adoption) = h
        .state
        .enter_application_mcp_bearer(
            "workspace-a",
            "awaken-flow",
            "HTTPS://FLOW.EXAMPLE.TEST:443/mcp/",
            "flow-bearer-1",
            1,
            RedactedString::new("token-1"),
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);
    assert_eq!(adoption, ManagedCredentialAdoptionProgress::Converged, "V1");
    let (replayed_vault, replayed_source, replayed_revision, replayed_adoption) = h
        .state
        .enter_application_mcp_bearer(
            "workspace-a",
            "awaken-flow",
            "https://flow.example.test/mcp",
            "flow-bearer-1",
            1,
            RedactedString::new("token-1"),
        )
        .await
        .unwrap();
    assert_eq!(
        (replayed_vault, replayed_source, replayed_revision),
        (vault_id.clone(), source_id.clone(), 1)
    );
    assert_eq!(
        replayed_adoption,
        ManagedCredentialAdoptionProgress::Converged,
        "V1"
    );
    let (_, rotated_source, rotated_revision, rotated_adoption) = h
        .state
        .enter_application_mcp_bearer(
            "workspace-a",
            "awaken-flow",
            "https://flow.example.test/mcp",
            "flow-bearer-2",
            2,
            RedactedString::new("token-2"),
        )
        .await
        .unwrap();
    assert_eq!(rotated_source, source_id);
    assert_eq!(rotated_revision, 2);
    assert_eq!(
        rotated_adoption,
        ManagedCredentialAdoptionProgress::Pending,
        "V2"
    );

    assert!(
        SessionCredentialSource::has_vault(h.state.as_ref(), "workspace-a", &vault_id)
            .await
            .unwrap(),
        "H5 hosted Vault is visible from its credential aggregate"
    );
    assert!(
        !SessionCredentialSource::has_vault(h.state.as_ref(), "workspace-b", &vault_id)
            .await
            .unwrap(),
        "H5 hosted Vault is tenant scoped"
    );
    let source = SessionCredentialSource::mcp_credential_source_for_url(
        h.state.as_ref(),
        "workspace-a",
        std::slice::from_ref(&vault_id),
        "https://FLOW.example.test:443/mcp/",
    )
    .await
    .unwrap()
    .expect("H5 exact hosted source");
    assert_eq!(source, source_id);
    assert!(
        SessionCredentialSource::mcp_credential_source_for_url(
            h.state.as_ref(),
            "workspace-b",
            &[vault_id],
            "https://flow.example.test/mcp",
        )
        .await
        .unwrap()
        .is_none(),
        "H5 source selection is tenant scoped"
    );
    let holder =
        awaken_credential_contract::CredentialRealizationProfile::self_hosted_native().mcp_holder;
    let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        "workspace-a",
        &"https://flow.example.test/mcp",
        &usage,
    );
    let target =
        awaken_session_contract::McpTarget::parse_http("https://flow.example.test/mcp").unwrap();
    assert_eq!(
        SessionCredentialSource::mcp_access_for_source(
            h.state.as_ref(),
            &source,
            "workspace-a",
            &target,
            &holder,
            &binding,
        )
        .await
        .unwrap()
        .credential
        .revision,
        2
    );
}

// Test design: replacement_control_instance_reads_the_same_vault_authority
// Cause/effect graph: a replacement control instance reopens the same durable Vault aggregate without process-local truth.
// Decision table: same workspace+id=identical secret-free projection; other workspace/unknown=404.
#[tokio::test]
async fn replacement_control_instance_reads_the_same_vault_authority() {
    // Cause/effect decision table for rolling Control replacement:
    // | Rule | first instance state | replacement request | Effect |
    // | R1 | vault + static bearer committed | retrieve/list | identical secret-free projection |
    // | R2 | R1 | Session exact URL selection | same CredentialSourceId |
    // | R3 | R1 | archived/deleted catalog entry | replacement rejects selection (covered by lifecycle tests) |
    // The replacement receives only the shared repositories; no cache snapshot,
    // source-id prefix fallback, or plaintext transfer is available.
    let h = harness();
    let vault_id = create_vault(&h, "rolling").await;
    let credential = create_mcp_oauth(&h, &vault_id, "https://mcp.example.com/mcp", None).await;
    let credential_id = credential["id"].as_str().unwrap();
    let expected_source = h
        .state
        .credential_source_id(&vault_id, credential_id)
        .await
        .unwrap();

    let replacement = Arc::new(VaultState::new(h.secrets.clone(), h.credentials.clone()));
    let replacement_app = vault_router(replacement.clone());
    let (status, retrieved) = call(
        &replacement_app,
        "GET",
        &format!("/v1/vaults/{vault_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "R1");
    assert_eq!(retrieved["id"], vault_id, "R1");
    assert_eq!(
        SessionCredentialSource::mcp_credential_source_for_url(
            replacement.as_ref(),
            "default",
            std::slice::from_ref(&vault_id),
            "https://MCP.example.com:443/mcp/",
        )
        .await
        .unwrap(),
        Some(expected_source),
        "R2"
    );
}

// Test design: vault_credential_lifecycle_and_resolution
// Cause/effect graph: Vault/Credential creation seals one secret, resolves authorized material, and returns only public metadata.
// Decision table: valid owner+kind=resolve; unknown/cross-owner=404; invalid target=4xx; no response leaks plaintext.
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

    // The vault credential is a real material-backed domain row, but the public
    // environment_variable shape carries no provider scope. Its sealed material
    // remains retrievable through the Vault port while inference resolution must
    // reject cross-provider use instead of guessing from the variable name.
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .await
        .expect("vault credential maps to a domain source");
    let catalog = seed_catalog(&h).await;
    let source: CredentialSource = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&source_id).await.unwrap()
    };
    assert_eq!(
        h.secrets
            .get(source.material_ref.as_ref().expect("sealed material ref"))
            .await
            .unwrap()
            .expose_secret(),
        "sk-vault-secret"
    );
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(source.id.0.clone(), source);
    let error = resolve_inference(
        &catalog,
        "claude-opus-4-8",
        &CredentialBinding::Exact {
            credential_source_id: CredentialSourceId(source_id.0.clone()),
        },
        &sources,
        &*h.secrets,
    )
    .await
    .expect_err("provider-unscoped environment credential must fail closed");
    assert!(matches!(
        error,
        awaken_config_resolver::ResolveError::IncompatibleCredential { .. }
    ));
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
        source: Default::default(),
        status: Default::default(),
        last_seen_at_unix_ms: None,
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

    for invalid_url in [
        "not-a-url",
        "file:///tmp/mcp.sock",
        "https://user:password@mcp.example.com/mcp",
        "https://mcp.example.com/mcp#fragment",
    ] {
        let (status, _) = call(
            &h.app,
            "POST",
            &format!("/v1/vaults/{vault_id}/credentials"),
            Some(json!({
                "type": "static_bearer",
                "mcp_server_url": invalid_url,
                "token": "x" // awaken-allow: secret
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "invalid credential target must fail closed: {invalid_url}"
        );
    }
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

// Test design: delete_vault_fences_children_and_converges_after_rollout_ack
// Cause/effect graph: delete fences child credentials, waits for rollout acknowledgement, then commits one terminal Vault result.
// Decision table: active+ack=delete; pending ack=non-visible transition; stale/wrong owner=conflict; repeated=404.
#[tokio::test]
async fn delete_vault_fences_children_and_converges_after_rollout_ack() {
    let h = harness();
    let vault_id = create_vault(&h, "doomed").await;
    let cred_id = create_credential(&h, &vault_id, "K").await;

    let (s, deleted) = call(&h.app, "DELETE", &format!("/v1/vaults/{vault_id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(deleted["type"], "vault_deleted");
    assert_eq!(deleted["id"], vault_id);

    // The durable root fence hides both aggregate surfaces immediately, before
    // rollout delivery permits the root tombstone to complete.
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
    assert!(
        h.state
            .credential_source_id(&vault_id, &cred_id)
            .await
            .is_none()
    );

    let (s, listed) = call(&h.app, "GET", "/v1/vaults?include_archived=true", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        listed["data"]
            .as_array()
            .expect("vault page")
            .iter()
            .all(|vault| vault["id"] != vault_id),
        "a requested deletion is hidden even from the archived projection"
    );

    // The fence rejects every later child creation; no new child can race the
    // deletion supervisor after it observed the aggregate.
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": "AFTER_DELETE",
            "secret_value": "sk-after-delete", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A retry observes the same durable operation and remains a successful,
    // idempotent DELETE while rollout acknowledgement is still pending.
    let (s, retried) = call(&h.app, "DELETE", &format!("/v1/vaults/{vault_id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(retried["type"], "vault_deleted");
    assert_eq!(retried["id"], vault_id);

    let requested = h
        .credentials
        .get_vault("default", &vault_id)
        .await
        .unwrap()
        .expect("delete keeps the durable root tombstone");
    assert_eq!(
        requested.deletion.as_ref().map(|deletion| deletion.phase),
        Some(ManagedVaultDeletionPhase::Requested),
        "rollout must be acknowledged before root completion"
    );
    let pending_roots = h
        .credentials
        .pending_managed_vault_deletions()
        .await
        .unwrap();
    assert_eq!(
        pending_roots
            .iter()
            .map(|vault| vault.id.as_str())
            .collect::<Vec<_>>(),
        vec![vault_id.as_str()]
    );

    let rollouts = h.credentials.pending_managed_rollouts().await.unwrap();
    assert_eq!(rollouts.len(), 1, "the deleted child publishes one rollout");
    assert_eq!(rollouts[0].vault_id, vault_id);
    for rollout in &rollouts {
        h.credentials
            .complete_managed_rollout(rollout)
            .await
            .unwrap();
    }
    assert_eq!(
        reconcile_managed_vault_deletions(h.secrets.as_ref(), h.credentials.as_ref())
            .await
            .unwrap(),
        1,
        "the supervisor completes the unblocked root"
    );
    let completed = h
        .credentials
        .get_vault("default", &vault_id)
        .await
        .unwrap()
        .expect("completed deletion remains as a durable tombstone");
    assert!(completed.is_deleted());
    assert!(
        h.credentials
            .pending_managed_vault_deletions()
            .await
            .unwrap()
            .is_empty()
    );

    // Completion is absorbing and remains idempotent at the HTTP boundary.
    let (s, _) = call(&h.app, "DELETE", &format!("/v1/vaults/{vault_id}"), None).await;
    assert_eq!(s, StatusCode::OK);

    // A never-existing Vault is still distinct from a replayed deletion.
    let (s, _) = call(&h.app, "DELETE", "/v1/vaults/vlt_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// Test design: list_vaults_returns_one_full_page_sorted_by_id
// Cause/effect graph: workspace-scoped live Vaults are deterministically sorted and wrapped in the official cursor envelope.
// Decision table: live=included once; archived excluded unless requested; foreign excluded; empty=valid empty page.
#[tokio::test]
async fn list_vaults_returns_one_full_page_sorted_by_id() {
    let h = harness();
    let a = create_vault(&h, "alpha").await;
    let b = create_vault(&h, "bravo").await;

    let (s, page) = call(&h.app, "GET", "/v1/vaults", None).await;
    assert_eq!(s, StatusCode::OK);
    // The SDK `PageCursor` shape is exactly data + next_page.
    assert!(page.get("has_more").is_none());
    assert!(page["next_page"].is_null());
    let data = page["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["type"], "vault");
    // Deterministic ascending-id order (`vlt_` is zero-padded == creation order).
    assert_eq!(data[0]["id"], a);
    assert_eq!(data[1]["id"], b);
}

// Test design: list_credentials_is_scoped_to_the_vault_and_404s_unknown
// Cause/effect graph: Vault identity and workspace jointly scope the credential page without revealing siblings or secrets.
// Decision table: known owned Vault=filtered page; foreign/unknown Vault=404; archived inclusion follows explicit flag.
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
    assert!(page.get("has_more").is_none());
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

// Test design: archive_vault_soft_deletes_and_hides_from_default_list
// Cause/effect graph: archive records one timestamp and removes the Vault from default live queries without hard deletion.
// Decision table: active=archive; archived=replay/terminal; default list=hides; include_archived=shows; unknown=404.
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
    let archived_revision = h
        .credentials
        .get_vault("default", &gone)
        .await
        .unwrap()
        .unwrap()
        .revision;
    let (s, replay) = call(&h.app, "POST", &format!("/v1/vaults/{gone}/archive"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(replay, archived);
    assert_eq!(
        h.credentials
            .get_vault("default", &gone)
            .await
            .unwrap()
            .unwrap()
            .revision,
        archived_revision,
        "archive replay is a true no-op"
    );

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

// Test design: archive_credential_soft_deletes_and_hides_from_list
// Cause/effect graph: credential archive retires selection and default visibility while preserving its secret-free audit projection.
// Decision table: active=archive; archived excluded by default/included explicitly; wrong Vault/unknown=404.
#[tokio::test]
async fn archive_credential_soft_deletes_and_hides_from_list() {
    let h = harness();
    let vault_id = create_vault(&h, "v").await;
    let keep = create_credential(&h, &vault_id, "KEEP").await;
    let gone = create_credential(&h, &vault_id, "GONE").await;
    let gone_source_id = h
        .state
        .credential_source_id(&vault_id, &gone)
        .await
        .unwrap();
    let gone_before = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&gone_source_id).await.unwrap()
    };
    let gone_ref = gone_before.material_ref.unwrap();

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
    // Cause/effect rule V1: archive keeps the wire audit projection but retires
    // the canonical source and physically reclaims its material.
    let gone_after = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&gone_source_id).await.unwrap()
    };
    assert_eq!(
        gone_after.status,
        awaken_credential_vault::CredentialStatus::Archived
    );
    assert!(gone_after.material_ref.is_none());
    assert!(h.secrets.get(&gone_ref).await.is_err());

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

// Test design: delete_credential_removes_one_and_scopes_by_vault
// Cause/effect graph: hard delete removes exactly one credential under its owning Vault and leaves siblings untouched.
// Decision table: correct Vault+id=delete; wrong Vault/unknown/repeated=404; sibling remains retrievable.
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
    assert!(h.state.credential_source_id(&vault_a, &c1).await.is_some());

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
    assert!(h.state.credential_source_id(&vault_a, &c1).await.is_none());

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

// Test design: update_vault_replaces_name_and_patches_metadata
// Cause/effect graph: update replaces display name and applies metadata patch to one durable Vault revision.
// Decision table: omitted=preserve; empty metadata value=remove; valid value=upsert; invalid/unknown=4xx/404.
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
    let updated_revision = h
        .credentials
        .get_vault("default", &vault_id)
        .await
        .unwrap()
        .unwrap()
        .revision;

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
    assert_eq!(
        h.credentials
            .get_vault("default", &vault_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        updated_revision,
        "empty update does not manufacture a revision"
    );

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

// Test design: update_credential_patches_fields_reseals_secret_and_rejects_type_change
// Cause/effect graph: mutable metadata/network fields patch in place while supplied secret material is atomically resealed.
// Decision table: omitted=preserve; explicit null=clear; new secret=rotate; credential kind change=400; unknown=404.
#[tokio::test]
async fn update_credential_patches_fields_reseals_secret_and_rejects_type_change() {
    let h = harness();
    let vault_id = create_vault(&h, "v").await;
    let cred_id = create_credential(&h, &vault_id, "ENVKEY").await;
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .await
        .unwrap();
    let before = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&source_id).await.unwrap()
    };
    let old_ref = before.material_ref.clone().unwrap();

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
    let source = {
        use awaken_credential_vault::repo::CredentialRepo;
        h.credentials.get(&source_id).await.unwrap()
    };
    assert_eq!(source.version, before.version + 1);
    assert_ne!(source.material_ref, before.material_ref);
    assert!(h.secrets.get(&old_ref).await.is_err());
    let secret = awaken_credential_vault::materialize(&source, &*h.secrets)
        .await
        .unwrap();
    assert_eq!(secret.expose_secret(), "rotated-secret");

    let child_revision = h
        .credentials
        .get_vault_credential("default", &cred_id)
        .await
        .unwrap()
        .unwrap()
        .revision;
    let rollout_count = h
        .credentials
        .pending_managed_rollouts()
        .await
        .unwrap()
        .len();
    let (s, replay) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{cred_id}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(replay, updated);
    assert_eq!(
        h.credentials
            .get_vault_credential("default", &cred_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        child_revision,
        "empty update does not manufacture a child revision"
    );
    assert_eq!(
        h.credentials
            .pending_managed_rollouts()
            .await
            .unwrap()
            .len(),
        rollout_count,
        "empty update does not manufacture a rollout"
    );

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

/// Test design — causal graph and decision table for the official Managed
/// Agents injection-location contract:
///
/// ```text
/// create JSON presence -> request DTO -> valid three-state domain value
///     -> durable credential row -> create/retrieve/update projection
///                                      ^                 |
///                                      +--- recovery ----+
/// ```
///
/// The create object has replacement semantics (an omitted object defaults both
/// locations to true, but an omitted member of a present object defaults false).
/// The update object has merge semantics (an omitted member preserves its current
/// value). Explicit null, unknown fields, and an effective `(false, false)` are
/// rejected before mutation. Pairwise coverage below crosses presence with both
/// booleans; the domain's Kani harness exhaustively proves the remaining boolean
/// combinations.
#[tokio::test]
async fn environment_credential_injection_location_is_exact_atomic_and_recoverable() {
    let h = harness();
    let vault_id = create_vault(&h, "injection-locations").await;

    let create_cases = [
        ("DEFAULT", None, json!({ "body": true, "header": true })),
        (
            "HEADER",
            Some(json!({ "header": true })),
            json!({ "body": false, "header": true }),
        ),
        (
            "BODY",
            Some(json!({ "body": true })),
            json!({ "body": true, "header": false }),
        ),
        (
            "BOTH",
            Some(json!({ "body": true, "header": true })),
            json!({ "body": true, "header": true }),
        ),
    ];
    let mut ids = HashMap::new();
    for (secret_name, injection_location, expected) in create_cases {
        let mut auth = json!({
            "type": "environment_variable",
            "secret_name": secret_name,
            "secret_value": "write-only", // awaken-allow: secret
            "networking": { "type": "unrestricted" }
        });
        if let Some(location) = injection_location {
            auth["injection_location"] = location;
        }
        let (status, credential) = call(
            &h.app,
            "POST",
            &format!("/v1/vaults/{vault_id}/credentials"),
            Some(auth),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create case {secret_name}");
        assert_eq!(
            credential["auth"]["injection_location"], expected,
            "create case {secret_name}"
        );
        ids.insert(
            secret_name,
            credential["id"].as_str().expect("credential id").to_owned(),
        );
    }

    let before_invalid = h
        .credentials
        .list_vault_credentials("default", &vault_id)
        .await
        .unwrap();
    for (name, location) in [
        ("empty object", json!({})),
        ("both disabled", json!({ "body": false, "header": false })),
        ("null member", json!({ "body": null, "header": true })),
        ("unknown member", json!({ "body": true, "other": true })),
    ] {
        let (status, _) = call(
            &h.app,
            "POST",
            &format!("/v1/vaults/{vault_id}/credentials"),
            Some(json!({
                "type": "environment_variable",
                "secret_name": format!("INVALID_{name}"),
                "secret_value": "never-stored", // awaken-allow: secret
                "networking": { "type": "unrestricted" },
                "injection_location": location,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
    }
    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "environment_variable",
            "secret_name": "NULL_OBJECT",
            "secret_value": "never-stored", // awaken-allow: secret
            "networking": { "type": "unrestricted" },
            "injection_location": null,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "null object");
    assert_eq!(
        h.credentials
            .list_vault_credentials("default", &vault_id)
            .await
            .unwrap(),
        before_invalid,
        "invalid creates are mutation-free"
    );

    let credential_id = ids["DEFAULT"].clone();
    let uri = format!("/v1/vaults/{vault_id}/credentials/{credential_id}");
    let update_cases = [
        (
            json!({ "body": false }),
            json!({ "body": false, "header": true }),
        ),
        (
            json!({ "body": true, "header": false }),
            json!({ "body": true, "header": false }),
        ),
        (
            json!({ "header": true }),
            json!({ "body": true, "header": true }),
        ),
    ];
    for (patch, expected) in update_cases {
        let (status, credential) = call(
            &h.app,
            "POST",
            &uri,
            Some(json!({
                "auth": {
                    "type": "environment_variable",
                    "injection_location": patch,
                }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(credential["auth"]["injection_location"], expected);
    }

    let before_rejected_update = h
        .credentials
        .get_vault_credential("default", &credential_id)
        .await
        .unwrap()
        .expect("credential");
    for (name, patch) in [
        (
            "effective both disabled",
            json!({ "body": false, "header": false }),
        ),
        ("empty update", json!({})),
        ("null member", json!({ "body": null })),
        ("unknown member", json!({ "other": true })),
    ] {
        let (status, _) = call(
            &h.app,
            "POST",
            &uri,
            Some(json!({
                "auth": {
                    "type": "environment_variable",
                    "injection_location": patch,
                }
            })),
        )
        .await;
        let expected = if name == "empty update" {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        };
        assert_eq!(status, expected, "{name}");
    }
    let (status, _) = call(
        &h.app,
        "POST",
        &uri,
        Some(json!({
            "auth": {
                "type": "environment_variable",
                "injection_location": null,
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "null update object");
    assert_eq!(
        h.credentials
            .get_vault_credential("default", &credential_id)
            .await
            .unwrap()
            .expect("credential"),
        before_rejected_update,
        "rejected updates are atomic"
    );

    // A fresh protocol state has no in-memory projection cache to rely on. It
    // must reconstruct the exact required response fields from the durable row.
    let recovered = vault_router(Arc::new(VaultState::new(
        h.secrets.clone(),
        h.credentials.clone(),
    )));
    let (status, credential) = call(&recovered, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        credential["auth"]["injection_location"],
        json!({ "body": true, "header": true })
    );
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
    let cred = create_mcp_oauth(
        &h,
        &vault_id,
        "https://mcp.example.com/sse",
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
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .await
        .unwrap();
    use awaken_credential_vault::repo::CredentialRepo;
    let before = h.credentials.get(&source_id).await.unwrap();

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

    // Cause/effect rule M2: changing two auxiliary secrets publishes one higher
    // source revision; both new refs belong to that revision and old refs are
    // reclaimed rather than overwritten.
    let source = h.credentials.get(&source_id).await.unwrap();
    assert_eq!(source.version, before.version + 1);
    assert_eq!(source.material_ref, before.material_ref);
    assert_ne!(
        source.auxiliary_material_refs,
        before.auxiliary_material_refs
    );
    let rt = h
        .secrets
        .get(
            source
                .auxiliary_material_ref(awaken_credential_vault::OAUTH_REFRESH_TOKEN_SLOT)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rt.expose_secret(), "rt-new");
    let cs = h
        .secrets
        .get(
            source
                .auxiliary_material_ref(awaken_credential_vault::OAUTH_CLIENT_SECRET_SLOT)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cs.expose_secret(), "cs-new");
    // The binding the session refresher reads now reflects the new scheme.
    let access = h.state.mcp_access_for_source(&source_id).await.unwrap();
    let binding = access.refresh.unwrap();
    assert_eq!(
        binding.token_endpoint_auth,
        TokenEndpointAuth::ClientSecretPost
    );
    assert_eq!(
        binding.client_secret_ref.as_deref(),
        source
            .auxiliary_material_ref(awaken_credential_vault::OAUTH_CLIENT_SECRET_SLOT)
            .map(|reference| reference.0.as_str())
    );

    // Updating refresh on a credential that has none is a 400.
    let plain = create_mcp_oauth(&h, &vault_id, "https://mcp.example.com/plain", None).await;
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
    let cred = create_mcp_oauth(
        &h,
        &vault_id,
        "https://mcp.example.com/oauth",
        Some(json!({
            "client_id": "cli",
            "refresh_token": "rt", // awaken-allow: secret
            "token_endpoint": "https://auth.example.com/token",
            "token_endpoint_auth": { "type": "client_secret_basic", "client_secret": "cs-orig" } // awaken-allow: secret
        })),
    )
    .await;
    let cred_id = cred["id"].as_str().unwrap().to_string();
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .await
        .unwrap();
    use awaken_credential_vault::repo::CredentialRepo;
    let before = h.credentials.get(&source_id).await.unwrap();

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
    // The original sealed client secret is preserved (not wiped), and the
    // binding still points at the aggregate-owned ref for the new scheme.
    let source = h.credentials.get(&source_id).await.unwrap();
    assert_eq!(source.version, before.version + 1);
    assert_eq!(source.material_ref, before.material_ref);
    assert_eq!(
        source.auxiliary_material_refs,
        before.auxiliary_material_refs
    );
    let cs = h
        .secrets
        .get(
            source
                .auxiliary_material_ref(awaken_credential_vault::OAUTH_CLIENT_SECRET_SLOT)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cs.expose_secret(), "cs-orig");
    let access = h.state.mcp_access_for_source(&source_id).await.unwrap();
    let binding = access.refresh.unwrap();
    assert_eq!(
        binding.token_endpoint_auth,
        TokenEndpointAuth::ClientSecretPost
    );
    assert_eq!(
        binding.client_secret_ref.as_deref(),
        source
            .auxiliary_material_ref(awaken_credential_vault::OAUTH_CLIENT_SECRET_SLOT)
            .map(|reference| reference.0.as_str())
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
    let bearer_source = h
        .state
        .credential_source_id(&vault_id, &bearer_id)
        .await
        .unwrap();
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
        "https://mcp.example.com/oauth",
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
    let oauth_source = h
        .state
        .credential_source_id(&vault_id, &oauth_id)
        .await
        .unwrap();
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
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .await
        .unwrap();
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
async fn exact_vault_admission_is_the_only_envelope_issuance_boundary() {
    let h = harness();
    let vault_id = create_vault(&h, "envelope authority").await;
    let (status, credential) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "recipient-bound-secret" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let source_id = h
        .state
        .credential_source_id(&vault_id, credential["id"].as_str().unwrap())
        .await
        .unwrap();
    use awaken_credential_vault::repo::CredentialRepo;
    let source_workspace = h.credentials.get(&source_id).await.unwrap().workspace_id;
    let issuer = Arc::new(RecordingEnvelopeIssuer::default());
    let state = VaultState::new(h.secrets.clone(), h.credentials.clone()).with_material_delivery(
        awaken_credential_contract::CredentialMaterialDelivery::RecipientEnvelope(issuer.clone()),
    );
    let holder =
        awaken_credential_contract::CredentialRealizationProfile::self_hosted_native().mcp_holder;
    let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        &source_workspace,
        &"https://mcp.example.com/sse",
        &usage,
    );
    let target =
        awaken_session_contract::McpTarget::parse_http("https://mcp.example.com/sse").unwrap();

    // Cause/effect decision table for the only plaintext-to-envelope boundary:
    //
    // | Rule | active exact source | exact Workspace | exact holder/target | Effect |
    // |---|---|---|---|---|
    // | R1 | yes | yes | yes | open the selected revision once and attach one envelope |
    // | R2 | yes | no | yes | reject before the issuer and attach nothing |
    // | R3 | no | yes | yes | reject before the issuer and attach nothing |
    // | R4 | yes | source-only | no | reject a cross-Workspace material binding before opening |
    // | R5 | yes | yes | no | reject a holder outside the exact policy before opening |
    // | R6 | described MCP | yes | undeclared HTTP-effect target | reject before opening or issuing an envelope |
    //
    // Holder/target integrity is cryptographically represented by the payload
    // fingerprint returned by the issuer; the contract admission decision table
    // separately proves mismatched recipients and fingerprints are rejected.
    let access = SessionCredentialSource::mcp_access_for_source(
        &state,
        &source_id,
        &source_workspace,
        &target,
        &holder,
        &binding,
    )
    .await
    .expect("R1 exact source issues one envelope");
    assert!(access.envelope.is_some());
    assert_eq!(
        issuer.issued.lock().unwrap().as_slice(),
        &[(
            source_id.0.clone(),
            access.credential.revision,
            "recipient-bound-secret".into(),
        )]
    );

    assert!(
        SessionCredentialSource::mcp_access_for_source(
            &state,
            &source_id,
            "workspace-b",
            &target,
            &holder,
            &binding,
        )
        .await
        .is_err(),
        "R2 cross-Workspace source must fail closed"
    );
    assert_eq!(issuer.issued.lock().unwrap().len(), 1);

    let platform_holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Platform,
        "awaken.platform.egress-gateway",
    );
    let platform_usage = awaken_credential_contract::CredentialUsage::HttpEffect {
        fields: std::collections::BTreeMap::from([(
            "token".into(),
            std::collections::BTreeSet::from([
                awaken_credential_contract::HttpEffectPlacement::Header {
                    name: "authorization".into(),
                },
            ]),
        )]),
    };
    let platform_binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        &source_workspace,
        &"https://mcp.example.com/sse",
        &platform_usage,
    );
    let platform_error = state
        .credential_access_for_source(
            &source_id,
            &source_workspace,
            awaken_session_application::SessionCredentialAccessRequest {
                target: awaken_credential_contract::CredentialTarget::new(
                    awaken_credential_contract::CredentialPurpose::HttpEffect,
                    "https://mcp.example.com/sse",
                ),
                usage: platform_usage,
                policy: awaken_credential_contract::CredentialExecutionPolicy::exact(
                    platform_holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::Forbidden,
                ),
                selected_holder: platform_holder,
                binding: platform_binding,
            },
        )
        .await
        .expect_err("R6 MCP source cannot be rebound to an HTTP-effect target");
    assert!(
        platform_error
            .to_string()
            .contains("credential target is not declared"),
        "R6"
    );
    assert_eq!(issuer.issued.lock().unwrap().len(), 1, "R6");

    let cross_workspace_binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        "workspace-b",
        &"https://mcp.example.com/sse",
        &usage,
    );
    assert!(
        SessionCredentialSource::mcp_access_for_source(
            &state,
            &source_id,
            &source_workspace,
            &target,
            &holder,
            &cross_workspace_binding,
        )
        .await
        .is_err(),
        "R4 binding Workspace must match the admitted source Workspace"
    );
    assert_eq!(issuer.issued.lock().unwrap().len(), 1);

    let unauthorized_holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Platform,
        "untrusted-platform",
    );
    assert!(
        SessionCredentialSource::mcp_access_for_source(
            &state,
            &source_id,
            &source_workspace,
            &target,
            &unauthorized_holder,
            &binding,
        )
        .await
        .is_err(),
        "R5 selected holder must be authorized before material opens"
    );
    assert_eq!(issuer.issued.lock().unwrap().len(), 1);

    let mut source = h.credentials.get(&source_id).await.unwrap();
    source.status = awaken_credential_vault::CredentialStatus::Disabled;
    h.credentials.put(source).await.unwrap();
    assert!(
        SessionCredentialSource::mcp_access_for_source(
            &state,
            &source_id,
            &source_workspace,
            &target,
            &holder,
            &binding,
        )
        .await
        .is_err(),
        "R3 disabled source must fail before plaintext opens"
    );
    assert_eq!(issuer.issued.lock().unwrap().len(), 1);

    // A hosted issuer is untrusted output at this boundary: substitution and
    // failure both reject the whole access compilation. Neither may silently
    // fall back to an unsealed Control reference after plaintext was opened.
    let mut active_source = h.credentials.get(&source_id).await.unwrap();
    active_source.status = awaken_credential_vault::CredentialStatus::Active;
    h.credentials.put(active_source).await.unwrap();
    for response in [
        AdversarialEnvelopeResponse::SubstitutedPayload,
        AdversarialEnvelopeResponse::Failure,
    ] {
        let adversarial = VaultState::new(h.secrets.clone(), h.credentials.clone())
            .with_material_delivery(
                awaken_credential_contract::CredentialMaterialDelivery::RecipientEnvelope(
                    Arc::new(AdversarialEnvelopeIssuer(response)),
                ),
            );
        assert!(
            SessionCredentialSource::mcp_access_for_source(
                &adversarial,
                &source_id,
                &source_workspace,
                &target,
                &holder,
                &binding,
            )
            .await
            .is_err(),
            "substitution or issuer failure must reject without fallback"
        );
    }
}

#[tokio::test]
async fn exact_vault_admission_publishes_one_revision_to_external_custody() {
    let h = harness();
    let vault_id = create_vault(&h, "external custody").await;
    let (_, credential) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "custody-secret" // awaken-allow: secret
        })),
    )
    .await;
    let source_id = h
        .state
        .credential_source_id(&vault_id, credential["id"].as_str().unwrap())
        .await
        .unwrap();
    use awaken_credential_vault::repo::CredentialRepo;
    let workspace = h.credentials.get(&source_id).await.unwrap().workspace_id;
    let custodian = Arc::new(RecordingCustodian::default());
    let state = VaultState::new(h.secrets.clone(), h.credentials.clone()).with_material_delivery(
        awaken_credential_contract::CredentialMaterialDelivery::ExternalCustody(custodian.clone()),
    );
    let holder =
        awaken_credential_contract::CredentialRealizationProfile::self_hosted_native().mcp_holder;
    let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        &workspace,
        &"https://mcp.example.com/sse",
        &usage,
    );
    let target =
        awaken_session_contract::McpTarget::parse_http("https://mcp.example.com/sse").unwrap();

    // Cause/effect: only an active, Workspace-bound, holder-authorized exact
    // revision opens once and reaches custody. The returned execution fact is
    // still secret-free and carries no Worker envelope.
    let access = SessionCredentialSource::mcp_access_for_source(
        &state, &source_id, &workspace, &target, &holder, &binding,
    )
    .await
    .unwrap();
    assert!(access.envelope.is_none());
    assert_eq!(
        custodian.published.lock().unwrap().as_slice(),
        &[(
            source_id.0,
            access.credential.revision,
            "custody-secret".into()
        )]
    );
}

#[tokio::test]
async fn external_custody_never_observes_an_unowned_plaintext_path() {
    let h = harness();
    let vault_id = create_vault(&h, "selective custody").await;
    let (_, credential) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/sse",
            "token": "selective-secret" // awaken-allow: secret
        })),
    )
    .await;
    let source_id = h
        .state
        .credential_source_id(&vault_id, credential["id"].as_str().unwrap())
        .await
        .unwrap();
    use awaken_credential_vault::repo::CredentialRepo;
    let workspace = h.credentials.get(&source_id).await.unwrap().workspace_id;
    let custodian = Arc::new(SelectiveCustodian {
        holder: awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Platform,
            "external-provider-custody",
        ),
        usage: awaken_credential_contract::CredentialUsage::ProviderAdapter,
        published: Mutex::new(0),
    });
    let state = VaultState::new(h.secrets.clone(), h.credentials.clone()).with_material_delivery(
        awaken_credential_contract::CredentialMaterialDelivery::ExternalCustody(custodian.clone()),
    );
    let profile = awaken_credential_contract::CredentialRealizationProfile::self_hosted_native();
    let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
        &workspace,
        &"https://mcp.example.com/sse",
        &usage,
    );
    let target =
        awaken_session_contract::McpTarget::parse_http("https://mcp.example.com/sse").unwrap();

    // Partition testing: a Platform/Provider-only custodian and an MCP/Worker
    // request are disjoint classes. The request must retain the ordinary MCP
    // path without disclosing its material to the external custodian.
    let access = SessionCredentialSource::mcp_access_for_source(
        &state,
        &source_id,
        &workspace,
        &target,
        &profile.mcp_holder,
        &binding,
    )
    .await
    .unwrap();
    assert!(access.envelope.is_none());
    assert_eq!(*custodian.published.lock().unwrap(), 0);
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

    // The client secret was SEALED (not dropped) in the aggregate's named slot,
    // so the confidential-client refresh grant can authenticate later.
    let source_id = h
        .state
        .credential_source_id(&vault_id, &cred_id)
        .await
        .unwrap();
    use awaken_credential_vault::repo::CredentialRepo;
    let source = h.credentials.get(&source_id).await.unwrap();
    let sealed = h
        .secrets
        .get(
            source
                .auxiliary_material_ref(awaken_credential_vault::OAUTH_CLIENT_SECRET_SLOT)
                .unwrap(),
        )
        .await
        .expect("the client secret is sealed in its named material slot");
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

// Credential selection cases come from this cause graph:
// active requested Vault (in caller order) -> active compatible credential ->
// canonical target equality -> selected source; a false cause skips that row and
// continues, while no match is explicitly unauthenticated.
//
// | Rule | Vault active | Credential active | Target equal | Effect |
// |------|--------------|-------------------|--------------|--------|
// | M1   | T            | T                 | T            | select first Vault |
// | M2   | T            | T                 | F            | continue/no match |
// | M3   | T            | F                 | T            | skip credential |
// | M4   | F            | -                 | -            | skip Vault |
//
// The normalization, order, archived-credential and archived-Vault tests below
// are generated from M1-M4 and call the same production selector.
#[tokio::test]
async fn mcp_binding_normalizes_urls_and_supports_both_credential_kinds() {
    let h = harness();
    let oauth_vault = create_vault(&h, "oauth").await;
    let bearer_vault = create_vault(&h, "bearer").await;

    let oauth = create_mcp_oauth(&h, &oauth_vault, "HTTPS://MCP.EXAMPLE.COM:443/sse/", None).await;
    let oauth_id = oauth["id"].as_str().unwrap().to_string();
    let (status, bearer) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{bearer_vault}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/mcp",
            "token": "bearer-secret" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bearer_id = bearer["id"].as_str().unwrap().to_string();

    let oauth_source = h
        .state
        .credential_source_id(&oauth_vault, &oauth_id)
        .await
        .unwrap();
    assert_eq!(
        h.state
            .mcp_credential_source_for_url(
                "default",
                std::slice::from_ref(&oauth_vault),
                "https://mcp.example.com/sse"
            )
            .await
            .unwrap(),
        Some(oauth_source)
    );

    let bearer_source = h
        .state
        .credential_source_id(&bearer_vault, &bearer_id)
        .await
        .unwrap();
    assert_eq!(
        h.state
            .mcp_credential_source_for_url(
                "default",
                std::slice::from_ref(&bearer_vault),
                "https://MCP.example.com:443/mcp/"
            )
            .await
            .unwrap(),
        Some(bearer_source)
    );

    for different in [
        "https://other.example.com/mcp",
        "https://sub.mcp.example.com/mcp",
        "https://mcp.example.com:8443/mcp",
        "https://mcp.example.com/other",
    ] {
        assert!(
            h.state
                .mcp_credential_source_for_url(
                    "default",
                    std::slice::from_ref(&bearer_vault),
                    different
                )
                .await
                .unwrap()
                .is_none(),
            "a structurally different target must not match: {different}"
        );
    }
    assert!(
        h.state
            .mcp_credential_source_for_url("default", &[], "https://mcp.example.com/mcp")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn mcp_binding_uses_vault_id_order_as_credential_precedence() {
    let h = harness();
    let first_created = create_vault(&h, "first-created").await;
    let second_created = create_vault(&h, "second-created").await;
    let url = "https://mcp.example.com/mcp";
    let first_credential = create_mcp_oauth(&h, &first_created, url, None).await;
    let second_credential = create_mcp_oauth(&h, &second_created, url, None).await;
    let first_source = h
        .state
        .credential_source_id(&first_created, first_credential["id"].as_str().unwrap())
        .await
        .unwrap();
    let second_source = h
        .state
        .credential_source_id(&second_created, second_credential["id"].as_str().unwrap())
        .await
        .unwrap();

    assert_eq!(
        h.state
            .mcp_credential_source_for_url(
                "default",
                &[second_created.clone(), first_created.clone()],
                url
            )
            .await
            .unwrap(),
        Some(second_source)
    );
    assert_eq!(
        h.state
            .mcp_credential_source_for_url("default", &[first_created, second_created], url)
            .await
            .unwrap(),
        Some(first_source)
    );
}

#[tokio::test]
async fn same_target_credentials_are_authored_and_selected_deterministically() {
    let h = harness();
    let vault_id = create_vault(&h, "mcp").await;
    let first = create_mcp_oauth(&h, &vault_id, "https://MCP.example.com:443/mcp/", None).await;

    let (second_status, second) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/mcp",
            "token": "replacement" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(second_status, StatusCode::OK);
    let first_source = h
        .state
        .credential_source_id(&vault_id, first["id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        h.state
            .mcp_credential_source_for_url(
                "default",
                std::slice::from_ref(&vault_id),
                "https://mcp.example.com/mcp"
            )
            .await
            .unwrap(),
        Some(first_source),
        "the lowest credential id breaks a same-Vault tie"
    );

    let first_id = first["id"].as_str().unwrap();
    let (archived, _) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials/{first_id}/archive"),
        None,
    )
    .await;
    assert_eq!(archived, StatusCode::OK);
    let second_source = h
        .state
        .credential_source_id(&vault_id, second["id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        h.state
            .mcp_credential_source_for_url(
                "default",
                std::slice::from_ref(&vault_id),
                "https://mcp.example.com/mcp"
            )
            .await
            .unwrap(),
        Some(second_source),
        "archiving the first credential exposes the next deterministic candidate"
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

/// Cause-effect graph for the sole Vault -> execution compiler:
///
/// C1 exact active source exists
///  -> C2 source has MCP OAuth refresh
///      ├─ F -> E1 exact access without refresh
///      └─ T -> C3 endpoint auth kind
///              ├─ none -> E2 no client-secret ref
///              ├─ basic -> E3 basic + exact client-secret ref
///              └─ post -> E4 post + exact client-secret ref
/// Every refresh effect also carries one self-verifying fingerprint over all
/// executable fields; no intermediate refresh binding exists.
///
/// | Rule | Source kind | Refresh | Endpoint auth | Result |
/// |---|---|---|---|---|
/// | V1 | MCP OAuth | yes | none | E2 |
/// | V2 | MCP OAuth | no | - | E1 |
/// | V3 | MCP OAuth | yes | basic | E3 |
/// | V4 | MCP OAuth | yes | post | E4 |
/// | V5 | environment | no | - | E1 |
/// | V6 | static bearer | no | - | E1 |
#[tokio::test]
async fn exact_mcp_access_compiles_public_and_confidential_refresh() {
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
        .await
        .unwrap();
    let access = h
        .state
        .mcp_access_for_source(&source_id)
        .await
        .expect("exact MCP access compiles");
    assert_eq!(
        access.policy.model_exposure,
        awaken_credential_contract::ModelExposurePolicy::VirtualOnly,
        "the sole MCP compiler explicitly authorizes only a synthetic relay capability"
    );
    let binding = access.refresh.expect("public-client refresh is pinned");
    assert_eq!(binding.token_endpoint, "https://auth.example.com/token");
    assert_eq!(binding.client_id, "cli_pub");
    assert_eq!(binding.scope.as_deref(), Some("mcp:read"));
    assert_eq!(binding.resource.as_deref(), Some("https://mcp.example.com"));
    assert_eq!(binding.token_endpoint_auth, TokenEndpointAuth::None);
    // The refresh token itself stays sealed: the binding carries only its ref.
    let logical_prefix = format!(
        "sec:{}:r1:{}:attempt:",
        source_id.0,
        awaken_credential_vault::OAUTH_REFRESH_TOKEN_SLOT
    );
    assert!(binding.refresh_token_ref.starts_with(&logical_prefix));
    assert!(binding.refresh_token_ref.len() > logical_prefix.len());
    assert!(binding.has_valid_configuration_fingerprint());

    // An mcp_oauth credential entered WITHOUT a refresh object yields none.
    let plain = create_mcp_oauth(&h, &vault_id, "https://mcp.example.com/plain", None).await;
    let plain_id = plain["id"].as_str().unwrap().to_string();
    let plain_source = h
        .state
        .credential_source_id(&vault_id, &plain_id)
        .await
        .unwrap();
    assert!(
        h.state
            .mcp_access_for_source(&plain_source)
            .await
            .unwrap()
            .refresh
            .is_none()
    );

    // A confidential-client scheme is exposed too: its client_secret was sealed
    // at create, so the binding carries the sealed ref for the grant's client
    // authentication (basic → the Basic header, post → the form field).
    for (index, auth_type) in ["client_secret_basic", "client_secret_post"]
        .into_iter()
        .enumerate()
    {
        let confidential = create_mcp_oauth(
            &h,
            &vault_id,
            &format!("https://mcp.example.com/confidential-{index}"),
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
            .await
            .unwrap();
        let access = h
            .state
            .mcp_access_for_source(&confidential_source)
            .await
            .expect("exact confidential MCP access compiles");
        let binding = access.refresh.expect("confidential refresh is pinned");
        assert_eq!(binding.client_id, "cli_conf");
        // The auth binding carries the aggregate-owned sealed client-secret
        // ref, never material.
        use awaken_credential_vault::repo::CredentialRepo;
        let source = h.credentials.get(&confidential_source).await.unwrap();
        let secret_ref = &source
            .auxiliary_material_ref(awaken_credential_vault::OAUTH_CLIENT_SECRET_SLOT)
            .unwrap()
            .0;
        let expected = match auth_type {
            "client_secret_basic" => TokenEndpointAuth::ClientSecretBasic,
            _ => TokenEndpointAuth::ClientSecretPost,
        };
        assert_eq!(binding.token_endpoint_auth, expected, "{auth_type}");
        assert_eq!(
            binding.client_secret_ref.as_deref(),
            Some(secret_ref.as_str())
        );
        assert!(binding.has_valid_configuration_fingerprint());
    }

    // Env-var and static_bearer rows never carry a refresh configuration.
    let env_id = create_credential(&h, &vault_id, "K").await;
    let env_source = h
        .state
        .credential_source_id(&vault_id, &env_id)
        .await
        .unwrap();
    assert!(h.state.mcp_access_for_source(&env_source).await.is_err());
    let (s, bearer) = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "static_bearer",
            "mcp_server_url": "https://mcp.example.com/bearer",
            "token": "brr" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let bearer_id = bearer["id"].as_str().unwrap().to_string();
    let bearer_source = h
        .state
        .credential_source_id(&vault_id, &bearer_id)
        .await
        .unwrap();
    assert!(
        h.state
            .mcp_access_for_source(&bearer_source)
            .await
            .unwrap()
            .refresh
            .is_none()
    );
}
