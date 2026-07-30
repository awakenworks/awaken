//! The Managed **environments** family (`/v1/environments`) + the **work queue**
//! (`/v1/environments/:id/work…`), the official `@anthropic-ai/sdk`
//! `beta.environments.*` and `beta.environments.work.*` surfaces. An environment
//! is where a self-hosted worker runs sessions; the work queue is how the platform
//! hands work to that worker (poll → ack → heartbeat → stop).
//!
//! Open-tier semantics: the API shape is complete and usable, but a single-machine
//! build leases work to **one** worker at a time — `poll` hands out a queued item
//! only when no item in the environment is already `active`. Multi-worker
//! fan-out (many concurrent leases) is the managed scaling boundary; here the
//! queue is one in-process store. Every new environment is seeded with one
//! `healthcheck` work item so the queue is exercisable end to end.

use std::sync::Arc;

use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;

use crate::env_registry::{
    EnvItem, EnvRegistry, EnvUpdate, EnvironmentConfigMutation, EnvironmentNetworkingMutation,
    EnvironmentPackagesMutation, InMemoryEnvRegistry,
};
use crate::routes::ManagedJson;
use crate::types::environment::{
    CloudNetworkingParams, CloudNetworkingUpdateParams, DeletedEnvironment, Environment,
    EnvironmentConfigParams, EnvironmentConfigUpdateParams, EnvironmentCreateParams,
    EnvironmentUpdateParams, PackagesUpdateParams, Work, WorkHeartbeat, WorkQueueStats,
    WorkUpdateParams,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};
use awaken_work_store::InMemoryWorkQueue;

use crate::work_queue::{HeartbeatResult, LeaseHeartbeat, WorkQueue};

/// Canonical unrestricted snapshot for a composition without an Environment
/// registry. An installed registry reuses it only for its implicit `env_local`.
pub(crate) fn default_environment_snapshot(
    environment_id: String,
    runtime: Option<&str>,
) -> awaken_session_contract::EnvironmentSnapshot {
    let acp = runtime.is_some_and(|runtime| runtime.starts_with("acp:"));
    let inference_holder = if acp {
        awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Workload,
            "awaken.workload.acp",
        )
    } else {
        awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            "awaken.worker",
        )
    };
    let sandbox = serde_json::json!({});
    let sandbox_provisioning = awaken_provisioning_contract::SandboxProvisioning::Eager;
    let packages = awaken_session_contract::env_registry::EnvironmentPackages::default();
    let network = awaken_session_contract::SessionNetworkPolicy::Unrestricted;
    let credential_realization = awaken_credential_contract::CredentialRealizationProfile {
        inference_holder,
        mcp_holder: awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),
        resource_holder: awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),
    };
    awaken_session_contract::EnvironmentSnapshot {
        environment_id,
        revision: awaken_session_contract::env_registry::EnvironmentRevision(0),
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
            awaken_session_contract::stable_fingerprint(&(
                &sandbox,
                &sandbox_provisioning,
                &packages,
                &network,
                &credential_realization,
            )),
        ),
        sandbox,
        sandbox_provisioning,
        packages,
        network,
        credential_realization,
    }
}

/// The self-hosted environment registry + work queue, both behind ports so a
/// durable backend (sqlite/postgres) serves standalone and distributed deployments
/// unchanged; the default is in-memory.
pub struct EnvironmentState {
    envs: Arc<dyn EnvRegistry>,
    work: Arc<dyn WorkQueue>,
    sandbox_policies: Option<Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>>,
}

impl Default for EnvironmentState {
    fn default() -> Self {
        Self {
            envs: Arc::new(InMemoryEnvRegistry::new()),
            work: Arc::new(InMemoryWorkQueue::new()),
            sandbox_policies: None,
        }
    }
}

impl EnvironmentState {
    /// Read the authoritative Environment row, including archived rows. Session
    /// snapshot compilation intentionally hides archived rows; deployment launch
    /// classification needs to distinguish archived from never-created without a
    /// second Environment registry.
    pub async fn get(&self, environment_id: &str) -> Option<EnvItem> {
        self.envs.get(environment_id).await
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install durable registry + work-queue backends (sqlite/postgres) in place of
    /// the in-memory defaults; the same routes then serve any deployment mode.
    #[must_use]
    pub fn with_stores(envs: Arc<dyn EnvRegistry>, work: Arc<dyn WorkQueue>) -> Self {
        Self {
            envs,
            work,
            sandbox_policies: None,
        }
    }

    #[must_use]
    pub fn with_sandbox_policies(
        mut self,
        store: Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
    ) -> Self {
        self.sandbox_policies = Some(store);
        self
    }

    /// Compile the one immutable, normalized Environment snapshot consumed by a
    /// Session. The Host never re-reads the mutable registry after this boundary.
    pub async fn snapshot(
        &self,
        env_id: &str,
        runtime: Option<&str>,
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        self.snapshot_for_session(env_id, runtime, &[]).await
    }

    /// Compile a Session-specific snapshot from the exact MCP desired set that
    /// was already normalized by the Managed application boundary.
    pub async fn snapshot_for_session(
        &self,
        env_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        let Some(item) = self.envs.get(env_id).await else {
            return (env_id == "env_local")
                .then(|| default_environment_snapshot(env_id.to_string(), runtime));
        };
        if item.archived_at.is_some() {
            return None;
        }
        let packages = item.config.packages();
        let network = item
            .config
            .network_policy_for_session(mcp_targets)
            .normalized();
        let acp = runtime.is_some_and(|value| value.starts_with("acp:"));
        let holder = if acp {
            awaken_credential_contract::PlaintextHolder::new(
                awaken_credential_contract::PlaintextBoundary::Workload,
                "awaken.workload.acp",
            )
        } else {
            awaken_credential_contract::PlaintextHolder::new(
                awaken_credential_contract::PlaintextBoundary::Worker,
                "awaken.worker",
            )
        };
        let credential_realization = awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: holder,
            mcp_holder: awaken_credential_contract::PlaintextHolder::new(
                awaken_credential_contract::PlaintextBoundary::Worker,
                awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
            resource_holder: awaken_credential_contract::PlaintextHolder::new(
                awaken_credential_contract::PlaintextBoundary::Worker,
                awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
        };
        // Anthropic Environment config owns cloud networking/packages and
        // self-hosted routing only. Awaken sandbox policy has a separate owner.
        let (sandbox, sandbox_provisioning) = match &self.sandbox_policies {
            Some(store) => match store.environment_binding(env_id).await {
                Ok(Some(reference)) => {
                    let policy = store.get_exact(&reference).await.ok()?;
                    if policy.disabled {
                        return None;
                    }
                    (
                        serde_json::to_value(policy.config).ok()?,
                        policy.provisioning,
                    )
                }
                Ok(None) => (
                    serde_json::json!({}),
                    awaken_provisioning_contract::SandboxProvisioning::Eager,
                ),
                Err(_) => return None,
            },
            None => (
                serde_json::json!({}),
                awaken_provisioning_contract::SandboxProvisioning::Eager,
            ),
        };
        let config_fingerprint = awaken_session_contract::EnvironmentFingerprint(
            awaken_session_contract::stable_fingerprint(&(
                &sandbox,
                &sandbox_provisioning,
                &packages,
                &network,
                &credential_realization,
            )),
        );
        Some(awaken_session_contract::EnvironmentSnapshot {
            environment_id: item.id,
            revision: item.revision,
            config_fingerprint,
            sandbox,
            sandbox_provisioning,
            packages,
            network,
            credential_realization,
        })
    }

    /// Resolve an Agent-published exact Environment revision. The registry never
    /// substitutes its current revision when the binding is stale.
    pub async fn snapshot_exact(
        &self,
        env_id: &str,
        revision: u64,
        runtime: Option<&str>,
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        self.snapshot_exact_for_session(env_id, revision, runtime, &[])
            .await
    }

    pub async fn snapshot_exact_for_session(
        &self,
        env_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        let snapshot = self
            .snapshot_for_session(env_id, runtime, mcp_targets)
            .await?;
        (snapshot.revision.0 == revision).then_some(snapshot)
    }

    /// Create an environment named `name` with the official typed config union and
    /// seed its healthcheck work item — the same
    /// effect as `POST /v1/environments`, exposed so an in-process author (the admin
    /// assistant's `admin_draft_environment`) persists through the SAME registry path.
    /// Returns the new environment id.
    pub async fn author(&self, name: &str, config: serde_json::Value) -> Result<String, String> {
        let typed = serde_json::from_value::<EnvironmentConfigParams>(config)
            .map_err(|error| format!("invalid Environment config: {error}"))?;
        Ok(self
            .author_config(name, canonical_environment_config(typed))
            .await)
    }

    /// Persist an already-admitted canonical Environment config through the same
    /// registry/work side effects as the HTTP route.
    pub async fn author_config(
        &self,
        name: &str,
        config: awaken_session_contract::env_registry::EnvironmentConfig,
    ) -> String {
        let item = self
            .envs
            .create_scoped(
                name.to_string(),
                String::new(),
                Default::default(),
                None,
                config,
            )
            .await;
        self.work.enqueue_healthcheck(&item.id).await;
        item.id
    }

    /// Whether `env_id` is a self-hosted environment. Sessions assigned to one are
    /// dispatched through the work queue for an external worker to run.
    pub async fn is_self_hosted(&self, env_id: &str) -> bool {
        self.envs
            .get(env_id)
            .await
            .is_some_and(|rec| rec.is_self_hosted())
    }

    /// Enqueue a `session` work item for `session_id` on `env_id`'s queue — the way
    /// the control plane dispatches a session assigned to a self-hosted environment,
    /// so a worker polling the environment can claim and run it. Returns the work id.
    pub async fn enqueue_session_work(&self, env_id: &str, session_id: &str) -> String {
        self.work.enqueue_session(env_id, session_id).await
    }
}

/// Mount the environments + work routes.
pub fn environments_router(state: Arc<EnvironmentState>) -> Router {
    Router::new()
        .route("/v1/environments", post(create_env).get(list_envs))
        .route(
            "/v1/environments/{id}",
            get(retrieve_env).post(update_env).delete(delete_env),
        )
        .route("/v1/environments/{id}/archive", post(archive_env))
        .route("/v1/environments/{id}/work", get(list_work))
        .route("/v1/environments/{id}/work/poll", get(poll_work))
        .route("/v1/environments/{id}/work/stats", get(work_stats))
        .route(
            "/v1/environments/{id}/work/{wid}",
            get(retrieve_work).post(update_work),
        )
        .route("/v1/environments/{id}/work/{wid}/ack", post(ack_work))
        .route(
            "/v1/environments/{id}/work/{wid}/heartbeat",
            post(heartbeat_work),
        )
        .route("/v1/environments/{id}/work/{wid}/stop", post(stop_work))
        .route(
            "/v1/awaken/sandbox-execution-policies",
            post(create_sandbox_policy),
        )
        .route(
            "/v1/awaken/sandbox-execution-policies/{id}/versions",
            post(publish_sandbox_policy),
        )
        .route(
            "/v1/awaken/environments/{id}/sandbox-execution-policy",
            get(get_environment_sandbox_policy).post(bind_environment_sandbox_policy),
        )
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyCreate {
    id: String,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_provisioning_contract::SandboxProvisioning,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyPublish {
    expected_current: u64,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_provisioning_contract::SandboxProvisioning,
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
    provisioning: awaken_provisioning_contract::SandboxProvisioning,
}

async fn project_policy_binding(
    state: &EnvironmentState,
    environment_id: String,
    reference: awaken_provisioning_contract::SandboxExecutionPolicyRef,
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

fn policy_store(
    state: &EnvironmentState,
) -> Result<&dyn awaken_provisioning_contract::SandboxExecutionPolicyStore, StatusCode> {
    state
        .sandbox_policies
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

async fn create_sandbox_policy(
    State(state): State<Arc<EnvironmentState>>,
    Json(input): Json<SandboxPolicyCreate>,
) -> Result<
    (
        StatusCode,
        Json<awaken_provisioning_contract::SandboxExecutionPolicy>,
    ),
    StatusCode,
> {
    let policy = awaken_provisioning_contract::SandboxExecutionPolicy {
        id: awaken_provisioning_contract::SandboxExecutionPolicyId(input.id),
        version: awaken_provisioning_contract::SandboxExecutionPolicyVersion::INITIAL,
        config: input.config,
        provisioning: input.provisioning,
        disabled: input.disabled,
    };
    policy_store(&state)?
        .create(policy.clone())
        .await
        .map_err(map_policy_error)?;
    Ok((StatusCode::CREATED, Json(policy)))
}

async fn publish_sandbox_policy(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    Json(input): Json<SandboxPolicyPublish>,
) -> Result<Json<awaken_provisioning_contract::SandboxExecutionPolicy>, StatusCode> {
    let next = input
        .expected_current
        .checked_add(1)
        .ok_or(StatusCode::CONFLICT)?;
    let policy = awaken_provisioning_contract::SandboxExecutionPolicy {
        id: awaken_provisioning_contract::SandboxExecutionPolicyId(id),
        version: awaken_provisioning_contract::SandboxExecutionPolicyVersion(next),
        config: input.config,
        provisioning: input.provisioning,
        disabled: input.disabled,
    };
    policy_store(&state)?
        .publish(
            awaken_provisioning_contract::SandboxExecutionPolicyVersion(input.expected_current),
            policy.clone(),
        )
        .await
        .map_err(map_policy_error)?;
    Ok(Json(policy))
}

async fn bind_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentState>>,
    Path(environment_id): Path<String>,
    Json(input): Json<SandboxPolicyBindingInput>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    if !state.envs.exists(&environment_id).await {
        return Err(StatusCode::NOT_FOUND);
    }
    let reference = awaken_provisioning_contract::SandboxExecutionPolicyRef {
        id: awaken_provisioning_contract::SandboxExecutionPolicyId(input.policy_id),
        version: awaken_provisioning_contract::SandboxExecutionPolicyVersion(input.version),
    };
    policy_store(&state)?
        .bind_environment(&environment_id, reference.clone())
        .await
        .map_err(map_policy_error)?;
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

async fn get_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentState>>,
    Path(environment_id): Path<String>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let reference = policy_store(&state)?
        .environment_binding(&environment_id)
        .await
        .map_err(map_policy_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

fn map_policy_error(
    error: awaken_provisioning_contract::SandboxExecutionPolicyError,
) -> StatusCode {
    use awaken_provisioning_contract::SandboxExecutionPolicyError::*;
    match error {
        NotFound | BindingUnavailable => StatusCode::NOT_FOUND,
        VersionConflict => StatusCode::CONFLICT,
        Disabled | Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        StoreFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
    State(state): State<Arc<EnvironmentState>>,
    ManagedJson(params): ManagedJson<EnvironmentCreateParams>,
) -> Result<Json<Environment>, WireError> {
    let config = canonical_environment_config(params.config.unwrap_or_default());
    // No `scope` on the wire: ownership is credential-implicit (authz enforces the
    // workspace) and any awaken tenancy is an ingress concern.
    let item = state
        .envs
        .create_scoped(
            params.name,
            params.description.unwrap_or_default(),
            params.metadata,
            params.scope.map(|scope| scope.as_str().to_string()),
            config,
        )
        .await;
    // Seed one healthcheck work item so the queue is exercisable end to end.
    state.work.enqueue_healthcheck(&item.id).await;
    Ok(Json(crate::env_registry::project_env(&item)))
}

async fn retrieve_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    let item = state
        .envs
        .get(&id)
        .await
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(crate::env_registry::project_env(&item)))
}

async fn list_envs(
    State(state): State<Arc<EnvironmentState>>,
    Query(page): Query<PageQuery>,
) -> Json<Page<Environment>> {
    let data: Vec<Environment> = state
        .envs
        .list_active()
        .await
        .iter()
        .map(crate::env_registry::project_env)
        .collect();
    Json(paginate(data, &page, |e| e.id.as_str()))
}

async fn update_env(
    State(state): State<Arc<EnvironmentState>>,
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
    };
    let item = state
        .envs
        .update(&id, patch)
        .await
        .ok_or_else(|| not_found("environment"))?;
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
) -> awaken_session_contract::env_registry::EnvironmentConfig {
    use awaken_session_contract::env_registry::{
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
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedEnvironment>, WireError> {
    if !state.envs.delete(&id).await {
        return Err(not_found("environment"));
    }
    state.work.remove_env(&id).await;
    Ok(Json(DeletedEnvironment {
        id,
        object_type: "environment_deleted",
    }))
}

async fn archive_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    let item = state
        .envs
        .archive(&id)
        .await
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(crate::env_registry::project_env(&item)))
}

// ---- Work routes -----------------------------------------------------------

async fn require_env(state: &EnvironmentState, id: &str) -> Result<(), WireError> {
    if state.envs.exists(id).await {
        Ok(())
    } else {
        Err(not_found("environment"))
    }
}

/// `GET /v1/environments/:id/work` — the environment's work items.
async fn list_work(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Work>>, WireError> {
    require_env(&state, &id).await?;
    let data: Vec<Work> = state
        .work
        .list(&id)
        .await
        .iter()
        .map(crate::work_queue::project_work)
        .collect();
    Ok(Json(paginate(data, &page, |w| w.id.as_str())))
}

/// `GET /v1/environments/:id/work/poll` — lease the next queued item to the
/// single worker. Open-tier cap: returns `null` when an item is already `active`
/// in this environment (one lease at a time) or the queue is empty.
async fn poll_work(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<Option<Work>>, WireError> {
    require_env(&state, &id).await?;
    let poll = parse_poll_params(raw.as_deref())?;
    // The official SDK sends worker identity in `Anthropic-Worker-ID`, not in
    // the query string. Long polling repeatedly drives the same authoritative
    // atomic claim; it does not introduce a second queue or lease registry.
    let worker_id = worker_id(&headers);
    let started = tokio::time::Instant::now();
    loop {
        let claimed = state
            .work
            .claim_with_reclaim(&id, worker_id, now_ms(), poll.reclaim_older_than_ms)
            .await;
        if let Some(work) = claimed {
            return Ok(Json(Some(crate::work_queue::project_work(&work))));
        }
        let Some(wait) = poll.block_ms else {
            return Ok(Json(None));
        };
        if started.elapsed() >= wait {
            return Ok(Json(None));
        }
        tokio::time::sleep(
            wait.saturating_sub(started.elapsed())
                .min(std::time::Duration::from_millis(20)),
        )
        .await;
    }
}

/// Parsed poll timing. `None` means the caller explicitly sent `block_ms=null`
/// (serialized by the official SDK as an empty query value); omission uses the
/// documented 999 ms default.
struct PollParams {
    block_ms: Option<std::time::Duration>,
    reclaim_older_than_ms: Option<u64>,
}

fn parse_poll_params(raw: Option<&str>) -> Result<PollParams, WireError> {
    let mut block_ms = Some(std::time::Duration::from_millis(999));
    let mut reclaim_older_than_ms = None;
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "block_ms" if value.is_empty() => block_ms = None,
            "block_ms" => {
                let millis = value.parse::<u64>().map_err(|_| {
                    bad_request("block_ms must be null or an integer from 1 through 999")
                })?;
                if !(1..=999).contains(&millis) {
                    return Err(bad_request(
                        "block_ms must be null or an integer from 1 through 999",
                    ));
                }
                block_ms = Some(std::time::Duration::from_millis(millis));
            }
            "reclaim_older_than_ms" if value.is_empty() => reclaim_older_than_ms = None,
            "reclaim_older_than_ms" => {
                reclaim_older_than_ms = Some(value.parse::<u64>().map_err(|_| {
                    bad_request("reclaim_older_than_ms must be a non-negative integer")
                })?);
            }
            _ => {}
        }
    }
    Ok(PollParams {
        block_ms,
        reclaim_older_than_ms,
    })
}

#[derive(serde::Deserialize)]
struct HeartbeatParams {
    desired_ttl_seconds: Option<u64>,
    expected_last_heartbeat: Option<String>,
}

/// `GET /v1/environments/:id/work/stats` — the queue's depth + pending count.
async fn work_stats(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkQueueStats>, WireError> {
    require_env(&state, &id).await?;
    let s = state.work.stats(&id, now_ms()).await;
    Ok(Json(WorkQueueStats {
        object_type: "work_queue_stats",
        depth: s.depth,
        pending: s.pending,
        oldest_queued_at: s.oldest_queued_at,
        workers_polling: s.workers_polling,
    }))
}

async fn retrieve_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .get(&id, &wid)
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

async fn update_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<WorkUpdateParams>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .update_metadata(&id, &wid, params.metadata.unwrap_or_default())
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

/// `POST …/work/:wid/ack` — the worker acknowledges it picked up the item.
async fn ack_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .ack(&id, &wid)
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

/// `POST …/work/:wid/heartbeat` — extend the lease; returns the TTL.
async fn heartbeat_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
    headers: HeaderMap,
    Query(params): Query<HeartbeatParams>,
) -> Result<Json<WorkHeartbeat>, WireError> {
    require_env(&state, &id).await?;
    let command = LeaseHeartbeat {
        condition: crate::work_queue::HeartbeatCondition::from_wire(
            params.expected_last_heartbeat.as_deref(),
        ),
        desired_ttl_seconds: params.desired_ttl_seconds,
    };
    let hb = match state
        .work
        .heartbeat(&id, &wid, worker_id(&headers), now_ms(), command)
        .await
    {
        HeartbeatResult::Accepted(receipt) => receipt,
        HeartbeatResult::PreconditionFailed => {
            return Err((
                StatusCode::PRECONDITION_FAILED,
                Json(ErrorResponse::new(
                    "precondition_error",
                    "expected_last_heartbeat does not match",
                )),
            ));
        }
        HeartbeatResult::NotFound => return Err(not_found("work")),
    };
    Ok(Json(WorkHeartbeat {
        object_type: "work_heartbeat",
        last_heartbeat: hb.last_heartbeat,
        lease_extended: hb.lease_extended,
        state: hb.state,
        ttl_seconds: hb.ttl_seconds,
    }))
}

/// The Managed worker identity carried consistently on poll and worker-owned
/// lease mutations. It is compared atomically with the claim owner by WorkQueue.
fn worker_id(headers: &HeaderMap) -> &str {
    headers
        .get("anthropic-worker-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// Wall-clock now in epoch ms — read only at this HTTP edge and passed into the
/// (clock-free) work queue, so the queue's lease/poll bookkeeping is deterministic
/// under test while production uses real time.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `POST …/work/:wid/stop` — request the worker stop the item.
async fn stop_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .stop(&id, &wid)
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn config(
        value: serde_json::Value,
    ) -> awaken_session_contract::env_registry::EnvironmentConfig {
        serde_json::from_value(value).expect("valid neutral Environment config")
    }

    #[tokio::test]
    async fn with_stores_selects_self_hosted_and_handles_missing() {
        let state = EnvironmentState::with_stores(
            Arc::new(InMemoryEnvRegistry::new()),
            Arc::new(InMemoryWorkQueue::new()),
        );
        let e = state
            .envs
            .create(
                "e".into(),
                String::new(),
                BTreeMap::new(),
                config(json!({ "type": "self_hosted" })),
            )
            .await;
        assert!(state.is_self_hosted(&e.id).await);
        assert!(
            !state.is_self_hosted("missing").await,
            "unknown env is not self-hosted"
        );
    }

    #[tokio::test]
    async fn environment_snapshot_decision_table() {
        // Cause graph: active exact Anthropic record -> normalize networking and choose the
        // inference holder while MCP remains on the one Worker relay boundary,
        // then fingerprint. Missing/archived records fail closed;
        // a later edit increments the registry revision but cannot mutate an old
        // value snapshot.
        //
        // | Rule | Record | Runtime | Later edit | Effect |
        // |------|--------|---------|------------|--------|
        // | S1   | active | native  | F          | Worker inference + MCP |
        // | S2   | active | ACP     | F          | Workload inference + Worker MCP |
        // | S3   | active | native  | T          | new rev/fingerprint; old frozen |
        // | S4   | missing/archived custom | any | - | None |
        // | S5   | implicit env_local | native/ACP | - | canonical local snapshot |
        // | S6   | active empty limited allowlist | any | - | network None |
        // Sandbox policy is not part of the Environment graph.
        let state = EnvironmentState::new();
        let item = state
            .envs
            .create(
                "snapshot".into(),
                String::new(),
                BTreeMap::new(),
                config(json!({
                    "type": "cloud",
                    "networking": {"type": "limited", "allowed_hosts": ["a.test", "shared.test"]}
                })),
            )
            .await;
        let native = state.snapshot(&item.id, None).await.expect("S1");
        assert_eq!(
            native.network,
            awaken_session_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["a.test".into(), "shared.test".into()]
            }
        );
        assert_eq!(
            native.sandbox,
            json!({}),
            "sandbox policy is not an Environment field"
        );
        assert_eq!(
            native.credential_realization.mcp_holder.boundary,
            awaken_credential_contract::PlaintextBoundary::Worker,
            "S1"
        );
        let acp = state
            .snapshot(&item.id, Some("acp:claude"))
            .await
            .expect("S2");
        assert_eq!(
            acp.credential_realization.inference_holder.boundary,
            awaken_credential_contract::PlaintextBoundary::Workload,
            "S2 inference"
        );
        assert_eq!(
            acp.credential_realization.mcp_holder.boundary,
            awaken_credential_contract::PlaintextBoundary::Worker,
            "S2 MCP"
        );
        state
            .envs
            .update(
                &item.id,
                EnvUpdate {
                    name: Some("changed".into()),
                    ..Default::default()
                },
            )
            .await;
        let changed = state.snapshot(&item.id, None).await.expect("S3");
        assert_ne!(changed.revision, native.revision, "S3 revision");
        assert_eq!(
            changed.config_fingerprint, native.config_fingerprint,
            "S3 irrelevant authoring metadata does not alter normalized config"
        );
        assert!(
            state.snapshot("missing", None).await.is_none(),
            "S4 missing"
        );
        state.envs.archive(&item.id).await;
        assert!(
            state.snapshot(&item.id, None).await.is_none(),
            "S4 archived"
        );
        let local = state.snapshot("env_local", None).await.expect("S5");
        assert_eq!(
            local,
            default_environment_snapshot("env_local".into(), None),
            "S5 native"
        );
        let local_acp = state
            .snapshot("env_local", Some("acp:claude"))
            .await
            .expect("S5 ACP");
        assert_eq!(
            local_acp,
            default_environment_snapshot("env_local".into(), Some("acp:claude")),
            "S5 ACP"
        );
        let closed = state
            .envs
            .create(
                "closed".into(),
                String::new(),
                BTreeMap::new(),
                config(json!({"type": "cloud", "networking": {"type": "limited"}})),
            )
            .await;
        assert_eq!(
            state.snapshot(&closed.id, None).await.expect("S6").network,
            awaken_session_contract::SessionNetworkPolicy::None,
            "S6"
        );
    }

    #[tokio::test]
    async fn assistant_authoring_uses_the_same_official_union_guard() {
        let state = EnvironmentState::new();
        let id = state
            .author(
                "cloud",
                json!({
                    "type": "cloud",
                    "networking": {"type": "limited", "allowed_hosts": ["api.example.com"]}
                }),
            )
            .await
            .expect("official cloud config");
        let exact = state.snapshot(&id, None).await.expect("snapshot");
        assert!(
            state
                .snapshot_exact(&id, exact.revision.0, None)
                .await
                .is_some(),
            "exact current revision"
        );
        state
            .envs
            .update(
                &id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            state
                .snapshot_exact(&id, exact.revision.0, None)
                .await
                .is_none(),
            "a stale binding never substitutes current"
        );

        for (case, config) in [
            (
                "runtime",
                json!({"type": "self_hosted", "runtime": "acp:codex"}),
            ),
            (
                "sandbox",
                json!({"type": "self_hosted", "sandbox": {"isolation": "namespace"}}),
            ),
        ] {
            let error = state.author(case, config).await.expect_err(case);
            assert!(
                error.contains("invalid Environment config"),
                "{case}: {error}"
            );
            assert!(error.contains("unknown field"), "{case}: {error}");
        }
    }

    #[tokio::test]
    async fn environment_snapshot_freezes_the_exact_sandbox_policy_version() {
        use awaken_provisioning_contract::{
            IsolationClass, SandboxExecutionPolicy, SandboxExecutionPolicyId,
            SandboxExecutionPolicyRef, SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
            SandboxOverride,
        };

        let policies =
            Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
        let state = EnvironmentState::new().with_sandbox_policies(policies.clone());
        let environment_id = state
            .author("bound", json!({"type": "self_hosted"}))
            .await
            .unwrap();
        let v1 = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("strict".into()),
            version: SandboxExecutionPolicyVersion(1),
            config: SandboxOverride {
                isolation: Some(IsolationClass::Namespace),
                ..Default::default()
            },
            provisioning: awaken_provisioning_contract::SandboxProvisioning::OnToolUse,
            disabled: false,
        };
        policies.create(v1.clone()).await.unwrap();
        policies
            .bind_environment(
                &environment_id,
                SandboxExecutionPolicyRef {
                    id: v1.id.clone(),
                    version: v1.version,
                },
            )
            .await
            .unwrap();
        policies
            .publish(
                SandboxExecutionPolicyVersion(1),
                SandboxExecutionPolicy {
                    id: v1.id,
                    version: SandboxExecutionPolicyVersion(2),
                    config: SandboxOverride {
                        isolation: Some(IsolationClass::Container),
                        ..Default::default()
                    },
                    provisioning: awaken_provisioning_contract::SandboxProvisioning::Eager,
                    disabled: false,
                },
            )
            .await
            .unwrap();

        let snapshot = state.snapshot(&environment_id, None).await.unwrap();
        let frozen: SandboxOverride = serde_json::from_value(snapshot.sandbox).unwrap();
        assert_eq!(frozen.isolation, Some(IsolationClass::Namespace));
        assert_eq!(
            snapshot.sandbox_provisioning,
            awaken_provisioning_contract::SandboxProvisioning::OnToolUse
        );
    }

    #[tokio::test]
    async fn environment_snapshot_defaults_to_eager_and_fingerprints_provisioning() {
        use awaken_provisioning_contract::{
            SandboxExecutionPolicy, SandboxExecutionPolicyId, SandboxExecutionPolicyRef,
            SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion, SandboxOverride,
            SandboxProvisioning,
        };

        let policies =
            Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
        let state = EnvironmentState::new().with_sandbox_policies(policies.clone());
        let environment_id = state
            .author("timing", json!({"type": "self_hosted"}))
            .await
            .unwrap();
        let eager = state.snapshot(&environment_id, None).await.unwrap();
        assert_eq!(eager.sandbox_provisioning, SandboxProvisioning::Eager);

        let policy = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("lazy".into()),
            version: SandboxExecutionPolicyVersion::INITIAL,
            config: SandboxOverride::default(),
            provisioning: SandboxProvisioning::OnToolUse,
            disabled: false,
        };
        policies.create(policy.clone()).await.unwrap();
        let reference = SandboxExecutionPolicyRef {
            id: policy.id.clone(),
            version: policy.version,
        };
        policies
            .bind_environment(&environment_id, reference.clone())
            .await
            .unwrap();

        let lazy = state.snapshot(&environment_id, None).await.unwrap();
        assert_eq!(lazy.sandbox_provisioning, SandboxProvisioning::OnToolUse);
        assert_ne!(lazy.config_fingerprint, eager.config_fingerprint);
        let projected = project_policy_binding(&state, environment_id, reference)
            .await
            .unwrap();
        assert_eq!(projected.provisioning, SandboxProvisioning::OnToolUse);
    }
}
