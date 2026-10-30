// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Read-only derivation lends each retained Session fact beside its stable revision
// @formal reason: identity and withholds updates whose accepted predecessor has no projection anchor.
// @formal reason: It writes no truth; warm/cold projection tests own ordering and visibility evidence.
//! Pure update facts from the same retained Session root as inbound provenance.
use crate::{
    DerivedSessionEvent, EventIdentity, FactProvenance, ManagedEventType, ProjectionError,
    SessionEventKind,
};
use awaken_session_contract::{
    PersistedSession, SessionUpdateAgentConfiguration, SessionUpdateFact,
};

/// Derive each marker beside the exact immutable fact it projects. The borrowed
/// source remains on this same Root; an adapter never reselects a payload by id.
pub fn derive_session_update_events(
    session: &PersistedSession,
) -> Result<Vec<(DerivedSessionEvent, &SessionUpdateFact)>, ProjectionError> {
    awaken_session_contract::SessionUpdateHistory::validate_for(session)
        .map_err(|error| ProjectionError::corrupt(error.to_string()))?;
    let Some(history) = awaken_session_contract::SessionUpdateHistory::for_session(session) else {
        return Ok(Vec::new());
    };
    let mut events = Vec::with_capacity(history.facts().len());
    for fact in history.facts() {
        if let Some(predecessor) = fact.predecessor_operation_id() {
            let count = usize::try_from(fact.admitted_batch_count())
                .map_err(|_| ProjectionError::corrupt("Session update batch count overflows"))?;
            let entry = session
                .event_batches
                .iter()
                .take(count)
                .flat_map(|batch| &batch.events)
                .find(|entry| entry.event.operation_id() == predecessor)
                .ok_or_else(|| {
                    ProjectionError::corrupt("Session update predecessor is not retained")
                })?;
            if entry.projection_anchor.is_none() {
                continue;
            }
        }
        if matches!(
            fact.agent(),
            Some(SessionUpdateAgentConfiguration::Unrecorded)
        ) {
            return Err(ProjectionError::recovery_required(
                "Session update Agent configuration was not recorded; current catalog cannot reconstruct it",
            ));
        }
        events.push((
            DerivedSessionEvent::new(
                EventIdentity::new(
                    session.session_id.as_str(),
                    ManagedEventType::SessionUpdated,
                    FactProvenance::SessionUpdate {
                        revision: fact.committed_revision().0,
                    },
                ),
                SessionEventKind::SessionUpdated,
            ),
            fact,
        ));
    }
    Ok(events)
}
