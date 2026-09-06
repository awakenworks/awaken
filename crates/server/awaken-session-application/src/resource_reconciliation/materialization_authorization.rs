//! Session-root authorization for ordinary Resource materialization.

use awaken_session_contract::{
    SessionRealizationControlFailure, SessionResourceMaterializationIntent,
};

use crate::{SessionApplication, realization::repository_control};

impl SessionApplication {
    pub(crate) async fn authorize_resource_materialization_intent_from_root(
        &self,
        intent: &SessionResourceMaterializationIntent,
    ) -> Result<String, SessionRealizationControlFailure> {
        let session = self
            .session_repository()
            .get(intent.session_id())
            .await
            .map_err(repository_control)?;
        if session.is_terminal() {
            return Err(SessionRealizationControlFailure::Terminal);
        }
        // Reuse the Session lease authority: renewal extends one generation
        // and must not invalidate an in-flight immutable Resource read. Exact
        // equality here incorrectly turned every heartbeat into ownership loss.
        if !session.realization.as_ref().is_some_and(|current| {
            awaken_session_contract::realization_lease_authorizes(
                current,
                intent.lease(),
                crate::activity::now_unix_ms(),
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
                awaken_session_contract::SessionResourceMaterializationTarget::Input(input) => {
                    resources.inputs().contains(input)
                }
                awaken_session_contract::SessionResourceMaterializationTarget::Skill(binding) => {
                    resources.skills().contains(binding)
                }
            });
        if !exact_target {
            return Err(SessionRealizationControlFailure::Invalid(
                "Session Resource materialization intent does not match the authoritative generation"
                    .into(),
            ));
        }
        self.session_repository()
            .owner(intent.session_id())
            .await
            .map_err(repository_control)
    }
}
