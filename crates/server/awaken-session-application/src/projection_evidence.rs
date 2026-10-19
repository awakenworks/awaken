// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Read-only evidence gathering moved from the Managed committed projector behind the
// @formal reason: SessionProjectionSource port: a root prefix before retained terminal Outcome queries,
// @formal reason: that root lent to coordinated-link projection, child prefixes, then the ordered
// @formal reason: merge of their lifecycle (the merge itself lives beside the port in the runtime contract). It
// @formal reason: publishes nothing; the Managed projector still re-reads and compares the same
// @formal reason: prefixes before its CAS publication. Warm/cold, read-order and prefix-race projection
// @formal reason: tests cover behavior; reducer TCB linked to the paired-publication obligation.
//! Read-only committed evidence for Session projections.
//!
//! The Managed adapter used to gather this itself, straight from the Runtime.
//! It now reads through the Session application, which in turn sees only the
//! [`awaken_session_execution_contract::SessionProjectionSource`] port: committed Thread prefixes, coordinated
//! links and terminal Outcomes, never an execution context or Environment.

use crate::agent_configuration::{
    RehydrationPublicationDecision, agent_configuration_from_profile,
    rehydration_publication_decision, resolve_session_multiagent,
};

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_session_event_contract::{SessionEventBatch, SessionEventCommand};
use awaken_session_execution_contract::{
    CommittedSessionFacts, CoordinatedThreadLink, ProjectionThreadSnapshots,
    SessionProjectionReadTimings, committed_lifecycle_prefix, outcome_terminal_commit_cursor,
};
use awaken_session_state_contract::PersistedSession;
use awaken_session_types::RunError;

use crate::{
    RecoveredSessionProjection, SessionMutationError, SessionProjectionReads,
    SessionProjectionRecoveryError, SessionQueries, SessionRootReads, session_repository_error,
};
use awaken_session_configuration_contract::SessionBaselineState;
use awaken_session_state_contract::SessionRepositoryError;

#[async_trait::async_trait]
impl SessionProjectionReads for SessionQueries {
    fn recover_agent_configuration<'a>(
        &self,
        id: &str,
        owner_scope: &str,
        persisted: &'a PersistedSession,
        purpose: crate::SessionRecoveryPurpose,
    ) -> Result<
        (
            &'a awaken_session_configuration_contract::SessionBaseline,
            awaken_session_configuration_contract::SessionAgentConfiguration,
        ),
        RunError,
    > {
        let baseline = persisted.frozen_baseline().ok_or_else(|| {
            RunError::unavailable("Session root is not finalized and cannot be rehydrated")
        })?;

        if let Some(configuration) =
            awaken_session_state_contract::SessionUpdateHistory::agent_configuration(persisted)
        {
            return Ok((baseline, configuration));
        }
        let caps = self.capabilities_for(id)?;
        let profile =
            self.selected_session_profile(owner_scope, &baseline.agent_id, baseline.agent_revision);
        let decision = rehydration_publication_decision(
            purpose,
            baseline.agent_revision.is_some(),
            profile.is_some(),
        );
        if decision == RehydrationPublicationDecision::RejectMissingExact {
            let revision = baseline
                .agent_revision
                .expect("missing exact decision requires a revision");
            return Err(RunError::unavailable(format!(
                "Agent `{}` publication revision {revision} is unavailable during Session recovery",
                baseline.agent_id
            )));
        }
        let multiagent = match decision {
            RehydrationPublicationDecision::NonInteractiveFrozenProjection => None,
            RehydrationPublicationDecision::Available
            | RehydrationPublicationDecision::Unpinned => {
                resolve_session_multiagent(profile.as_ref(), &caps, None, |agent_id, revision| {
                    self.selected_session_profile(owner_scope, agent_id, revision)
                })?
            }
            RehydrationPublicationDecision::RejectMissingExact => {
                unreachable!("missing exact publication was rejected before projection")
            }
        };
        let mut configuration = agent_configuration_from_profile(
            &baseline.agent_id,
            profile.clone().unwrap_or_default(),
        );
        configuration.version = profile
            .as_ref()
            .map(|value| value.source_revision)
            .or(baseline.agent_revision)
            .unwrap_or(1)
            .max(1);
        configuration.model = awaken_session_configuration_contract::SessionModelChoice {
            model_id: baseline.model.clone(),
            inference: baseline
                .model_override
                .as_ref()
                .map(|value| value.inference.clone())
                .or_else(|| profile.as_ref().map(|value| value.inference.clone()))
                .unwrap_or_default(),
            source: awaken_session_configuration_contract::SessionModelSource::Publication,
        };
        configuration.system = baseline
            .system_prompt
            .resolve(profile.as_ref().and_then(|value| value.system.clone()));
        configuration.tools = persisted.tools.clone();
        configuration.mcp_servers = persisted.configured_mcp_servers();
        configuration.skills = persisted
            .resources
            .desired()
            .skills()
            .iter()
            .map(|value| awaken_agent_contract::AgentSkillBinding {
                kind: value.kind,
                skill_id: value.skill_id.clone(),
                version: value.version.to_string(),
            })
            .collect();
        configuration.multiagent = multiagent;
        Ok((baseline, configuration))
    }

    /// Read one logical Thread prefix through the explicit parent Session
    /// partition, optionally selecting an original Run. A foreign Thread or
    /// selected Run is a Runtime fault, never a fallback to the latest Run.
    async fn projection_thread_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        selected_run: Option<&awaken_agent_contract::RunId>,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        self.projection
            .as_ref()
            .projection_thread_snapshot(session_id, thread_id, selected_run)
            .await
    }

    /// Collect one deterministic coordinated-Thread prefix. Callers compare two
    /// collections around projection reads so independent child commits cannot
    /// form a mixed public prefix. Each link's latest Run is the one its own
    /// snapshot committed.
    async fn projection_coordinated_prefix(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&RunRecoverySnapshot>,
    ) -> Result<(Vec<CoordinatedThreadLink>, ProjectionThreadSnapshots), RunError> {
        let mut links = self
            .projection
            .as_ref()
            .projection_coordinated_threads(session_id, observed_root)
            .await?;
        let mut snapshots = ProjectionThreadSnapshots::new();
        for link in &links {
            // A link may commit before its child has a first Run.
            if let Some(snapshot) = self
                .projection_thread_snapshot(session_id, &link.thread_id.0, None)
                .await?
            {
                snapshots.insert(link.thread_id.0.clone(), snapshot);
            }
        }
        for link in &mut links {
            if let Some(snapshot) = snapshots.get(&link.thread_id.0) {
                link.latest_run_id = snapshot.latest_run_id.clone();
            }
        }
        links.sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
        Ok((links, snapshots))
    }

    /// Read the complete Runtime evidence of one projection candidate after
    /// the caller has read the durable Session root (so a Worker-side
    /// realization mutation joins the same candidate) and taken any
    /// process-local publication token it fences on.
    async fn read_projection_evidence(
        &self,
        persisted: &PersistedSession,
        started: awaken_time::mono::Stopwatch,
    ) -> Result<CommittedSessionFacts, RunError> {
        let session_id = &persisted.session_id;
        // Read the root Thread first. A terminal Outcome query from before a
        // concurrent commit must never be paired with a newer root prefix that
        // already makes later facts listable.
        let root = self
            .projection_thread_snapshot(session_id, session_id.as_str(), None)
            .await?;
        let mut outcome_terminals = Vec::new();
        let mut outcome_query_incomplete = false;
        for outcome_id in retained_outcome_ids(&persisted.event_batches) {
            let terminal = self
                .projection
                .as_ref()
                .projection_outcome(session_id.as_str(), &outcome_id)
                .await?;
            let root_terminal = root
                .as_ref()
                .map(|snapshot| {
                    outcome_terminal_commit_cursor(&outcome_id, snapshot)
                        .map_err(|error| RunError::internal(error.to_string()))
                })
                .transpose()?
                .flatten()
                .is_some();
            // A lagging terminal query must not let newer root facts pass it.
            // An ahead query has no anchor in this root and stays withheld;
            // the trailing root fence retries if it moved during the read.
            if root_terminal && terminal.is_none() {
                outcome_query_incomplete = true;
            }
            if let Some(terminal) = terminal {
                outcome_terminals.push((outcome_id, terminal));
            }
        }
        let outcomes = started.elapsed();
        // Lend that atomic root prefix to the
        // relationship projection: it includes every link committed at or
        // before this fence, so an unknown lifecycle event is classifiable
        // without guessing from its Thread id.
        let (links, children) = self
            .projection_coordinated_prefix(session_id, root.as_ref())
            .await?;
        let snapshots = started.elapsed();
        // Lifecycle is derived from these exact atomic prefixes; reopening the
        // global feed would scan unrelated Sessions and add an unfenced read.
        let lifecycle = committed_lifecycle_prefix(root.iter().chain(children.values()))
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(CommittedSessionFacts {
            outcome_terminals,
            outcome_query_incomplete,
            root,
            links,
            children,
            lifecycle,
            timings: SessionProjectionReadTimings {
                outcomes,
                snapshots,
                lifecycle: started.elapsed(),
            },
        })
    }

    /// Read the one durable Session projection used by every protocol without
    /// driving Runtime effects. Queries must remain projections; realization is
    /// owned by create, run admission, Worker claims, and reconciliation.
    /// `Ok(None)` means no Session aggregate exists; callers with a separate
    /// Thread read model may still project that Thread without inventing one.
    async fn read_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError> {
        let session = match self
            .session(&awaken_session_types::SessionId::from(thread_id))
            .await
        {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => return Ok(None),
            Err(error) => {
                return Err(SessionProjectionRecoveryError::Rejected(
                    session_repository_error(error),
                ));
            }
        };
        if !session.is_publicly_readable() {
            return Err(SessionProjectionRecoveryError::NotFound);
        }
        let owner = self
            .owner(&awaken_session_types::SessionId::from(thread_id))
            .await
            .map_err(|error| match error {
                SessionMutationError::NotFound => SessionProjectionRecoveryError::NotFound,
                SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
                    SessionProjectionRecoveryError::Unavailable(error.to_string())
                }
                SessionMutationError::Unavailable(message) => {
                    SessionProjectionRecoveryError::Unavailable(message)
                }
            })?;
        if expected_owner.is_some_and(|expected| expected != owner) {
            return Err(SessionProjectionRecoveryError::NotFound);
        }
        if matches!(session.baseline, SessionBaselineState::Preparing(_)) {
            return Err(SessionProjectionRecoveryError::Unavailable(
                "Session creation finalization is incomplete".into(),
            ));
        }
        Ok(Some(RecoveredSessionProjection {
            owner_scope: owner,
            session,
        }))
    }
}

/// Outcome ids named by retained `DefineOutcome` provenance, first-seen order.
fn retained_outcome_ids(batches: &[SessionEventBatch]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    batches
        .iter()
        .flat_map(|batch| &batch.events)
        .filter_map(|entry| match &entry.event {
            SessionEventCommand::DefineOutcome { outcome_id, .. }
                if seen.insert(outcome_id.clone()) =>
            {
                Some(outcome_id.clone())
            }
            _ => None,
        })
        .collect()
}
