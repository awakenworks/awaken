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
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
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
        .with_state(repo)
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
        // An unknown subject is 404.
    }
}
