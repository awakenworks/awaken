// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Product credential preparation borrows the original final claim/Work/Root gate without retaining a connection.
//! Current preparation authority over the selected PostgreSQL resume gate.

use super::RootGate;
use awaken_session_application_contract::SessionWorkAuthorityReceipt;
use awaken_session_execution_contract::{
    ClaimedSessionCredentialPreparationContext, SessionRealizationControlFailure, SessionRootCommit,
};

pub(super) struct PostgresClaimedPreparation<'a> {
    gate: &'a RootGate,
    work: &'a SessionWorkAuthorityReceipt,
}

impl<'a> PostgresClaimedPreparation<'a> {
    pub(super) fn new(gate: &'a RootGate, work: &'a SessionWorkAuthorityReceipt) -> Self {
        Self { gate, work }
    }
}

#[async_trait::async_trait]
impl ClaimedSessionCredentialPreparationContext for PostgresClaimedPreparation<'_> {
    fn claim(&self) -> &awaken_worker_contract::RunClaim {
        &self.gate.claim
    }

    fn session_id(&self) -> &str {
        &self.gate.original.session_thread_id().0
    }

    fn thread_id(&self) -> &awaken_agent_contract::ThreadId {
        self.gate.original.thread_id()
    }

    fn activation(&self) -> &awaken_runtime_contract::RunActivation {
        &self.gate.original.activation
    }

    fn execution_scope(&self) -> Option<&awaken_agent_contract::ExecutionScopeRef> {
        self.gate.original.execution_scope.as_ref()
    }

    fn session_work_authority(&self) -> &SessionWorkAuthorityReceipt {
        self.work
    }

    fn verify_at(&self, _: u64) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Unavailable(
            "PostgreSQL preparation requires current asynchronous claim validation".into(),
        ))
    }

    async fn verify_current_at(&self, _: u64) -> Result<(), SessionRealizationControlFailure> {
        self.gate.validate_claim().await.map_err(Into::into)
    }

    fn root_commit<'a>(
        &'a self,
        _original: &'a dyn SessionRootCommit,
    ) -> &'a dyn SessionRootCommit {
        self.gate
    }

    async fn validate_root_revision(
        &self,
        session_id: &awaken_session_types::SessionId,
        revision: awaken_session_types::SessionRevision,
    ) -> Result<bool, awaken_session_execution_contract::SessionMutationError> {
        self.gate.validate(session_id, revision).await
    }
}
