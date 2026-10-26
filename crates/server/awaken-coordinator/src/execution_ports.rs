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
//! | Worker claim / settle / commit transport | coordinator dispatch, commit owner, recovery, completion, settlement, stream, file content | the `worker_*`, `coordinator_dispatch` and `commit_owners` ports; the commit-owner adapter lives in the root |
//! | Pool, terminal reconciliation (commit-boundary Workers), drain | `CoordinatorRoles` | `dispatch_lifecycle` ([`ProcessDispatchLifecycle`], dispatch-execution) implemented once by Run delivery's `SessionDispatchLifecycle` |
//! | Child-Run coordination binding, committed-progress wakeup | `SessionApplicationBindings` | bound once by the root before the Coordinator exists |
//! | Worker observation reads | coordinator dispatch, looked up per request | `coordinator_dispatch`, given at construction |
//! | Persistence selection | `DeploymentConfig` | `CoordinatorStorage` (dispatch-execution) |
//! | Scenario executor, Session application and reference authority | `SessionExecutorBuilder`, `EphemeralRuntimeAuthority` | the scenario host and `awaken-cli` roots |

use std::path::PathBuf;
use std::sync::Arc;

use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch::{
    ClaimedOperationCoordinatorResolver, CompletionSink, DispatchSettlementObserver,
};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::{ProcessDispatch, ProcessDispatchLifecycle};
use awaken_resource_contract::FileContentSource;
use awaken_session_application::SessionRunApplication;
use awaken_session_execution_contract::DurableRunOperations;
use awaken_session_realization_contract::ResourceOperationFence;

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
    /// The Session commit owner every claimed commit writes through.
    pub commit_owners: Arc<dyn ClaimedOperationCoordinatorResolver>,
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
