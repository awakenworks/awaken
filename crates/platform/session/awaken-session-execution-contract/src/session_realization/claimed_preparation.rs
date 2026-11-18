// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This borrowed role preserves original Dispatch exclusion and raw Run coordinates;
// @formal reason: it grants no durable execution authority or replacement of the Session root CAS.
//! Borrowed authority for credential preparation before physical Run admission.
//!
//! The authenticated Dispatch adapter retains the original claim coordinates.
//! SQLite keeps its exact-row guard. PostgreSQL releases each short transaction
//! before credential work and uses the same final Root gate for every current
//! validation and CAS. Each check observes the owner clock at its boundary.

use super::SessionRealizationControlFailure;

#[async_trait::async_trait]
pub trait ClaimedSessionCredentialPreparationContext: Send + Sync {
    fn claim(&self) -> &awaken_worker_contract::RunClaim;
    fn session_id(&self) -> &str;
    fn thread_id(&self) -> &awaken_agent_contract::ThreadId;
    fn activation(&self) -> &awaken_runtime_contract::RunActivation;
    fn execution_scope(&self) -> Option<&awaken_agent_contract::ExecutionScopeRef>;
    fn session_work_authority(
        &self,
    ) -> &awaken_session_application_contract::SessionWorkAuthorityReceipt;

    /// Check a retained exact-row guard for a live claim, no cancellation and
    /// an empty physical-attempt slot. Contexts requiring asynchronous current
    /// observation refuse this synchronous entry point.
    fn verify_at(&self, now_unix_ms: u64) -> Result<(), SessionRealizationControlFailure>;

    /// Revalidate the current owner without retaining a database connection
    /// during cold credential work. Held contexts keep their original check.
    async fn verify_current_at(
        &self,
        now_unix_ms: u64,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.verify_at(now_unix_ms)
    }

    /// Condition consumed by the object engine at the final Root CAS.
    fn publication_condition(&self) -> Option<&awaken_session_types::SessionPublicationCondition> {
        None
    }

    /// Select final admission from the explicitly supplied original Root role.
    /// Held contexts retain that role. PostgreSQL returns its same final gate.
    fn root_commit<'a>(
        &'a self,
        original: &'a dyn crate::SessionRootCommit,
    ) -> &'a dyn crate::SessionRootCommit {
        original
    }

    /// Validate the observed Root through the same selected PostgreSQL gate.
    /// Held-guard contexts retain their original current-row read semantics.
    async fn validate_root_revision(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _observed_revision: awaken_session_types::SessionRevision,
    ) -> Result<bool, crate::SessionMutationError> {
        Ok(true)
    }
}

impl dyn ClaimedSessionCredentialPreparationContext + '_ {
    /// Check the shared frozen coordinates as well as the retained owner's
    /// current exclusion. Session activity and mutation laws remain with the
    /// application command; this check grants no mutation by itself.
    pub fn verify_session_at(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.verify_at(now_unix_ms)?;
        self.require_session_coordinates(session_id)
    }

    fn require_session_coordinates(
        &self,
        session_id: &str,
    ) -> Result<(), SessionRealizationControlFailure> {
        if self.session_id() != session_id
            || self.session_work_authority().session_id().as_str() != session_id
            || self.claim().run_id != self.activation().run_id
            || self.thread_id() != &self.activation().thread_id
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "claimed preparation targets another Session or Run".into(),
            ));
        }
        Ok(())
    }

    pub async fn verify_current_session_at(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.verify_current_at(now_unix_ms).await?;
        self.require_session_coordinates(session_id)
    }

    /// Retain the current observed Root revision for replay and no-change
    /// results. A command's older receipt revision is not this observation.
    pub async fn validate_session_revision(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_revision: awaken_session_types::SessionRevision,
        now_unix_ms: u64,
    ) -> Result<bool, crate::SessionMutationError> {
        self.verify_current_session_at(session_id.as_str(), now_unix_ms)
            .await
            .map_err(crate::SessionMutationError::from)?;
        self.validate_root_revision(session_id, observed_revision)
            .await
    }
}
