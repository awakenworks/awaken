//! Handler-level coverage for the dry-run resolve surface driven end-to-end
//! through the `axum` router over HTTP (the existing resolve tests call
//! `resolve_inference` / `resolve_profile*` directly and never touch the
//! handlers). Drives `resolve_route`, `resolve_profile_route`, the
//! `put_profile`/`get_profile` round-trip, and `resolve_profile_candidates_route`
//! — including its all-unresolvable FAIL-CLOSED path — plus the `validate_credential`
//! VC4 probe-returns-`invalid` arm the `router_cases` suite skips (it labels VC1/2/3/5).
//!
//! Every route is exercised through `oneshot` + `http-body-util`, reusing the same
//! store-injection harness idiom as `router_cases.rs`.
//!
//! Profile cause/effect design (executable rules below):
//! Causes: C1 primary target is exact; C2 ordered fallbacks exist; C3 one target
//! is unresolvable; C4 all targets are unresolvable; C5 a target is duplicated;
//! C6 legacy bare-model JSON is loaded; C7 candidates use different providers and
//! credential bindings.
//! Effects: E1 canonical structured profile is saved; E2 candidates preserve
//! authored order; E3 only the bad candidate is skipped; E4 resolution fails
//! closed; E5 save is rejected; E6 legacy input is rewritten canonically; E7 each
//! candidate resolves only with its own binding.
//!
//! Decision table:
//! | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Effect |
//! | T1   | 1  | 0  | 0  | 0  | 0  | 0  | 0  | E1     |
//! | T2   | 1  | 1  | 0  | 0  | 0  | 0  | 0  | E1,E2  |
//! | T3   | 1  | 1  | 1  | 0  | 0  | 0  | 0  | E3     |
//! | T4   | 0  | 1  | 1  | 1  | 0  | 0  | 0  | E4     |
//! | T5   | 1  | 1  | 0  | 0  | 1  | 0  | 0  | E5     |
//! | T6   | 0  | 0  | 0  | 0  | 0  | 1  | 0  | E6     |
//! | T7   | 1  | 1  | 0  | 0  | 0  | 0  | 1  | E2,E7  |

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

/// Records every probe call and answers with a fixed status (same double the
/// `router_cases` suite uses), so a test can prove the probe ran with the resolved
/// endpoint + materialized secret and still returned its configured verdict.
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

fn harness_with(probe: Option<Arc<dyn CredentialProbe>>) -> Harness {
    let catalog = Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new());
    let app = admin_router(AdminState {
        catalog: catalog.clone(),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: Arc::new(awaken_admin_config_api::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_admin_config_api::InMemoryAgentInputBindingRepository::new()),
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

/// Author a resolvable model: provider + endpoint (`dialect`) + offering.
async fn author_model(harness: &Harness, provider: &str, dialect: &str, model: &str) {
    author_model_at(harness, provider, dialect, model, "ep1").await;
}

async fn author_model_at(
    harness: &Harness,
    provider: &str,
    dialect: &str,
    model: &str,
    endpoint: &str,
) {
    support::seed_model(&harness.catalog, provider, dialect, model, endpoint).await;
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
// TASK 2 — resolve_route (POST /v1/config/inference/resolve) through the handler
// ---------------------------------------------------------------------------

/// resolve_route (success, None binding): a dry-run resolve returns the secret-free
/// view — the execution triple + adapter/endpoint — with `credential_present:false`
/// when the binding materializes no secret.
#[tokio::test]
async fn resolve_route_none_binding_is_secret_free_view() {
    let h = harness();
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;

    let (s, view) = call(
        &h.app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "claude-opus-4-8",
            "binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["model_id"], "claude-opus-4-8");
    assert_eq!(view["provider_id"], "anthropic");
    assert_eq!(view["protocol_endpoint_id"], "ep1");
    assert_eq!(view["adapter_kind"], "anthropic");
    assert_eq!(view["base_url"], "https://api.example.com/v1/");
    assert_eq!(view["credential_present"], false);
}

/// resolve_route (success, Exact binding): the same view reports
/// `credential_present:true` once a credential materializes, but never carries the
/// secret itself.
#[tokio::test]
async fn resolve_route_exact_binding_present_and_secret_free() {
    let h = harness();
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-resolve-route").await;

    let (s, view) = call(
        &h.app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "claude-opus-4-8",
            "binding": { "type": "exact", "credential_source_id": cred }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["credential_present"], true);
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("sk-resolve-route"),
        "secret leaked on resolve view"
    );
}

/// resolve_route (fail-closed): a model with no offering → the resolve problem
/// mapper answers 404 `model_unresolved`.
#[tokio::test]
async fn resolve_route_unknown_model_is_404_model_unresolved() {
    let h = harness();
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference/resolve",
        Some(json!({
            "workspace_id": "ws",
            "model_id": "ghost-model",
            "binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "model_unresolved");
}

// ---------------------------------------------------------------------------
// TASK 2 — put_profile / get_profile round-trip + resolve_profile_route
// ---------------------------------------------------------------------------

/// put_profile / get_profile: authoring a profile round-trips through the store,
/// and the path id is what the profile is fetched under.
#[tokio::test]
async fn put_then_get_profile_round_trips() {
    let h = harness();

    // A fresh id is not found until authored.
    let (s, err) = call(&h.app, "GET", "/v1/config/inference-profiles/prof-1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");

    let (s, put) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-1",
        Some(json!({
            "primary": {
                "target": { "model_id": "claude-opus-4-8", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" },
                "credential_binding": { "type": "none" }
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(put["primary"]["target"]["model_id"], "claude-opus-4-8");

    let (s, got) = call(&h.app, "GET", "/v1/config/inference-profiles/prof-1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["primary"]["target"]["model_id"], "claude-opus-4-8");
    assert_eq!(got["primary"]["credential_binding"]["type"], "none");
}

/// T5: duplicates (including the primary) make failover intent ambiguous and are
/// rejected before persistence. Empty ids and an excessive chain hit the other
/// validation leaves of the same effect.
#[tokio::test]
async fn put_profile_rejects_invalid_fallback_chains() {
    let h = harness();
    let duplicate = json!({
        "primary": { "target": { "model_id": "m", "provider_id": "p", "protocol_endpoint_id": "e" }, "credential_binding": { "type": "none" } },
        "fallbacks": [{ "target": { "model_id": "m", "provider_id": "p", "protocol_endpoint_id": "e" }, "credential_binding": { "type": "none" } }]
    });
    let empty = json!({
        "primary": { "target": { "model_id": "m", "provider_id": "p", "protocol_endpoint_id": "e" }, "credential_binding": { "type": "none" } },
        "fallbacks": [{ "target": { "model_id": "   " }, "credential_binding": { "type": "none" } }]
    });
    let too_many = json!({
        "primary": { "target": { "model_id": "m" }, "credential_binding": { "type": "none" } },
        "fallbacks": (0..9).map(|i| json!({ "target": { "model_id": format!("f{i}") }, "credential_binding": { "type": "none" } })).collect::<Vec<_>>()
    });
    for (id, body) in [
        ("duplicate", duplicate),
        ("empty", empty),
        ("long", too_many),
    ] {
        let (status, problem) = call(
            &h.app,
            "PUT",
            &format!("/v1/config/inference-profiles/{id}"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem["code"], "invalid_inference_profile");
        let (status, _) = call(
            &h.app,
            "GET",
            &format!("/v1/config/inference-profiles/{id}"),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "invalid profile was persisted"
        );
    }
}

/// T6: old persisted/API input is accepted once, then projected using only the
/// canonical structured contract so clients converge without a bulk migration.
#[tokio::test]
async fn legacy_profile_input_is_returned_as_structured_targets() {
    let h = harness();
    let (status, profile) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/legacy",
        Some(json!({
            "model_id": "primary",
            "model_fallbacks": ["fallback"],
            "credential_binding": { "type": "none" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(profile["primary"]["target"]["model_id"], "primary");
    assert_eq!(profile["fallbacks"][0]["target"]["model_id"], "fallback");
    assert!(profile.get("model_id").is_none());
    assert!(profile.get("model_fallbacks").is_none());
}

/// resolve_profile_route (success): a stored profile resolves its *primary* model
/// through the handler into the secret-free view.
#[tokio::test]
async fn resolve_profile_route_resolves_primary_model() {
    let h = harness();
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-1",
        Some(json!({
            "primary": { "target": { "model_id": "claude-opus-4-8", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, view) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-1/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(view["model_id"], "claude-opus-4-8");
    assert_eq!(view["adapter_kind"], "anthropic");
    assert_eq!(view["credential_present"], false);
}

/// resolve_profile_route (missing profile): an unauthored id → 404 `not_found`
/// (`profile_missing`).
#[tokio::test]
async fn resolve_profile_route_missing_profile_is_404() {
    let h = harness();
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/nobody/resolve",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["status"], 404);
}

// ---------------------------------------------------------------------------
// TASK 1 — resolve_profile_candidates_route through the handler.
// `/v1/config/inference-profiles/{id}/resolve-candidates` is mounted by
// `admin_router` AND documented in `openapi::paths()` (response schema
// `ResolvedCandidatesView`), so the `openapi_contract` drift gate probes it; these
// tests exercise the handler behavior end-to-end.
// ---------------------------------------------------------------------------

/// resolve_profile_candidates_route (success): a multi-model profile resolves its
/// whole model axis into the ordered candidate list, one secret-free view per model
/// in failover order.
#[tokio::test]
async fn resolve_candidates_route_returns_ordered_axis() {
    let h = harness();
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    support::seed_model(
        &h.catalog,
        "anthropic",
        "anthropic_messages",
        "claude-haiku-4-8",
        "ep1",
    )
    .await;

    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-multi",
        Some(json!({
            "primary": { "target": { "model_id": "claude-opus-4-8", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } },
            "fallbacks": [{ "target": { "model_id": "claude-haiku-4-8", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, out) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-multi/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let candidates = out["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2);
    // Axis order is preserved: primary first, fallback second.
    assert_eq!(candidates[0]["model_id"], "claude-opus-4-8");
    assert_eq!(candidates[1]["model_id"], "claude-haiku-4-8");
}

/// T7: a BYOK primary and managed fallback retain separate identities. Reusing
/// the Anthropic key for OpenAI would fail `can_consume`; the per-step `none`
/// binding proves the fallback no longer inherits the primary credential.
#[tokio::test]
async fn resolve_candidates_use_each_steps_own_credential_binding() {
    let h = harness();
    author_model_at(
        &h,
        "anthropic",
        "anthropic_messages",
        "primary-model",
        "anthropic-ep",
    )
    .await;
    author_model_at(
        &h,
        "openai",
        "open_ai_responses",
        "managed-fallback",
        "openai-ep",
    )
    .await;
    let credential = enter_vault_cred(&h.app, Some("anthropic"), "sk-primary").await;
    let (status, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-cross-provider",
        Some(json!({
            "primary": {
                "target": { "model_id": "primary-model", "provider_id": "anthropic", "protocol_endpoint_id": "anthropic-ep" },
                "credential_binding": { "type": "exact", "credential_source_id": credential }
            },
            "fallbacks": [{
                "target": { "model_id": "managed-fallback", "provider_id": "openai", "protocol_endpoint_id": "openai-ep" },
                "credential_binding": { "type": "none" }
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-cross-provider/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let candidates = body["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["provider_id"], "anthropic");
    assert_eq!(candidates[0]["credential_present"], true);
    assert_eq!(candidates[1]["provider_id"], "openai");
    assert_eq!(candidates[1]["credential_present"], false);
}

/// resolve_profile_candidates_route (partial resolve is not terminal): an
/// unresolvable fallback (no offering) is *skipped*, so the primary still yields a
/// one-element candidate list — one bad model does not sink the profile.
#[tokio::test]
async fn resolve_candidates_route_skips_unresolvable_fallback() {
    let h = harness();
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;

    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-partial",
        Some(json!({
            "primary": { "target": { "model_id": "claude-opus-4-8", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } },
            "fallbacks": [{ "target": { "model_id": "ghost-model", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, out) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-partial/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let candidates = out["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["model_id"], "claude-opus-4-8");
}

/// resolve_profile_candidates_route (ALL-UNRESOLVABLE fail-closed): a profile whose
/// every model has no offering resolves to *no* candidate, and the handler fails
/// closed on the last reason — 404 `model_unresolved`, never an empty-list success.
#[tokio::test]
async fn resolve_candidates_route_all_unresolvable_is_fail_closed_404() {
    let h = harness();
    // No catalog authored at all → both models are unresolvable.
    let (s, _) = call(
        &h.app,
        "PUT",
        "/v1/config/inference-profiles/prof-dead",
        Some(json!({
            "primary": { "target": { "model_id": "ghost-primary", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } },
            "fallbacks": [{ "target": { "model_id": "ghost-fallback", "provider_id": "anthropic", "protocol_endpoint_id": "ep1" }, "credential_binding": { "type": "none" } }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/prof-dead/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "model_unresolved");
    // Fail-closed: no candidate list ever leaks on the error body.
    assert!(err.get("candidates").is_none());
}

/// resolve_profile_candidates_route (missing profile): an unauthored profile id →
/// 404 `not_found` (`profile_missing`), before any catalog snapshot.
#[tokio::test]
async fn resolve_candidates_route_missing_profile_is_404() {
    let h = harness();
    let (s, err) = call(
        &h.app,
        "POST",
        "/v1/config/inference-profiles/nobody/resolve-candidates",
        Some(json!({ "workspace_id": "ws" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["status"], 404);
}

// ---------------------------------------------------------------------------
// TASK 2 — validate_credential VC4: probe runs and returns `invalid`
// (router_cases labels VC1 valid / VC2 no-probe / VC3 non-anthropic / VC5 resolve
// failure, but never the probe-returns-invalid arm).
// ---------------------------------------------------------------------------

/// VC4: probe wired + anthropic adapter + materialized secret → the real probe runs
/// and its `invalid` verdict is returned end-to-end, with the resolved endpoint +
/// materialized secret + model fed to it, and no secret on the wire response.
#[tokio::test]
async fn vc4_probe_invalid_is_returned_end_to_end() {
    let probe = RecordingProbe::new(ProbeStatus::Invalid);
    let h = harness_with(Some(probe.clone()));
    author_model(&h, "anthropic", "anthropic_messages", "claude-opus-4-8").await;
    let cred = enter_vault_cred(&h.app, Some("anthropic"), "sk-live-invalid").await;

    let (s, body) = call(
        &h.app,
        "POST",
        &format!("/v1/config/credentials/{cred}/validate"),
        Some(json!({ "workspace_id": "ws", "model_id": "claude-opus-4-8" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "invalid");
    assert_eq!(body["adapter_kind"], "anthropic");

    // The probe ran exactly once, with the resolved endpoint + materialized secret.
    assert_eq!(probe.call_count(), 1);
    let calls = probe.calls.lock().unwrap();
    assert_eq!(calls[0].0, "https://api.example.com/v1/");
    assert_eq!(calls[0].1, "sk-live-invalid");
    assert_eq!(calls[0].2, "claude-opus-4-8");
    assert!(
        !serde_json::to_string(&body)
            .unwrap()
            .contains("sk-live-invalid"),
        "secret leaked on validate response"
    );
}
