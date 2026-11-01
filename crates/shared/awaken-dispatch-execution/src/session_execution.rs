// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Consumer-owned execution seams carry existing Session projections and Run factories;
// @formal reason: they do not mint leases, claims, desired state, or committed truth.
//! The roles claimed-Run resolution consumes, each given at construction.
//!
//! One trait per caller role: the installed Session as the claimed resolve
//! reads and changes it ([`ClaimedSessionInstallation`]), the preparation of
//! the Session's executable surface ([`ClaimedRunPreparation`]), the
//! construction of executable Runs ([`ClaimedRunWorkers`]) and of
//! Environment-free commit-boundary Workers ([`CommitBoundaryWorkers`]). The
//! implementations retain their original authority owners; dispatch decides
//! and never writes a field of an installed Session.

use std::any::Any;
use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_contract::{Claimed, RunClaim, RunDispatch};
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_runtime_contract::{
    ExecutableAgentSnapshot, RuntimeRunContext, StaticPublishedAgentSnapshots,
};
use awaken_session_configuration_contract::SessionRuntimeProjection;
use awaken_session_configuration_contract::resource::{
    SessionRealizationLease, SessionResourceManifest,
};
use awaken_session_execution_contract::SessionRealizationPermit;

use crate::{AdmissionError, DispatchWorker, Error};

/// Identity of one installed Session allocation. Dispatch only carries it from
/// an observation to the bind that requires the same installation.
#[derive(Clone)]
pub struct SessionInstallation(Arc<dyn Any + Send + Sync>);

impl SessionInstallation {
    #[must_use]
    pub fn new(identity: Arc<dyn Any + Send + Sync>) -> Self {
        Self(identity)
    }

    /// The identity the installation's owner minted.
    #[must_use]
    pub fn identity(&self) -> Arc<dyn Any + Send + Sync> {
        self.0.clone()
    }
}

/// What a claimed resolve observed of the installed Session projection.
pub struct ObservedProjection {
    /// The Session application published its dispatch marker.
    pub session_dispatch: bool,
    /// The Control-frozen baseline is installed.
    pub has_frozen_baseline: bool,
    /// The installation observed.
    pub installation: SessionInstallation,
}

/// The installed Session as claimed-Run resolution reads and changes it,
/// through the realization's named operations.
#[async_trait::async_trait]
pub trait ClaimedSessionInstallation: Send + Sync {
    /// The Workspace that resolves publications when the claim carries none.
    fn local_workspace(&self) -> &str;
    /// Bind the execution Workspace; false, changing nothing, when another is bound.
    fn bind_execution_workspace(
        &self,
        session: &str,
        workspace: &str,
    ) -> Result<bool, AdmissionError>;
    /// Acquire the realization permit and the lease a retry of the same claim
    /// continues; an unavailable installed-Session registry is refused.
    async fn acquire_claimed_realization(
        &self,
        session: &str,
        claim: &RunClaim,
    ) -> Result<(SessionRealizationPermit, Option<SessionRealizationLease>), AdmissionError>;
    /// Stage the Resource manifest a claim carries.
    async fn install_dispatched_resources(
        &self,
        session: &str,
        manifest: &SessionResourceManifest,
        claim: &RunClaim,
    ) -> Result<(), awaken_session_types::RunError>;
    /// Observe the installed projection; an absent Session is `None`.
    fn observe_projection(
        &self,
        session: &str,
    ) -> Result<Option<ObservedProjection>, AdmissionError>;
    /// Bind `claim` only while `observed` is still the installation and, when
    /// required, its frozen projection is still complete.
    fn bind_claim_if_projection_unchanged(
        &self,
        session: &str,
        observed: &SessionInstallation,
        require_installed_projection: bool,
        claim: RunClaim,
    ) -> Result<bool, AdmissionError>;
    /// The durable Sandbox binding of the Session's Environment owner.
    fn durable_environment_binding(&self, session: &str) -> Result<Option<String>, AdmissionError>;
    /// Pin the Agent, backend and Workspace a compatibility dispatch delivered.
    fn pin_dispatched_identity(
        &self,
        session: &str,
        agent_id: &str,
        backend_ref: &str,
        workspace: Option<&str>,
    ) -> Result<(), AdmissionError>;
    /// Adopt the Sandbox a claim carries, failing with the claim outcome its
    /// realization disposition names.
    async fn adopt_bound_environment(
        &self,
        session: &str,
        encoded: Option<&str>,
        run_id: &RunId,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
        recovery: awaken_dispatch_contract::WorkerRecoveryMode,
    ) -> Result<(), Error>;
    /// The immutable Agent publication retained by the Session.
    fn published_snapshot(
        &self,
        session: &str,
    ) -> Result<Option<ExecutableAgentSnapshot>, AdmissionError>;
}

/// Preparation of the Session's executable surface for a claimed Run.
#[async_trait::async_trait]
pub trait ClaimedRunPreparation: Send + Sync {
    /// Install the frozen runtime projection a claim carries.
    fn install_runtime_projection(
        &self,
        session: &str,
        projection: SessionRuntimeProjection,
    ) -> Result<(), AdmissionError>;
    /// Drive the canonical realization of a Control-resumed projection.
    async fn drive_claimed_projection(
        &self,
        control: &dyn awaken_session_execution_contract::SessionRealizationControl,
        session: &awaken_session_types::SessionId,
        directive: awaken_session_execution_contract::SessionRealizationDirective,
        claimed: &Claimed,
        started: awaken_lease_contract::authority::AuthorityProofStarted,
        permit: &SessionRealizationPermit,
    ) -> Result<(), Error>;
    /// Ensure the physical Session substrate exists before sandbox-stdio staging.
    async fn ensure_session_substrate(
        &self,
        session: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), AdmissionError>;
    /// Bind the deferred Sandbox executor an on-tool-use Environment needs.
    fn bind_deferred_executor(&self, session: &str) -> Result<(), AdmissionError>;
}

/// Constructs executable Runs on the Worker that claimed them.
#[async_trait::async_trait]
pub trait ClaimedRunWorkers: Send + Sync {
    /// Process-local credential realization evidence for local claims.
    fn credential_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities;
    /// Construct cancellation at the claim's original commit boundary without
    /// realizing a Session. Only the claiming Worker owns a remote attempt executor.
    async fn cancellation_worker(
        &self,
        claimed: &Claimed,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error>;
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
}

/// Constructs Environment-free Workers bound to a Thread's commit boundary:
/// admission repair, failure settlement and terminal recovery. Remote cancellation
/// belongs to the claiming Worker's [`ClaimedRunWorkers`] role.
#[async_trait::async_trait]
pub trait CommitBoundaryWorkers: Send + Sync {
    async fn open_commit(&self, thread: &ThreadId) -> Result<Box<dyn ThreadCommitBoundary>, Error>;
    /// The recovery reader of physical attempts, held only during one candidate.
    fn recovery_source(&self) -> Result<Arc<dyn RunRecoverySource>, Error>;
}

/// One opened Thread commit boundary. It holds no cache, mutation, or new
/// authority; it lives for one resolve or terminal candidate.
#[async_trait::async_trait]
pub trait ThreadCommitBoundary: Send + Sync {
    fn committed(&self) -> &dyn CommittedThreadView;
    /// This recovery role cannot request a remote cancellation executor.
    ///
    /// Cause/effect rule: a Coordinator commit boundary plus an execution-capability
    /// selector is rejected by the type system; only `ClaimedRunWorkers` admits it.
    /// ```compile_fail,E0061
    /// use awaken_dispatch_execution::session_execution::ThreadCommitBoundary;
    /// async fn install_cancellation(
    ///     boundary: &dyn ThreadCommitBoundary,
    ///     claimed: &awaken_dispatch_contract::Claimed,
    /// ) {
    ///     let _ = boundary.boundary_worker(claimed, None, true).await;
    /// }
    /// ```
    async fn boundary_worker(
        &self,
        claimed: &Claimed,
        context: Option<RuntimeRunContext>,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error>;
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
