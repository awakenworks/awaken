// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Defines required execution roles and selects only the already-resolved inference variant.
// @formal reason: Concrete model and credential construction remains with the supplying composition callbacks.
//! [`ExecutionPorts`] — the Session execution the Coordinator composes.
//!
//! The process composition root that builds the Session executor (a
//! co-located process, a scenario host, a test) selects every port from it
//! before the Coordinator component exists. The Coordinator drives Runs,
//! Worker commits and its dispatch work only through these ports, so its
//! dependency closure holds no executor, realization or Sandbox
//! implementation (ADR-0091 composition boundary).
//!
//! Where each Coordinator path that used executor types now belongs:
//!
//! | Coordinator path | Executor type it used | Decision |
//! |---|---|---|
//! | AI SDK / AG-UI / A2A Runs | `SessionRuns`, `RunCapabilities` thread scope | `session_runs`: the root builds the `SessionRunApplication` and binds it back as background submission |
//! | Durable-ingress verbs | `SessionRuns` | `durable_runs` (session-runtime contract) |
//! | Worker claim / settle / commit transport | coordinator dispatch, commit owner, recovery, completion, settlement, stream, file content | the `worker_*`, `coordinator_dispatch` and `worker_commits` ports; the commit-owner adapter lives in the root |
//! | Pool, terminal reconciliation (commit-boundary Workers), drain | `CoordinatorRoles` | `dispatch_lifecycle` ([`ProcessDispatchLifecycle`], dispatch-execution) implemented once by Run delivery's `SessionDispatchLifecycle` |
//! | Child-Run coordination binding, committed-progress wakeup | `SessionApplicationBinding` (session-execution contract) | bound once by the root before the Coordinator exists |
//! | Worker observation reads | coordinator dispatch, looked up per request | `coordinator_dispatch`, given at construction |
//! | Persistence selection | `DeploymentConfig` | `CoordinatorStorage` (dispatch-execution) |
//! | Scenario executor, Session application and reference authority | `SessionExecutorBuilder`, `ReferenceAuthority` | the scenario host and `awaken-cli` roots |

use awaken_tool_contract::tool::RawTool;
use std::path::PathBuf;
use std::sync::Arc;

use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch::{CompletionSink, DispatchSettlementObserver};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::{ProcessDispatch, ProcessDispatchLifecycle};
use awaken_resource_contract::FileContentSource;
use awaken_session_application::SessionRunApplication;
use awaken_session_execution_contract::DurableRunOperations;
use awaken_session_execution_contract::ResourceOperationFence;

/// The executor's ports the Coordinator consumes, each given once at
/// construction. Consumers: [`crate::build_coordinator_component`] and the
/// scenario mounts.
#[derive(Clone)]
pub struct ExecutionPorts {
    /// The protocol-neutral Run application every wire adapter drives,
    /// admitted through the Session application.
    pub session_runs: Arc<SessionRunApplication>,
    /// The durable-ingress verbs over the executor's Run delivery.
    pub durable_runs: Arc<dyn DurableRunOperations>,
    /// This process's dispatch: topology, preparation placement and the
    /// Worker evidence its terminal reconciliation retains.
    pub dispatch: Arc<ProcessDispatch>,
    /// Starts and drains the dispatch work over the executor's resolver.
    pub dispatch_lifecycle: Arc<dyn ProcessDispatchLifecycle>,
    /// The durable dispatch authority Workers claim and settle through.
    pub coordinator_dispatch: Arc<AnyCoordinatorDispatch>,
    /// The selected claim-fenced writer for authenticated Worker ingress.
    pub worker_commits: Arc<
        dyn awaken_dispatch_contract::ClaimedRunCommit<awaken_runtime_contract::runs::RunError>,
    >,
    /// Committed-run recovery the Worker transport serves.
    pub worker_recovery: Arc<dyn RunRecoverySource>,
    /// Completion of the claimed Runs Workers settle.
    pub worker_completion: Arc<dyn CompletionSink>,
    /// Terminal Memory settlement of the Runs Workers settle.
    pub worker_settlement: Arc<dyn DispatchSettlementObserver>,
    /// Live progress Workers stream.
    pub worker_stream: Arc<dyn StreamSink>,
    /// File content Workers read under their claim fence.
    pub worker_file_content: Arc<dyn FileContentSource<ResourceOperationFence>>,
    /// The durable local root, when this process persists.
    pub storage_dir: Option<PathBuf>,
}

/// The runtime selection already resolved with publication policy. A process
/// with registered Workers uses only its default model label; the co-located
/// launcher constructs executors from the exact same selection.
#[derive(Clone)]
pub enum InferenceSelection {
    PublishedProviders,
    NoModelConfigured,
    #[cfg(any(test, feature = "test-support"))]
    Host {
        executor: Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
        model_ref: String,
    },
}

impl InferenceSelection {
    /// Resolve the selected realization at the enum owner. A dependency's
    /// test-support feature may be unified on without enabling its consumer's
    /// same-named feature; consumers must not repeat this enum's cfg branches.
    /// Host construction stays gated, while every consumer supplies its exact
    /// Host realization without gaining another publication or routing policy.
    pub fn resolve_with<T>(
        self,
        published: impl FnOnce() -> T,
        missing: impl FnOnce() -> T,
        host: impl FnOnce(Arc<dyn awaken_runtime_contract::llm::LlmExecutor>, String) -> T,
    ) -> T {
        #[cfg(not(any(test, feature = "test-support")))]
        drop(host);
        match self {
            Self::PublishedProviders => published(),
            Self::NoModelConfigured => missing(),
            #[cfg(any(test, feature = "test-support"))]
            Self::Host {
                executor,
                model_ref,
            } => host(executor, model_ref),
        }
    }

    #[must_use]
    pub fn default_model_ref(&self) -> &str {
        match self {
            Self::PublishedProviders | Self::NoModelConfigured => {
                awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF
            }
            #[cfg(any(test, feature = "test-support"))]
            Self::Host { model_ref, .. } => model_ref,
        }
    }
}

/// Required selected inputs of the existing co-located execution composition.
/// They are original ports/values, not another Resource or credential owner.
pub struct ColocatedExecutionInputs {
    pub deployment: awaken_deployment_config::DeploymentConfig,
    /// The append-only trace sink actually selected by this process.
    pub trace_file_present: bool,
    pub local_workspace: String,
    pub clock: Arc<dyn awaken_time::WallClock>,
    pub namespace_hand_path: Option<std::path::PathBuf>,
    pub runtime_authority: Arc<dyn awaken_dispatch_erasure::RuntimeAuthority>,
    pub environment_bindings:
        Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    pub model_selection: InferenceSelection,
    pub cloud_models_enabled: bool,
    pub brokered_client:
        Option<Arc<dyn awaken_inference_endpoint::brokered_inference::BrokeredInferenceClient>>,
    pub local_credential_roles: Option<(
        Arc<dyn awaken_credential_contract::CredentialMaterialResolver>,
        Arc<dyn awaken_provisioning_contract::SecretBroker>,
    )>,
    pub external_credentials:
        Option<Arc<dyn awaken_credential_contract::CredentialMaterialResolver>>,
    pub inference_override:
        Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
    pub credential_refresh: Option<Arc<dyn awaken_credential_contract::CredentialRefreshFactory>>,
    pub file_content: Arc<
        dyn awaken_resource_contract::FileContentSource<
                awaken_session_execution_contract::ResourceOperationFence,
            >,
    >,
    pub memory_content: awaken_resource_contract::MemoryExecutionContent,
    pub memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    pub artifact_publisher: Arc<
        dyn awaken_resource_contract::ArtifactPublisher<
                awaken_session_execution_contract::ResourceOperationFence,
            >,
    >,
    pub skill_bundles: Arc<
        dyn awaken_session_configuration_contract::SkillBundleSource<
                awaken_session_execution_contract::ResourceOperationFence,
            >,
    >,
    pub skill_catalog: Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication>,
    pub resource_validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    pub repository_bindings: Arc<
        dyn awaken_resource_contract::RepositoryBindingVerifier<
                awaken_session_execution_contract::ResourceOperationFence,
            >,
    >,
    pub memory_source: awaken_resource_contract::MemoryMountSource,
    pub memory_invalidator: Arc<awaken_resource_contract::LocalInvalidator>,
    pub web_search: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    pub agent_publications: Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    pub capture_sink: Arc<dyn awaken_runtime_contract::CaptureSink>,
    pub capture_policy: awaken_data_governance_contract::CapturePolicySources,
    pub admin_tools: Vec<Arc<dyn RawTool>>,
}

/// Once-created roles followed by the one completion that connects them to the
/// Session application. The launcher owns the captured executor; shared startup
/// sees only the existing role contracts and original process components.
pub struct PreparedColocatedExecution {
    pub roles: awaken_session_execution_contract::SessionExecutionRoleSelection,
    pub dispatch: Arc<awaken_dispatch_execution::ProcessDispatch>,
    pub dispatch_observation: Arc<dyn awaken_dispatch::DispatchOperationalFeed>,
    pub capture_policy: awaken_data_governance_contract::CapturePolicy,
    pub artifact_fence: Arc<crate::ClaimFencedArtifactPublisher>,
    pub finish: Box<
        dyn FnOnce(
                Arc<awaken_session_application::SessionApplication>,
            ) -> Result<ExecutionPorts, String>
            + Send,
    >,
}

/// A move-only construction role supplied by the launcher that co-locates
/// execution. It consumes the selected inputs once, prepares roles, then hands
/// shared startup its completion. No SDK or concrete materializer crosses it.
#[async_trait::async_trait]
pub trait ColocatedExecution: Send {
    async fn prepare(
        self: Box<Self>,
        inputs: ColocatedExecutionInputs,
    ) -> Result<PreparedColocatedExecution, String>;
}

#[cfg(test)]
mod inference_selection_tests {
    use super::InferenceSelection;
    use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result};
    use std::sync::Arc;

    struct NeverInvoked;

    #[async_trait::async_trait]
    impl LlmExecutor for NeverInvoked {
        async fn infer(&self, _: ChatRequest) -> Result<ChatResponse> {
            panic!("selection must not execute the selected model")
        }
    }

    #[test]
    fn inference_selection_resolves_only_the_selected_production_branch() {
        assert_eq!(
            InferenceSelection::PublishedProviders.resolve_with(
                || "published",
                || panic!("must not fall back to missing"),
                |_, _| panic!("must not construct a Host")
            ),
            "published"
        );
        assert_eq!(
            InferenceSelection::NoModelConfigured.resolve_with(
                || panic!("must not select a publication"),
                || "missing",
                |_, _| panic!("must not construct a Host")
            ),
            "missing"
        );
    }

    #[test]
    fn inference_selection_retains_the_exact_host_executor_and_model_ref() {
        let executor: Arc<dyn LlmExecutor> = Arc::new(NeverInvoked);
        let (selected, model_ref) = InferenceSelection::Host {
            executor: executor.clone(),
            model_ref: "frozen-host-model".to_owned(),
        }
        .resolve_with(
            || panic!("must not select a publication"),
            || panic!("must not replace Host with missing"),
            |executor, model_ref| (executor, model_ref),
        );
        assert!(Arc::ptr_eq(&selected, &executor));
        assert_eq!(model_ref, "frozen-host-model");
    }
}
