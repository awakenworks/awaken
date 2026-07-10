//! Data-subject erasure endpoint (ADR-0050 Slice 10), an Awaken extension.
//!
//! `POST /v1/user_profiles/:id/erasure` executes GDPR Art. 17 right-to-erasure
//! for the data subject, returning a receipt of how many content records were
//! removed. It takes the neutral [`DataSubjectResolver`] port, so the concrete
//! store/backend is injected by the assembly (server-local) — this router names
//! no store. Mounted separately from the SDK-compatible user-profiles router.

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_data_subject::{ConsentGrant, ConsentStatus, DataSubject, DataSubjectRepo, LawfulBasis};
use awaken_runtime_contract::{
    CaptureSink, ContentCapture, DataSubjectId, DataSubjectResolver, ErasureReceipt, Purpose,
};

/// Process-global captured-content sink (ADR-0050): the single-machine composition
/// root installs one, and every session's `context()` reads it so a run's captured
/// content lands in the same store the erasure endpoint fans out to — without
/// threading the sink through every `SharedHost` construction.
static PROCESS_SINK: OnceLock<Arc<dyn CaptureSink>> = OnceLock::new();

/// Install the process-global captured-content sink (idempotent; first wins).
pub fn install_capture_sink(sink: Arc<dyn CaptureSink>) {
    let _ = PROCESS_SINK.set(sink);
}

/// The process-global captured-content sink, if one was installed.
pub(crate) fn process_capture_sink() -> Option<Arc<dyn CaptureSink>> {
    PROCESS_SINK.get().cloned()
}
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// Mount the erasure route over an injected resolver.
pub fn erasure_router(resolver: Arc<dyn DataSubjectResolver>) -> Router {
    Router::new()
        .route("/v1/user_profiles/:id/erasure", post(erase))
        .with_state(resolver)
}

async fn erase(
    State(resolver): State<Arc<dyn DataSubjectResolver>>,
    Path(id): Path<String>,
) -> Json<ErasureReceipt> {
    Json(resolver.erase(&DataSubjectId(id)).await)
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
pub fn consent_router(repo: Arc<dyn DataSubjectRepo>) -> Router {
    Router::new()
        .route(
            "/v1/user_profiles/:id/consent",
            post(grant_consent).get(read_consent),
        )
        .route(
            "/v1/user_profiles/:id/capture-decision",
            get(capture_decision),
        )
        .with_state(repo)
}

/// Query for the capture-decision projection: the level the caller requests.
#[derive(Debug, Deserialize)]
struct DecisionQuery {
    #[serde(default)]
    requested: Option<ContentCapture>,
}

/// The read-only decision projection (ADR-0050 D8): the `meet` of the (env)
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
    State(repo): State<Arc<dyn DataSubjectRepo>>,
    Path(id): Path<String>,
    Query(q): Query<DecisionQuery>,
) -> Json<CaptureDecisionView> {
    let requested = q.requested.unwrap_or(ContentCapture::Full);
    // The open ceiling is the env default; managed will substitute the resolved
    // Org→Workspace→Agent ceiling here.
    let ceiling = crate::redact::env_capture_decision().level;
    let consent = match repo.get(&DataSubjectId(id)).await {
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
    State(repo): State<Arc<dyn DataSubjectRepo>>,
    Path(id): Path<String>,
    Json(body): Json<GrantBody>,
) -> Result<Json<ConsentView>, (StatusCode, String)> {
    let sid = DataSubjectId(id.clone());
    let now = now_millis();
    let mut subject = repo
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
    repo.put(subject.clone())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ConsentView::of(&subject)))
}

async fn read_consent(
    State(repo): State<Arc<dyn DataSubjectRepo>>,
    Path(id): Path<String>,
) -> Result<Json<ConsentView>, StatusCode> {
    let subject = repo
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

    #[async_trait::async_trait]
    impl DataSubjectResolver for StubResolver {
        async fn consent_ceiling(&self, _s: &DataSubjectId, _p: Purpose) -> ContentCapture {
            ContentCapture::Structured
        }
        async fn erase(&self, _s: &DataSubjectId) -> ErasureReceipt {
            ErasureReceipt {
                records_removed: self.removed,
            }
        }
    }

    #[tokio::test]
    async fn erase_returns_the_resolver_receipt() {
        let resolver: Arc<dyn DataSubjectResolver> = Arc::new(StubResolver { removed: 3 });
        let Json(receipt) = erase(State(resolver), Path("dsub_1".to_string())).await;
        assert_eq!(receipt.records_removed, 3);
    }

    #[tokio::test]
    async fn consent_grant_then_read_reflects_full_ceiling() {
        use awaken_data_subject::InMemoryDataSubjectRepo;
        let repo: Arc<dyn DataSubjectRepo> = Arc::new(InMemoryDataSubjectRepo::new());
        let Json(view) = grant_consent(
            State(repo.clone()),
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

        let Json(read) = read_consent(State(repo), Path("dsub_1".to_string()))
            .await
            .unwrap();
        assert_eq!(read.telemetry_content_ceiling, ContentCapture::Full);
    }

    #[tokio::test]
    async fn capture_decision_no_consent_clamps_and_reasons() {
        use awaken_data_subject::InMemoryDataSubjectRepo;
        let repo: Arc<dyn DataSubjectRepo> = Arc::new(InMemoryDataSubjectRepo::new());
        // Unknown subject: consent caps at Structured. With the env ceiling
        // defaulting to Structured, a Full request lands Structured.
        let Json(view) = capture_decision(
            State(repo.clone()),
            Path("nobody".to_string()),
            Query(DecisionQuery {
                requested: Some(ContentCapture::Full),
            }),
        )
        .await;
        assert_eq!(view.requested, ContentCapture::Full);
        assert_eq!(view.consent, ContentCapture::Structured);
        assert_eq!(view.effective, ContentCapture::Structured);
        // With the default env ceiling also Structured, the ceiling is the binding
        // clamp (consent is not strictly below it), so reason is clamped_by_ceiling.
        assert_eq!(view.reason, "clamped_by_ceiling");

        // A request at or below the effective level is "ok".
        let Json(ok) = capture_decision(
            State(repo),
            Path("nobody".to_string()),
            Query(DecisionQuery {
                requested: Some(ContentCapture::Structured),
            }),
        )
        .await;
        assert_eq!(ok.effective, ContentCapture::Structured);
        assert_eq!(ok.reason, "ok");
    }
}
