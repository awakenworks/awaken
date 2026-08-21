//! Causal-graph coverage for the admin config router (CEG 09) that needs the live
//! HTTP surface: live credential validation (`validate_credential` VC1–VC5),
//! credential cooldown + pool eligibility (`cooldown_credential` / `get_pool_eligible`
//! CD1–CD5), the authoritative-path-id override on `put_pool`,
//! `archive_credential`, and `post_credential`'s secret-free-out
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

mod support;

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
    catalog: Arc<awaken_model_catalog::repo::InMemoryCatalogRepo>,
}

/// Build a router with the in-memory stores; `probe` is the optional live validator.
fn harness_with(probe: Option<Arc<dyn CredentialProbe>>) -> Harness {
    let catalog = Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_config_resolver::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new()),
        probe,
        model_discovery: None,
        brokered_catalog: None,
        availability: Arc::new(AvailabilityLedger::new()),
    });
    Harness { app, catalog }
}

fn harness() -> Harness {
    harness_with(None)
}

#[tokio::test]
async fn provider_descriptors_are_read_only_capabilities_not_catalog_rows() {
    let harness = harness();
    let (status, descriptors) =
        call(&harness.app, "GET", "/v1/config/provider-descriptors", None).await;
    assert_eq!(status, StatusCode::OK);
    let descriptors = descriptors.as_array().unwrap();
    assert!(descriptors.iter().any(|value| {
        value["provider_kind"] == "openai" && value["supported_dialects"][0] == "open_ai_responses"
    }));

    let (status, catalog) = call(&harness.app, "GET", "/v1/config/catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(catalog["providers"], json!({}));
    assert_eq!(catalog["endpoints"], json!({}));
    assert_eq!(catalog["offerings"], json!([]));
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
    let mut request = builder.body(body).unwrap();
    if uri.starts_with("/v1/config/agents/") && uri.ends_with("/resources") {
        request
            .extensions_mut()
            .insert(awaken_tenancy::WorkspaceScope("workspace-test".into()));
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

/// Author a resolvable model: provider + endpoint (`dialect`) + offering. `dialect`
/// picks the adapter kind the resolver reports (`anthropic_messages`→"anthropic",
/// `open_ai_chat`→"openai").
async fn author_model(harness: &Harness, provider: &str, dialect: &str, model: &str) {
    support::seed_model(&harness.catalog, provider, dialect, model, "ep1").await;
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
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
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
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
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
    author_model(&h, "openai", "open_ai_chat", "gpt-x").await;
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
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
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
// Authoritative path id override (put_pool)
// ---------------------------------------------------------------------------

/// The path id is authoritative on the pool upsert route: a client cannot smuggle
/// a different id in the body to write under a scope it did not address.
#[tokio::test]
async fn path_id_overrides_body_id_on_pool() {
    let h = harness();

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
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
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
// rotate_credential: exact CAS + write-only replacement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotate_credential_replaces_material_without_leaking_and_rejects_stale_versions() {
    // | expected revision | material | effect |
    // | current           | new      | revision advances; validation uses new material |
    // | stale             | another  | 409; committed material remains unchanged |
    let probe = RecordingProbe::new(ProbeStatus::Valid);
    let h = harness_with(Some(probe.clone()));
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let credential = enter_vault_cred(&h.app, Some("anthropic"), "sk-before-rotation").await;

    let replacement = "sk-after-rotation"; // awaken-allow: secret
    let (status, rotated) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{credential}/rotate"),
        Some(json!({"expected_version": 1, "secret": replacement})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    assert_eq!(rotated["version"], 2);
    assert!(
        !serde_json::to_string(&rotated)
            .unwrap()
            .contains(replacement)
    );

    let (status, _) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{credential}/validate"),
        Some(json!({"workspace_id": "ws", "model_id": "claude-opus-4-8"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(probe.call_count(), 1);
    assert_eq!(probe.calls.lock().unwrap()[0].1, replacement);

    let (status, problem) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{credential}/rotate"),
        Some(json!({"expected_version": 1, "secret": "sk-stale-replacement"})), // awaken-allow: secret
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "credential_version_conflict");
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

/// Hosted Credential Resource cause/effect graph:
/// C1 exact Workspace/provider/operation tuple is valid; C2 a source exists;
/// C3 submitted material equals the sealed material; C4 operation lookup repeats
/// the exact tuple; C5 validation supplies the durable source id plus its exact
/// Workspace/provider. Effects are E1 one stable secret-free receipt,
/// E2 exact replay is the same receipt without desired-state drift, E3 different
/// material conflicts, E4 a mismatched operation tuple is undiscoverable, and E5
/// the durable reference validates without retaining a second operation mapping.
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
/// |---|---|---|---|---|---|---|
/// | H1 | T | F | - | T | - | E1 |
/// | H2 | T | T | T | T | - | E2 |
/// | H3 | T | T | F | T | - | E3 |
/// | H4 | T | T | - | F | - | E4 |
/// | H5 | - | T | - | - | T | E5 |
#[tokio::test]
async fn hosted_credential_operation_is_idempotent_exact_and_secret_free() {
    let h = harness();
    let request = json!({
        "workspace_id": "workspace-a",
        "kind": "vault",
        "provider_id": "domain-pack/provider",
        "idempotency_key": "credential-resource:create:42",
        "secret": "hosted-business-secret" // awaken-allow: secret
    });
    let (status, first) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "H1: {first}");
    assert!(
        first["id"]
            .as_str()
            .unwrap()
            .starts_with("cred:hosted-business:")
    );
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("hosted-business-secret")
    );

    let (status, replay) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "H2: {replay}");
    assert_eq!(replay, first, "H2");

    let mut conflicting = request;
    conflicting["secret"] = json!("different-hosted-business-secret"); // awaken-allow: secret
    let (status, problem) = call(&h.app, "POST", "/v1/config/credentials", Some(conflicting)).await;
    assert_eq!(status, StatusCode::CONFLICT, "H3: {problem}");
    assert_eq!(problem["code"], "credential_version_conflict", "H3");

    let (status, found) = call(
        &h.app,
        "GET",
        "/v1/config/credentials?workspace_id=workspace-a&provider_ref=domain-pack%2Fprovider&idempotency_key=credential-resource%3Acreate%3A42",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H2: {found}");
    assert_eq!(
        found.as_array().unwrap(),
        std::slice::from_ref(&first),
        "H2"
    );

    let source_id = first["id"].as_str().unwrap();
    let (status, validated) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{source_id}/validate"),
        Some(json!({
            "workspace_id": "workspace-a",
            "provider_ref": "domain-pack/provider"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H2: {validated}");
    assert_eq!(validated["status"], "valid", "H2");
    assert_eq!(validated["adapter_kind"], "credential_reference", "H2");
    assert_eq!(validated["credential_version"], first["version"], "H5");

    let (status, missing) = call(
        &h.app,
        "GET",
        "/v1/config/credentials?workspace_id=workspace-b&provider_ref=domain-pack%2Fprovider&idempotency_key=credential-resource%3Acreate%3A42",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H4: {missing}");
    assert_eq!(missing, json!([]), "H4");

    let (status, problem) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{source_id}/validate"),
        Some(json!({
            "workspace_id": "workspace-a",
            "provider_ref": "another-provider"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "H4: {problem}");
}

/// Hosted-list cause/effect graph: C1 the Workspace contains ordinary and hosted
/// sources; C2 `hosted_only=true`. C1+C2 yields only operation-owned receipts;
/// C1+!C2 preserves the existing complete management inventory.
#[tokio::test]
async fn hosted_credential_list_does_not_mix_model_credentials() {
    let h = harness();
    enter_vault_cred(&h.app, Some("model-provider"), "model-secret").await;
    let (status, hosted) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "vault",
            "provider_id": "domain-pack/provider",
            "idempotency_key": "resource-create-1",
            "secret": "business-secret" // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{hosted}");

    let (status, hosted_only) = call(
        &h.app,
        "GET",
        "/v1/config/credentials?workspace_id=ws&hosted_only=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hosted_only}");
    assert_eq!(hosted_only.as_array().unwrap(), &[hosted]);

    let (status, all) = call(
        &h.app,
        "GET",
        "/v1/config/credentials?workspace_id=ws",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all.as_array().unwrap().len(), 2);
}

/// Cause-effect graph: the generic API accepts a namespaced material type and
/// opaque fields without knowing an SSH/database/vendor schema; exactly one of
/// legacy scalar or structured material may cross the write-only seam.
///
/// | Rule | scalar | structured | effect |
/// |---|---|---|---|
/// | E1 | no | external SSH document | 201, secret-free response |
/// | E2 | yes | external document | 422 ambiguous input |
#[tokio::test]
async fn credential_entry_is_open_to_external_material_types_without_secret_leaks() {
    let h = harness();
    let private_key = "external-private-key"; // awaken-allow: secret
    let material = json!({
        "type_id": "acme.ssh-key/v1",
        "fields": {
            "private_key": private_key,
            "known_hosts": "example ssh-ed25519 AAAA"
        }
    });
    let (status, credential) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "vault",
            "material": material
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "E1: {credential}");
    assert!(
        !serde_json::to_string(&credential)
            .unwrap()
            .contains(private_key)
    );

    let (status, problem) = call(
        &h.app,
        "POST",
        "/v1/config/credentials",
        Some(json!({
            "workspace_id": "ws",
            "kind": "vault",
            "secret": "legacy",
            "material": {
                "type_id": "acme.ssh-key/v1",
                "fields": {"private_key": private_key}
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "E2: {problem}");
    assert_eq!(problem["code"], "credential_invalid", "E2");
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

#[tokio::test]
async fn generic_admin_route_never_creates_worker_local_credentials() {
    // Cause graph: WorkerLocal identity requires an atomic driver/subject locator
    // -> only `ensure_worker_local` may create it. The generic credential route
    // has no such contract and must reject both secretless and secret-bearing
    // inputs instead of reviving a second registration path.
    //
    // | Rule | generic input | secret | result |
    // | W1 | worker_local | absent | 422; use automatic Worker registration |
    // | W2 | worker_local | present | 422; secret never crosses control plane |
    let h = harness();
    for (rule, body) in [
        (
            "W1",
            json!({
                "workspace_id": "ws",
                "kind": "worker_local",
                "provider_id": "openai"
            }),
        ),
        (
            "W2",
            json!({
                "workspace_id": "ws",
                "kind": "worker_local",
                "provider_id": "openai",
                "secret": "must-not-cross-the-control-plane", // awaken-allow: secret
            }),
        ),
    ] {
        let (status, problem) = call(&h.app, "POST", "/v1/config/credentials", Some(body)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{rule}: {problem}"
        );
        assert_eq!(problem["code"], "credential_invalid", "{rule}");
    }
}

/// Agent input bindings are a versioned aggregate resolved at Session creation.
/// Invalid revisions fail at the write boundary and never enter the repository.
#[tokio::test]
async fn agent_input_bindings_reject_non_positive_versions() {
    let h = harness();
    for version in [-1, 0] {
        let (status, problem) = call(
            &h.app,
            "PUT",
            "/v1/config/agents/agent-1/resources",
            Some(json!({
                "agent_id": "ignored-body-id",
                "inputs": [],
                "revision": version
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
        assert_eq!(problem["code"], "invalid_revision");
    }

    let (status, problem) = call(&h.app, "GET", "/v1/config/agents/agent-1/resources", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{problem}");
}
