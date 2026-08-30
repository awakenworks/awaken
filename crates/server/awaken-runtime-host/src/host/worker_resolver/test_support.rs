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

pub(super) async fn prepare_deferred_session(
    host: Arc<SharedHost>,
    thread: &str,
) -> crate::ManagedHost {
    let managed = crate::ManagedHost::new(host.clone());
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
        .install_test_session_init(
            thread,
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: deferred_environment(),
            },
        )
        .await
        .expect("prepare deferred Session");
    managed
}

pub(super) struct ToggleBindingSink {
    pub(super) fail: std::sync::atomic::AtomicBool,
    pub(super) calls: AtomicUsize,
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
    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(awaken_session_contract::RunError::internal(
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
