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
use std::sync::atomic::{AtomicU64, Ordering};

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
use awaken_environment_application::{
    EnvironmentApplication, EnvironmentApplicationError, default_environment_registration,
};
use awaken_work_store::InMemoryWorkQueue;

use crate::work_queue::{HeartbeatResult, LeaseHeartbeat, WorkQueue};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentRegistrationSource, ExecutableEnvironmentWithdrawal,
    ExecutableEnvironmentWithdrawalOutcome,
};

/// Control-owned Environment definitions, immutable revision history, policy
/// versions, and publication application.
pub struct EnvironmentAuthoringState {
    envs: Arc<dyn EnvRegistry>,
    application: Arc<EnvironmentApplication>,
    sandbox_policies: Option<Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>>,
}

/// Coordinator-owned executable projection and WorkQueue. It has no definition
/// registry or sandbox-policy repository.
pub struct EnvironmentExecutionState {
    work: Arc<dyn WorkQueue>,
    execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    image_readiness:
        Option<Arc<dyn awaken_environment_realization_contract::EnvironmentImageReadiness>>,
}

/// AllInOne/test composition facade. It contains no business behavior of its
/// own and only combines the two canonical domain states.
pub struct EnvironmentState {
    authoring: Arc<EnvironmentAuthoringState>,
    execution: Arc<EnvironmentExecutionState>,
}

/// Coordinator application adapter around the rebuildable catalog. Registration
/// and WorkQueue healthcheck convergence share one idempotent boundary; Control
/// sees only `ExecutableEnvironmentRegistrar` and cannot access the queue.
pub struct CoordinatorEnvironmentRegistrar {
    delegate: Arc<dyn ExecutableEnvironmentRegistrar>,
    work: Arc<dyn WorkQueue>,
}

impl CoordinatorEnvironmentRegistrar {
    #[must_use]
    pub fn new(
        delegate: Arc<dyn ExecutableEnvironmentRegistrar>,
        work: Arc<dyn WorkQueue>,
    ) -> Self {
        Self { delegate, work }
    }
}

#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for CoordinatorEnvironmentRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let environment_id = registration.definition.id.clone();
        let self_hosted = registration.definition.is_self_hosted();
        let outcome = self.delegate.register(registration).await?;
        if self_hosted {
            self.work.ensure_healthcheck(&environment_id).await;
        }
        Ok(outcome)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let environment_id = withdrawal.environment_id.clone();
        let outcome = self.delegate.withdraw(withdrawal).await?;
        self.work.remove_env(&environment_id).await;
        Ok(outcome)
    }
}

impl EnvironmentAuthoringState {
    #[must_use]
    pub fn new(
        envs: Arc<dyn EnvRegistry>,
        sandbox_policies: Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
        registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    ) -> Self {
        Self {
            application: Arc::new(EnvironmentApplication::new(
                envs.clone(),
                registrar,
                Some(sandbox_policies.clone()),
            )),
            envs,
            sandbox_policies: Some(sandbox_policies),
        }
    }

    fn without_policies(
        envs: Arc<dyn EnvRegistry>,
        registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    ) -> Self {
        Self {
            application: Arc::new(EnvironmentApplication::new(envs.clone(), registrar, None)),
            envs,
            sandbox_policies: None,
        }
    }

    #[must_use]
    pub fn application(&self) -> Arc<EnvironmentApplication> {
        self.application.clone()
    }
}

impl EnvironmentExecutionState {
    #[must_use]
    pub fn new(
        work: Arc<dyn WorkQueue>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            work,
            execution_source,
            image_readiness: None,
        }
    }

    #[must_use]
    pub fn with_image_readiness(
        mut self,
        readiness: Arc<dyn awaken_environment_realization_contract::EnvironmentImageReadiness>,
    ) -> Self {
        self.image_readiness = Some(readiness);
        self
    }

    /// Read Coordinator's current executable definition projection. Archived or
    /// withdrawn definitions are unavailable even though exact history remains.
    pub async fn get(&self, environment_id: &str) -> Option<EnvItem> {
        self.execution_source
            .current_registration(environment_id)
            .await
            .ok()
            .flatten()
            .map(|registration| registration.definition)
    }

    /// Compile the one immutable, normalized Environment snapshot consumed by a
    /// Session. The Host never re-reads the mutable registry after this boundary.
    pub async fn snapshot(
        &self,
        env_id: &str,
        runtime: Option<&str>,
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        self.snapshot_for_session(env_id, runtime, &[])
            .await
            .ok()
            .flatten()
    }

    /// Compile a Session-specific snapshot from the exact MCP desired set that
    /// was already normalized by the Managed application boundary.
    pub async fn snapshot_for_session(
        &self,
        env_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_contract::EnvironmentSnapshot>,
        awaken_environment_realization_contract::EnvironmentImageBuildError,
    > {
        let registration = self
            .execution_source
            .current_registration(env_id)
            .await
            .map_err(|error| {
                awaken_environment_realization_contract::EnvironmentImageBuildError::Unavailable(
                    error.to_string(),
                )
            })?;
        let Some(registration) = registration else {
            return Ok(None);
        };
        snapshot_from_registration(
            registration,
            runtime,
            mcp_targets,
            self.image_readiness.as_ref(),
        )
        .await
    }

    pub async fn snapshot_exact(
        &self,
        env_id: &str,
        revision: u64,
        runtime: Option<&str>,
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        self.snapshot_exact_for_session(env_id, revision, runtime, &[])
            .await
            .ok()
            .flatten()
    }

    pub async fn snapshot_exact_for_session(
        &self,
        env_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_contract::EnvironmentSnapshot>,
        awaken_environment_realization_contract::EnvironmentImageBuildError,
    > {
        // Current availability is a live deny overlay. If Control withdrew the
        // Environment, an old Agent binding cannot start a new Session even though
        // the immutable historical registration remains queryable for audit.
        let current = self
            .execution_source
            .current_registration(env_id)
            .await
            .map_err(|error| {
                awaken_environment_realization_contract::EnvironmentImageBuildError::Unavailable(
                    error.to_string(),
                )
            })?;
        if current.is_none() {
            return Ok(None);
        }
        let registration = self
            .execution_source
            .registration_at_revision(
                env_id,
                awaken_environment_contract::EnvironmentRevision(revision),
            )
            .await
            .map_err(|error| {
                awaken_environment_realization_contract::EnvironmentImageBuildError::Unavailable(
                    error.to_string(),
                )
            })?;
        let Some(registration) = registration else {
            return Ok(None);
        };
        snapshot_from_registration(
            registration,
            runtime,
            mcp_targets,
            self.image_readiness.as_ref(),
        )
        .await
    }

    /// Whether `env_id` is a self-hosted environment. Sessions assigned to one are
    /// dispatched through the work queue for an external worker to run.
    pub async fn is_self_hosted(&self, env_id: &str) -> bool {
        self.execution_source
            .current_registration(env_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|registration| registration.definition.is_self_hosted())
    }

    /// Enqueue a `session` work item for `session_id` on `env_id`'s queue — the way
    /// the control plane dispatches a session assigned to a self-hosted environment,
    /// so a worker polling the environment can claim and run it. Returns the work id.
    pub async fn enqueue_session_work(&self, env_id: &str, session_id: &str) -> String {
        self.work.enqueue_session(env_id, session_id).await
    }
}

impl Default for EnvironmentExecutionState {
    fn default() -> Self {
        let catalog =
            Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
        catalog
            .install_seed(default_environment_registration())
            .expect("install built-in local Environment");
        Self::new(Arc::new(InMemoryWorkQueue::new()), catalog)
    }
}

impl Default for EnvironmentState {
    fn default() -> Self {
        let envs: Arc<dyn EnvRegistry> = Arc::new(InMemoryEnvRegistry::new());
        let work: Arc<dyn WorkQueue> = Arc::new(InMemoryWorkQueue::new());
        let catalog =
            Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
        catalog
            .install_seed(default_environment_registration())
            .expect("install built-in local Environment");
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> =
            Arc::new(CoordinatorEnvironmentRegistrar::new(
                Arc::new(
                    awaken_executable_environment_catalog::LocalExecutableEnvironmentRegistrar::new(
                        catalog.clone(),
                    ),
                ),
                work.clone(),
            ));
        Self {
            authoring: Arc::new(EnvironmentAuthoringState::without_policies(envs, registrar)),
            execution: Arc::new(EnvironmentExecutionState::new(work, catalog)),
        }
    }
}

impl EnvironmentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_stores(envs: Arc<dyn EnvRegistry>, work: Arc<dyn WorkQueue>) -> Self {
        let catalog =
            Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
        catalog
            .install_seed(default_environment_registration())
            .expect("install built-in local Environment");
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> =
            Arc::new(CoordinatorEnvironmentRegistrar::new(
                Arc::new(
                    awaken_executable_environment_catalog::LocalExecutableEnvironmentRegistrar::new(
                        catalog.clone(),
                    ),
                ),
                work.clone(),
            ));
        Self {
            authoring: Arc::new(EnvironmentAuthoringState::without_policies(envs, registrar)),
            execution: Arc::new(EnvironmentExecutionState::new(work, catalog)),
        }
    }

    #[must_use]
    pub fn application(&self) -> Arc<EnvironmentApplication> {
        self.authoring.application()
    }

    #[must_use]
    pub fn authoring(&self) -> Arc<EnvironmentAuthoringState> {
        self.authoring.clone()
    }

    #[must_use]
    pub fn execution(&self) -> Arc<EnvironmentExecutionState> {
        self.execution.clone()
    }

    pub async fn get(&self, environment_id: &str) -> Option<EnvItem> {
        self.execution.get(environment_id).await
    }

    pub async fn snapshot(
        &self,
        env_id: &str,
        runtime: Option<&str>,
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        self.execution.snapshot(env_id, runtime).await
    }

    pub async fn snapshot_for_session(
        &self,
        env_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_contract::EnvironmentSnapshot>,
        awaken_environment_realization_contract::EnvironmentImageBuildError,
    > {
        self.execution
            .snapshot_for_session(env_id, runtime, mcp_targets)
            .await
    }

    pub async fn snapshot_exact(
        &self,
        env_id: &str,
        revision: u64,
        runtime: Option<&str>,
    ) -> Option<awaken_session_contract::EnvironmentSnapshot> {
        self.execution
            .snapshot_exact(env_id, revision, runtime)
            .await
    }

    pub async fn snapshot_exact_for_session(
        &self,
        env_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_contract::EnvironmentSnapshot>,
        awaken_environment_realization_contract::EnvironmentImageBuildError,
    > {
        self.execution
            .snapshot_exact_for_session(env_id, revision, runtime, mcp_targets)
            .await
    }

    pub async fn is_self_hosted(&self, environment_id: &str) -> bool {
        self.execution.is_self_hosted(environment_id).await
    }

    pub async fn enqueue_session_work(&self, environment_id: &str, session_id: &str) -> String {
        self.execution
            .enqueue_session_work(environment_id, session_id)
            .await
    }

    #[must_use]
    pub fn with_sandbox_policies(
        mut self,
        store: Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
    ) -> Self {
        self.authoring = Arc::new(EnvironmentAuthoringState {
            envs: self.authoring.envs.clone(),
            application: Arc::new(
                self.authoring
                    .application
                    .with_sandbox_policies(store.clone()),
            ),
            sandbox_policies: Some(store),
        });
        self
    }

    #[cfg(test)]
    async fn author(&self, name: &str, config: serde_json::Value) -> Result<String, String> {
        static NEXT_TEST_COMMAND: AtomicU64 = AtomicU64::new(0);
        let typed = serde_json::from_value::<EnvironmentConfigParams>(config)
            .map_err(|error| format!("invalid Environment config: {error}"))?;
        self.application()
            .create(awaken_environment_contract::CreateEnvironmentCommand {
                command_id: format!("test:{}", NEXT_TEST_COMMAND.fetch_add(1, Ordering::Relaxed)),
                name: name.to_owned(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: canonical_environment_config(typed),
            })
            .await
            .map(|item| item.id)
            .map_err(|error| error.to_string())
    }
}

async fn snapshot_from_registration(
    registration: ExecutableEnvironmentRegistration,
    runtime: Option<&str>,
    mcp_targets: &[awaken_session_contract::McpTarget],
    image_readiness: Option<
        &Arc<dyn awaken_environment_realization_contract::EnvironmentImageReadiness>,
    >,
) -> Result<
    Option<awaken_session_contract::EnvironmentSnapshot>,
    awaken_environment_realization_contract::EnvironmentImageBuildError,
> {
    let item = &registration.definition;
    if item.archived_at.is_some() {
        return Ok(None);
    }
    let packages = item.config.packages();
    let network = session_network_policy(&item.config, mcp_targets);
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
    let (sandbox, sandbox_provisioning, base_image) = match &registration.sandbox_policy {
        Some(policy) if policy.disabled => return Ok(None),
        Some(policy) => {
            let base_image = match &policy.config.environment {
                Some(awaken_provisioning_contract::EnvironmentKind::Image { reference }) => {
                    Some(reference.clone())
                }
                _ => None,
            };
            let Ok(config) = serde_json::to_value(&policy.config) else {
                return Ok(None);
            };
            (config, policy.provisioning, base_image)
        }
        None => (
            serde_json::json!({}),
            awaken_session_contract::SandboxProvisioning::Eager,
            None,
        ),
    };
    let prepared_image = match image_readiness {
        Some(readiness) => {
            readiness
                .ready_image(&registration, base_image.as_deref())
                .await?
        }
        None => None,
    };
    let config_fingerprint = awaken_session_contract::EnvironmentFingerprint(
        awaken_session_contract::stable_fingerprint(&(
            &sandbox,
            &sandbox_provisioning,
            &packages,
            &network,
            &credential_realization,
            &prepared_image,
        )),
    );
    Ok(Some(awaken_session_contract::EnvironmentSnapshot {
        environment_id: item.id.clone(),
        revision: item.revision,
        config_fingerprint,
        sandbox,
        sandbox_provisioning,
        packages,
        prepared_image,
        network,
        credential_realization,
    }))
}

/// Compile static Environment networking plus the exact Session MCP set into
/// the dynamic frozen Session policy. Keeping this at the projection boundary
/// prevents the Control aggregate from depending on Session/runtime types.
fn session_network_policy(
    config: &awaken_environment_contract::EnvironmentConfig,
    mcp_targets: &[awaken_session_contract::McpTarget],
) -> awaken_session_contract::SessionNetworkPolicy {
    use awaken_environment_contract::EnvironmentNetworking;
    use awaken_session_contract::SessionNetworkPolicy;

    let EnvironmentNetworking::Limited {
        allowed_hosts,
        allow_mcp_servers,
        allow_package_managers,
    } = (match config {
        awaken_environment_contract::EnvironmentConfig::Cloud { networking, .. } => networking,
        awaken_environment_contract::EnvironmentConfig::SelfHosted => {
            return SessionNetworkPolicy::Unrestricted;
        }
    })
    else {
        return SessionNetworkPolicy::Unrestricted;
    };

    let mut hosts = allowed_hosts.clone();
    if *allow_mcp_servers {
        hosts.extend(mcp_targets.iter().filter_map(|target| {
            target.http_url().and_then(|url| {
                awaken_session_contract::McpTarget::identity(url)
                    .ok()
                    .map(|identity| identity.host)
            })
        }));
    }
    if *allow_package_managers {
        hosts.extend(
            awaken_environment_contract::PUBLIC_PACKAGE_REGISTRY_HOSTS
                .iter()
                .map(ToString::to_string),
        );
    }
    SessionNetworkPolicy::Allowlist { hosts }.normalized()
}

/// AllInOne/test composition of the two canonical Environment route groups.
pub fn environments_router(state: Arc<EnvironmentState>) -> Router {
    environment_authoring_router(state.authoring())
        .merge(environment_work_router(state.execution()))
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

/// Mount Coordinator-owned work coordination routes.
pub fn environment_work_router(state: Arc<EnvironmentExecutionState>) -> Router {
    Router::new()
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
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyCreate {
    id: String,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_session_contract::SandboxProvisioning,
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

async fn project_policy_binding(
    state: &EnvironmentAuthoringState,
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
    state: &EnvironmentAuthoringState,
) -> Result<&dyn awaken_provisioning_contract::SandboxExecutionPolicyStore, StatusCode> {
    state
        .sandbox_policies
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

async fn create_sandbox_policy(
    State(state): State<Arc<EnvironmentAuthoringState>>,
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
    State(state): State<Arc<EnvironmentAuthoringState>>,
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
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Path(environment_id): Path<String>,
    Json(input): Json<SandboxPolicyBindingInput>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let reference = awaken_provisioning_contract::SandboxExecutionPolicyRef {
        id: awaken_provisioning_contract::SandboxExecutionPolicyId(input.policy_id),
        version: awaken_provisioning_contract::SandboxExecutionPolicyVersion(input.version),
    };
    state
        .application
        .bind_sandbox_policy(&environment_id, reference.clone())
        .await
        .map_err(|error| match error {
            EnvironmentApplicationError::NotFound => StatusCode::NOT_FOUND,
            EnvironmentApplicationError::BuiltinImmutable => StatusCode::CONFLICT,
            EnvironmentApplicationError::Policy(_) => StatusCode::UNPROCESSABLE_ENTITY,
            EnvironmentApplicationError::Create(_)
            | EnvironmentApplicationError::Registration(_) => StatusCode::SERVICE_UNAVAILABLE,
        })?;
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

async fn get_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentAuthoringState>>,
    Path(environment_id): Path<String>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let reference = state
        .application
        .get(&environment_id)
        .await
        .and_then(|item| item.sandbox_policy)
        .map(
            |reference| awaken_provisioning_contract::SandboxExecutionPolicyRef {
                id: awaken_provisioning_contract::SandboxExecutionPolicyId(reference.policy_id),
                version: awaken_provisioning_contract::SandboxExecutionPolicyVersion(
                    reference.version,
                ),
            },
        )
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
        NotFound => StatusCode::NOT_FOUND,
        VersionConflict => StatusCode::CONFLICT,
        Disabled | Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        StoreFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
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
        EnvironmentApplicationError::BuiltinImmutable => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new("conflict_error", message)),
        ),
        EnvironmentApplicationError::Policy(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse::new("invalid_request_error", message)),
        ),
        EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::Store(_),
        )
        | EnvironmentApplicationError::Registration(_) => (
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
) -> Json<Page<Environment>> {
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

// ---- Work routes -----------------------------------------------------------

async fn require_env(state: &EnvironmentExecutionState, id: &str) -> Result<(), WireError> {
    if state
        .execution_source
        .current_registration(id)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        Ok(())
    } else {
        Err(not_found("environment"))
    }
}

/// `GET /v1/environments/:id/work` — the environment's work items.
async fn list_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    State(state): State<Arc<EnvironmentExecutionState>>,
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
    use super::*;

    fn config(value: serde_json::Value) -> awaken_environment_contract::EnvironmentConfig {
        serde_json::from_value(value).expect("valid neutral Environment config")
    }

    async fn create_definition(
        state: &EnvironmentState,
        name: &str,
        environment_config: awaken_environment_contract::EnvironmentConfig,
    ) -> EnvItem {
        static NEXT_COMMAND: AtomicU64 = AtomicU64::new(0);
        state
            .application()
            .create(awaken_environment_contract::CreateEnvironmentCommand {
                command_id: format!(
                    "environment-test:{}",
                    NEXT_COMMAND.fetch_add(1, Ordering::Relaxed)
                ),
                name: name.into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: environment_config,
            })
            .await
            .expect("create and publish test Environment")
    }

    #[tokio::test]
    async fn control_directory_owns_builtin_and_terminal_history() {
        // Cause/effect decision table:
        // R1 empty Control store + reconciliation -> built-in env_local is
        // queryable in Control and registered current in Coordinator;
        // R2 built-in mutation -> conflict and no new revision;
        // R3 authored Environment delete -> final archived revision retained in
        // Control, current execution withdrawn, every exact revision retained.
        use awaken_executable_environment_catalog::{
            ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
        };

        let envs: Arc<dyn EnvRegistry> = Arc::new(InMemoryEnvRegistry::new());
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let authoring = EnvironmentAuthoringState::without_policies(
            envs.clone(),
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
        );
        let application = authoring.application();
        application.reconcile_registrations().await.expect("R1");
        assert_eq!(
            application.get("env_local").await.expect("R1").revision.0,
            1
        );
        assert!(catalog.current("env_local").is_some(), "R1");
        assert!(
            matches!(
                application.update("env_local", EnvUpdate::default()).await,
                Err(EnvironmentApplicationError::BuiltinImmutable)
            ),
            "R2"
        );

        let authored = application
            .create(awaken_environment_contract::CreateEnvironmentCommand {
                command_id: "terminal-history".into(),
                name: "terminal".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: Default::default(),
            })
            .await
            .expect("R3 create");
        application.delete(&authored.id).await.expect("R3 delete");
        let terminal = envs.get(&authored.id).await.expect("R3 tombstone");
        assert!(terminal.archived_at.is_some(), "R3");
        assert_eq!(terminal.revision.0, 2, "R3");
        assert!(catalog.current(&authored.id).is_none(), "R3");
        assert!(
            catalog
                .at_revision(
                    &authored.id,
                    awaken_environment_contract::EnvironmentRevision(1),
                )
                .is_some(),
            "R3"
        );
    }

    #[tokio::test]
    async fn with_stores_selects_self_hosted_and_handles_missing() {
        let state = EnvironmentState::with_stores(
            Arc::new(InMemoryEnvRegistry::new()),
            Arc::new(InMemoryWorkQueue::new()),
        );
        let e = create_definition(&state, "e", config(json!({ "type": "self_hosted" }))).await;
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
        // An exact sandbox-policy reference, when present, belongs to the authored
        // Environment revision and the registration carries the resolved body.
        let state = EnvironmentState::new();
        let item = create_definition(
            &state,
            "snapshot",
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
            .application()
            .update(
                &item.id,
                EnvUpdate {
                    name: Some("changed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
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
        state.application().archive(&item.id).await.unwrap();
        assert!(
            state.snapshot(&item.id, None).await.is_none(),
            "S4 archived"
        );
        let local = state.snapshot("env_local", None).await.expect("S5");
        assert_eq!(
            local,
            snapshot_from_registration(default_environment_registration(), None, &[], None)
                .await
                .unwrap()
                .unwrap(),
            "S5 native"
        );
        let local_acp = state
            .snapshot("env_local", Some("acp:claude"))
            .await
            .expect("S5 ACP");
        assert_eq!(
            local_acp,
            snapshot_from_registration(
                default_environment_registration(),
                Some("acp:claude"),
                &[],
                None,
            )
            .await
            .unwrap()
            .unwrap(),
            "S5 ACP"
        );
        let closed = create_definition(
            &state,
            "closed",
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
        // Cause/effect rules: R1 a valid official union creates an executable
        // revision; R2 a later active revision preserves exact historical reads
        // without substituting current; R3 wire-only runtime/sandbox fields are
        // rejected before authoring or registration.
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
            .application()
            .update(
                &id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            state
                .snapshot_exact(&id, exact.revision.0, None)
                .await
                .is_some(),
            "R2 exact history remains executable while current is active"
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

    /// Environment policy snapshot decision table:
    /// C1 no binding -> eager default; C7 disabled exact policy -> unavailable;
    /// C8 old exact binding + newer published version -> freeze the old version.
    /// This case owns C8 and proves isolation, provisioning, and fingerprint all
    /// derive from the bound immutable version rather than the current policy.
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
            provisioning: awaken_session_contract::SandboxProvisioning::OnToolUse,
            disabled: false,
        };
        policies.create(v1.clone()).await.unwrap();
        state
            .application()
            .bind_sandbox_policy(
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
                    provisioning: awaken_session_contract::SandboxProvisioning::Eager,
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
            awaken_session_contract::SandboxProvisioning::OnToolUse
        );
    }

    /// C1 proves the backward-compatible eager default. Binding an active
    /// on-tool-use policy causes the exact timing to enter both the snapshot and
    /// its fingerprint; a timing change can therefore never alias eager truth.
    #[tokio::test]
    async fn environment_snapshot_defaults_to_eager_and_fingerprints_provisioning() {
        use awaken_provisioning_contract::{
            SandboxExecutionPolicy, SandboxExecutionPolicyId, SandboxExecutionPolicyRef,
            SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion, SandboxOverride,
        };
        use awaken_session_contract::SandboxProvisioning;

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
        state
            .application()
            .bind_sandbox_policy(&environment_id, reference.clone())
            .await
            .unwrap();

        let lazy = state.snapshot(&environment_id, None).await.unwrap();
        assert_eq!(lazy.sandbox_provisioning, SandboxProvisioning::OnToolUse);
        assert_ne!(lazy.config_fingerprint, eager.config_fingerprint);
        let projected = project_policy_binding(&state.authoring, environment_id, reference)
            .await
            .unwrap();
        assert_eq!(projected.provisioning, SandboxProvisioning::OnToolUse);
    }
}
