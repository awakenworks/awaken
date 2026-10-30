// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Creation presentation and authoring facts are recorded on the existing revisioned
// @formal reason: Session root; recording uses its retained admissions and actual configuration delta.
// @formal reason: Aggregate and backend tests check retention and CAS; this classification does not
// @formal reason: prove database durability or Runtime source observations.
//! One state-owned recording law for creation presentation and authoring updates.
use super::{SessionAggregate, SessionAggregateCommandError};
use crate::event::SessionEventProjectionAnchor;
use crate::state::{
    PersistedSession, SessionFieldUpdate, SessionRevision, SessionUpdateAgentConfiguration,
    SessionUpdateChanges, SessionUpdateFact, SessionUpdateHistory,
};
use awaken_session_configuration_contract::SessionAgentConfiguration;

impl SessionAggregate {
    /// Record exactly once on the original fresh, not-yet-observed creation root.
    pub fn record_initial_agent_configuration(
        &mut self,
        configuration: Box<SessionAgentConfiguration>,
    ) -> Result<(), SessionAggregateCommandError> {
        let fresh = self.state.revision.0 == 0
            && self.state.created_at_unix_ms.is_none()
            && self.state.updated_at_unix_ms.is_none()
            && self.state.initial_agent_configuration.is_none()
            && self.state.update_history.as_deref().is_some_and(|history| {
                history.known_after_revision.0 == 0 && history.facts.is_empty()
            })
            && self
                .state
                .run_activities()
                .is_some_and(|(records, unknown)| records.is_empty() && !unknown)
            && self
                .state
                .environment_materialization()
                .is_some_and(crate::state::SessionEnvironmentMaterialization::is_empty);
        if !fresh
            || !crate::state::update_history::initial_configuration_identity_matches(
                &self.state,
                &configuration,
            )
        {
            return Err(SessionAggregateCommandError::InvalidInitialAgentConfiguration);
        }
        let mut configuration = configuration;
        // Current execution fields keep their sole root ownership. This history
        // value records the exact first projection without another selection law.
        configuration.tools = self.state.tools.clone();
        configuration.mcp_servers = self.state.configured_mcp_servers();
        self.state.initial_agent_configuration = Some(configuration);
        Ok(())
    }

    /// Derive the actual change once and retain its exact CAS candidate fact.
    pub fn record_configuration_update(
        &mut self,
        before: &PersistedSession,
        mcp_changed: bool,
        observed_runtime: SessionEventProjectionAnchor,
    ) -> Result<SessionUpdateChanges, SessionAggregateCommandError> {
        if before.session_id != self.state.session_id
            || before.revision != self.state.revision
            || before.event_batches != self.state.event_batches
            || before.update_history != self.state.update_history
            || before.initial_agent_configuration != self.state.initial_agent_configuration
            || mcp_changed != (before.mcp != self.state.mcp)
        {
            return Err(SessionAggregateCommandError::InvalidUpdateHistory);
        }
        let changes = self.configuration_update_changes(before, mcp_changed);
        if !changes.any() {
            return Ok(changes);
        }
        let committed_revision = SessionRevision(
            self.state
                .revision
                .0
                .checked_add(1)
                .ok_or(SessionAggregateCommandError::InvalidUpdateHistory)?,
        );
        let agent = changes.agent().then(|| {
            SessionUpdateHistory::agent_configuration(&self.state)
                .map_or(SessionUpdateAgentConfiguration::Unrecorded, |value| {
                    SessionUpdateAgentConfiguration::Recorded(Box::new(value))
                })
        });
        let fact = SessionUpdateFact {
            committed_revision,
            recorded_at_unix_ms: None,
            admitted_batch_count: u64::try_from(self.state.event_batches.len())
                .map_err(|_| SessionAggregateCommandError::InvalidUpdateHistory)?,
            observed_runtime,
            predecessor_operation_id: self
                .state
                .event_batches
                .iter()
                .rev()
                .flat_map(|batch| batch.events.iter().rev())
                .next()
                .map(|entry| entry.event.operation_id().to_owned()),
            changes,
            title: changes.title.then(|| {
                self.state
                    .title
                    .clone()
                    .map_or(SessionFieldUpdate::Clear, SessionFieldUpdate::Replace)
            }),
            metadata: changes.metadata.then(|| self.state.metadata.clone()),
            agent,
            budget: changes.budget.then(|| {
                self.state
                    .budget
                    .max_list_cost_minor()
                    .map_or(SessionFieldUpdate::Clear, SessionFieldUpdate::Replace)
            }),
        };
        self.state
            .update_history
            .get_or_insert_with(|| {
                Box::new(SessionUpdateHistory {
                    known_after_revision: before.revision,
                    facts: Vec::new(),
                })
            })
            .facts
            .push(fact);
        Ok(changes)
    }
    /// One pure change law; callers avoid new prefix IO for actual no-op commands.
    #[must_use]
    pub fn configuration_update_changes(
        &self,
        before: &PersistedSession,
        mcp_changed: bool,
    ) -> SessionUpdateChanges {
        let changes = SessionUpdateChanges::between(before, &self.state);
        debug_assert_eq!(before.mcp != self.state.mcp, mcp_changed);
        changes
    }
}
