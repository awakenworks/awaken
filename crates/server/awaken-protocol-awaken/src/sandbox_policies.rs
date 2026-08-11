//! Awaken sandbox execution-policy authoring protocol.

use std::sync::Arc;

use awaken_environment_application::{EnvironmentApplication, EnvironmentApplicationError};
use awaken_provisioning_contract::{
    SandboxExecutionPolicy, SandboxExecutionPolicyError, SandboxExecutionPolicyId,
    SandboxExecutionPolicyRef, SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

struct EnvironmentExtensionsState {
    application: Arc<EnvironmentApplication>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
}

pub fn environment_extensions_router(
    application: Arc<EnvironmentApplication>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
) -> Router {
    Router::new()
        .route(
            "/v1/awaken/sandbox-execution-policies",
            post(create_sandbox_policy),
        )
        .route(
            "/v1/awaken/sandbox-execution-policies/{id}/versions",
            post(publish_sandbox_policy),
        )
        .route(
            "/v1/awaken/sandbox-execution-policies/{id}/versions/{version}",
            get(get_exact_sandbox_policy),
        )
        .route(
            "/v1/awaken/environments/{id}/sandbox-execution-policy",
            get(get_environment_sandbox_policy).post(bind_environment_sandbox_policy),
        )
        .with_state(Arc::new(EnvironmentExtensionsState {
            application,
            sandbox_policies,
        }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyCreate {
    id: String,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_session_contract::SandboxProvisioning,
    #[serde(default)]
    idle_retention: awaken_session_contract::EnvironmentIdleRetentionPolicy,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyPublish {
    expected_current: u64,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_session_contract::SandboxProvisioning,
    #[serde(default)]
    idle_retention: awaken_session_contract::EnvironmentIdleRetentionPolicy,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyBindingInput {
    policy_id: String,
    version: u64,
}

#[derive(Serialize)]
struct SandboxPolicyBindingOutput {
    environment_id: String,
    policy_id: String,
    version: u64,
    provisioning: awaken_session_contract::SandboxProvisioning,
}

fn policy_store(
    state: &EnvironmentExtensionsState,
) -> Result<Arc<dyn SandboxExecutionPolicyStore>, StatusCode> {
    state
        .sandbox_policies
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

async fn project_policy_binding(
    state: &EnvironmentExtensionsState,
    environment_id: String,
    reference: SandboxExecutionPolicyRef,
) -> Result<SandboxPolicyBindingOutput, StatusCode> {
    let policy = policy_store(state)?
        .get_exact(&reference)
        .await
        .map_err(map_policy_error)?;
    Ok(SandboxPolicyBindingOutput {
        environment_id,
        policy_id: reference.id.0,
        version: reference.version.0,
        provisioning: policy.provisioning,
    })
}

async fn create_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Json(input): Json<SandboxPolicyCreate>,
) -> Result<(StatusCode, Json<SandboxExecutionPolicy>), StatusCode> {
    let policy = SandboxExecutionPolicy {
        id: SandboxExecutionPolicyId(input.id),
        version: SandboxExecutionPolicyVersion::INITIAL,
        config: input.config,
        provisioning: input.provisioning,
        idle_retention: input.idle_retention,
        disabled: input.disabled,
    };
    policy_store(&state)?
        .create(policy.clone())
        .await
        .map_err(map_policy_error)?;
    Ok((StatusCode::CREATED, Json(policy)))
}

async fn publish_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path(id): Path<String>,
    Json(input): Json<SandboxPolicyPublish>,
) -> Result<Json<SandboxExecutionPolicy>, StatusCode> {
    let next = input
        .expected_current
        .checked_add(1)
        .ok_or(StatusCode::CONFLICT)?;
    let policy = SandboxExecutionPolicy {
        id: SandboxExecutionPolicyId(id),
        version: SandboxExecutionPolicyVersion(next),
        config: input.config,
        provisioning: input.provisioning,
        idle_retention: input.idle_retention,
        disabled: input.disabled,
    };
    policy_store(&state)?
        .publish(
            SandboxExecutionPolicyVersion(input.expected_current),
            policy.clone(),
        )
        .await
        .map_err(map_policy_error)?;
    Ok(Json(policy))
}

async fn get_exact_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path((id, version)): Path<(String, u64)>,
) -> Result<Json<SandboxExecutionPolicy>, StatusCode> {
    let policy = policy_store(&state)?
        .get_exact(&SandboxExecutionPolicyRef {
            id: SandboxExecutionPolicyId(id),
            version: SandboxExecutionPolicyVersion(version),
        })
        .await
        .map_err(map_policy_error)?;
    Ok(Json(policy))
}

async fn bind_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path(environment_id): Path<String>,
    Json(input): Json<SandboxPolicyBindingInput>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let reference = SandboxExecutionPolicyRef {
        id: SandboxExecutionPolicyId(input.policy_id),
        version: SandboxExecutionPolicyVersion(input.version),
    };
    state
        .application
        .bind_sandbox_policy(&environment_id, reference.clone())
        .await
        .map_err(map_application_error)?;
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

async fn get_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path(environment_id): Path<String>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let reference = state
        .application
        .get(&environment_id)
        .await
        .and_then(|item| item.sandbox_policy)
        .map(|reference| SandboxExecutionPolicyRef {
            id: SandboxExecutionPolicyId(reference.policy_id),
            version: SandboxExecutionPolicyVersion(reference.version),
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

fn map_application_error(error: EnvironmentApplicationError) -> StatusCode {
    match error {
        EnvironmentApplicationError::NotFound => StatusCode::NOT_FOUND,
        EnvironmentApplicationError::BuiltinImmutable | EnvironmentApplicationError::Archived => {
            StatusCode::CONFLICT
        }
        EnvironmentApplicationError::Policy(error) => map_policy_error(error),
        EnvironmentApplicationError::PolicyStoreUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        EnvironmentApplicationError::Create(_) | EnvironmentApplicationError::Registration(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn map_policy_error(error: SandboxExecutionPolicyError) -> StatusCode {
    match error {
        SandboxExecutionPolicyError::NotFound => StatusCode::NOT_FOUND,
        SandboxExecutionPolicyError::VersionConflict => StatusCode::CONFLICT,
        SandboxExecutionPolicyError::Disabled | SandboxExecutionPolicyError::Invalid(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        SandboxExecutionPolicyError::StoreFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn sandbox_policy_error_decision_table_is_explicit() {
        // Cause/effect rules: C1 absent policy -> E1 404; C2 stale publish ->
        // E2 409; C3 invalid/disabled policy -> E3 422; C4 store failure -> E4
        // 500. This is Awaken-only wire behavior, never an Anthropic endpoint.
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::NotFound),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::VersionConflict),
            StatusCode::CONFLICT
        );
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::Disabled),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::StoreFailed("x".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // Application composition must preserve the same domain cause instead
        // of string-erasing NotFound into the generic 422 arm. C5 an absent
        // application store is distinct infrastructure unavailability -> E5 503.
        assert_eq!(
            map_application_error(EnvironmentApplicationError::Policy(
                SandboxExecutionPolicyError::NotFound
            )),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_application_error(EnvironmentApplicationError::PolicyStoreUnavailable),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn exact_policy_http_read_is_versioned_and_fail_closed() {
        // Cause/effect graph: immutable create/publish commands -> exact store
        // versions -> GET(id,version), never the mutable current pointer.
        // Decision table: R0 create/publish HTTP commands carry resource
        // requests into immutable versions; R1 v1 exists -> return the exact v1 config; R2 v2
        // exists with different config -> each URL preserves its own version;
        // R3 absent id/version -> 404; R4 unavailable policy store -> 503. The
        // test drives the public Awaken-only router so path extraction, status
        // mapping, and response serialization are covered together.
        let store =
            Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
        let first = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("design-runtime".into()),
            version: SandboxExecutionPolicyVersion::INITIAL,
            config: awaken_provisioning_contract::SandboxOverride {
                isolation: Some(awaken_provisioning_contract::IsolationClass::Container),
                requests: awaken_provisioning_contract::ResourceRequests {
                    cpu_millis: Some(500),
                    memory_bytes: Some(1_073_741_824),
                    disk_bytes: Some(2_147_483_648),
                },
                ..Default::default()
            },
            provisioning: Default::default(),
            idle_retention: Default::default(),
            disabled: false,
        };
        let mut second = first.clone();
        second.version = SandboxExecutionPolicyVersion(2);
        second.config.requests.cpu_millis = Some(750);
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let application = Arc::new(EnvironmentApplication::new(
            Arc::new(awaken_env_store::InMemoryEnvRegistry::new()),
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
            Some(store.clone()),
        ));
        let app = environment_extensions_router(application, Some(store));
        let get = |path: &'static str| Request::get(path).body(Body::empty()).expect("GET request");
        let post = |path: &'static str, body: serde_json::Value| {
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("POST request")
        };

        let created = app
            .clone()
            .oneshot(post(
                "/v1/awaken/sandbox-execution-policies",
                serde_json::json!({
                    "id": "design-runtime",
                    "config": {
                        "isolation": "container",
                        "requests": {
                            "cpu_millis": 500,
                            "memory_bytes": 1_073_741_824u64,
                            "disk_bytes": 2_147_483_648u64
                        }
                    }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED, "R0");
        let published = app
            .clone()
            .oneshot(post(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions",
                serde_json::json!({
                    "expected_current": 1,
                    "config": {
                        "isolation": "container",
                        "requests": {
                            "cpu_millis": 750,
                            "memory_bytes": 1_073_741_824u64,
                            "disk_bytes": 2_147_483_648u64
                        }
                    }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::OK, "R0");

        let response = app
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/1",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "R1");
        let exact: SandboxExecutionPolicy =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(exact, first, "R1");

        let response = app
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/2",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "R2");
        let exact: SandboxExecutionPolicy =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(exact, second, "R2");

        let missing = app
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/3",
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND, "R3");

        let unavailable = environment_extensions_router(
            Arc::new(EnvironmentApplication::new(
                Arc::new(awaken_env_store::InMemoryEnvRegistry::new()),
                Arc::new(LocalExecutableEnvironmentRegistrar::new(Arc::new(
                    ExecutableEnvironmentCatalog::new(),
                ))),
                None,
            )),
            None,
        )
        .oneshot(get(
            "/v1/awaken/sandbox-execution-policies/design-runtime/versions/1",
        ))
        .await
        .unwrap();
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE, "R4");
    }
}
