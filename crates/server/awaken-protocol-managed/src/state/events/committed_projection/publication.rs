//! Atomic publication of one fully reduced Managed projection candidate.

use super::*;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use std::collections::{HashMap, HashSet};

pub(super) fn stamp_source(
    candidate: &mut SessionRecord,
    session_revision: awaken_session_contract::SessionRevision,
    root: Option<&RunRecoverySnapshot>,
    children: &HashMap<String, RunRecoverySnapshot>,
) {
    candidate.checkpoint.source.session_revision = session_revision;
    candidate.checkpoint.source.root_thread_version = root.map_or(0, |value| value.thread_version);
    candidate.checkpoint.source.child_thread_versions = children
        .iter()
        .map(|(thread_id, snapshot)| (thread_id.clone(), snapshot.thread_version))
        .collect();
}

impl ManagedState {
    pub(super) fn publish_projection_candidate(
        &self,
        session_id: &str,
        base_cache_revision: u64,
        mut candidate: SessionRecord,
        previous_event_ids: &HashSet<String>,
    ) -> Result<bool, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let current = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let projection_changed = !current.same_projected_content(&candidate)?;
        match decide_projection_publish(
            base_cache_revision,
            current.cache_revision,
            &current.checkpoint.source,
            &candidate.checkpoint.source,
            projection_changed,
        ) {
            ProjectionPublishDecision::Apply => {}
            ProjectionPublishDecision::AlreadyCurrent => return Ok(true),
            ProjectionPublishDecision::RetryStaleCache
            | ProjectionPublishDecision::RejectSourceRegression => return Ok(false),
        }
        candidate.advance_cache_revision()?;
        *current = candidate;
        self.broadcast_new_event_ids(session_id, current, previous_event_ids);
        Ok(true)
    }
}
