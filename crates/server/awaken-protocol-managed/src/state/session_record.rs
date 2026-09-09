//! Disposable per-process projection of one durable Session aggregate.

use std::collections::{HashMap, HashSet};

use super::*;

pub(super) struct ProjectedToolIndex {
    pub(super) all: HashSet<String>,
    pub(super) mcp: Vec<String>,
    pub(super) latest: HashMap<String, String>,
    /// Exact committed source occurrence of every qualified tool-use Event.
    /// Rebuilt from the append-only Event log; never persisted as another
    /// projection cursor or tool authority.
    pub(super) sources: HashSet<(String, String)>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct ManagedProjectionSourceVersion {
    /// Exact Session root revision paired with the rendered projection. Root
    /// mutations and Runtime Thread commits have independent clocks, so neither
    /// coordinate may stand in for the other.
    pub(super) session_revision: awaken_session_contract::SessionRevision,
    /// Per-Thread optimistic-concurrency coordinates of the committed prefixes
    /// consumed by the reducer. The backend-wide store cursor is deliberately
    /// absent: unrelated Threads must not conflict this Session projection.
    pub(super) root_thread_version: u64,
    pub(super) child_thread_versions: HashMap<String, u64>,
    pub(super) lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor,
    pub(super) budget_reach_generation: u64,
}

impl ManagedProjectionSourceVersion {
    /// Component-wise ordering for the independent Session/Thread/lifecycle
    /// authorities consumed by one projection. A removed or older child prefix
    /// is not comparable and must be rebuilt rather than guessed forward.
    fn dominates(&self, previous: &Self) -> bool {
        self.session_revision >= previous.session_revision
            && self.root_thread_version >= previous.root_thread_version
            && self.lifecycle_cursor >= previous.lifecycle_cursor
            && self.budget_reach_generation >= previous.budget_reach_generation
            && previous
                .child_thread_versions
                .iter()
                .all(|(thread_id, version)| {
                    self.child_thread_versions.get(thread_id) >= Some(version)
                })
    }
}

#[derive(Clone, Default, PartialEq)]
pub(super) struct ManagedProjectionProgress {
    /// Runtime message identities consumed by the one prefix reducer. This is
    /// incremental reducer progress, never a durable source coordinate.
    pub(super) thread_message_ids: HashSet<(String, String)>,
    pub(super) terminal_cursors: HashSet<awaken_agent_contract::RunLifecycleCursor>,
}

#[derive(Clone, Default, PartialEq)]
pub(super) struct ManagedProjectionCheckpoint {
    /// Independently versioned durable facts paired with the rendered result.
    pub(super) source: ManagedProjectionSourceVersion,
    /// Disposable progress used only to reduce the next committed suffix.
    pub(super) progress: ManagedProjectionProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectionPublishDecision {
    Apply,
    AlreadyCurrent,
    RetryStaleCache,
    RejectSourceRegression,
}

const fn decide_projection_publish_from_facts(
    cache_matches: bool,
    candidate_dominates: bool,
    source_equal: bool,
    projection_changed: bool,
) -> ProjectionPublishDecision {
    if !cache_matches {
        ProjectionPublishDecision::RetryStaleCache
    } else if !candidate_dominates {
        ProjectionPublishDecision::RejectSourceRegression
    } else if source_equal && !projection_changed {
        ProjectionPublishDecision::AlreadyCurrent
    } else {
        ProjectionPublishDecision::Apply
    }
}

/// Pure publication kernel shared by production and its exhaustive decision
/// table. Cache CAS is checked first because a concurrent writer may already
/// have published a strictly newer source than this candidate observed.
pub(super) fn decide_projection_publish(
    expected_cache_revision: u64,
    current_cache_revision: u64,
    current: &ManagedProjectionSourceVersion,
    candidate: &ManagedProjectionSourceVersion,
    projection_changed: bool,
) -> ProjectionPublishDecision {
    decide_projection_publish_from_facts(
        expected_cache_revision == current_cache_revision,
        candidate.dominates(current),
        candidate == current,
        projection_changed,
    )
}

#[cfg(kani)]
#[kani::proof]
fn projection_publish_decision_is_total_exact_and_fail_closed() {
    // Decision table causes: C1 cache CAS matches; C2 candidate dominates every
    // source coordinate; C3 source is equal; C4 rendered content changed.
    // Effects: E1 !C1=>retry; E2 C1+!C2=>reject; E3 C1+C2+C3+!C4=>read-only;
    // E4 every remaining C1+C2 row=>apply. Kani covers all 16 combinations;
    // impossible source fact combinations are deliberately fail-closed too.
    let cache_matches: bool = kani::any();
    let candidate_dominates: bool = kani::any();
    let source_equal: bool = kani::any();
    let projection_changed: bool = kani::any();
    let actual = decide_projection_publish_from_facts(
        cache_matches,
        candidate_dominates,
        source_equal,
        projection_changed,
    );
    let expected = if !cache_matches {
        ProjectionPublishDecision::RetryStaleCache
    } else if !candidate_dominates {
        ProjectionPublishDecision::RejectSourceRegression
    } else if source_equal && !projection_changed {
        ProjectionPublishDecision::AlreadyCurrent
    } else {
        ProjectionPublishDecision::Apply
    };
    assert_eq!(actual, expected);
    assert_eq!(
        actual == ProjectionPublishDecision::AlreadyCurrent,
        cache_matches && candidate_dominates && source_equal && !projection_changed
    );
    assert!(!cache_matches || actual != ProjectionPublishDecision::RetryStaleCache);
}

#[derive(Clone, Default)]
pub(super) struct ManagedLiveOverlay {
    /// Process-local Session update events waiting for the durable command that
    /// preceded their root CAS to acquire its immutable projection anchor.
    pub(super) pending_events: Vec<(Event, String)>,
    /// Disposable predecessor hints for visible `evt_N` overlays.
    pub(super) anchors: HashMap<String, String>,
}

#[derive(Clone)]
pub(super) struct SessionRecord {
    /// Process-local compare-and-swap coordinate. It is advanced by every
    /// visible cache mutation and is never persisted or treated as domain truth.
    pub(super) cache_revision: u64,
    pub(super) agent_id: String,
    pub(super) session: Session,
    /// Durable source of truth for the runtime's currently applied input projection.
    pub(super) resource_state: awaken_session_contract::SessionResourceState,
    /// Rendered projection result. Durable entries advance only through the one
    /// committed-prefix reducer; only `evt_N` entries originate from
    /// [`Self::overlay`].
    pub(super) events: Vec<Event>,
    /// Source coordinates paired with `events`; no separate child/session state
    /// machine or independently synchronized cursor exists.
    pub(super) checkpoint: ManagedProjectionCheckpoint,
    /// The only process-local state allowed to survive a committed rebuild.
    pub(super) overlay: ManagedLiveOverlay,
    /// The optional root Run owner and reason whose aggregate idle event is
    /// waiting for coordinated Thread settlement. The owner lets a later terminal
    /// of that same Run supersede an earlier Awaiting reason without allowing an
    /// unrelated terminal to overwrite it. This is disposable event-projector
    /// continuation state, never Session lifecycle authority.
    pub(super) deferred_session_stop_reason:
        Option<(Option<awaken_agent_contract::agent::run::Id>, StopReason)>,
    /// Disposable visibility owner for transcript-derived events. Most entries
    /// name the child whose local stream owns the event; advisor advice names the
    /// primary because the official wire excludes that delivery from the
    /// advisor Thread stream. The event itself remains in the one `events` log.
    pub(super) event_thread_owners: HashMap<String, String>,
    /// Subagent child threads spawned in this Session. Each is announced by a
    /// `session.thread_created` event and remains a projection of durable truth.
    pub(super) child_threads: Vec<SessionThread>,
    /// Cumulative usage for the primary logical Thread, read from the same
    /// parent Session commit partition as child usage. This is a disposable wire
    /// projection; committed ThreadUsage remains the sole accounting authority.
    pub(super) primary_thread_usage: Option<crate::types::SessionThreadUsage>,
}

impl SessionRecord {
    /// Compare every projection and continuation field while deliberately
    /// excluding the process-local CAS token. Equal durable coordinates are not
    /// sufficient: legacy and auxiliary committed sources may lack an
    /// independent monotonic coordinate, but a changed rendered candidate must
    /// still publish.
    pub(super) fn same_projected_content(&self, other: &Self) -> Result<bool, StateError> {
        // These protocol DTOs intentionally do not define domain equality. Their
        // serialized wire value is exactly what Managed publishes, so compare
        // that value instead of spreading PartialEq through the public schema.
        let self_wire = serde_json::to_value((
            &self.session,
            &self.events,
            &self.overlay.pending_events,
            &self.child_threads,
            &self.primary_thread_usage,
        ))
        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        let other_wire = serde_json::to_value((
            &other.session,
            &other.events,
            &other.overlay.pending_events,
            &other.child_threads,
            &other.primary_thread_usage,
        ))
        .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        Ok(self.agent_id == other.agent_id
            && self.resource_state == other.resource_state
            && self.checkpoint == other.checkpoint
            && self.overlay.anchors == other.overlay.anchors
            && self.deferred_session_stop_reason == other.deferred_session_stop_reason
            && self.event_thread_owners == other.event_thread_owners
            && self_wire == other_wire)
    }

    pub(super) fn advance_cache_revision(&mut self) -> Result<(), StateError> {
        self.cache_revision = self.cache_revision.checked_add(1).ok_or_else(|| {
            StateError::Run(RunError::internal(
                "Managed projection cache revision exhausted",
            ))
        })?;
        Ok(())
    }

    /// Derive the exact tool-use Events named by a primary-visible
    /// `requires_action` boundary. The append-only Event vector remains the
    /// only projection truth; this set exists only for one list/live filtering
    /// pass and is never persisted as a parallel index.
    pub(super) fn primary_answerable_event_ids(&self) -> HashSet<&str> {
        self.events
            .iter()
            .filter(|event| {
                self.event_thread_owners
                    .get(&event.id)
                    .is_none_or(|owner| owner == &self.session.id)
            })
            .filter_map(|event| match &event.kind {
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                }
                | OutboundKind::SessionThreadStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                    ..
                } => Some(event_ids.as_slice()),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// One owner-aware index over the sole event projection. All tool
    /// classification, MCP correlation, and public-id lookup consume this same
    /// scan so root/child and legacy-qualified ids cannot drift.
    pub(super) fn projected_tool_index_for(
        &self,
        owner_thread_id: Option<&str>,
    ) -> ProjectedToolIndex {
        let mut all = HashSet::new();
        let mut mcp = Vec::new();
        let mut latest = HashMap::new();
        let mut sources = HashSet::new();
        for event in &self.events {
            if self.event_thread_owners.get(&event.id).map(String::as_str) != owner_thread_id {
                continue;
            }
            let runtime_call_id = crate::project::decode_managed_tool_event_id(&event.id)
                .map_or(event.id.as_str(), |identity| identity.call_id);
            if let Some(identity) = crate::project::decode_managed_tool_event_id(&event.id) {
                sources.insert((identity.source_id.to_string(), identity.call_id.to_string()));
            }
            match event.kind {
                OutboundKind::AgentToolUse { .. } | OutboundKind::AgentCustomToolUse { .. } => {
                    all.insert(runtime_call_id.to_string());
                    latest.insert(runtime_call_id.to_string(), event.id.clone());
                }
                OutboundKind::AgentMcpToolUse { .. } => {
                    all.insert(runtime_call_id.to_string());
                    mcp.push(runtime_call_id.to_string());
                    latest.insert(runtime_call_id.to_string(), event.id.clone());
                }
                _ => {}
            }
        }
        ProjectedToolIndex {
            all,
            mcp,
            latest,
            sources,
        }
    }

    pub(super) fn message_was_projected(&self, thread_id: &str, message_id: &str) -> bool {
        self.checkpoint
            .progress
            .thread_message_ids
            .contains(&(thread_id.to_string(), message_id.to_string()))
    }

    pub(super) fn consume_message(&mut self, thread_id: &str, message_id: &str) -> bool {
        self.checkpoint
            .progress
            .thread_message_ids
            .insert((thread_id.to_string(), message_id.to_string()))
    }

    pub(super) fn new(
        agent_id: String,
        session: Session,
        session_revision: awaken_session_contract::SessionRevision,
        resource_state: awaken_session_contract::SessionResourceState,
        events: Vec<Event>,
    ) -> Self {
        let checkpoint = ManagedProjectionCheckpoint {
            source: ManagedProjectionSourceVersion {
                session_revision,
                ..Default::default()
            },
            ..Default::default()
        };
        Self {
            cache_revision: 0,
            agent_id,
            session,
            resource_state,
            events,
            checkpoint,
            overlay: Default::default(),
            deferred_session_stop_reason: None,
            event_thread_owners: Default::default(),
            child_threads: Vec::new(),
            primary_thread_usage: None,
        }
    }

    /// Project the HTTP Session DTO from typed aggregate state. The stored
    /// `Session` keeps `resources` empty so JSON cannot become another index.
    pub(super) fn session_projection(&self) -> Session {
        let mut session = self.session.clone();
        session.resources = self
            .resource_state
            .desired()
            .inputs()
            .iter()
            .map(|input| resolved_resource_dto(&session.id, input))
            .collect();
        session
    }
}

#[cfg(test)]
mod tests;
