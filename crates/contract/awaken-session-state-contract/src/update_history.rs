// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Retained update facts share the original Session root and revision. Validation
// @formal reason: rejects changed history and missing authoring facts while preserving Runtime MCP
// @formal reason: and budget accounting transitions. Executable aggregate/backend tests, rather than
// @formal reason: this surface link, own replacement and serialization evidence.
//! Immutable update facts retained by the existing Session root.
use crate::{PersistedSession, SessionRevision};
use awaken_session_configuration_contract::SessionAgentConfiguration;
use awaken_session_event_contract::SessionEventProjectionAnchor;
use std::collections::BTreeMap;

/// The original initial-presentation identity law, shared by one-shot recording
/// and persisted recovery. Mutable tools/MCP remain their own historical values.
pub(crate) fn initial_configuration_identity_matches(
    session: &PersistedSession,
    configuration: &SessionAgentConfiguration,
) -> bool {
    configuration.version > 0
        && session.frozen_baseline().is_some_and(|baseline| {
            baseline.agent_id == configuration.agent_id
                && baseline
                    .agent_revision
                    .is_none_or(|revision| revision == configuration.version)
                && baseline.model == configuration.model.model_id
        })
}

/// Explicit mutation of an optional Session field. `None` on the command means
/// unchanged; the enum distinguishes clear from replace without nested Options.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum SessionFieldUpdate<T> {
    Clear,
    Replace(T),
}

/// Which durable projections changed while applying an update.
///
/// Privacy rule P1: an external consumer can read the aggregate's decision but
/// cannot author a change flag independently of the recording transition.
/// ```compile_fail
/// fn overwrite(mut changes: awaken_session_state_contract::SessionUpdateChanges) {
///     changes.title = true;
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionUpdateChanges {
    pub(crate) title: bool,
    pub(crate) metadata: bool,
    pub(crate) tools: bool,
    pub(crate) mcp: bool,
    pub(crate) budget: bool,
}

impl SessionUpdateChanges {
    #[must_use]
    pub const fn title(self) -> bool {
        self.title
    }

    #[must_use]
    pub const fn metadata(self) -> bool {
        self.metadata
    }

    #[must_use]
    pub const fn tools(self) -> bool {
        self.tools
    }

    #[must_use]
    pub const fn mcp(self) -> bool {
        self.mcp
    }

    #[must_use]
    pub const fn budget(self) -> bool {
        self.budget
    }

    /// One actual-delta law for both recording and current-root CAS validation.
    pub(crate) fn between(previous: &PersistedSession, replacement: &PersistedSession) -> Self {
        Self {
            title: previous.title != replacement.title,
            metadata: previous.metadata != replacement.metadata,
            tools: previous.tools != replacement.tools,
            mcp: previous.mcp != replacement.mcp,
            budget: previous.budget != replacement.budget,
        }
    }

    /// Authoring changes cannot disappear from retained history. Runtime MCP
    /// phases/retry generations and budget accounting remain separate state
    /// transitions; only their desired configuration/limit requires a fact.
    /// An explicit MCP retry update may still record its existing command delta.
    fn requires_fact(self, previous: &PersistedSession, replacement: &PersistedSession) -> bool {
        self.title
            || self.metadata
            || self.tools
            || (self.mcp && previous.mcp.desired_fingerprint != replacement.mcp.desired_fingerprint)
            || (self.budget
                && previous.budget.max_list_cost_minor()
                    != replacement.budget.max_list_cost_minor())
    }

    #[must_use]
    pub fn any(self) -> bool {
        self.title || self.metadata || self.tools || self.mcp || self.budget
    }

    #[must_use]
    pub fn agent(self) -> bool {
        self.tools || self.mcp
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "configuration", rename_all = "snake_case")]
pub enum SessionUpdateAgentConfiguration {
    Recorded(Box<SessionAgentConfiguration>),
    Unrecorded,
}

/// One retained aggregate update. Readers borrow its payload without a setter.
/// Privacy rule P2: owning a deserialized fact still does not grant field-write
/// access; untrusted wire values remain subject to the existing root validator.
/// ```compile_fail
/// fn overwrite(mut fact: awaken_session_state_contract::SessionUpdateFact) {
///     fact.title = None;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateFact {
    pub(crate) committed_revision: SessionRevision,
    pub(crate) recorded_at_unix_ms: Option<u64>,
    pub(crate) admitted_batch_count: u64,
    pub(crate) observed_runtime: SessionEventProjectionAnchor,
    pub(crate) predecessor_operation_id: Option<String>,
    pub(crate) changes: SessionUpdateChanges,
    pub(crate) title: Option<SessionFieldUpdate<String>>,
    pub(crate) metadata: Option<BTreeMap<String, String>>,
    pub(crate) agent: Option<SessionUpdateAgentConfiguration>,
    pub(crate) budget: Option<SessionFieldUpdate<u64>>,
}

impl SessionUpdateFact {
    #[must_use]
    pub fn committed_revision(&self) -> SessionRevision {
        self.committed_revision
    }

    #[must_use]
    pub fn recorded_at_unix_ms(&self) -> Option<u64> {
        self.recorded_at_unix_ms
    }

    #[must_use]
    pub fn admitted_batch_count(&self) -> u64 {
        self.admitted_batch_count
    }

    #[must_use]
    pub fn observed_runtime(&self) -> SessionEventProjectionAnchor {
        self.observed_runtime
    }

    #[must_use]
    pub fn predecessor_operation_id(&self) -> Option<&str> {
        self.predecessor_operation_id.as_deref()
    }

    #[must_use]
    pub fn changes(&self) -> SessionUpdateChanges {
        self.changes
    }

    #[must_use]
    pub fn title(&self) -> Option<&SessionFieldUpdate<String>> {
        self.title.as_ref()
    }

    #[must_use]
    pub fn metadata(&self) -> Option<&BTreeMap<String, String>> {
        self.metadata.as_ref()
    }

    #[must_use]
    pub fn agent(&self) -> Option<&SessionUpdateAgentConfiguration> {
        self.agent.as_ref()
    }

    #[must_use]
    pub fn budget(&self) -> Option<&SessionFieldUpdate<u64>> {
        self.budget.as_ref()
    }
}

/// The aggregate's append-only update prefix.
/// Privacy rule P3: a reader cannot erase or append the retained prefix; only
/// the aggregate transition records it and CAS validates its extension.
/// ```compile_fail
/// fn erase(mut history: awaken_session_state_contract::SessionUpdateHistory) {
///     history.facts.clear();
/// }
/// ```
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateHistory {
    pub(crate) known_after_revision: SessionRevision,
    pub(crate) facts: Vec<SessionUpdateFact>,
}

impl SessionUpdateHistory {
    #[must_use]
    pub fn known_after_revision(&self) -> SessionRevision {
        self.known_after_revision
    }

    #[must_use]
    pub fn facts(&self) -> &[SessionUpdateFact] {
        &self.facts
    }

    pub(crate) fn fresh() -> Self {
        Self {
            known_after_revision: SessionRevision(0),
            facts: Vec::new(),
        }
    }
}

impl SessionUpdateHistory {
    /// Actual retained updates only. Absence is unrecorded historical provenance.
    #[must_use]
    pub fn for_session(session: &PersistedSession) -> Option<&Self> {
        session.update_history.as_deref()
    }

    /// Rebuild the current display from its immutable first fact and current
    /// authoritative mutable fields. It is never an execution input.
    #[must_use]
    pub fn agent_configuration(session: &PersistedSession) -> Option<SessionAgentConfiguration> {
        let mut configuration = session.initial_agent_configuration.as_deref()?.clone();
        configuration.tools = session.tools.clone();
        configuration.mcp_servers = session.configured_mcp_servers();
        Some(configuration)
    }

    pub(crate) fn validate_replacement(
        previous: &PersistedSession,
        replacement: &PersistedSession,
    ) -> Result<(), crate::SessionMutationValidationError> {
        let invalid = || crate::SessionMutationValidationError::UpdateHistoryMismatch;
        Self::validate_at(
            replacement,
            SessionRevision(previous.revision.0.checked_add(1).ok_or_else(invalid)?),
        )?;
        if previous.initial_agent_configuration != replacement.initial_agent_configuration {
            return Err(invalid());
        }
        let existing_facts = previous
            .update_history
            .as_deref()
            .map_or(0, |history| history.facts.len());
        let replacement_facts = replacement
            .update_history
            .as_deref()
            .map_or(0, |history| history.facts.len());
        if SessionUpdateChanges::between(previous, replacement).requires_fact(previous, replacement)
            && replacement_facts <= existing_facts
        {
            return Err(invalid());
        }
        match (
            previous.update_history.as_deref(),
            replacement.update_history.as_deref(),
        ) {
            (None, None) => Ok(()),
            (Some(_), None) => Err(invalid()),
            (before, Some(after)) => {
                let existing = before.map_or(0, |history| history.facts.len());
                if before.is_some_and(|before| {
                    before.known_after_revision != after.known_after_revision
                        || after.facts.len() < existing
                        || after.facts[..existing] != before.facts
                }) || (before.is_none()
                    && (after.known_after_revision != previous.revision || after.facts.len() != 1))
                    || after.facts.len() > existing + 1
                {
                    return Err(invalid());
                }
                for fact in &after.facts[existing..] {
                    if previous.revision.0.checked_add(1) != Some(fact.committed_revision.0)
                        || fact.admitted_batch_count
                            != u64::try_from(previous.event_batches.len()).map_err(|_| invalid())?
                        || previous.event_batches != replacement.event_batches
                        || fact.predecessor_operation_id
                            != previous
                                .event_batches
                                .iter()
                                .rev()
                                .flat_map(|batch| batch.events.iter().rev())
                                .next()
                                .map(|entry| entry.event.operation_id().to_owned())
                        || fact.changes != SessionUpdateChanges::between(previous, replacement)
                        || fact.title
                            != fact.changes.title.then(|| {
                                replacement
                                    .title
                                    .clone()
                                    .map_or(SessionFieldUpdate::Clear, SessionFieldUpdate::Replace)
                            })
                        || fact.metadata
                            != fact.changes.metadata.then(|| replacement.metadata.clone())
                        || fact.budget
                            != fact.changes.budget.then(|| {
                                replacement
                                    .budget
                                    .max_list_cost_minor()
                                    .map_or(SessionFieldUpdate::Clear, SessionFieldUpdate::Replace)
                            })
                        || fact.agent
                            != fact.changes.agent().then(|| {
                                Self::agent_configuration(replacement).map_or(
                                    SessionUpdateAgentConfiguration::Unrecorded,
                                    |value| {
                                        SessionUpdateAgentConfiguration::Recorded(Box::new(value))
                                    },
                                )
                            })
                        || fact.recorded_at_unix_ms != replacement.updated_at_unix_ms
                    {
                        return Err(invalid());
                    }
                }
                Ok(())
            }
        }
    }

    pub fn validate_for(
        session: &PersistedSession,
    ) -> Result<(), crate::SessionMutationValidationError> {
        Self::validate_at(session, session.revision)
    }

    pub(crate) fn validate_at(
        session: &PersistedSession,
        maximum: SessionRevision,
    ) -> Result<(), crate::SessionMutationValidationError> {
        if session
            .initial_agent_configuration
            .as_deref()
            .is_some_and(|configuration| {
                !initial_configuration_identity_matches(session, configuration)
            })
        {
            return Err(crate::SessionMutationValidationError::UpdateHistoryMismatch);
        }
        let Some(history) = session.update_history.as_deref() else {
            return Ok(());
        };
        if history.known_after_revision > session.revision {
            return Err(crate::SessionMutationValidationError::UpdateHistoryMismatch);
        }
        let mut previous = history.known_after_revision;
        let mut count = 0;
        for fact in &history.facts {
            if fact.committed_revision <= previous
                || fact.committed_revision > maximum
                || fact.recorded_at_unix_ms.is_none()
                || !fact.changes.any()
                || fact.changes.title != fact.title.is_some()
                || fact.changes.metadata != fact.metadata.is_some()
                || fact.changes.agent() != fact.agent.is_some()
                || fact.changes.budget != fact.budget.is_some()
                || fact.admitted_batch_count < count
                || fact.admitted_batch_count > session.event_batches.len() as u64
            {
                return Err(crate::SessionMutationValidationError::UpdateHistoryMismatch);
            }
            let admitted = usize::try_from(fact.admitted_batch_count)
                .map_err(|_| crate::SessionMutationValidationError::UpdateHistoryMismatch)?;
            let (prefix, suffix) = session.event_batches.split_at(admitted);
            // Positive admission revisions must fall on their original side of
            // this update CAS. Legacy revision-zero batches keep their prefix
            // position; no historical admission is inferred or rewritten.
            if prefix.iter().any(|batch| {
                batch.admitted_revision.0 != 0 && batch.admitted_revision >= fact.committed_revision
            }) || suffix.iter().any(|batch| {
                batch.admitted_revision.0 == 0 || batch.admitted_revision <= fact.committed_revision
            }) || fact.predecessor_operation_id.as_deref()
                != prefix
                    .iter()
                    .rev()
                    .flat_map(|batch| batch.events.iter().rev())
                    .next()
                    .map(|entry| entry.event.operation_id())
                || prefix.iter().any(|batch| {
                    batch.admission_projection_anchor.is_some_and(|anchor| {
                        anchor.source_commit_cursor > fact.observed_runtime.source_commit_cursor
                    })
                })
            {
                return Err(crate::SessionMutationValidationError::UpdateHistoryMismatch);
            }
            previous = fact.committed_revision;
            count = fact.admitted_batch_count;
        }
        Ok(())
    }
}
