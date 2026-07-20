//! Causal-graph coverage for the admin config router (CEG 09) that needs the live
//! HTTP surface: live credential validation (`validate_credential` VC1–VC5),
//! credential cooldown + pool eligibility (`cooldown_credential` / `get_pool_eligible`
//! CD1–CD5), the authoritative-path-id override on `put_provider`/`put_endpoint`/
//! `put_pool`, `archive_credential`, and `post_credential`'s secret-free-out
//! contract. The pure error-mapper cases live inline in `router.rs`.
//!
//! Every route is driven end-to-end through `axum` `oneshot`, reusing the same
//! store-injection harness the existing tests use. The security invariants are
//! asserted explicitly: a path id always wins over a body id, and no response ever
//! carries a cleartext secret.

use std::sync::{Arc, Mutex};

use awaken_admin_config_api::{AdminState, CredentialProbe, ProbeStatus, admin_router};
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::AvailabilityLedger;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A test double for the live-probe port. Records every call (base_url, secret,
/// model) and answers with a configured status, so a test can prove the probe both
/// ran (VC1) and was skipped (VC2/VC3) with the exact arguments the resolver fed it.
struct RecordingProbe {
    status: ProbeStatus,
    calls: Mutex<Vec<(String, String, String)>>,
}

impl RecordingProbe {
    fn new(status: ProbeStatus) -> Arc<Self> {
        Arc::new(Self {
            status,
            calls: Mutex::new(Vec::new()),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl CredentialProbe for RecordingProbe {
    async fn probe(&self, base_url: &str, secret: &RedactedString, model: &str) -> ProbeStatus {
        self.calls.lock().unwrap().push((
            base_url.to_string(),
            secret.expose_secret().to_string(),
            model.to_string(),
        ));
        self.status
    }
}

struct Harness {
    app: Router,
}

/// Build a router with the in-memory stores; `probe` is the optional live validator.
fn harness_with(probe: Option<Arc<dyn CredentialProbe>>) -> Harness {
    let app = admin_router(AdminState {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        mcp: Arc::new(awaken_admin_config_api::InMemoryMcpStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryResourceStore::new()),
        probe,
        availability: Arc::new(AvailabilityLedger::new()),
    });
    Harness { app }
}

fn harness() -> Harness {
    harness_with(None)
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

/// Author a resolvable model: provider + endpoint (`dialect`) + offering. `dialect`
/// picks the adapter kind the resolver reports (`anthropic_messages`→"anthropic",
/// `open_ai_chat`→"openai").
async fn author_model(app: &Router, provider: &str, dialect: &str, model: &str) {
    let (s, _) = call(
        app,
        "PUT",
        &format!("/v1/config/providers/{provider}"),
        Some(json!({ "id": provider, "slug": provider, "display_name": provider, "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        app,
        "PUT",
        "/v1/config/endpoints/ep1",
        Some(json!({
            "id": "ep1", "provider_id": provider, "dialect": dialect,
            "base_url": "https://api.example.com/v1/", "timeout_secs": 300,
            "display_name": "prod", "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        app,
        "POST",
        "/v1/config/offerings",
        Some(json!({
            "model_id": model, "provider_id": provider,
            "protocol_endpoint_id": "ep1", "dialect": dialect, "upstream_model": null
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

/// Enter a vault credential scoped to `provider` and return its id.
async fn enter_vault_cred(app: &Router, provider: Option<&str>, secret: &str) -> String {
    let (s, cred) = call(
        app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws", "kind": "vault", "provider_id": provider,
            "env_key": null, "secret": secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    cred["id"].as_str().expect("credential id").to_string()
}

// ---------------------------------------------------------------------------
// validate_credential (VC1–VC5)
// ---------------------------------------------------------------------------

/// VC1: probe wired + anthropic adapter + materialized secret → the real probe runs
/// and its verdict is returned. Also proves the resolver fed the probe the resolved
/// base_url, materialized secret, and model.
#[tokio::test]
async fn vc1_probe_runs_for_anthropic_with_secret() {
    let probe = RecordingProbe::new(ProbeStatus::Valid);
    let h = harness_with(Some(probe.clone()));
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-live-probe").await;

    let (s, body) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "claude-opus-4-8" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "valid");
    assert_eq!(body["adapter_kind"], "anthropic");

    // The probe ran exactly once, with the resolved endpoint + materialized secret.
    let calls = probe.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "https://api.example.com/v1/");
    assert_eq!(calls[0].1, "sk-live-probe");
    assert_eq!(calls[0].2, "claude-opus-4-8");
    // The wire response never carries the secret.
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains("sk-live-probe")
    );
}

/// VC2: no probe wired → `unknown` (the CRUD crate is SDK-free; validation degrades).
#[tokio::test]
async fn vc2_no_probe_is_unknown() {
    let h = harness(); // probe: None
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-x").await;

    let (s, body) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "claude-opus-4-8" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "unknown");
    assert_eq!(body["adapter_kind"], "anthropic");
}

/// VC3: adapter is not anthropic → `unknown` even with a probe wired; the probe (which
/// only reaches the anthropic wire) is never called.
#[tokio::test]
async fn vc3_non_anthropic_adapter_is_unknown_and_skips_probe() {
    let probe = RecordingProbe::new(ProbeStatus::Valid);
    let h = harness_with(Some(probe.clone()));
    author_model(&h.app, "openai", "open_ai_chat", "gpt-x").await;
    let cred = enter_vault_cred(&h.app, Some("openai"), "sk-openai").await;

    let (s, body) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "gpt-x" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "unknown");
    assert_eq!(body["adapter_kind"], "openai");
    assert_eq!(
        probe.call_count(),
        0,
        "probe must not reach a non-anthropic wire"
    );
}

/// VC5: the resolve step fails (no offering for the model) → the resolve problem
/// mapper answers 4xx and the response carries no secret.
#[tokio::test]
async fn vc5_resolve_failure_is_problem_json_without_secret() {
    let probe = RecordingProbe::new(ProbeStatus::Valid);
    let h = harness_with(Some(probe.clone()));
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-secret-vc5").await;

    // A model with no offering → ModelUnresolved → 404 model_unresolved.
    let (s, err) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "ghost-model" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "model_unresolved");
    assert_eq!(probe.call_count(), 0);
    assert!(
        !serde_json::to_string(&err)
            .unwrap()
            .contains("sk-secret-vc5")
    );
}

// ---------------------------------------------------------------------------
// cooldown_credential + get_pool_eligible (CD1–CD5)
// ---------------------------------------------------------------------------

async fn cooldown(app: &Router, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        &format!("/v1/config/credentials/{id}/cooldown"),
        Some(body),
    )
    .await
}

/// CD1: `quota` + retry cools the source; the returned state is `cooled_down`.
#[tokio::test]
async fn cd1_quota_cools_down() {
    let h = harness();
    let (s, state) = cooldown(
        &h.app,
        "src-1",
        json!({ "kind": "quota", "retry_after_secs": 3600 }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(state["state"], "cooled_down");
    assert!(state["retry_at_ms"].as_u64().unwrap() > 0);
    // The availability GET agrees.
    let (_, avail) = call(
        &h.app,
        "GET",
        "/v1/config/credentials/src-1/availability",
        None,
    )
    .await;
    assert_eq!(avail["state"], "cooled_down");
}

/// CD2: `exhausted` marks a hard exhaustion with no known reset.
#[tokio::test]
async fn cd2_exhausted() {
    let h = harness();
    let (s, state) = cooldown(&h.app, "src-2", json!({ "kind": "exhausted" })).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(state["state"], "exhausted");
}

/// CD3: `available` / `clear` lifts any cooldown back to `available`.
#[tokio::test]
async fn cd3_clear_restores_available() {
    let h = harness();
    // Cool it first, then clear via each alias.
    let (_, st) = cooldown(&h.app, "src-3", json!({ "kind": "exhausted" })).await;
    assert_eq!(st["state"], "exhausted");
    let (s, state) = cooldown(&h.app, "src-3", json!({ "kind": "available" })).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(state["state"], "available");

    let (_, st) = cooldown(&h.app, "src-3", json!({ "kind": "exhausted" })).await;
    assert_eq!(st["state"], "exhausted");
    let (_, state) = cooldown(&h.app, "src-3", json!({ "kind": "clear" })).await;
    assert_eq!(state["state"], "available");
}

/// CD4: `permanent` / `transient` / unknown kinds are no-ops on availability — the
/// disposition yields no deadline, so the source stays `available`.
#[tokio::test]
async fn cd4_permanent_transient_and_unknown_are_no_ops() {
    let h = harness();
    for kind in ["permanent", "transient", "gibberish"] {
        let (s, state) = cooldown(&h.app, "src-4", json!({ "kind": kind })).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(state["state"], "available", "kind {kind} must not cool");
    }
}

/// CD5: `get_pool_eligible` partitions the pool's members into eligible vs cooled by
/// the live availability ledger — a cooled member rotates out.
#[tokio::test]
async fn cd5_pool_eligible_partitions_cooled_members() {
    let h = harness();
    // Author a two-member pool (members need not be real sources for eligibility).
    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/credential-pools/pool-a",
        Some(json!({
            "id": "ignored-by-path", "workspace_id": "ws",
            "members": [
                { "credential_source_id": "m1", "ordinal": 0, "enabled": true },
                { "credential_source_id": "m2", "ordinal": 1, "enabled": true }
            ]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // All eligible before any cooldown.
    let (s, view) = call(
        &h.app,
        "GET",
        "/v1/config/credential-pools/pool-a/eligible",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["eligible"], json!(["m1", "m2"]));
    assert_eq!(view["cooled"], json!([]));

    // Cool m1 → it rotates out.
    let (_, st) = cooldown(&h.app, "m1", json!({ "kind": "exhausted" })).await;
    assert_eq!(st["state"], "exhausted");
    let (_, view) = call(
        &h.app,
        "GET",
        "/v1/config/credential-pools/pool-a/eligible",
        None,
    )
    .await;
    assert_eq!(view["eligible"], json!(["m2"]));
    assert_eq!(view["cooled"], json!(["m1"]));
}

// ---------------------------------------------------------------------------
// Authoritative path id override (put_provider / put_endpoint / put_pool)
// ---------------------------------------------------------------------------

/// The path id is authoritative on all three upsert routes: a client cannot smuggle
/// a different id in the body to write under a scope it did not address.
#[tokio::test]
async fn path_id_overrides_body_id_on_provider_endpoint_and_pool() {
    let h = harness();

    let (s, provider) = call(
        &h.app,
        "PUT",
        "/v1/config/providers/real-provider",
        Some(json!({ "id": "evil-body-id", "slug": "p", "display_name": "P", "version": 1 })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(provider["id"], "real-provider");
    // And it is stored under the path id, not the body id.
    let (s, got) = call(&h.app, "GET", "/v1/config/providers/real-provider", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["id"], "real-provider");
    let (s, _) = call(&h.app, "GET", "/v1/config/providers/evil-body-id", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Endpoint needs its provider to exist (ref integrity).
    let (s, endpoint) = call(
        &h.app,
        "PUT",
        "/v1/config/endpoints/real-endpoint",
        Some(json!({
            "id": "evil-body-id", "provider_id": "real-provider",
            "dialect": "anthropic_messages", "base_url": "https://x/", "timeout_secs": 30,
            "display_name": "e", "version": 1
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(endpoint["id"], "real-endpoint");

    let (s, pool) = call(
        &h.app,
        "PUT",
        "/v1/config/credential-pools/real-pool",
        Some(json!({ "id": "evil-body-id", "workspace_id": "ws", "members": [] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pool["id"], "real-pool");
    let (s, got) = call(&h.app, "GET", "/v1/config/credential-pools/real-pool", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["id"], "real-pool");
}

// ---------------------------------------------------------------------------
// archive_credential
// ---------------------------------------------------------------------------

/// archive (a): a missing credential → cred_problem 404, nothing stored.
#[tokio::test]
async fn archive_missing_credential_is_404() {
    let h = harness();
    let (s, err) = call(&h.app, "POST", "/v1/config/credentials/ghost/archive", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
}

/// archive (b): an existing credential → status Disabled + version bumped, and it now
/// fails closed on materialization (a disabled source is `NotActive` → 409).
#[tokio::test]
async fn archive_disables_credential_and_bumps_version() {
    let h = harness();
    author_model(&h.app, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-to-archive").await;

    // Freshly entered → active, version 1.
    let (s, before) = call(
        &h.app,
        "GET",
        &format!("/v1/config/credentials/{cred}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(before["status"], "active");
    assert_eq!(before["version"], 1);

    let (s, archived) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/archive"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(archived["status"], "disabled");
    assert_eq!(archived["version"], 2);
    // No secret leaks on the archive response.
    assert!(
        !serde_json::to_string(&archived)
            .unwrap()
            .contains("sk-to-archive")
    );

    // Disabled fails closed at materialization: validate now resolves through the
    // resolver, whose `materialize` rejects a non-active source (NotActive). That
    // surfaces via `resolve_problem`'s `Credential(_)` arm as 422 credential_invalid
    // (the raw NotActive→409 mapping is on the cred_problem routes, unit-covered).
    let (s, err) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "claude-opus-4-8" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err["code"], "credential_invalid");
}

// ---------------------------------------------------------------------------
// post_credential: secret-in / secret-free-out
// ---------------------------------------------------------------------------

/// post_credential (a)+(b): a create returns 201 with a secret-free public view;
/// neither the cleartext nor the internal vault reference crosses the boundary.
#[tokio::test]
async fn post_credential_is_201_and_never_echoes_the_secret() {
    let h = harness();
    let secret = "sk-post-cred-cleartext"; // awaken-allow: secret
    let (s, cred) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws", "kind": "vault", "provider_id": "anthropic",
            "env_key": "ANTHROPIC_API_KEY", "secret": secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    // A secret-free projection: carries stable identity, not vault internals.
    assert_eq!(cred["status"], "active");
    assert!(cred.get("material_ref").is_none());
    let text = serde_json::to_string(&cred).unwrap();
    assert!(
        !text.contains(secret),
        "secret leaked on create response: {text}"
    );

    // Nor does a subsequent GET / list ever surface it.
    let id = cred["id"].as_str().unwrap();
    let (_, got) = call(&h.app, "GET", &format!("/v1/config/credentials/{id}"), None).await;
    assert!(!serde_json::to_string(&got).unwrap().contains(secret));
    let (_, listed) = call(
        &h.app,
        "GET",
        "/v1/config/credentials?workspace_id=ws",
        None,
    )
    .await;
    assert!(!serde_json::to_string(&listed).unwrap().contains(secret));
}

#[tokio::test]
async fn oauth_credentials_accept_only_the_allowlisted_gcloud_helper() {
    let h = harness();
    let (status, credential) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "oauth",
            "provider_id": "google-vertex",
            "oauth_helper": "gcloud"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{credential}");
    assert_eq!(credential["kind"], "oauth");
    assert_eq!(credential["oauth_helper"], "gcloud");
    assert!(credential.get("material_ref").is_none());
    assert!(credential.get("oauth_command").is_none());

    let (status, problem) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "oauth",
            "provider_id": "google-vertex"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "credential_invalid");
}
