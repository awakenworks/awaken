//! Data-subject erasure endpoint (ADR-0050 Slice 10), an Awaken extension.
//!
//! `POST /v1/user_profiles/:id/erasure` executes GDPR Art. 17 right-to-erasure
//! for the data subject, returning a receipt of how many content records were
//! removed. It takes the neutral [`DataSubjectResolver`] port, so the concrete
//! store/backend is injected by the assembly (server-local) — this router names
//! no store. Mounted separately from the SDK-compatible user-profiles router.

use std::sync::Arc;

use awaken_runtime_contract::{DataSubjectId, DataSubjectResolver, ErasureReceipt};
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};

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
}
