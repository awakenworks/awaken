//! Exact, Workspace-scoped immutable publication projection.

use awaken_config_store::{ConfigRegistry, StoredPublication};
use awaken_tenancy::ScopeId;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde_json::{Value, json};

use super::{ConfigPlane, request_scope};

impl ConfigPlane {
    /// Read one exact immutable publication owned by `scope`.
    pub async fn publication(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, String> {
        self.registry_for(scope)
            .get_publication(fingerprint)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Absence is scoped, so another Workspace's fingerprint is never disclosed.
pub(super) async fn get(
    State(plane): State<ConfigPlane>,
    Path(fingerprint): Path<String>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
) -> (StatusCode, Json<Value>) {
    if fingerprint.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "publication fingerprint is required" })),
        );
    }
    match plane.publication(&request_scope(scope), &fingerprint).await {
        Ok(Some(publication)) => (
            StatusCode::OK,
            Json(serde_json::to_value(publication).expect("StoredPublication serializes")),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "publication not found" })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_config_store::SqliteConfigStore;

    use super::*;
    use crate::config_plane::resource_prompt_tests::{
        agent_config, failing_scoped_plane, test_service,
    };

    // Causal decision table: present in trusted scope => exact value;
    // absent/cross-scope => 404; repository failure => 500.
    #[tokio::test]
    async fn exact_scoped_and_fail_closed() {
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let owner = ScopeId::from("workspace-owner");
        plane.put(&owner, &agent_config("agent-a")).await.unwrap();
        let published = plane.publish(&owner, "agent-a").await.unwrap();

        let (status, body) = get(
            State(plane.clone()),
            Path(published.fingerprint.clone()),
            Some(Extension(awaken_tenancy::WorkspaceScope(
                owner.as_str().to_owned(),
            ))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["fingerprint"], published.fingerprint);

        let (status, _) = get(
            State(plane),
            Path(published.fingerprint),
            Some(Extension(awaken_tenancy::WorkspaceScope("intruder".into()))),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            get(State(failing_scoped_plane()), Path("fp".into()), None)
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
