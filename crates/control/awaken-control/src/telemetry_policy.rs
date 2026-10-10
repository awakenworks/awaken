// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Static owner: the Workspace layer of the content-capture ceiling (ADR-0050 D3).
// @formal reason: Dynamic path: an authored narrowing is admitted only when it stays within the
// @formal reason: deployment ceiling, written by compare-and-set, and projected with the composed
// @formal reason: effective level; the Run boundary composes the same value through one port. Evidence:
// @formal reason: analytics.observability.projection_only plus this module's route tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Workspace telemetry narrowing authoring (`/v1/config/telemetry`).
//!
//! The deployment configuration holds the binding capture ceiling in a
//! self-hosted deployment; a Workspace may only tighten it, and a wider value
//! is rejected. The authored value reaches every Run through
//! [`RepositoryTelemetryNarrowing`], which the execution host composes at the
//! Run boundary with the same [`TelemetryCeiling`] this projection reports.

use std::sync::Arc;

use awaken_config_resolver::{
    TelemetryConfigError, WorkspaceTelemetryConfiguration, WorkspaceTelemetryRepository,
};
use awaken_data_governance_contract::{
    TelemetryCeiling, TelemetryNarrowing, TelemetryNarrowingSource, TelemetryPolicyError,
};
use awaken_runtime_contract::ContentCapture;
use awaken_tenancy::WorkspaceScope;
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Serves the Workspace telemetry repository as the Run-boundary
/// [`TelemetryNarrowingSource`]: an unwritten Workspace has no narrowing; an
/// unreadable one is an error the Run boundary turns into a closed ceiling.
pub struct RepositoryTelemetryNarrowing {
    repository: Arc<dyn WorkspaceTelemetryRepository>,
}

impl RepositoryTelemetryNarrowing {
    #[must_use]
    pub fn new(repository: Arc<dyn WorkspaceTelemetryRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait::async_trait]
impl TelemetryNarrowingSource for RepositoryTelemetryNarrowing {
    async fn workspace_narrowing(
        &self,
        workspace: &str,
    ) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
        match self.repository.get_telemetry_configuration(workspace).await {
            Ok(stored) => Ok(TelemetryNarrowing {
                content_capture: stored.and_then(|config| config.content_capture),
            }),
            Err(error) => {
                tracing::warn!(%workspace, %error, "Workspace telemetry narrowing unreadable");
                Err(TelemetryPolicyError::StoreUnavailable)
            }
        }
    }
}

/// Authoring service for Workspace telemetry narrowing.
#[derive(Clone)]
pub struct TelemetryPolicyService {
    store: Arc<dyn WorkspaceTelemetryRepository>,
    ceiling: ContentCapture,
}

impl TelemetryPolicyService {
    /// `ceiling` is the binding top-layer level (the deployment's
    /// `runtime.content_capture.level` in a self-hosted deployment).
    #[must_use]
    pub fn new(store: Arc<dyn WorkspaceTelemetryRepository>, ceiling: ContentCapture) -> Self {
        Self { store, ceiling }
    }

    /// The Run-boundary source over the same repository this service writes.
    #[must_use]
    pub fn narrowing_source(&self) -> Arc<dyn TelemetryNarrowingSource> {
        Arc::new(RepositoryTelemetryNarrowing::new(self.store.clone()))
    }

    async fn load(
        &self,
        workspace: &str,
    ) -> Result<WorkspaceTelemetryConfiguration, TelemetryConfigError> {
        Ok(self
            .store
            .get_telemetry_configuration(workspace)
            .await?
            .unwrap_or_else(|| WorkspaceTelemetryConfiguration::new(workspace.into())))
    }

    async fn save(
        &self,
        workspace: &str,
        expected: u64,
        content_capture: Option<ContentCapture>,
    ) -> Result<WorkspaceTelemetryConfiguration, TelemetryConfigError> {
        let current = self.load(workspace).await?;
        let next = current.narrowed(expected, content_capture, self.ceiling)?;
        if next.content_capture == current.content_capture {
            return Ok(current);
        }
        self.store
            .put_telemetry_configuration(expected, &next)
            .await?;
        Ok(next)
    }

    fn view(&self, config: &WorkspaceTelemetryConfiguration) -> TelemetryConfigurationView {
        TelemetryConfigurationView {
            workspace_id: config.workspace_id.clone(),
            revision: config.revision,
            content_capture: config.content_capture,
            ceiling: self.ceiling,
            effective: TelemetryCeiling::top(self.ceiling)
                .narrow(&TelemetryNarrowing {
                    content_capture: config.content_capture,
                })
                .content_capture,
        }
    }
}

/// The Workspace's narrowing with the ceiling it narrows and the result.
#[derive(Debug, Serialize)]
struct TelemetryConfigurationView {
    workspace_id: String,
    revision: u64,
    /// The Workspace's narrowing; `null` inherits the ceiling.
    content_capture: Option<ContentCapture>,
    /// The binding top-layer ceiling.
    ceiling: ContentCapture,
    /// `ceiling` tightened by `content_capture`; Agent, Session and consent
    /// may narrow it further for an individual Run.
    effective: ContentCapture,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PutTelemetry {
    expected_revision: u64,
    #[serde(default)]
    content_capture: Option<ContentCapture>,
}

type ApiResult = Result<Json<TelemetryConfigurationView>, (StatusCode, Json<serde_json::Value>)>;

fn failure(error: TelemetryConfigError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, kind) = match &error {
        TelemetryConfigError::Conflict => (StatusCode::CONFLICT, "invalid_request_error"),
        TelemetryConfigError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        TelemetryConfigError::Storage(_) => (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
    };
    (
        status,
        Json(json!({"error": {"type": kind, "message": error.to_string()}})),
    )
}

async fn read(
    State(service): State<TelemetryPolicyService>,
    Extension(workspace): Extension<WorkspaceScope>,
) -> ApiResult {
    let config = service.load(&workspace.0).await.map_err(failure)?;
    Ok(Json(service.view(&config)))
}

async fn write(
    State(service): State<TelemetryPolicyService>,
    Extension(workspace): Extension<WorkspaceScope>,
    Json(request): Json<PutTelemetry>,
) -> ApiResult {
    let config = service
        .save(
            &workspace.0,
            request.expected_revision,
            request.content_capture,
        )
        .await
        .map_err(failure)?;
    Ok(Json(service.view(&config)))
}

pub(crate) fn router(service: TelemetryPolicyService) -> Router {
    Router::new()
        .route("/v1/config/telemetry", get(read).put(write))
        .with_state(service)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Default)]
    pub(crate) struct MapRepository {
        pub(crate) rows: Mutex<BTreeMap<String, WorkspaceTelemetryConfiguration>>,
        pub(crate) unavailable: bool,
    }

    #[async_trait::async_trait]
    impl WorkspaceTelemetryRepository for MapRepository {
        async fn get_telemetry_configuration(
            &self,
            workspace: &str,
        ) -> Result<Option<WorkspaceTelemetryConfiguration>, TelemetryConfigError> {
            if self.unavailable {
                return Err(TelemetryConfigError::Storage("down".into()));
            }
            Ok(self.rows.lock().unwrap().get(workspace).cloned())
        }

        async fn put_telemetry_configuration(
            &self,
            expected: u64,
            next: &WorkspaceTelemetryConfiguration,
        ) -> Result<(), TelemetryConfigError> {
            awaken_config_resolver::validate_telemetry_configuration_write(expected, next)?;
            let mut rows = self.rows.lock().unwrap();
            let current = rows.get(&next.workspace_id).map_or(0, |row| row.revision);
            if current != expected {
                return Err(TelemetryConfigError::Conflict);
            }
            rows.insert(next.workspace_id.clone(), next.clone());
            Ok(())
        }
    }

    async fn call(
        router: &Router,
        workspace: &str,
        method: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri("/v1/config/telemetry")
            .header("content-type", "application/json")
            .extension(WorkspaceScope(workspace.into()))
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        // Axum's own extractor rejections (e.g. 422) answer in plain text.
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });
        (status, value)
    }

    #[tokio::test]
    async fn workspace_narrowing_routes_decision_table() {
        // Causes: C1 deployment ceiling = structured; C2 requested narrowing
        // (unset/off/structured/full); C3 expected revision current or stale;
        // C4 Workspace. Effects: E1 200 with effective = min(ceiling, narrowing);
        // E2 400 for a narrowing wider than the ceiling; E3 409 for a stale
        // revision; E4 one Workspace's narrowing never applies to another.
        let repository = Arc::new(MapRepository::default());
        let service = TelemetryPolicyService::new(repository.clone(), ContentCapture::Structured);
        let router = router(service.clone());

        let (status, initial) = call(&router, "ws-a", "GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            initial,
            json!({"workspace_id": "ws-a", "revision": 0, "content_capture": null,
                   "ceiling": "structured", "effective": "structured"})
        );

        let (status, _) = call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 0, "content_capture": "full"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "E2 wider is rejected");

        let (status, narrowed) = call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 0, "content_capture": "off"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(narrowed["revision"], 1);
        assert_eq!(narrowed["effective"], "off", "E1");

        let (status, _) = call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 0, "content_capture": "structured"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "E3");

        let (status, other) = call(&router, "ws-b", "GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(other["effective"], "structured", "E4");

        let (status, unchanged) = call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 1, "content_capture": "off"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(unchanged["revision"], 1, "an identical write is idempotent");

        let (status, cleared) = call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cleared["revision"], 2);
        assert_eq!(cleared["content_capture"], serde_json::Value::Null);
        assert_eq!(cleared["effective"], "structured");

        let (status, _) = call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 2, "retention_days": 1})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "closed request shape"
        );

        // The Run-boundary source reads exactly what the route wrote.
        let source = service.narrowing_source();
        call(
            &router,
            "ws-a",
            "PUT",
            Some(json!({"expected_revision": 2, "content_capture": "off"})),
        )
        .await;
        assert_eq!(
            source
                .workspace_narrowing("ws-a")
                .await
                .unwrap()
                .content_capture,
            Some(ContentCapture::Off)
        );
        assert_eq!(
            source.workspace_narrowing("ws-b").await.unwrap(),
            TelemetryNarrowing::default()
        );
    }

    #[tokio::test]
    async fn unavailable_storage_is_a_service_error_not_an_open_ceiling() {
        let service = TelemetryPolicyService::new(
            Arc::new(MapRepository {
                unavailable: true,
                ..MapRepository::default()
            }),
            ContentCapture::Full,
        );
        let router = router(service.clone());
        let (status, body) = call(&router, "ws", "GET", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(
            service.narrowing_source().workspace_narrowing("ws").await,
            Err(TelemetryPolicyError::StoreUnavailable)
        );
    }
}
