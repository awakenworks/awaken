//! Data-subject erasure endpoint (ADR-0050 Slice 10), an Awaken extension.
//!
//! `POST /v1/user_profiles/:id/erasure` executes GDPR Art. 17 right-to-erasure
//! for the data subject, returning a receipt of how many content records were
//! removed. It takes the neutral [`DataSubjectResolver`] port, so the concrete
//! store/backend is injected by the assembly (server-local) — this router names
//! no store. Mounted separately from the SDK-compatible user-profiles router.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_data_subject::{ConsentGrant, ConsentStatus, DataSubject, DataSubjectRepo, LawfulBasis};
use awaken_runtime_contract::{
    ContentCapture, DataSubjectId, DataSubjectResolver, ErasureReceipt, Purpose,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Mount the erasure route over an injected resolver.
pub fn erasure_router(resolver: Arc<dyn DataSubjectResolver>) -> Router {
    Router::new()
        .route("/v1/user_profiles/{id}/erasure", post(erase))
        .with_state(resolver)
}

async fn erase(
    State(resolver): State<Arc<dyn DataSubjectResolver>>,
    Path(id): Path<String>,
) -> Result<Json<ErasureReceipt>, StatusCode> {
    // Fail-closed: a backend erasure/accountability failure is a 500, never a
    // success receipt — the caller must not be told the data was erased when it
    // was not (GDPR Art. 17).
    resolver
        .erase(&DataSubjectId(id))
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// The `POST` body granting consent for one purpose (ADR-0050): the controller's
/// assertion, or the enrollment page's grant. Awaken-neutral — NOT the Anthropic
/// `trust_grants` field.
#[derive(Debug, Deserialize)]
struct GrantBody {
    purpose: Purpose,
    #[serde(default)]
    version: String,
}

/// The consent projection returned by the consent routes.
#[derive(Debug, Serialize)]
struct ConsentView {
    id: String,
    grants: Vec<ConsentGrant>,
    /// The resolved capture ceiling this subject's consent permits for telemetry.
    telemetry_content_ceiling: ContentCapture,
}

impl ConsentView {
    fn of(subject: &DataSubject) -> Self {
        Self {
            id: subject.id.0.clone(),
            grants: subject.consents.clone(),
            telemetry_content_ceiling: subject.consent_ceiling(Purpose::TelemetryContent),
        }
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Mount the consent read/write routes over an injected subject repo (ADR-0050).
/// `POST /v1/user_profiles/:id/consent` records a `Granted` grant (creating the
/// subject if absent); `GET` reflects the subject's grants + resolved ceiling.
pub fn consent_router(repo: Arc<dyn DataSubjectRepo>, ceiling: ContentCapture) -> Router {
    let state = Arc::new(ConsentApiState {
        repo,
        ceiling,
        // Enrollment signing is an internal process capability, not operator
        // configuration. A fresh high-entropy process key avoids ambient secret
        // input and invalidates outstanding links across a restart.
        enrollment_secret: uuid::Uuid::new_v4().as_bytes().to_vec(),
    });
    Router::new()
        .route(
            "/v1/user_profiles/{id}/consent",
            post(grant_consent).get(read_consent),
        )
        .route(
            "/v1/user_profiles/{id}/capture-decision",
            get(capture_decision),
        )
        // Enrollment web flow (ADR-0050 D3/G3): mint a signed URL, the end user
        // visits an HTML consent page and accepts, which records the grant.
        .route("/v1/user_profiles/{id}/enroll", post(mint_enrollment))
        .route("/enroll/{token}", get(enroll_page))
        .route("/enroll/{token}/grant", post(enroll_grant))
        .with_state(state)
}

struct ConsentApiState {
    repo: Arc<dyn DataSubjectRepo>,
    ceiling: ContentCapture,
    enrollment_secret: Vec<u8>,
}

/// Keyed digest over the base64 payload: an attacker cannot forge a token
/// without the secret.
fn sign(secret: &[u8], payload_b64: &str) -> String {
    let mut h = Sha256::new();
    h.update(secret);
    h.update(b".");
    h.update(payload_b64.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Serialize, Deserialize)]
struct EnrollPayload {
    subject: String,
    purpose: Purpose,
    /// Epoch-millis expiry.
    exp: i64,
}

fn b64_engine() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Verify a `<payload>.<sig>` token and its expiry, returning the payload.
fn verify_token(secret: &[u8], token: &str) -> Option<EnrollPayload> {
    let (payload_b64, sig) = token.split_once('.')?;
    if sign(secret, payload_b64) != sig {
        return None;
    }
    let bytes = b64_engine().decode(payload_b64).ok()?;
    let payload: EnrollPayload = serde_json::from_slice(&bytes).ok()?;
    (payload.exp >= now_millis()).then_some(payload)
}

/// `POST /v1/user_profiles/:id/enroll?purpose=…` — mint a signed, expiring URL to
/// send to the end user.
async fn mint_enrollment(
    State(state): State<Arc<ConsentApiState>>,
    Path(id): Path<String>,
    Query(q): Query<GrantBody>,
) -> Json<serde_json::Value> {
    let exp = now_millis() + 15 * 60 * 1000;
    let payload = EnrollPayload {
        subject: id,
        purpose: q.purpose,
        exp,
    };
    let payload_b64 = b64_engine().encode(serde_json::to_vec(&payload).unwrap_or_default());
    let token = format!(
        "{payload_b64}.{}",
        sign(&state.enrollment_secret, &payload_b64)
    );
    Json(serde_json::json!({
        "type": "enrollment_url",
        "url": format!("/enroll/{token}"),
        "expires_at": exp,
    }))
}

/// `GET /enroll/:token` — the end-user consent page.
async fn enroll_page(
    State(state): State<Arc<ConsentApiState>>,
    Path(token): Path<String>,
) -> Html<String> {
    match verify_token(&state.enrollment_secret, &token) {
        Some(p) => Html(format!(
            "<!doctype html><h1>Consent</h1><p>Grant <b>{:?}</b> for <b>{}</b>?</p>\
             <form method=\"post\" action=\"/enroll/{token}/grant\">\
             <button type=\"submit\">Accept</button></form>",
            p.purpose, p.subject
        )),
        None => Html("<!doctype html><p>This enrollment link is invalid or expired.</p>".into()),
    }
}

/// `POST /enroll/:token/grant` — the end user accepts; record the grant.
async fn enroll_grant(
    State(state): State<Arc<ConsentApiState>>,
    Path(token): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let payload = verify_token(&state.enrollment_secret, &token).ok_or(StatusCode::BAD_REQUEST)?;
    let sid = DataSubjectId(payload.subject.clone());
    let now = now_millis();
    let mut subject = state
        .repo
        .get(&sid)
        .await
        .unwrap_or_else(|_| DataSubject::new(sid.clone(), "open", now));
    subject.upsert_consent(ConsentGrant {
        purpose: payload.purpose,
        status: ConsentStatus::Granted,
        basis: LawfulBasis::Consent,
        granted_at: now,
        version: "enrollment".to_string(),
    });
    subject.updated_at = now;
    state
        .repo
        .put(subject)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(
        "<!doctype html><p>Thank you — your consent has been recorded.</p>".to_string(),
    ))
}

/// Query for the capture-decision projection: the level the caller requests.
#[derive(Debug, Deserialize)]
struct DecisionQuery {
    #[serde(default)]
    requested: Option<ContentCapture>,
}

/// The read-only decision projection (ADR-0050 D8): the `meet` of the deployment
/// ceiling × the requested level × the subject's consent, plus the reason the
/// effective level landed where it did. GDPR auditability is a response field.
#[derive(Debug, Serialize)]
struct CaptureDecisionView {
    requested: ContentCapture,
    ceiling: ContentCapture,
    consent: ContentCapture,
    effective: ContentCapture,
    reason: &'static str,
}

async fn capture_decision(
    State(state): State<Arc<ConsentApiState>>,
    Path(id): Path<String>,
    Query(q): Query<DecisionQuery>,
) -> Json<CaptureDecisionView> {
    let requested = q.requested.unwrap_or(ContentCapture::Full);
    let ceiling = state.ceiling;
    let consent = match state.repo.get(&DataSubjectId(id)).await {
        Ok(s) => s.consent_ceiling(Purpose::TelemetryContent),
        Err(_) => ContentCapture::Structured,
    };
    let effective = ceiling.meet(requested).meet(consent);
    let reason = if effective == requested {
        "ok"
    } else if effective == consent && consent < ceiling {
        "no_consent"
    } else {
        "clamped_by_ceiling"
    };
    Json(CaptureDecisionView {
        requested,
        ceiling,
        consent,
        effective,
        reason,
    })
}

async fn grant_consent(
    State(state): State<Arc<ConsentApiState>>,
    Path(id): Path<String>,
    Json(body): Json<GrantBody>,
) -> Result<Json<ConsentView>, (StatusCode, String)> {
    let sid = DataSubjectId(id.clone());
    let now = now_millis();
    let mut subject = state
        .repo
        .get(&sid)
        .await
        .unwrap_or_else(|_| DataSubject::new(sid.clone(), "open", now));
    subject.upsert_consent(ConsentGrant {
        purpose: body.purpose,
        status: ConsentStatus::Granted,
        basis: LawfulBasis::Consent,
        granted_at: now,
        version: body.version,
    });
    subject.updated_at = now;
    state
        .repo
        .put(subject.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ConsentView::of(&subject)))
}

async fn read_consent(
    State(state): State<Arc<ConsentApiState>>,
    Path(id): Path<String>,
) -> Result<Json<ConsentView>, StatusCode> {
    let subject = state
        .repo
        .get(&DataSubjectId(id))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(ConsentView::of(&subject)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{ContentCapture, Purpose};

    struct StubResolver {
        removed: usize,
    }

    fn consent_state(
        repo: Arc<dyn DataSubjectRepo>,
        ceiling: ContentCapture,
    ) -> Arc<ConsentApiState> {
        Arc::new(ConsentApiState {
            repo,
            ceiling,
            enrollment_secret: b"test-only-enrollment-secret".to_vec(),
        })
    }

    #[async_trait::async_trait]
    impl DataSubjectResolver for StubResolver {
        async fn consent_ceiling(&self, _s: &DataSubjectId, _p: Purpose) -> ContentCapture {
            ContentCapture::Structured
        }
        async fn erase(
            &self,
            _s: &DataSubjectId,
        ) -> Result<ErasureReceipt, awaken_runtime_contract::ErasureError> {
            Ok(ErasureReceipt {
                records_removed: self.removed,
            })
        }
    }

    #[tokio::test]
    async fn erase_returns_the_resolver_receipt() {
        let resolver: Arc<dyn DataSubjectResolver> = Arc::new(StubResolver { removed: 3 });
        let Json(receipt) = erase(State(resolver), Path("dsub_1".to_string()))
            .await
            .expect("erase succeeds");
        assert_eq!(receipt.records_removed, 3);
    }

    #[tokio::test]
    async fn consent_grant_then_read_reflects_full_ceiling() {
        use awaken_data_subject::InMemoryDataSubjectRepo;
        let repo: Arc<dyn DataSubjectRepo> = Arc::new(InMemoryDataSubjectRepo::new());
        let state = consent_state(repo, ContentCapture::Structured);
        let Json(view) = grant_consent(
            State(state.clone()),
            Path("dsub_1".to_string()),
            Json(GrantBody {
                purpose: Purpose::TelemetryContent,
                version: "v1".to_string(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(view.telemetry_content_ceiling, ContentCapture::Full);
        assert_eq!(view.grants.len(), 1);

        let Json(read) = read_consent(State(state), Path("dsub_1".to_string()))
            .await
            .unwrap();
        assert_eq!(read.telemetry_content_ceiling, ContentCapture::Full);
    }

    #[test]
    fn enrollment_token_roundtrips_and_rejects_tampering() {
        // Cause/effect decision table:
        // | payload | signature key | expiry | Effect |
        // | valid | exact | future | accept |
        // | valid | wrong/tampered | future | reject |
        // | valid | exact | past | reject |
        let secret = b"test-only-enrollment-secret";
        let payload = EnrollPayload {
            subject: "dsub_1".into(),
            purpose: Purpose::TelemetryContent,
            exp: now_millis() + 60_000,
        };
        let b64 = b64_engine().encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("{b64}.{}", sign(secret, &b64));
        let got = verify_token(secret, &token).expect("valid token verifies");
        assert_eq!(got.subject, "dsub_1");
        assert_eq!(got.purpose, Purpose::TelemetryContent);

        // A tampered signature is rejected.
        assert!(verify_token(secret, &format!("{b64}.deadbeef")).is_none());
        // An expired token is rejected.
        let expired = EnrollPayload {
            exp: now_millis() - 1,
            ..EnrollPayload {
                subject: "s".into(),
                purpose: Purpose::TelemetryContent,
                exp: 0,
            }
        };
        let eb = b64_engine().encode(serde_json::to_vec(&expired).unwrap());
        assert!(verify_token(secret, &format!("{eb}.{}", sign(secret, &eb))).is_none());
    }

    #[tokio::test]
    async fn capture_decision_no_consent_clamps_and_reasons() {
        use awaken_data_subject::InMemoryDataSubjectRepo;
        let repo: Arc<dyn DataSubjectRepo> = Arc::new(InMemoryDataSubjectRepo::new());
        let state = consent_state(repo, ContentCapture::Structured);
        // Unknown subject: consent caps at Structured. With the deployment ceiling
        // set to Structured, a Full request lands Structured.
        let Json(view) = capture_decision(
            State(state.clone()),
            Path("nobody".to_string()),
            Query(DecisionQuery {
                requested: Some(ContentCapture::Full),
            }),
        )
        .await;
        assert_eq!(view.requested, ContentCapture::Full);
        assert_eq!(view.consent, ContentCapture::Structured);
        assert_eq!(view.effective, ContentCapture::Structured);
        // With the deployment ceiling also Structured, the ceiling is the binding
        // clamp (consent is not strictly below it), so reason is clamped_by_ceiling.
        assert_eq!(view.reason, "clamped_by_ceiling");

        // A request at or below the effective level is "ok".
        let Json(ok) = capture_decision(
            State(state),
            Path("nobody".to_string()),
            Query(DecisionQuery {
                requested: Some(ContentCapture::Structured),
            }),
        )
        .await;
        assert_eq!(ok.effective, ContentCapture::Structured);
        assert_eq!(ok.reason, "ok");
    }

    /// Cause/effect graph:
    /// - C1: Alice and Bob each own captured telemetry and a harvested ACP home.
    /// - C2: the HTTP erasure target is Alice.
    /// - E1: the receipt sums Alice's two stores; E2: Alice's blob is gone;
    ///   E3: Bob's blob and records remain; E4: retry is idempotent.
    /// Decision-table rule R1 = C1(true) × C2(Alice) -> E1+E2+E3+E4. Store
    /// failure -> HTTP 500 is covered by the handler's fail-closed unit seam.
    #[tokio::test]
    async fn art17_http_fans_out_over_capture_and_harvested_acp_content() {
        use awaken_data_subject::{
            InMemoryCapturedContentStore, InMemoryDataSubjectRepo, RepoDataSubjectResolver,
        };
        use awaken_run_executor_acp::{
            ConfigHome, DirSessionHome, FsSessionBlobStore, SessionBlobStore, SessionHomeKey,
            SessionHomePlan, SessionHomeProvider,
        };
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        let config_root = tempfile::tempdir().unwrap();
        let blob_root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(FsSessionBlobStore::new(blob_root.path().to_path_buf()));
        let homes = DirSessionHome::new(Some(config_root.path().to_path_buf()), blobs.clone());
        let plan = SessionHomePlan {
            config_home_env: "CODEX_HOME".into(),
            session_subpath: "sessions".into(),
            exclude: vec!["auth.json".into()],
            keyed_by_cwd: false,
        };
        let alice = SessionHomeKey {
            data_subject_id: Some("dsub_alice".into()),
            thread_id: "shared-thread".into(),
            adapter: "codex".into(),
        };
        let bob = SessionHomeKey {
            data_subject_id: Some("dsub_bob".into()),
            ..alice.clone()
        };
        let home = ConfigHome::open(Some(config_root.path()), "shared-thread").unwrap();
        home.write("sessions/opaque", b"alice-session").unwrap();
        homes.harvest(&alice, &plan).await;
        home.write("sessions/opaque", b"bob-session").unwrap();
        homes.harvest(&bob, &plan).await;

        let captured = Arc::new(InMemoryCapturedContentStore::new());
        captured.insert(
            DataSubjectId("dsub_alice".into()),
            Purpose::TelemetryContent,
            "alice prompt",
            now_millis(),
        );
        captured.insert(
            DataSubjectId("dsub_bob".into()),
            Purpose::TelemetryContent,
            "bob prompt",
            now_millis(),
        );
        let repo = Arc::new(InMemoryDataSubjectRepo::new());
        let resolver: Arc<dyn DataSubjectResolver> = Arc::new(
            RepoDataSubjectResolver::new(repo)
                .with_eraser(captured)
                .with_eraser(blobs.clone()),
        );
        let app = erasure_router(resolver);
        let erase_alice = || {
            Request::builder()
                .method("POST")
                .uri("/v1/user_profiles/dsub_alice/erasure")
                .body(Body::empty())
                .unwrap()
        };

        let response = app.clone().oneshot(erase_alice()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let receipt: ErasureReceipt =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(receipt.records_removed, 2);
        let alice_dest = tempfile::tempdir().unwrap();
        let bob_dest = tempfile::tempdir().unwrap();
        assert!(!blobs.fetch(&alice, alice_dest.path()).await.unwrap());
        assert!(blobs.fetch(&bob, bob_dest.path()).await.unwrap());

        let retry = app.oneshot(erase_alice()).await.unwrap();
        let receipt: ErasureReceipt =
            serde_json::from_slice(&to_bytes(retry.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(receipt.records_removed, 2);
    }
}
