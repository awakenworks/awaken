//! WORKER capability of [`SharedHost`]: the claim-fenced transports a
//! database-less Worker installs before it accepts work. A direct or
//! Coordinator-embedded Host leaves every one of them absent.

use crate::host::*;

/// Claim-fenced Worker transports. Consumer: Session realization, coordination,
/// terminal cleanup, Memory/credential materialization, and live-progress relay
/// on a registered Worker.
#[derive(Default)]
pub(crate) struct WorkerTransports {
    /// Outbound claim-fenced Control client used to realize an already-frozen
    /// Session projection on this Worker.
    pub(crate) session_control:
        Option<Arc<dyn awaken_dispatch_contract::ClaimedSessionRealizationControl>>,
    pub(crate) session_coordination_control:
        Option<Arc<dyn awaken_dispatch_contract::ClaimedSessionCoordinationControl>>,
    /// Terminal cleanup is an independent aggregate role. A Worker may expose
    /// claimed coordination without owning cold cleanup; composition installs
    /// this exact role only when the transport supports it.
    pub(crate) terminal_cleanup_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionTerminalCleanupControl>>,
    /// Claim-fenced commit transport a database-less Worker injects beside its
    /// upstream ([`with_worker_claimed_commit`](SharedHost::with_worker_claimed_commit)).
    /// The Host holds only the neutral port; the wire client is composed by the
    /// Worker binary.
    pub(crate) claimed_commit: Option<Arc<dyn awaken_dispatch::ClaimedRunCommit>>,
    /// Optional Resource-transport encoder installed by a remote Worker. The
    /// runtime knows only the neutral Resources contract, never an HTTP wire type.
    pub(crate) memory_reference_encoder: Option<Arc<dyn RuntimeMemoryReferenceEncoder>>,
    /// Sole adapter for opaque credentials owned by this Worker process. Control
    /// and Session code retain only exact non-secret references.
    pub(crate) worker_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    /// Database-less Workers replace the process-local relay with the one
    /// authenticated, claim-fenced Coordinator publisher.
    pub(crate) worker_stream_publisher: Option<Arc<dyn awaken_dispatch::ClaimedStreamPublisher>>,
}

impl SharedHost {
    /// Install the sole outbound claim-fenced client for frozen Session
    /// realization on a Worker.
    #[must_use]
    pub fn with_session_realization_control(
        mut self,
        control: Arc<dyn awaken_dispatch_contract::ClaimedSessionRealizationControl>,
    ) -> Self {
        self.worker.session_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_session_coordination_control(
        mut self,
        control: Arc<dyn awaken_dispatch_contract::ClaimedSessionCoordinationControl>,
    ) -> Self {
        self.worker.session_coordination_control = Some(control);
        self
    }

    /// Install both independent claim-fenced Session roles from the canonical
    /// Worker Control client.
    #[must_use]
    pub fn with_session_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_dispatch_contract::ClaimedSessionRealizationControl
            + awaken_dispatch_contract::ClaimedSessionCoordinationControl
            + 'static,
    {
        self.worker.session_control = Some(control.clone());
        self.worker.session_coordination_control = Some(control);
        self
    }

    /// Install realization resume and terminal cleanup from one recovery
    /// client without claiming that it owns Agent coordination.
    #[must_use]
    pub fn with_session_recovery_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_dispatch_contract::ClaimedSessionRealizationControl
            + awaken_session_realization_contract::SessionTerminalCleanupControl
            + 'static,
    {
        self.worker.session_control = Some(control.clone());
        self.worker.terminal_cleanup_control = Some(control);
        self
    }

    /// Install the independent terminal-cleanup role carried by the same
    /// authenticated Worker Control client in production.
    #[must_use]
    pub fn with_terminal_cleanup_control(
        mut self,
        control: Arc<dyn awaken_session_realization_contract::SessionTerminalCleanupControl>,
    ) -> Self {
        self.worker.terminal_cleanup_control = Some(control);
        self
    }

    /// Install the Worker process's one opaque local-credential adapter.
    #[must_use]
    pub fn with_worker_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>,
    ) -> Self {
        self.worker.worker_credential_resolver = Some(resolver);
        self
    }

    /// Install the claim-fenced commit transport of a database-less Worker.
    /// It is consulted only when the Worker constructor selected the
    /// database-less role; missing transport fails closed at claim execution.
    #[must_use]
    pub fn with_worker_claimed_commit(
        mut self,
        claimed_commit: Arc<dyn awaken_dispatch::ClaimedRunCommit>,
    ) -> Self {
        self.worker.claimed_commit = Some(claimed_commit);
        self
    }

    /// Install the Resource transport's opaque Memory capability encoder.
    /// The execution host does not own or inspect the wire representation.
    #[must_use]
    pub fn with_memory_reference_encoder<E>(mut self, encoder: Arc<E>) -> Self
    where
        E: awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                awaken_dispatch_contract::RunClaim,
            > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                awaken_session_realization_contract::SessionResourceMaterializationIntent,
            > + awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                awaken_session_state_contract::SessionTerminalMemoryIntent,
            > + 'static,
    {
        self.worker.memory_reference_encoder = Some(encoder);
        self
    }

    /// Install the registered Worker's claim-fenced live-progress publisher.
    #[must_use]
    pub fn with_worker_stream_publisher(
        mut self,
        publisher: Arc<dyn awaken_dispatch::ClaimedStreamPublisher>,
    ) -> Self {
        self.worker.worker_stream_publisher = Some(publisher);
        self
    }
}
