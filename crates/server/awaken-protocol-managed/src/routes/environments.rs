// @formal surface obligation=environment.registration_intent_is_atomic_recoverable_and_idempotent
// @formal surface obligation=work.single_active_claim
// @formal reason: Environment authoring and execution projection route.
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

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::routes::{ManagedJson, ManagedQuery};
use crate::types::environment::{
    CloudNetworkingParams, CloudNetworkingUpdateParams, DeletedEnvironment, Environment,
    EnvironmentConfigParams, EnvironmentConfigUpdateParams, EnvironmentCreateParams,
    EnvironmentUpdateParams, PackagesUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};
use awaken_environment_application::{EnvironmentApplication, EnvironmentApplicationError};
use awaken_environment_contract::{
    EnvUpdate, EnvironmentConfigMutation, EnvironmentFieldUpdate, EnvironmentNetworkingMutation,
    EnvironmentPackagesMutation,
};
pub(crate) mod work_routes;

/// Mount definition authoring over the canonical Control application.
pub fn environment_authoring_router(state: Arc<EnvironmentApplication>) -> Router {
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
/// `clock` is the Coordinator's wall clock.
pub fn environment_work_router(
    environments: Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Router {
    let state = Arc::new(work_routes::WorkRoutes {
        queries: environments.work_queries(),
        consumer: environments.work_consumer(),
        metadata: environments.work_metadata(),
        session_leases: environments.session_work_leases(),
        clock,
    });
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
    use awaken_environment_contract::{CreateEnvironmentError, EnvironmentStoreError};
    use awaken_provisioning_contract::SandboxExecutionPolicyError;
    let message = error.to_string();
    match error {
        EnvironmentApplicationError::NotFound => not_found("environment"),
        EnvironmentApplicationError::Policy(SandboxExecutionPolicyError::NotFound) => {
            not_found("sandbox execution policy")
        }
        EnvironmentApplicationError::Create(CreateEnvironmentError::IdempotencyConflict)
        | EnvironmentApplicationError::Create(
            CreateEnvironmentError::OriginalResultUnavailable { .. },
        )
        | EnvironmentApplicationError::BuiltinImmutable
        | EnvironmentApplicationError::Archived
        | EnvironmentApplicationError::Store(
            EnvironmentStoreError::RevisionConflict | EnvironmentStoreError::RevisionExhausted,
        )
        | EnvironmentApplicationError::Policy(SandboxExecutionPolicyError::VersionConflict) => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("conflict_error", message)),
        ),
        EnvironmentApplicationError::Create(CreateEnvironmentError::InvalidConfig(_))
        | EnvironmentApplicationError::Store(EnvironmentStoreError::InvalidConfig(_))
        | EnvironmentApplicationError::Policy(
            SandboxExecutionPolicyError::Disabled | SandboxExecutionPolicyError::Invalid(_),
        ) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse::new("invalid_request_error", message)),
        ),
        EnvironmentApplicationError::Policy(SandboxExecutionPolicyError::StoreFailed(_))
        | EnvironmentApplicationError::PolicyStoreUnavailable
        | EnvironmentApplicationError::Create(CreateEnvironmentError::Store(_))
        | EnvironmentApplicationError::Registration(_)
        | EnvironmentApplicationError::Store(EnvironmentStoreError::Backend(_))
        | EnvironmentApplicationError::RegistrationInvariant(_)
        | EnvironmentApplicationError::CommittedRegistrationPending { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", message)),
        ),
    }
}

type WireError = (StatusCode, Json<ErrorResponse>);

/// Preserve the Managed body/status while preventing SDK automatic retries of
/// an authoring command whose definition transaction already committed.
fn map_environment_command_error(error: EnvironmentApplicationError) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let committed = error.committed_environment_id().map(str::to_owned);
    let revision = error
        .committed_environment()
        .map(|environment| environment.revision.0);
    let mut response = map_environment_application_error(error).into_response();
    if let Some(environment_id) = committed {
        response.headers_mut().insert(
            "x-awaken-command-status",
            axum::http::HeaderValue::from_static("committed"),
        );
        if let Ok(value) = axum::http::HeaderValue::from_str(&environment_id) {
            response
                .headers_mut()
                .insert("x-awaken-environment-id", value);
        }
        if let Some(revision) = revision {
            response.headers_mut().insert(
                "x-awaken-environment-revision",
                axum::http::HeaderValue::from(revision),
            );
        }
        response.headers_mut().insert(
            "x-should-retry",
            axum::http::HeaderValue::from_static("false"),
        );
    }
    response
}

fn map_environment_projection_error(message: &'static str) -> WireError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new("api_error", message)),
    )
}

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
    State(state): State<Arc<EnvironmentApplication>>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<EnvironmentCreateParams>,
) -> Result<Json<Environment>, axum::response::Response> {
    let config = canonical_environment_config(params.config.unwrap_or_default());
    let command_id =
        environment_command_id(&headers).map_err(axum::response::IntoResponse::into_response)?;
    let item = state
        .create(awaken_environment_contract::CreateEnvironmentCommand {
            command_id,
            name: params.name,
            description: params.description,
            metadata: params.metadata,
            scope: params.scope.map(|scope| scope.as_str().to_string()),
            config,
            sandbox_policy: None,
        })
        .await
        .map_err(map_environment_command_error)?;
    Ok(Json(crate::env_registry::project_env(&item).map_err(
        |error| {
            axum::response::IntoResponse::into_response(map_environment_projection_error(error))
        },
    )?))
}

fn environment_command_id(headers: &HeaderMap) -> Result<String, WireError> {
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
        // An unkeyed request is a new command. The identity must therefore be
        // unique across active-active Control replicas and process restarts;
        // process ids and local counters both repeat across those boundaries.
        None => Ok(format!("managed-unkeyed:{}", uuid::Uuid::now_v7().simple())),
    }
}

#[cfg(test)]
mod command_identity_tests {
    use super::*;

    #[test]
    fn environment_command_identity_follows_keyed_and_unkeyed_decision_table() {
        // Cause/effect graph: C1 an explicit valid Idempotency-Key denotes one
        // retryable command; C2 a second request omits the key; C3 another
        // independent request also omits it; C4 the explicit key is invalid.
        // E1 C1 maps deterministically into the Managed namespace; E2 C2/C3
        // mint different globally scoped command identities so active-active
        // replicas cannot collapse distinct creates; E3 C4 rejects before the
        // Environment application or store is called.
        //
        // | Rule | Header | Request relation | Effect |
        // |---|---|---|---|
        // | I1 | valid key | exact retry | stable managed key (E1) |
        // | I2 | absent | two independent creates | distinct UUIDs (E2) |
        // | I3 | empty | invalid command identity | 400 (E3) |
        let mut keyed = HeaderMap::new();
        keyed.insert("idempotency-key", "create-1".parse().unwrap());
        assert_eq!(
            environment_command_id(&keyed).unwrap(),
            "managed:create-1",
            "I1/E1"
        );
        assert_eq!(
            environment_command_id(&keyed).unwrap(),
            "managed:create-1",
            "I1/E1 exact retry"
        );

        let first = environment_command_id(&HeaderMap::new()).unwrap();
        let second = environment_command_id(&HeaderMap::new()).unwrap();
        assert_ne!(first, second, "I2/E2");
        for identity in [first, second] {
            let encoded = identity
                .strip_prefix("managed-unkeyed:")
                .expect("I2/E2 unkeyed namespace");
            let parsed = uuid::Uuid::parse_str(encoded).expect("I2/E2 UUID identity");
            assert_eq!(parsed.get_version_num(), 7, "I2/E2 UUIDv7");
        }

        let mut empty = HeaderMap::new();
        empty.insert("idempotency-key", "".parse().unwrap());
        assert_eq!(
            environment_command_id(&empty).unwrap_err().0,
            StatusCode::BAD_REQUEST,
            "I3/E3"
        );
    }
}

async fn retrieve_env(
    State(state): State<Arc<EnvironmentApplication>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    if id == awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID {
        return Err(not_found("environment"));
    }
    let item = state
        .get(&id)
        .await
        .map_err(map_environment_application_error)?
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(
        crate::env_registry::project_env(&item).map_err(map_environment_projection_error)?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentListParams {
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default, rename = "beta")]
    _beta_selector: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    include_archived: Option<bool>,
}

async fn list_envs(
    State(state): State<Arc<EnvironmentApplication>>,
    ManagedQuery(query): ManagedQuery<EnvironmentListParams>,
) -> Result<Json<PageCursor<Environment>>, WireError> {
    let definitions = if query.include_archived.unwrap_or(false) {
        state.list_all().await
    } else {
        state.list_active().await
    }
    .map_err(map_environment_application_error)?;
    let data: Vec<Environment> = definitions
        .iter()
        .filter(|item| item.id != awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID)
        .map(crate::env_registry::project_env)
        .collect::<Result<_, _>>()
        .map_err(map_environment_projection_error)?;
    Ok(Json(paginate(data, &query.page, |e| e.id.as_str())))
}

async fn update_env(
    State(state): State<Arc<EnvironmentApplication>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<EnvironmentUpdateParams>,
) -> Result<Json<Environment>, axum::response::Response> {
    let config = params.config.map(|config| match config {
        None => EnvironmentConfigMutation::Replace(Default::default()),
        Some(config) => environment_config_mutation(config),
    });
    let patch = EnvUpdate {
        name: params.name,
        description: params.description.map(|description| match description {
            Some(description) => EnvironmentFieldUpdate::Replace(description),
            None => EnvironmentFieldUpdate::Clear,
        }),
        config,
        scope: params
            .scope
            .map(|scope| EnvironmentFieldUpdate::Replace(scope.as_str().to_string())),
        metadata: params.metadata,
        ..Default::default()
    };
    let item = state
        .update(&id, patch)
        .await
        .map_err(map_environment_command_error)?;
    Ok(Json(crate::env_registry::project_env(&item).map_err(
        |error| {
            axum::response::IntoResponse::into_response(map_environment_projection_error(error))
        },
    )?))
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
                Some(CloudNetworkingUpdateParams::Unrestricted {}) => {
                    EnvironmentNetworkingMutation::Unrestricted
                }
                Some(CloudNetworkingUpdateParams::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                }) => EnvironmentNetworkingMutation::Limited {
                    allowed_hosts: allowed_hosts.map(|value| match value {
                        None => EnvironmentFieldUpdate::Clear,
                        Some(hosts) => EnvironmentFieldUpdate::Replace(
                            hosts
                                .into_iter()
                                .map(crate::types::environment::AllowedHost::into_inner)
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .collect(),
                        ),
                    }),
                    allow_mcp_servers: field_update(allow_mcp_servers),
                    allow_package_managers: field_update(allow_package_managers),
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
) -> Option<EnvironmentFieldUpdate<Vec<String>>> {
    value.map(|value| match value {
        Some(value) => EnvironmentFieldUpdate::Replace(package_values(value)),
        None => EnvironmentFieldUpdate::Clear,
    })
}

fn field_update<T>(value: Option<Option<T>>) -> Option<EnvironmentFieldUpdate<T>> {
    value.map(|value| match value {
        Some(value) => EnvironmentFieldUpdate::Replace(value),
        None => EnvironmentFieldUpdate::Clear,
    })
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
            let networking = match networking.unwrap_or(CloudNetworkingParams::Unrestricted {}) {
                CloudNetworkingParams::Unrestricted {} => EnvironmentNetworking::Unrestricted {},
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
    State(state): State<Arc<EnvironmentApplication>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedEnvironment>, axum::response::Response> {
    state
        .delete(&id)
        .await
        .map_err(map_environment_command_error)?;
    Ok(Json(DeletedEnvironment {
        id,
        object_type: "environment_deleted",
    }))
}

async fn archive_env(
    State(state): State<Arc<EnvironmentApplication>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, axum::response::Response> {
    let item = state
        .archive(&id)
        .await
        .map_err(map_environment_command_error)?;
    Ok(Json(crate::env_registry::project_env(&item).map_err(
        |error| {
            axum::response::IntoResponse::into_response(map_environment_projection_error(error))
        },
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn committed_pending_is_wire_compatible_and_prevents_automatic_command_retry() {
        let mut environment = awaken_environment_application::builtin_local_environment();
        environment.id = "env_committed".into();
        environment.revision = awaken_environment_contract::EnvironmentRevision(7);
        let response = map_environment_command_error(EnvironmentApplicationError::CommittedRegistrationPending {
            environment: Box::new(environment),
            cause: Box::new(EnvironmentApplicationError::Registration(
                awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError::Unavailable("private-upstream-diagnostic".into()),
            )),
        });
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(response.headers()["x-awaken-command-status"], "committed");
        assert_eq!(
            response.headers()["x-awaken-environment-id"],
            "env_committed"
        );
        assert_eq!(response.headers()["x-awaken-environment-revision"], "7");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["type"], "api_error");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("env_committed") && message.contains("revision 7"));
        assert!(!message.contains("private-upstream-diagnostic"));
        let rejected = map_environment_command_error(EnvironmentApplicationError::Store(
            awaken_environment_contract::EnvironmentStoreError::Backend("before commit".into()),
        ));
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(rejected.headers().get("x-should-retry").is_none());
        assert!(rejected.headers().get("x-awaken-command-status").is_none());
        let legacy = map_environment_command_error(EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::OriginalResultUnavailable {
                environment_id: "env_legacy".into(),
            },
        ));
        assert_eq!(legacy.status(), StatusCode::CONFLICT);
        assert_eq!(legacy.headers()["x-should-retry"], "false");
        assert_eq!(legacy.headers()["x-awaken-command-status"], "committed");
        assert_eq!(legacy.headers()["x-awaken-environment-id"], "env_legacy");
        assert!(
            legacy
                .headers()
                .get("x-awaken-environment-revision")
                .is_none(),
            "no fabricated original revision"
        );
        let bytes = axum::body::to_bytes(legacy.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["type"], "conflict_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("retrieve the current definition")
        );
    }

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
                EnvironmentApplicationError::Store(
                    awaken_environment_contract::EnvironmentStoreError::Backend("x".into()),
                ),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "P5b",
                EnvironmentApplicationError::RegistrationInvariant("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "P5c",
                EnvironmentApplicationError::Store(
                    awaken_environment_contract::EnvironmentStoreError::RevisionConflict,
                ),
                StatusCode::CONFLICT,
            ),
            (
                "P6a",
                EnvironmentApplicationError::Create(
                    awaken_environment_contract::CreateEnvironmentError::InvalidConfig(
                        awaken_environment_contract::InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" },
                    ),
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                "P5d",
                EnvironmentApplicationError::Store(
                    awaken_environment_contract::EnvironmentStoreError::RevisionExhausted,
                ),
                StatusCode::CONFLICT,
            ),
            (
                "P6b",
                EnvironmentApplicationError::Store(
                    awaken_environment_contract::EnvironmentStoreError::InvalidConfig(
                        awaken_environment_contract::InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" },
                    ),
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
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
