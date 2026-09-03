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
                max_steps: 1,
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

pub(super) fn deferred_environment() -> awaken_session_contract::EnvironmentSnapshot {
    awaken_session_contract::EnvironmentSnapshot {
        environment_id: "lazy-env".into(),
        revision: awaken_session_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("lazy-env-v1".into()),
        sandbox: Default::default(),
        sandbox_provisioning: awaken_session_contract::SandboxProvisioning::OnToolUse,
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
    }
}

pub(crate) fn eager_environment() -> awaken_session_contract::EnvironmentSnapshot {
    let mut environment = deferred_environment();
    environment.environment_id = "eager-env".into();
    environment.config_fingerprint =
        awaken_session_contract::EnvironmentFingerprint("eager-env-v1".into());
    environment.sandbox_provisioning = Default::default();
    environment
}

/// Shared complete-projection fixture owner. C1 no frozen model publication and
/// C2 one exact snapshot model publication both produce E1 one immutable
/// baseline plus E2 the exact empty rev0 Resource transition; callers vary only
/// the model cause and never reconstruct slot fields independently.
pub(super) fn empty_frozen_projection(
    workspace_id: &str,
    environment: awaken_session_contract::EnvironmentSnapshot,
) -> awaken_session_contract::FrozenSessionProjection {
    empty_frozen_projection_with_model(workspace_id, environment, "agent-a", "stub", None, None)
}

pub(crate) fn empty_frozen_projection_for_snapshot(
    workspace_id: &str,
    environment: awaken_session_contract::EnvironmentSnapshot,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) -> awaken_session_contract::FrozenSessionProjection {
    let primary = snapshot.resolved_spec.model_binding.clone();
    let runtime = primary.binding().backend_ref.clone();
    let model = primary.binding().model_ref.clone();
    let model_override = awaken_session_contract::SessionModelOverride {
        publication: Some(Box::new(awaken_session_contract::SessionModelPublication {
            primary,
            candidates: snapshot.resolved_spec.model_candidates.clone(),
        })),
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
    environment: awaken_session_contract::EnvironmentSnapshot,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
) {
    let mut projection = empty_frozen_projection_for_snapshot(workspace_id, environment, snapshot);
    projection.agent_publication = Some(snapshot.clone());
    awaken_session_contract::SessionRuntime::install_session_projection(
        managed,
        thread,
        projection,
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install complete frozen Session projection");
}

pub(super) fn frozen_projection_for_manifest(
    environment: awaken_session_contract::EnvironmentSnapshot,
    manifest: &awaken_session_contract::SessionResourceManifest,
) -> awaken_session_contract::FrozenSessionProjection {
    let mut projection = empty_frozen_projection(&manifest.workspace_id, environment);
    projection.resource_revision = manifest.revision;
    projection.resources = manifest.resources.clone();
    projection.previous_resource_manifest = Some(
        awaken_session_contract::SessionResourceManifest::at_revision(
            manifest.workspace_id.clone(),
            0,
            awaken_session_contract::ResolvedSessionResources::default(),
        ),
    );
    projection
}

fn empty_frozen_projection_with_model(
    workspace_id: &str,
    environment: awaken_session_contract::EnvironmentSnapshot,
    agent_id: &str,
    model: &str,
    runtime: Option<String>,
    model_override: Option<awaken_session_contract::SessionModelOverride>,
) -> awaken_session_contract::FrozenSessionProjection {
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment,
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
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
    awaken_session_contract::FrozenSessionProjection {
        workspace_id: workspace_id.into(),
        revision: awaken_session_contract::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(awaken_session_contract::SessionResourceManifest::new(
            workspace_id,
            awaken_session_contract::ResolvedSessionResources::default(),
        )),
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
    let managed = managed.install_dispatch_session_runtime();
    static APPLICATION: std::sync::OnceLock<
        Arc<dyn awaken_session_contract::SessionAgentCoordination>,
    > = std::sync::OnceLock::new();
    let application = APPLICATION.get_or_init(|| {
        Arc::new(crate::coordination::RecordingSessionAgentCoordination::default())
    });
    managed
        .install_agent_coordination_application(Arc::downgrade(application))
        .expect("install one test Session application authority");
    managed
}

/// Produce the sole valid ready Local-Environment fixture through the complete
/// frozen projection and provider-effective creation path used in production.
/// C1 the aggregate projection is complete; C2 its immutable model selects the
/// provider; C3 creation carries a current effect fence. E1 the returned V2
/// binding owns the exact spec fingerprint and physical incarnation required by
/// cold adoption. Tests never reconstruct handle payload or provider evidence.
pub(crate) async fn available_local_environment_binding(
    host: &crate::host::SharedHost,
    thread: &str,
    projection: &awaken_session_contract::FrozenSessionProjection,
) -> String {
    host.install_frozen_session_projection(thread, projection.clone(), None, true, None)
        .await
        .expect("install exact available-Environment projection");
    let provider = host
        .projected_session_environment_provider(thread, None)
        .expect("select available-Environment provider from frozen projection");
    let spec = host.sandbox_spec_for_resolved_resources_and_provider(
        thread,
        &projection.resources,
        provider,
    );
    let spec = host
        .validate_session_environment_capabilities(provider, &spec)
        .expect("validate exact available-Environment fixture spec");
    let create_fence = awaken_provisioning_contract::SandboxEffectFence::new(
        format!("fixture-create:{thread}"),
        "runtime-host-test",
        "runtime-host-test",
        1,
        u64::MAX,
    )
    .expect("create available-Environment fixture fence");
    let environment = provider
        .create_effective_for_effect(
            &spec,
            Some(&create_fence),
            None,
            None,
            awaken_sandbox_container::ContainerRealizationIntent::Create,
        )
        .await
        .expect("create typed available-Environment fixture");
    let binding = serde_json::to_string(&environment.handle())
        .expect("serialize typed available-Environment fixture");
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
    projection: &awaken_session_contract::FrozenSessionProjection,
) -> String {
    let binding = available_local_environment_binding(host, thread, projection).await;
    let root = host
        .storage_dir()
        .expect("unavailable Local Environment fixture requires durable storage")
        .join("sandboxes")
        .join(thread);
    let identity = awaken_sandbox_fs::directory_identity_nofollow(&root)
        .expect("observe exact unavailable-Environment root identity");
    awaken_sandbox_fs::remove_directory_tree_exact(&root, identity)
        .expect("remove exact unavailable-Environment physical root");
    binding
}

pub(crate) fn with_empty_session_resources(
    request: awaken_run_ingress::RunDispatch,
    workspace_id: &str,
) -> awaken_run_ingress::RunDispatch {
    let manifest = awaken_session_contract::SessionResourceManifest::new(
        workspace_id,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    request
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from(workspace_id),
        ))
        .with_session_resources(
            awaken_run_ingress::SessionResourceEnvelope::from_manifest(&manifest)
                .expect("serialize exact empty Resource manifest"),
        )
}

pub(super) async fn prepare_deferred_session(
    host: Arc<SharedHost>,
    thread: &str,
) -> crate::ManagedHost {
    let managed = managed_test_host(host.clone());
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        thread,
        empty_frozen_projection(host.local_workspace(), deferred_environment()),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
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
    receipt: awaken_session_contract::SessionEnvironmentReceipt,
) -> awaken_session_contract::SessionEnvironmentState {
    let generation = awaken_session_contract::SandboxGeneration::new(
        &receipt.session_id,
        1,
        u64::MAX,
        "test-environment",
        "test-image",
    );
    awaken_session_contract::SessionEnvironmentState::Resident {
        binding: receipt.binding,
        effect_id: Some(receipt.effect_id),
        generation: Some(generation),
        idle_since_unix_ms: None,
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for ToggleBindingSink {
    async fn authorize(
        &self,
        _intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        let binding = self.binding.lock().unwrap().clone();
        match binding {
            Some(_) if self.fail.swap(false, Ordering::SeqCst) => {
                Err(awaken_session_contract::RunError::unavailable(
                    "injected Session binding failure during readback",
                ))
            }
            Some(binding) => Ok(
                awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                    binding,
                },
            ),
            None => Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized),
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.binding.lock().unwrap() = Some(receipt.binding.clone());
        if self.fail.load(Ordering::SeqCst) {
            Err(awaken_session_contract::RunError::unavailable(
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
}

impl BlockingBindingSink {
    pub(crate) fn blocked() -> Self {
        Self {
            block: std::sync::atomic::AtomicBool::new(true),
            entered: tokio::sync::Notify::new(),
            calls: AtomicUsize::new(0),
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
impl awaken_session_contract::SessionEnvironmentBindingSink for BlockingBindingSink {
    async fn authorize(
        &self,
        _intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        // Cancellation fixture rule: C1 the aggregate authorizes the effect;
        // E1 only the following persist await may be cancelled and retried.
        // Using the contract authorization seam keeps the fixture from
        // reviving the removed persist-without-authorization path.
        Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized)
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.block.load(Ordering::SeqCst) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(committed_environment(receipt))
    }
}

pub(crate) async fn claim(
    store: &awaken_run_ingress::AnyDispatchStore,
    thread: &str,
    run: &str,
    owner: &str,
    now: u64,
) -> awaken_run_ingress::Claimed {
    use awaken_run_ingress::DispatchQueue;
    store
        .enqueue(awaken_run_ingress::RunDispatch::new(test_activation(
            thread, run,
        )))
        .await
        .expect("enqueue deferred run");
    store
        .claim(owner, 1_000, now, &Default::default())
        .await
        .expect("claim deferred run")
        .expect("deferred run available")
}
