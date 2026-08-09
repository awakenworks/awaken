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

pub(super) fn test_activation(thread: &str, run: &str) -> RunActivation {
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
        sandbox: serde_json::json!({}),
        sandbox_provisioning: awaken_session_contract::SandboxProvisioning::OnToolUse,
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
    use awaken_session_contract::SessionRuntime;
    let managed = crate::ManagedHost::new(host.clone());
    managed
        .prepare_session(
            thread,
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for ToggleBindingSink {
    async fn persist(
        &self,
        _receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(awaken_session_contract::RunError::internal(
                "injected Session binding failure",
            ))
        } else {
            Ok(())
        }
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
