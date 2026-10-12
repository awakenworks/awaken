// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-executor/src/host/worker_resolver/dispatched_mcp_tests.rs
// @formal reason: Claimed-dispatch fixtures and fake model compiled only through the test-gated
// @formal reason: worker_resolver module.
//! Shared claimed-dispatch fixtures for Worker resolver tests.
//!
//! The resolver scenarios reuse one activation builder, deferred Environment,
//! and binding sink so their failure matrices cannot drift through local copies.

use super::*;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct AdoptionModel;

#[async_trait::async_trait]
impl LlmExecutor for AdoptionModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok"),
            usage: None,
            stop_reason: None,
        })
    }
}

/// The dependencies of a Host over [`AdoptionModel`] with only its built-in
/// volatile stores.
pub(crate) async fn adoption_dependencies() -> crate::HostDependencies {
    crate::HostDependencies::ephemeral(Arc::new(AdoptionModel), "stub").await
}

/// [`adoption_dependencies`] with its durable stores opened under `dir`; two
/// Hosts over one `dir` are a restart of the same installation.
pub(crate) async fn adoption_dependencies_at(
    dir: impl Into<std::path::PathBuf>,
) -> crate::HostDependencies {
    adoption_dependencies().await.with_store_dir(dir).await
}

/// A Host over [`adoption_dependencies`].
pub(crate) async fn adoption_host() -> Arc<SharedHost> {
    (adoption_dependencies().await)
        .build()
        .expect("validated Host dependencies")
}

/// A Host over [`adoption_dependencies`] whose dispatch transport is `store`.
pub(crate) async fn adoption_host_over(
    store: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
) -> Arc<SharedHost> {
    (adoption_dependencies().await.with_ephemeral_dispatch(store))
        .build()
        .expect("validated Host dependencies")
}

/// A Host over [`adoption_dependencies_at`].
pub(crate) async fn adoption_host_at(dir: impl Into<std::path::PathBuf>) -> Arc<SharedHost> {
    (adoption_dependencies_at(dir).await)
        .build()
        .expect("validated Host dependencies")
}

pub(crate) fn test_activation(thread: &str, run: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint(format!("catalog-{run}"));
    RunActivation::new(
        RunId(run.to_string()),
        ThreadId(thread.to_string()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId(format!("snapshot-{run}")),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-a".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: "test".to_string(),
                max_steps: Some(1),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "backend"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        Vec::new(),
    )
}

pub(crate) fn deferred_environment() -> awaken_session_configuration_contract::EnvironmentSnapshot {
    awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "lazy-env".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "lazy-env-v1".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: awaken_session_configuration_contract::SandboxProvisioning::OnToolUse,
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    }
}

pub(crate) fn eager_environment() -> awaken_session_configuration_contract::EnvironmentSnapshot {
    let mut environment = deferred_environment();
    environment.environment_id = "eager-env".into();
    environment.config_fingerprint =
        awaken_session_configuration_contract::EnvironmentFingerprint("eager-env-v1".into());
    environment.sandbox_provisioning = Default::default();
    environment
}

/// Shared complete-projection fixture owner. C1 no frozen model publication and
/// C2 one exact snapshot model publication both produce E1 one immutable
/// baseline plus E2 the exact empty rev0 Resource transition; callers vary only
/// the model cause and never reconstruct slot fields independently.
pub(super) fn empty_frozen_projection(
    workspace_id: &str,
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    empty_frozen_projection_with_model(workspace_id, environment, "agent-a", "stub", None, None)
}

pub(crate) fn empty_frozen_projection_for_snapshot(
    workspace_id: &str,
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let primary = snapshot.resolved_spec.model_binding.clone();
    let runtime = primary.binding().identity().2.to_owned();
    let model = primary.binding().identity().1.to_owned();
    let model_override = awaken_session_configuration_contract::SessionModelOverride {
        publication: Some(Box::new(
            awaken_session_configuration_contract::SessionModelPublication {
                primary,
                candidates: snapshot.resolved_spec.model_candidates.clone(),
            },
        )),
        inference: Default::default(),
    };
    empty_frozen_projection_with_model(
        workspace_id,
        environment,
        &snapshot.root_agent_id.0,
        &model,
        Some(runtime),
        Some(model_override),
    )
}

pub(crate) async fn install_complete_projection_for_snapshot(
    managed: &crate::ManagedHost,
    thread: &str,
    workspace_id: &str,
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) {
    let mut projection = empty_frozen_projection_for_snapshot(workspace_id, environment, snapshot);
    projection.agent_publication = Some(snapshot.clone());
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        managed,
        thread,
        projection,
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install complete frozen Session projection");
}

pub(super) fn frozen_projection_for_manifest(
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
    manifest: &awaken_session_resource_contract::SessionResourceManifest,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let mut projection = empty_frozen_projection(&manifest.workspace_id, environment);
    projection.resource_revision = manifest.revision;
    projection.resources = manifest.resources.clone();
    projection.previous_resource_manifest = Some(
        awaken_session_resource_contract::SessionResourceManifest::at_revision(
            manifest.workspace_id.clone(),
            0,
            awaken_session_resource_contract::ResolvedSessionResources::default(),
        ),
    );
    projection
}

fn empty_frozen_projection_with_model(
    workspace_id: &str,
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
    agent_id: &str,
    model: &str,
    runtime: Option<String>,
    model_override: Option<awaken_session_configuration_contract::SessionModelOverride>,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment,
            runtime_placement:
                awaken_session_configuration_contract::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: agent_id.into(),
            agent_revision: None,
            model_override,
            model: model.into(),
            runtime,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    awaken_session_realization_contract::FrozenSessionProjection {
        workspace_id: workspace_id.into(),
        revision: awaken_session_types::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment_materialization: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(
            awaken_session_resource_contract::SessionResourceManifest::new(
                workspace_id,
                awaken_session_resource_contract::ResolvedSessionResources::default(),
            ),
        ),
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    }
}

pub(crate) fn managed_test_host(host: Arc<SharedHost>) -> crate::ManagedHost {
    complete_managed_test_host(crate::ManagedHost::new(host))
}

/// Install the one production composition required by every executable fixture:
/// DispatchSessionRuntime plus the single Session coordination application.
pub(super) fn complete_managed_test_host(managed: crate::ManagedHost) -> crate::ManagedHost {
    static APPLICATION: std::sync::OnceLock<
        Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    > = std::sync::OnceLock::new();
    let application = APPLICATION.get_or_init(|| {
        Arc::new(crate::coordination::RecordingSessionAgentCoordination::default())
    });
    managed
        .host
        .session_application_bindings()
        .bind_agent_coordination(application.clone())
        .expect("install one test Session application authority");
    managed
}

/// Produce a ready Local Environment under the caller's already-installed
/// original Root. C1 the same fixture configured the production sink before
/// Host build; C2 its Store-read projection/lease is installed; C3 the immutable
/// model selects the provider. E1 the original Create is admitted before I/O;
/// E2 only its captured current authority reaches the provider; E3 canonical
/// persistence/readback supplies Durable identity. An exact already-applied
/// Create reuses its binding without a second create. Missing/unowned Root
/// evidence is a fixture error, never a hand-made fence or historical seed.
pub(crate) async fn available_local_environment_binding(
    host: &crate::host::SharedHost,
    thread: &str,
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
) -> String {
    let intent = host.realizer.new_environment_effect_intent(
        thread,
        awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        None,
    );
    let effect = host
        .authorize_environment_effect_before_io(thread, &intent)
        .await
        .expect("admit original available-Environment Create under its existing Root");
    let (binding, environment) = match &effect.authorization {
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned {
            ..
        } => {
            panic!("available-Environment fixture requires its actual Managed Root")
        }
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding,
            ..
        } => (binding.clone(), None),
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized {
            ..
        } => {
            let provider = host
                .projected_session_environment_provider(thread, None)
                .expect("select available-Environment provider from frozen projection");
            let spec = host
                .realizer
                .staging
                .sandbox_spec_for_resolved_resources_and_provider(
                    thread,
                    &projection.resources,
                    provider,
                );
            let spec = host
                .realizer
                .validate_session_environment_capabilities(provider, &spec)
                .expect("validate exact available-Environment fixture spec");
            let provider_effect = crate::session_environment::ProviderEffect::from_parts(
                effect.provider_fence.as_ref(),
                effect.provider_authority.as_deref(),
            )
            .expect("original captured available-Environment authority");
            let environment = provider
                .realizer()
                .create_effective_for_effect(
                    &spec,
                    provider_effect,
                    None,
                    awaken_session_environment_contract::EnvironmentRealizationKind::Create,
                )
                .await
                .expect("create typed available-Environment fixture");
            let binding = serde_json::to_string(&environment.lifecycle().handle())
                .expect("serialize typed available-Environment fixture");
            (binding, Some(environment))
        }
    };
    let identity = host
        .persist_authorized_environment_binding(thread, binding.clone(), &effect)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "read original available-Environment identity: {}",
                failure.error
            )
        });
    assert!(
        matches!(
            identity,
            crate::environment_owner::BoundSessionEnvironmentIdentity::Durable { .. }
        ),
        "the original Root is the sole available-Environment identity owner"
    );
    drop(environment);
    binding
}

/// Derive the unavailable row from the same ready fixture, then remove only
/// its exact filesystem root. C1 a valid V2 binding exists; C2 the inode-fenced
/// root is removed. E1 provider observation reports definitive unavailability
/// for that same incarnation rather than fixture/spec drift.
pub(super) async fn unavailable_local_environment_binding(
    host: &crate::host::SharedHost,
    thread: &str,
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
) -> String {
    let binding = available_local_environment_binding(host, thread, projection).await;
    remove_local_environment_root(host, thread);
    binding
}

/// Fault the physical owner independently of the Worker and its retained Arcs.
/// Both cold and surviving-Worker recovery rows use this one exact-inode cut.
pub(super) fn remove_local_environment_root(host: &crate::host::SharedHost, thread: &str) {
    let root = host
        .storage_dir()
        .expect("unavailable Local Environment fixture requires durable storage")
        .join("sandboxes")
        .join(thread);
    let identity = awaken_sandbox_fs::directory_identity_nofollow(&root)
        .expect("observe exact unavailable-Environment root identity");
    awaken_sandbox_fs::remove_directory_tree_exact(&root, identity)
        .expect("remove exact unavailable-Environment physical root");
}

pub(crate) fn with_empty_session_resources(
    request: awaken_dispatch_contract::RunDispatch,
    workspace_id: &str,
) -> awaken_dispatch_contract::RunDispatch {
    let manifest = awaken_session_resource_contract::SessionResourceManifest::new(
        workspace_id,
        awaken_session_resource_contract::ResolvedSessionResources::default(),
    );
    request
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from(workspace_id),
        ))
        .with_session_resources(
            crate::dispatch_session_runtime::resource_envelope(&manifest)
                .expect("serialize exact empty Resource manifest"),
        )
}

pub(super) async fn prepare_deferred_session(
    host: Arc<SharedHost>,
    thread: &str,
) -> crate::ManagedHost {
    let managed = managed_test_host(host.clone());
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        thread,
        empty_frozen_projection(host.local_workspace(), deferred_environment()),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("prepare complete deferred Session projection");
    managed
}

pub(super) struct ToggleBindingSink {
    pub(super) fail: std::sync::atomic::AtomicBool,
    pub(super) calls: AtomicUsize,
    pub(super) binding: std::sync::Mutex<Option<String>>,
}

pub(super) fn committed_environment(
    receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
) -> awaken_session_state_contract::SessionEnvironmentState {
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &receipt.session_id,
        1,
        u64::MAX,
        "test-environment",
        "test-image",
    );
    awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: receipt.binding,
        effect_id: Some(receipt.effect_id),
        generation: Some(generation),
        idle_since_unix_ms: None,
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink for ToggleBindingSink {
    async fn authorize(
        &self,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        let binding = self.binding.lock().unwrap().clone();
        match binding {
            Some(_) if self.fail.swap(false, Ordering::SeqCst) => {
                Err(awaken_session_types::RunError::unavailable(
                    "injected Session binding failure during readback",
                ))
            }
            Some(binding) => Ok(
                awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                    binding,
                    intent: Box::new(intent.clone()),
                },
            ),
            None => Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized { intent: Box::new(intent.clone()) }),
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.binding.lock().unwrap() = Some(receipt.binding.clone());
        if self.fail.load(Ordering::SeqCst) {
            Err(awaken_session_types::RunError::unavailable(
                "injected Session binding failure",
            ))
        } else {
            Ok(committed_environment(receipt))
        }
    }
}

pub(crate) struct BlockingBindingSink {
    block: std::sync::atomic::AtomicBool,
    pub(crate) entered: tokio::sync::Notify,
    calls: AtomicUsize,
    delegate: Option<Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    commit_before_block: bool,
}

impl BlockingBindingSink {
    pub(crate) fn blocked() -> Self {
        Self {
            block: std::sync::atomic::AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
            calls: AtomicUsize::new(0),
            delegate: None,
            commit_before_block: false,
        }
    }

    pub(crate) fn around(
        delegate: Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
        commit_before_block: bool,
    ) -> Self {
        Self {
            delegate: Some(delegate),
            commit_before_block,
            ..Self::blocked()
        }
    }

    pub(crate) fn unblock(&self) {
        self.block.store(false, Ordering::SeqCst);
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink for BlockingBindingSink {
    async fn authorize(
        &self,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        // Cancellation fixture rule: C1 the aggregate authorizes the effect;
        // E1 only the following persist await may be cancelled and retried.
        // Using the contract authorization seam keeps the fixture from
        // reviving the removed persist-without-authorization path.
        match &self.delegate {
            Some(delegate) => delegate.authorize(intent).await,
            None => Ok(
                awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized {
                    intent: Box::new(intent.clone()),
                },
            ),
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Fault schedules share one blocking seam. With a delegate, the real
        // fixture root owns authorization and commitment; cancellation before
        // persist leaves it unchanged, cancellation after persist loses only
        // the response. Neither path manufactures a second root identity.
        let committed = if self.commit_before_block {
            Some(
                self.delegate
                    .as_ref()
                    .expect("commit-first delegate")
                    .persist(receipt.clone())
                    .await?,
            )
        } else {
            None
        };
        if self.block.load(Ordering::SeqCst) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        match (committed, &self.delegate) {
            (Some(committed), _) => Ok(committed),
            (None, Some(delegate)) => delegate.persist(receipt).await,
            (None, None) => Ok(committed_environment(receipt)),
        }
    }
}

pub(crate) async fn claim(
    store: &Arc<
        impl awaken_dispatch_contract::DispatchEnqueue + awaken_dispatch_contract::DispatchClaims,
    >,
    thread: &str,
    run: &str,
    owner: &str,
    now: u64,
    clock: &awaken_lease_testkit::ManualClock,
) -> awaken_dispatch_contract::Claimed {
    store
        .enqueue(awaken_dispatch_contract::RunDispatch::new(
            test_activation(thread, run),
        ))
        .await
        .expect("enqueue deferred run");
    clock.set(now);
    store
        .claim(owner, 1_000, &Default::default())
        .await
        .expect("claim deferred run")
        .expect("deferred run available")
}
