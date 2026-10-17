// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Consumer-owned execution seams carry existing Session projections and Run factories;
// @formal reason: they do not mint leases, claims, desired state, or committed truth.
//! Existing Run assembly consumed by the Session dispatch resolver. Each role
//! has one caller: claim routing, Run construction, projection effects, or
//! terminal recovery. Implementations retain their original authority owners.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore, DispatchBinding};
use awaken_dispatch_contract::{Claimed, RunDispatch};
use awaken_runtime_contract::{
    ExecutableAgentSnapshot, RuntimeRunContext, StaticPublishedAgentSnapshots,
};
use awaken_session_configuration_contract::SessionRuntimeProjection;
use awaken_session_realization_contract::ClaimedSessionRealizationControl;

use crate::session_reconciliation::TerminalReconciliationState;
use crate::{DispatchWorker, Error};

/// Immutable process selection and views of the original Session registry.
#[async_trait::async_trait]
pub trait SessionDispatch: Send + Sync + 'static {
    async fn drive_claimed_projection(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        session: &awaken_session_types::SessionId,
        directive: awaken_session_realization_contract::SessionRealizationDirective,
        claimed: &Claimed,
        started: awaken_lease_contract::authority::AuthorityProofStarted,
        permit: &awaken_session_realization_contract::SessionRealizationPermit,
    ) -> Result<(), Error>;
    fn install_runtime_projection(
        &self,
        thread: &str,
        projection: SessionRuntimeProjection,
    ) -> Result<(), crate::AdmissionError>;
    fn dispatch_binding(&self) -> Option<DispatchBinding>;
    fn runs_local_dispatch_pool(&self) -> bool;
    fn credential_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities;
    fn session_control(&self) -> Option<&dyn ClaimedSessionRealizationControl>;
    fn terminal_reconciliation(&self) -> &TerminalReconciliationState;
    fn dispatch_owner(&self) -> &str;
    fn run_mcp_attachments(&self) -> &dyn RunMcpAttachments;
}

/// The ordinary Run compatibility path uses SDK Direct staging. A Managed
/// Session reaches the distinct assigned-root role through projection execution.
pub trait RunMcpAttachments: awaken_session_execution_contract::McpAttachmentRealizer {
    fn active_stages(
        &self,
        session: &str,
    ) -> Result<
        Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment>,
        awaken_session_types::RunError,
    >;
}

/// Constructs an executable Run or an Environment-free commit-boundary Worker.
/// Dispatch selects which operation applies; the Run owner assembles executors.
#[async_trait::async_trait]
pub trait RunWorkerFactory: Send + Sync {
    type Commit: RunRecoverySource + Send + Sync + 'static;

    fn terminal_view<'a>(
        &self,
        commit: &'a Self::Commit,
    ) -> &'a dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    async fn open_commit(&self, thread: &ThreadId) -> Result<Arc<Self::Commit>, Error>;
    async fn root_worker(
        &self,
        thread: &ThreadId,
        agent: Option<&str>,
        publication: Option<ExecutableAgentSnapshot>,
        dispatch: Option<&RunDispatch>,
        publications: Option<Arc<StaticPublishedAgentSnapshots>>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error>;
    async fn child_worker(
        &self,
        claimed: &Claimed,
        session_thread: &ThreadId,
        publications: Arc<StaticPublishedAgentSnapshots>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error>;
    async fn boundary_worker(
        &self,
        claimed: &Claimed,
        commit: Arc<Self::Commit>,
        context: Option<RuntimeRunContext>,
        install_cancellation_executor: bool,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error>;
    fn ensure_deferred_executor(&self, thread: &str);
}

/// Discovery remains restricted to the existing Coordinator authority.
pub trait TerminalRecovery: Send + Sync {
    fn scans_committed_terminals(&self) -> bool;
    fn coordinator_dispatch(&self) -> Option<Arc<AnyCoordinatorDispatch>>;
    fn worker_recovery_source(self: &Arc<Self>) -> Arc<dyn RunRecoverySource>;
}

/// Preserve the dispatch owner's existing error vocabulary at assembly seams.
pub fn execution_error(message: impl Into<String>) -> Error {
    Error::Execution(awaken_runtime_contract::execution::Error::Execution(
        message.into(),
    ))
}

pub fn terminal_resolution_error(message: impl Into<String>) -> Error {
    Error::TerminalResolution(awaken_runtime_contract::execution::Error::Execution(
        message.into(),
    ))
}
