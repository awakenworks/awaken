//! Child execution-edge materialization and durable dispatch construction.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::RunDispatch;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use super::AgentRunError;

/// Execution-edge adapters already installed by the host process startup.
///
/// A delegated child selects only through its immutable `backend_ref`; this
/// value carries implementations and credential-realization evidence, never a
/// second target directory or backend-selection rule.
#[derive(Clone, Default)]
pub(crate) struct ChildExecutionAdapters {
    pub(crate) acp: Option<ChildAcpExecutorFactory>,
    pub(crate) remote: Option<Arc<dyn RunAttemptExecutor>>,
    pub(crate) remote_credentials: awaken_runtime_contract::CredentialRealizationCapabilities,
    /// The same host-configured plugin instance shape used by a root Native Run.
    /// The child snapshot still decides whether the plugin is selected.
    pub(crate) web_search: Option<Arc<awaken_ext_builtin_tools::WebSearchPlugin>>,
}

/// Materializes the already-selected ACP execution edge for a child snapshot.
/// Selection stays in the immutable `backend_ref`; the factory only binds that
/// exact backend to the Session-owned sandbox and permission context.
pub(crate) type ChildAcpExecutorFactory = Arc<
    dyn Fn(
            Backend,
            Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
        ) -> Result<Arc<dyn RunAttemptExecutor>, String>
        + Send
        + Sync,
>;

pub(super) fn isolated_child_recovery_projection(
    parent: Option<&Arc<awaken_run_ingress::RecoveryProjection>>,
) -> Option<Arc<awaken_run_ingress::RecoveryProjection>> {
    parent.map(|_| Arc::new(awaken_run_ingress::RecoveryProjection::new()))
}

struct ChildCredentialRecorder {
    ownership: Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
    bindings: Vec<awaken_runtime_contract::AttemptCredentialBinding>,
}

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialRealizationRecorder for ChildCredentialRecorder {
    async fn record(
        &self,
        receipt: awaken_runtime_contract::CredentialRealizationReceipt,
    ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
        self.ownership.verify_current().await.map_err(|error| {
            awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
        })?;
        awaken_runtime_contract::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| {
                awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
            })
    }
}

pub(crate) fn child_attempt_executor(
    runtime: Arc<awaken_runtime::Runtime>,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    adapters: &ChildExecutionAdapters,
    permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
) -> Result<Arc<dyn RunAttemptExecutor>, AgentRunError> {
    let backends = snapshot
        .resolved_spec
        .attempt_candidates(None)
        .into_iter()
        .map(|candidate| Backend::from_ref(&candidate.binding.backend_ref))
        .collect::<Vec<_>>();
    let acp = backends
        .iter()
        .find(|backend| matches!(backend, Backend::Acp(_)))
        .cloned()
        .map(|backend| {
            adapters.acp.as_ref().ok_or_else(|| {
                AgentRunError::Configuration(format!(
                    "delegate publication requires unavailable ACP backend {backend:?}"
                ))
            })?(backend, permission.clone())
            .map_err(AgentRunError::Configuration)
        })
        .transpose()?;
    let remote = if backends
        .iter()
        .any(|backend| matches!(backend, Backend::Remote(_)))
    {
        Some(adapters.remote.clone().ok_or_else(|| {
            AgentRunError::Configuration(
                "delegate publication requires an unavailable remote backend".to_string(),
            )
        })?)
    } else {
        None
    };
    Ok(Arc::new(
        crate::run_exec::SessionAttemptExecutor::from_executors(
            runtime,
            acp,
            remote,
            &[&snapshot.resolved_spec],
        ),
    ))
}

pub(super) fn bind_direct_child_credentials(
    activation: &RunActivation,
    mut context: RuntimeRunContext,
    adapters: &ChildExecutionAdapters,
) -> Result<RuntimeRunContext, AgentRunError> {
    let candidates = activation
        .snapshot
        .resolved_spec
        .attempt_candidates(activation.model_ref_override.as_deref())
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.provisioning,
                awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            )
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(context);
    }
    let ownership = context.ownership.clone().ok_or_else(|| {
        AgentRunError::Configuration(
            "direct delegated remote execution requires parent attempt ownership".to_string(),
        )
    })?;
    let epoch = LOCAL_CHILD_ATTEMPT_EPOCH
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .max(1);
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();
    let holder = awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
        .inference_holder;
    let bindings = awaken_runtime_contract::compile_candidate_credential_bindings(
        &candidates,
        Some(&holder),
        &adapters.remote_credentials,
        epoch,
        now_unix_ms,
    )
    .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    if !bindings.is_empty() {
        context = context.with_credential_realization(
            awaken_runtime_contract::AttemptCredentialRealization::new(
                bindings.clone(),
                Arc::new(ChildCredentialRecorder {
                    ownership,
                    bindings,
                }),
            ),
        );
    }
    Ok(context)
}

static LOCAL_CHILD_ATTEMPT_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

pub(super) fn child_dispatch_request(
    activation: RunActivation,
    parent_thread_id: ThreadId,
    session_resources: Option<awaken_session_contract::SessionResourceManifest>,
    agent_publications: Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> Result<RunDispatch, AgentRunError> {
    let workspace = session_resources
        .as_ref()
        .map_or("default", |resources| resources.workspace_id.as_str());
    let source =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(agent_publications)
            .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let child_publications = awaken_runtime_contract::freeze_delegation_publications(
        &activation.snapshot,
        Some(&source),
        workspace,
    )
    .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let inference_holder = crate::host::self_hosted_inference_holder(&activation)
        .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let placement = crate::host::remote_worker_placement(
        &activation.snapshot.resolved_spec,
        None,
        session_resources.as_ref(),
        false,
    );
    let mut request = RunDispatch::new(activation)
        .for_session(parent_thread_id)
        .with_traceparent(awaken_observability::current_traceparent())
        .with_agent_publications(child_publications)
        .with_placement(placement);
    if let Some(holder) = inference_holder {
        request = request.with_inference_plaintext_holder(holder);
    }
    if let Some(resources) = session_resources {
        let envelope = awaken_run_ingress::SessionResourceEnvelope::from_manifest(&resources)
            .map_err(|error| {
                AgentRunError::Configuration(format!(
                    "serialize child Session resource manifest: {error}"
                ))
            })?;
        request = request
            .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from(resources.workspace_id.clone()),
            ))
            .with_session_resources(envelope);
    }
    Ok(request)
}
