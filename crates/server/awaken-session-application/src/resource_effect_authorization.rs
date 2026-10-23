// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! Session-root authorization of Worker Resource effects.
//!
//! One use case: a Worker transport asks whether the current Session root
//! admits one Resource materialization, checkpoint Artifact publication or
//! terminal Memory effect. The component holds only the canonical root reader
//! and the application clock; it owns no state, retry policy or CAS. Each
//! decision is the aggregate's, read fresh from the root on every request.

use std::sync::Arc;

use awaken_session_realization_contract::{
    SessionCheckpointArtifactControl, SessionRealizationControlFailure,
    SessionResourceMaterializationControl, SessionResourceMaterializationIntent,
    SessionResourceMaterializationTarget, SessionTerminalMemoryControl,
};
use awaken_session_state_contract::{
    PersistedSession, SessionCleanupError, SessionEnvironmentOperation,
    SessionEnvironmentReceiptError, SessionMemoryReconciliationError, SessionTerminalMemoryIntent,
    SessionTerminalMemoryTarget,
};
use awaken_session_types::SessionId;
use awaken_time::WallClockExt as _;

use crate::SessionMutations;
use crate::realization::repository_control;

/// A terminal generation may carry an assertion whose original timestamp
/// elapsed, but starting any new Resource/provider effect still requires the
/// one current Session-root lease to be live. The aggregate's domain
/// authorization after this gate proves same-generation identity; this owns
/// only the shared current-clock decision.
pub(crate) fn require_live_root_lease(
    session: &PersistedSession,
    now_ms: u64,
) -> Result<(), SessionRealizationControlFailure> {
    if session.realization.as_ref().is_some_and(|current| {
        awaken_session_realization_contract::realization_lease_is_live_at(
            current.expires_at_unix_ms,
            now_ms,
        )
    }) {
        Ok(())
    } else {
        Err(SessionRealizationControlFailure::StaleOwnership)
    }
}

/// Worker Resource-effect authorization over the canonical Session root.
///
/// The application no longer implements these roles; composition hands each
/// transport this component instead.
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_application::{SessionApplication, SessionResourceEffectAuthorization};
/// use awaken_session_realization_contract::{
///     SessionCheckpointArtifactControl, SessionResourceMaterializationControl,
///     SessionTerminalMemoryControl,
/// };
/// fn selected(app: &SessionApplication) {
///     let authorization: Arc<SessionResourceEffectAuthorization> =
///         app.resource_effect_authorization();
///     let _: Arc<dyn SessionResourceMaterializationControl> = authorization.clone();
///     let _: Arc<dyn SessionCheckpointArtifactControl> = authorization.clone();
///     let _: Arc<dyn SessionTerminalMemoryControl> = authorization;
/// }
/// ```
///
/// ```compile_fail,E0277
/// use awaken_session_application::SessionApplication;
/// use awaken_session_realization_contract::SessionResourceMaterializationControl;
/// fn role<T: SessionResourceMaterializationControl>() {}
/// role::<SessionApplication>();
/// ```
///
/// ```compile_fail,E0277
/// use awaken_session_application::SessionApplication;
/// use awaken_session_realization_contract::SessionCheckpointArtifactControl;
/// fn role<T: SessionCheckpointArtifactControl>() {}
/// role::<SessionApplication>();
/// ```
///
/// ```compile_fail,E0277
/// use awaken_session_application::SessionApplication;
/// use awaken_session_realization_contract::SessionTerminalMemoryControl;
/// fn role<T: SessionTerminalMemoryControl>() {}
/// role::<SessionApplication>();
/// ```
pub struct SessionResourceEffectAuthorization {
    mutations: Arc<SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl SessionResourceEffectAuthorization {
    pub(crate) fn new(
        mutations: Arc<SessionMutations>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self { mutations, clock }
    }

    async fn owner(
        &self,
        session_id: &SessionId,
    ) -> Result<String, SessionRealizationControlFailure> {
        self.mutations
            .repository()
            .owner(session_id)
            .await
            .map_err(repository_control)
    }
}

#[async_trait::async_trait]
impl SessionResourceMaterializationControl for SessionResourceEffectAuthorization {
    async fn authorize_resource_materialization_intent(
        &self,
        intent: &SessionResourceMaterializationIntent,
    ) -> Result<String, SessionRealizationControlFailure> {
        let session = self
            .mutations
            .session_aggregate(intent.session_id())
            .await
            .map_err(repository_control)?;
        if session.is_terminal() {
            return Err(SessionRealizationControlFailure::Terminal);
        }
        // Reuse the Session lease authority: renewal extends one generation
        // and must not invalidate an in-flight immutable Resource read. Exact
        // equality here incorrectly turned every heartbeat into ownership loss.
        if !session.realization.as_ref().is_some_and(|current| {
            awaken_session_realization_contract::realization_lease_authorizes(
                current,
                intent.lease(),
                self.clock.now_ms(),
            )
        }) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        // A retained Run can rebuild the still-active generation while a later
        // replacement waits. Both pairs come from the same Session root; never
        // pair the attempted-revision watermark with unrelated active bytes
        // (including after rollback), or authorize an already-retired target.
        let exact_target = session
            .resources
            .retained_generation(intent.resource_revision())
            .is_some_and(|resources| match intent.target() {
                SessionResourceMaterializationTarget::Input(input) => {
                    resources.inputs().contains(input)
                }
                SessionResourceMaterializationTarget::Skill(binding) => {
                    resources.skills().contains(binding)
                }
            });
        if !exact_target {
            return Err(SessionRealizationControlFailure::Invalid(
                "Session Resource materialization intent does not match the authoritative generation"
                    .into(),
            ));
        }
        self.owner(intent.session_id()).await
    }
}

#[async_trait::async_trait]
impl SessionCheckpointArtifactControl for SessionResourceEffectAuthorization {
    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &SessionId,
        operation: &SessionEnvironmentOperation,
    ) -> Result<String, SessionRealizationControlFailure> {
        let session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_control)?;
        session
            .authorize_checkpoint_release_artifact_effect(operation)
            .map_err(|error| match error {
                SessionEnvironmentReceiptError::Rejected(rejection) => rejection.into(),
                SessionEnvironmentReceiptError::RealizationStale
                | SessionEnvironmentReceiptError::Mismatch => {
                    SessionRealizationControlFailure::StaleOwnership
                }
                error => SessionRealizationControlFailure::Invalid(error.to_string()),
            })?;
        self.owner(session_id).await
    }
}

#[async_trait::async_trait]
impl SessionTerminalMemoryControl for SessionResourceEffectAuthorization {
    async fn authorize_terminal_memory_intent(
        &self,
        intent: &SessionTerminalMemoryIntent,
    ) -> Result<SessionTerminalMemoryTarget, SessionRealizationControlFailure> {
        let session_id = &intent.effect().command.session_id;
        let session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_control)?;
        require_live_root_lease(&session, self.clock.now_ms())?;
        session
            .authorize_terminal_memory_intent(intent)
            .map_err(|error| match error {
                SessionMemoryReconciliationError::Cleanup(
                    SessionCleanupError::RealizationMismatch,
                ) => SessionRealizationControlFailure::StaleOwnership,
                error => SessionRealizationControlFailure::Invalid(error.to_string()),
            })?;
        let workspace_id = self.owner(session_id).await?;
        SessionTerminalMemoryTarget::from_authorized_root(workspace_id, intent.clone())
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))
    }
}
