// @formal entrypoint session.runtime_projection_equal_source_and_content_is_a_read_without_cache_write_or_broadcast
// @formal entrypoint session.runtime_projection_stale_writer_never_rolls_back_visible_result
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Disposable per-process projection of one durable Session aggregate.

use std::collections::{HashMap, HashSet};

use super::*;

mod event_page_index;
use event_page_index::EventPageIndex;

pub(super) struct ProjectedToolIndex {
    pub(super) all: HashSet<String>,
    /// Exact public tool-use occurrences already settled by a client-provided
    /// result. The neutral Runtime transcript retains those results for model
    /// continuation, while Managed publishes the corresponding `user.*` event
    /// as their one wire representation.
    pub(super) client_settled_occurrences: HashSet<String>,
    /// Authored custom-tool family of each exact public occurrence.
    pub(super) custom: HashSet<String>,
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
    pub(super) session_revision: awaken_session_types::SessionRevision,
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
    /// The timeline fact behind every issued multiagent event id. Its public
    /// id is a one-way hash of this key, so canonical ordering resolves the
    /// key itself instead of rebuilding every candidate id from the evidence.
    pub(super) fact_keys: HashMap<String, awaken_managed_session_events::EventIdentity>,
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

/// The process-local cache CAS guards the shared monotone admission kernel.
/// The wire rendering is derived from its source coordinates, so a divergent
/// rendering at the same source is a legitimate re-render and applies.
const fn decide_projection_publish_from_facts(
    cache_matches: bool,
    candidate_dominates: bool,
    source_equal: bool,
    projection_changed: bool,
) -> ProjectionPublishDecision {
    use awaken_session_types::{MonotoneAdmission, SourceOrder, monotone_admission};
    if !cache_matches {
        return ProjectionPublishDecision::RetryStaleCache;
    }
    match monotone_admission(
        SourceOrder::of_dominance(candidate_dominates, source_equal),
        !projection_changed,
    ) {
        MonotoneAdmission::Advance | MonotoneAdmission::Divergent => {
            ProjectionPublishDecision::Apply
        }
        MonotoneAdmission::Replay => ProjectionPublishDecision::AlreadyCurrent,
        MonotoneAdmission::Reject => ProjectionPublishDecision::RejectSourceRegression,
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
/// @obligation session.runtime_projection_equal_source_and_content_is_a_read_without_cache_write_or_broadcast
/// @obligation session.runtime_projection_stale_writer_never_rolls_back_visible_result
/// @obligation session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
/// @kani profile=solver-kissat-v1
/// @proof-input-debt free_symbol decide_projection_publish_from_facts cache_matches
/// @proof-input-debt free_symbol decide_projection_publish_from_facts source_equal
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

/// The only state a warm record lends to a committed-prefix rebuild. The
/// consumer is the one candidate builder: its CAS coordinate and local
/// `evt_N` overlays survive, while every durable Event, child, checkpoint and
/// message-consumption decision is rebuilt from committed evidence.
#[derive(Default)]
pub(super) struct ProjectionDeliveryMemo {
    pub(super) cache_revision: u64,
    pub(super) overlay: ManagedLiveOverlay,
    pub(super) transient_events: Vec<Event>,
    pub(super) transient_owners: HashMap<String, String>,
}

/// The root's original optional observations. The v1 Session DTO renders a
/// historical sentinel for missing values; versioned readers use this same
/// candidate's typed source instead of interpreting that sentinel's bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RootTimeEvidence {
    pub(crate) created_at_unix_ms: Option<u64>,
    pub(crate) updated_at_unix_ms: Option<u64>,
    pub(crate) archived_at_unix_ms: Option<u64>,
}

impl From<&PersistedSession> for RootTimeEvidence {
    fn from(root: &PersistedSession) -> Self {
        Self {
            created_at_unix_ms: root.created_at_unix_ms,
            updated_at_unix_ms: root.updated_at_unix_ms,
            archived_at_unix_ms: root.archived_at_unix_ms(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ThreadTimeEvidence {
    pub(crate) created_at_unix_ms: Option<u64>,
    pub(crate) updated_at_unix_ms: Option<u64>,
    pub(crate) archived_at_unix_ms: Option<u64>,
}

#[derive(Clone)]
pub(super) struct SessionRecord {
    /// Process-local compare-and-swap coordinate. It is advanced by every
    /// visible cache mutation and is never persisted or treated as domain truth.
    pub(super) cache_revision: u64,
    /// True only after the sole committed-prefix reducer has completed and its
    /// candidate passed the canonical cache/source CAS. A base record recovered
    /// from the Session aggregate alone is not a complete cross-source prefix
    /// and therefore cannot be served as stale Event/Thread history.
    pub(super) committed_projection_ready: bool,
    pub(super) agent_id: String,
    pub(super) session: Session,
    pub(super) root_time: RootTimeEvidence,
    /// Times from the same canonical committed orders as `events`; absent
    /// means no trustworthy persisted observation for this event identity.
    pub(super) event_times: HashMap<String, u64>,
    pub(super) child_times: HashMap<String, ThreadTimeEvidence>,
    /// Durable source of truth for the runtime's currently applied input projection.
    pub(super) resource_state: awaken_session_state_contract::SessionResourceState,
    /// Rendered projection result. Durable entries advance only through the one
    /// committed-prefix reducer; only `evt_N` entries originate from
    /// [`Self::overlay`].
    pub(super) events: Vec<Event>,
    /// Derived lookup data for this published vector and its visibility owners.
    /// Candidate clones share it cheaply; the sole publisher replaces it after
    /// final ordering and before installing the complete record. It is neither
    /// durable truth nor an input to the projection-content comparison.
    pub(super) event_page_index: Arc<EventPageIndex>,
    /// Source coordinates paired with `events`; no separate child/session state
    /// machine or independently synchronized cursor exists.
    pub(super) checkpoint: ManagedProjectionCheckpoint,
    /// The only process-local state allowed to survive a committed rebuild.
    pub(super) overlay: ManagedLiveOverlay,
    /// What the permission gate decided, per tool call, harvested from the
    /// committed audit trail.
    ///
    /// The transcript shows that a call happened and that a result came back;
    /// it cannot show whether the gate permitted or refused it. Keeping the
    /// decision beside the transcript lets the projection report the recorded
    /// answer instead of inferring one from the shape of the result — an
    /// inference that read a refusal as a permission.
    pub(super) permission_decisions: HashMap<String, crate::types::session::RecordedPermission>,
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
    /// Consume a published record at the candidate boundary without lending
    /// any of its durable projection fields to the next reduction. Exhaustive
    /// destructuring makes a new record field require an explicit authority
    /// decision here before it can enter the rebuild.
    pub(super) fn into_delivery_memo(self) -> ProjectionDeliveryMemo {
        let Self {
            cache_revision,
            committed_projection_ready: _,
            agent_id: _,
            session: _,
            root_time: _,
            event_times: _,
            child_times: _,
            resource_state: _,
            events,
            event_page_index: _,
            checkpoint: _,
            overlay,
            permission_decisions: _,
            event_thread_owners,
            child_threads: _,
            primary_thread_usage: _,
        } = self;
        let transient_events = events
            .into_iter()
            .filter(|event| super::helpers::is_transient_event_id(&event.id))
            .collect::<Vec<_>>();
        let transient_ids = transient_events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<HashSet<_>>();
        let transient_owners = event_thread_owners
            .into_iter()
            .filter(|(id, _)| transient_ids.contains(id.as_str()))
            .collect();
        ProjectionDeliveryMemo {
            cache_revision,
            overlay,
            transient_events,
            transient_owners,
        }
    }

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
            && self.committed_projection_ready == other.committed_projection_ready
            && self.resource_state == other.resource_state
            && self.checkpoint == other.checkpoint
            && self.overlay.anchors == other.overlay.anchors
            && self.event_thread_owners == other.event_thread_owners
            && self.root_time == other.root_time
            && self.event_times == other.event_times
            && self.child_times == other.child_times
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

    pub(super) fn rebuild_event_page_index(&mut self) {
        self.event_page_index = Arc::new(EventPageIndex::new(
            &self.session.id,
            &self.events,
            &self.event_thread_owners,
        ));
    }

    pub(super) fn index_delivery_delta(&mut self, previous: &Self, delta: &[Event]) {
        Arc::make_mut(&mut self.event_page_index).inherit_delivery_changes(
            &previous.event_page_index,
            self.cache_revision,
            delta,
        );
    }

    pub(super) fn delivery_page(
        &self,
        after: (u64, usize),
        limit: usize,
    ) -> Vec<((u64, usize), Event)> {
        self.event_page_index
            .delivery_page(after, limit)
            .into_iter()
            .map(|(cursor, position)| (cursor, self.events[position].clone()))
            .collect()
    }

    /// The public id of one timeline fact of this Session.
    pub(super) fn fact_id(&self, key: &awaken_managed_session_events::EventIdentity) -> String {
        super::events::fact_event_id(&self.session.id, key)
    }

    /// Append one timeline fact as a public Event, recording the fact behind
    /// its id. Returns the id.
    pub(super) fn push_fact(
        &mut self,
        key: awaken_managed_session_events::EventIdentity,
        kind: OutboundKind,
    ) -> String {
        let id = self.fact_id(&key);
        self.checkpoint.progress.fact_keys.insert(id.clone(), key);
        self.events.push(Event {
            id: id.clone(),
            kind,
            processed_at: None,
        });
        id
    }

    /// Append one timeline fact unless its id is already present.
    pub(super) fn append_fact_once(
        &mut self,
        key: awaken_managed_session_events::EventIdentity,
        kind: OutboundKind,
    ) -> bool {
        let id = self.fact_id(&key);
        if self.has_event_id(&id) {
            return false;
        }
        self.checkpoint.progress.fact_keys.insert(id.clone(), key);
        self.append_event_once(Event {
            id,
            kind,
            processed_at: None,
        })
    }

    /// Append one deterministic public Event if its id is not already present.
    /// The published page index is extended only over the candidate's new tail;
    /// it remains derived from `events` and is rebuilt after any reorder/removal.
    pub(super) fn append_event_once(&mut self, event: Event) -> bool {
        if self.has_event_id(&event.id) {
            return false;
        }
        self.events.push(event);
        Arc::make_mut(&mut self.event_page_index).extend(
            &self.session.id,
            &self.events,
            &self.event_thread_owners,
        );
        true
    }

    pub(super) fn has_event_id(&mut self, event_id: &str) -> bool {
        Arc::make_mut(&mut self.event_page_index).extend(
            &self.session.id,
            &self.events,
            &self.event_thread_owners,
        );
        self.event_page_index.positions(event_id).is_some()
    }

    /// Exact positions in the one published canonical event vector.
    pub(super) fn event_positions(&self, id: &str) -> Option<&[usize]> {
        self.event_page_index.positions(id)
    }

    /// Exact tool-use identities named by primary-visible action boundaries.
    /// Computed from the same published event/owner pair used for pagination.
    pub(super) fn primary_answerable_event_ids(&self) -> &HashSet<String> {
        &self.event_page_index.primary_answerable_ids
    }

    /// One owner-aware index over the sole event projection. All tool
    /// classification, MCP correlation, and public-id lookup consume this same
    /// scan so root/child and legacy-qualified ids cannot drift.
    pub(super) fn projected_tool_index_for(
        &self,
        owner_thread_id: Option<&str>,
    ) -> ProjectedToolIndex {
        let mut all = HashSet::new();
        let mut client_settled_occurrences = HashSet::new();
        let mut custom = HashSet::new();
        let mut mcp = Vec::new();
        let mut latest = HashMap::new();
        let mut sources = HashSet::new();
        for event in &self.events {
            // Client replies are admitted on the Session root event log even
            // when their globally qualified tool id targets a child Thread.
            // Index the exact occurrence before applying the tool owner's
            // Thread filter; the qualified id itself prevents cross-Thread or
            // reused-call aliasing.
            match &event.kind {
                OutboundKind::UserCustomToolResult {
                    custom_tool_use_id, ..
                } => {
                    client_settled_occurrences.insert(custom_tool_use_id.clone());
                }
                OutboundKind::UserToolResult { tool_use_id, .. } => {
                    client_settled_occurrences.insert(tool_use_id.clone());
                }
                _ => {}
            }
            if self.event_thread_owners.get(&event.id).map(String::as_str) != owner_thread_id {
                continue;
            }
            let runtime_call_id = crate::project::decode_managed_tool_event_id(&event.id)
                .map_or(event.id.as_str(), |identity| identity.call_id);
            if let Some(identity) = crate::project::decode_managed_tool_event_id(&event.id) {
                sources.insert((identity.source_id.to_string(), identity.call_id.to_string()));
            }
            match &event.kind {
                OutboundKind::AgentToolUse { .. } | OutboundKind::AgentCustomToolUse { .. } => {
                    if matches!(event.kind, OutboundKind::AgentCustomToolUse { .. }) {
                        custom.insert(event.id.clone());
                    }
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
            client_settled_occurrences,
            custom,
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
        session_revision: awaken_session_types::SessionRevision,
        resource_state: awaken_session_state_contract::SessionResourceState,
        root_time: RootTimeEvidence,
        events: Vec<Event>,
    ) -> Self {
        let checkpoint = ManagedProjectionCheckpoint {
            source: ManagedProjectionSourceVersion {
                session_revision,
                ..Default::default()
            },
            ..Default::default()
        };
        let event_page_index = Arc::new(EventPageIndex::new(&session.id, &events, &HashMap::new()));
        Self {
            cache_revision: 0,
            committed_projection_ready: false,
            agent_id,
            session,
            root_time,
            event_times: HashMap::new(),
            child_times: HashMap::new(),
            resource_state,
            events,
            event_page_index,
            checkpoint,
            overlay: Default::default(),
            permission_decisions: HashMap::new(),
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
