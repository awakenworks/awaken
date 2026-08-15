//! The Managed **environments** family (`/v1/environments`) + the **work queue**
//! (`/v1/environments/:id/work…`), the official `@anthropic-ai/sdk`
//! `beta.environments.*` and `beta.environments.work.*` surfaces. An environment
//! is where a self-hosted worker runs sessions; the work queue is how the platform
//! hands work to that worker (poll → ack → heartbeat → stop).
//!
//! Open-tier semantics: the API shape is complete and usable, but a single-machine
//! build leases work to **one** worker at a time — `poll` hands out a queued item
//! only when no item in the environment is already `active`. Multi-worker
//! fan-out (many concurrent leases) is the managed scaling boundary. Product
//! startup injects a durable SQLite/PostgreSQL queue; only explicit test-support
//! startup uses the reference in-memory queue. Every new environment is seeded
//! with one `healthcheck` work item so the queue is exercisable end to end.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::env_registry::{
    EnvUpdate, EnvironmentConfigMutation, EnvironmentNetworkingMutation,
    EnvironmentPackagesMutation,
};
use crate::routes::ManagedJson;
use crate::types::environment::{
    CloudNetworkingParams, CloudNetworkingUpdateParams, DeletedEnvironment, Environment,
    EnvironmentConfigParams, EnvironmentConfigUpdateParams, EnvironmentCreateParams,
    EnvironmentUpdateParams, PackagesUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};
use awaken_environment_application::{EnvironmentApplication, EnvironmentApplicationError};
mod work_routes;

/// Control-owned Environment definitions, immutable revision history, policy
/// versions, and publication application.
pub struct EnvironmentAuthoringState {
    application: Arc<EnvironmentApplication>,
    sandbox_policies: Option<Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>>,
}

impl EnvironmentAuthoringState {
    #[must_use]
    pub fn new(
        application: Arc<EnvironmentApplication>,
        sandbox_policies: Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
    ) -> Self {
        Self {
            application,
            sandbox_policies: Some(sandbox_policies),
        }
    }

    #[must_use]
    pub fn application(&self) -> Arc<EnvironmentApplication> {
        self.application.clone()
    }

    #[must_use]
    pub fn sandbox_policy_store(
        &self,
    ) -> Option<Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>> {
        self.sandbox_policies.clone()
    }
}

/// Mount Control-owned definition and sandbox-policy authoring routes.
pub fn environment_authoring_router(state: Arc<EnvironmentAuthoringState>) -> Router {
    Router::new()
        .route("/v1/environments", post(create_env).get(list_envs))
        .route(
            "/v1/environments/{id}",
            get(retrieve_env).post(update_env).delete(delete_env),
        )
        .route("/v1/environments/{id}/archive", post(archive_env))
        .with_state(state)
}

/// Mount Coordinator-owned work coordination routes.
pub fn environment_work_router(
    state: Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
) -> Router {
    Router::new()
        .route("/v1/environments/{id}/work", get(work_routes::list_work))
        .route(
            "/v1/environments/{id}/work/poll",
            get(work_routes::poll_work),
        )
        .route(
            "/v1/environments/{id}/work/stats",
            get(work_routes::work_stats),
        )
        .route(
            "/v1/environments/{id}/work/{wid}",
            get(work_routes::retrieve_work).post(work_routes::update_work),
        )
        .route(
            "/v1/environments/{id}/work/{wid}/ack",
            post(work_routes::ack_work),
        )
        .route(
            "/v1/environments/{id}/work/{wid}/heartbeat",
            post(work_routes::heartbeat_work),
        )
        .route(
            "/v1/environments/{id}/work/{wid}/stop",
            post(work_routes::stop_work),
        )
        .with_state(state)
}

fn map_environment_application_error(error: EnvironmentApplicationError) -> WireError {
    let message = error.to_string();
    match error {
        EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::IdempotencyConflict,
        ) => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("conflict_error", message)),
        ),
        EnvironmentApplicationError::NotFound => not_found("environment"),
        EnvironmentApplicationError::BuiltinImmutable | EnvironmentApplicationError::Archived => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("conflict_error", message)),
        ),
        EnvironmentApplicationError::Policy(
            awaken_provisioning_contract::SandboxExecutionPolicyError::NotFound,
        ) => not_found("sandbox execution policy"),
        EnvironmentApplicationError::Policy(
            awaken_provisioning_contract::SandboxExecutionPolicyError::VersionConflict,
        ) => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("conflict_error", message)),
        ),
        EnvironmentApplicationError::Policy(
            awaken_provisioning_contract::SandboxExecutionPolicyError::Disabled
            | awaken_provisioning_contract::SandboxExecutionPolicyError::Invalid(_),
        ) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse::new("invalid_request_error", message)),
        ),
        EnvironmentApplicationError::Policy(
            awaken_provisioning_contract::SandboxExecutionPolicyError::StoreFailed(_),
        )
        | EnvironmentApplicationError::PolicyStoreUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", message)),
        ),
        EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::Store(_),
        )
        | EnvironmentApplicationError::Registration(_)
        | EnvironmentApplicationError::RegistrationOutbox(_)
        | EnvironmentApplicationError::RegistrationInvariant(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", message)),
        ),
    }
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn not_found(what: &str) -> WireError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "not_found_error",
            format!("{what} not found"),
        )),
    )
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

// ---- Environment routes ----------------------------------------------------

async fn create_env(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<EnvironmentCreateParams>,
) -> Result<Json<Environment>, WireError> {
    let config = canonical_environment_config(params.config.unwrap_or_default());
    let command_id = environment_command_id(&headers)?;
    let item = state
        .application
        .create(awaken_environment_contract::CreateEnvironmentCommand {
            command_id,
            name: params.name,
            description: params.description.unwrap_or_default(),
            metadata: params.metadata,
            scope: params.scope.map(|scope| scope.as_str().to_string()),
            config,
        })
        .await
        .map_err(map_environment_application_error)?;
    Ok(Json(crate::env_registry::project_env(&item)))
}

fn environment_command_id(headers: &HeaderMap) -> Result<String, WireError> {
    static NEXT_UNKEYED: AtomicU64 = AtomicU64::new(0);
    match headers.get("idempotency-key") {
        Some(value) => {
            let value = value
                .to_str()
                .map_err(|_| bad_request("Idempotency-Key must be visible ASCII"))?;
            if value.is_empty() || value.len() > 255 {
                return Err(bad_request(
                    "Idempotency-Key must contain 1 to 255 characters",
                ));
            }
            Ok(format!("managed:{value}"))
        }
        None => Ok(format!(
            "managed-unkeyed:{}:{}",
            std::process::id(),
            NEXT_UNKEYED.fetch_add(1, Ordering::Relaxed)
        )),
    }
}

async fn retrieve_env(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    let item = state
        .application
        .get(&id)
        .await
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(crate::env_registry::project_env(&item)))
}

async fn list_envs(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Query(page): Query<PageQuery>,
) -> Json<PageCursor<Environment>> {
    let data: Vec<Environment> = state
        .application
        .list_active()
        .await
        .iter()
        .map(crate::env_registry::project_env)
        .collect();
    Json(paginate(data, &page, |e| e.id.as_str()))
}

async fn update_env(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<EnvironmentUpdateParams>,
) -> Result<Json<Environment>, WireError> {
    let config = params.config.map(|config| match config {
        None => EnvironmentConfigMutation::Replace(Default::default()),
        Some(config) => environment_config_mutation(config),
    });
    let patch = EnvUpdate {
        name: params.name,
        description: params.description,
        config,
        scope: params.scope.map(|scope| Some(scope.as_str().to_string())),
        metadata: params.metadata,
        ..Default::default()
    };
    let item = state
        .application
        .update(&id, patch)
        .await
        .map_err(map_environment_application_error)?;
    Ok(Json(crate::env_registry::project_env(&item)))
}

fn environment_config_mutation(config: EnvironmentConfigUpdateParams) -> EnvironmentConfigMutation {
    match config {
        EnvironmentConfigUpdateParams::SelfHosted {} => {
            EnvironmentConfigMutation::Replace(Default::default())
        }
        EnvironmentConfigUpdateParams::Cloud {
            networking,
            packages,
        } => EnvironmentConfigMutation::PatchCloud {
            networking: networking.map(|value| match value {
                None => EnvironmentNetworkingMutation::Reset,
                Some(CloudNetworkingUpdateParams::Unrestricted) => {
                    EnvironmentNetworkingMutation::Unrestricted
                }
                Some(CloudNetworkingUpdateParams::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                }) => EnvironmentNetworkingMutation::Limited {
                    allowed_hosts: allowed_hosts.map(|value| {
                        value.map(|hosts| {
                            hosts
                                .into_iter()
                                .map(crate::types::environment::AllowedHost::into_inner)
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .collect()
                        })
                    }),
                    allow_mcp_servers,
                    allow_package_managers,
                },
            }),
            packages: packages.map(|value| match value {
                None => EnvironmentPackagesMutation {
                    reset: true,
                    ..Default::default()
                },
                Some(PackagesUpdateParams {
                    apt,
                    cargo,
                    gem,
                    go,
                    npm,
                    pip,
                    kind: _,
                }) => EnvironmentPackagesMutation {
                    reset: false,
                    apt: package_patch(apt),
                    cargo: package_patch(cargo),
                    gem: package_patch(gem),
                    go: package_patch(go),
                    npm: package_patch(npm),
                    pip: package_patch(pip),
                },
            }),
        },
    }
}

fn package_values(values: Vec<crate::types::environment::PackageSpec>) -> Vec<String> {
    values
        .into_iter()
        .map(crate::types::environment::PackageSpec::into_inner)
        .collect()
}

fn package_patch(
    value: Option<Option<Vec<crate::types::environment::PackageSpec>>>,
) -> Option<Option<Vec<String>>> {
    value.map(|value| value.map(package_values))
}

fn canonical_environment_config(
    config: EnvironmentConfigParams,
) -> awaken_environment_contract::EnvironmentConfig {
    use awaken_environment_contract::{
        EnvironmentConfig, EnvironmentNetworking, EnvironmentPackages, EnvironmentPackagesKind,
    };
    match config {
        EnvironmentConfigParams::SelfHosted {} => EnvironmentConfig::SelfHosted,
        EnvironmentConfigParams::Cloud {
            networking,
            packages,
        } => {
            let networking = match networking.unwrap_or(CloudNetworkingParams::Unrestricted) {
                CloudNetworkingParams::Unrestricted => EnvironmentNetworking::Unrestricted,
                CloudNetworkingParams::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                } => EnvironmentNetworking::Limited {
                    allowed_hosts: allowed_hosts
                        .unwrap_or_default()
                        .into_iter()
                        .map(crate::types::environment::AllowedHost::into_inner)
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    allow_mcp_servers: allow_mcp_servers.unwrap_or(false),
                    allow_package_managers: allow_package_managers.unwrap_or(false),
                },
            };
            let packages = packages.unwrap_or_default();
            EnvironmentConfig::Cloud {
                networking,
                packages: EnvironmentPackages {
                    kind: EnvironmentPackagesKind::Packages,
                    apt: package_values(packages.apt.unwrap_or_default()),
                    cargo: package_values(packages.cargo.unwrap_or_default()),
                    gem: package_values(packages.gem.unwrap_or_default()),
                    go: package_values(packages.go.unwrap_or_default()),
                    npm: package_values(packages.npm.unwrap_or_default()),
                    pip: package_values(packages.pip.unwrap_or_default()),
                },
            }
        }
    }
}

async fn delete_env(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedEnvironment>, WireError> {
    state
        .application
        .delete(&id)
        .await
        .map_err(map_environment_application_error)?;
    Ok(Json(DeletedEnvironment {
        id,
        object_type: "environment_deleted",
    }))
}

async fn archive_env(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    let item = state
        .application
        .archive(&id)
        .await
        .map_err(map_environment_application_error)?;
    Ok(Json(crate::env_registry::project_env(&item)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_policy_application_errors_keep_their_wire_taxonomy() {
        // Cause/effect graph: C1 missing exact policy; C2 stale version; C3
        // disabled/invalid; C4 store failure or absent store; C5 outbox read or
        // invariant failure. Effects are the
        // stable Managed statuses 404/409/422/503. Each decision-table row is
        // asserted so typed application failures cannot collapse into one 422.
        use awaken_provisioning_contract::SandboxExecutionPolicyError as PolicyError;

        for (rule, error, expected) in [
            (
                "P1",
                EnvironmentApplicationError::Policy(PolicyError::NotFound),
                StatusCode::NOT_FOUND,
            ),
            (
                "P2",
                EnvironmentApplicationError::Policy(PolicyError::VersionConflict),
                StatusCode::CONFLICT,
            ),
            (
                "P3",
                EnvironmentApplicationError::Policy(PolicyError::Disabled),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "P4a",
                EnvironmentApplicationError::Policy(PolicyError::StoreFailed("x".into())),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "P4b",
                EnvironmentApplicationError::PolicyStoreUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "P5a",
                EnvironmentApplicationError::RegistrationOutbox("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "P5b",
                EnvironmentApplicationError::RegistrationInvariant("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            assert_eq!(
                map_environment_application_error(error).0,
                expected,
                "{rule}"
            );
        }
    }

    #[test]
    fn official_environment_union_rejects_parallel_runtime_and_sandbox_fields() {
        // Cause/effect graph: C1 official SelfHosted union; C2 wire-only runtime
        // override; C3 wire-only sandbox override. E1 C1 parses; E2 C2/C3 fail
        // before authoring/registration. Rules U1=T/F/F->accept,
        // U2=T/T/F->reject, U3=T/F/T->reject.
        assert!(
            serde_json::from_value::<EnvironmentConfigParams>(
                serde_json::json!({"type": "self_hosted"})
            )
            .is_ok(),
            "U1"
        );
        for (rule, value) in [
            (
                "U2",
                serde_json::json!({"type": "self_hosted", "runtime": "acp:codex"}),
            ),
            (
                "U3",
                serde_json::json!({"type": "self_hosted", "sandbox": {"isolation": "namespace"}}),
            ),
        ] {
            let error = serde_json::from_value::<EnvironmentConfigParams>(value)
                .expect_err(rule)
                .to_string();
            assert!(error.contains("unknown field"), "{rule}: {error}");
        }
    }
}
