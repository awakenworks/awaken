// @formal evidence session_execution.terminal_state_never_reopens
//! Pure identity of the existing Session archive command and its recovery observation.
//!
//! Archive identity, execution admission and cleanup-affinity queries belong to
//! the aggregate's state owner. The application still obtains receipts and
//! rechecks its root CAS; these queries grant no command or physical absence.
//! Identity and receipt decision tables are executable evidence, not a new
//! proof of repository delivery or Sandbox termination. The original evidence
//! link above is this file's sole formal surface classification.

use crate::state::{
    ManagedLifecycleFact, PersistedSession, SessionDisposition, SessionRepositoryPublicationIntent,
};
use awaken_provisioning_contract::RepositoryPublicationExpectation;
use awaken_resource_contract::BindingId;

/// Modern, explicitly empty materialization history proves that no Environment
/// Create was admitted, including model-only lazy Sessions. This is a historical
/// fact, not a fence: terminal cleanup still requires terminality, and live
/// Worker retirement separately requires closed assignment and work admission.
/// Missing historical facts do not prove absence.
#[must_use]
pub fn session_has_no_create_evidence(session: &PersistedSession) -> bool {
    matches!(
        session.environment,
        crate::state::SessionEnvironmentState::Unmaterialized
    ) && session
        .environment_materialization()
        .is_some_and(crate::state::SessionEnvironmentMaterialization::is_empty)
}

/// The existing archive entrances, selected and rechecked by the root mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionArchiveAdmission {
    IdleOnly,
    TerminalOnly,
    UnattemptedProfiledPreparation,
    ProfiledRelease,
    FailedProfiledExecution,
    ForceTerminal,
}

impl SessionArchiveAdmission {
    pub const fn for_profiled_release(execution: crate::state::SessionExecutionState) -> Self {
        if matches!(execution, crate::state::SessionExecutionState::Preparing) {
            Self::UnattemptedProfiledPreparation
        } else {
            Self::FailedProfiledExecution
        }
    }

    pub const fn resolve(self, execution: crate::state::SessionExecutionState) -> Self {
        if matches!(self, Self::ProfiledRelease) {
            Self::for_profiled_release(execution)
        } else {
            self
        }
    }

    pub const fn as_identity_str(self) -> &'static str {
        match self {
            Self::IdleOnly => "idle_only",
            Self::TerminalOnly => "terminal_only",
            Self::UnattemptedProfiledPreparation => "unattempted_profiled_preparation",
            Self::ProfiledRelease => "profiled_release",
            Self::FailedProfiledExecution => "failed_profiled_execution",
            Self::ForceTerminal => "force_terminal",
        }
    }

    /// The existing execution-state admission. Hidden roots, pending Events,
    /// receipt reads and the archive CAS remain separate root/application gates.
    pub fn validate_execution(
        self,
        session: &PersistedSession,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        use crate::state::SessionExecutionState;
        let refusal = match self.resolve(session.execution) {
            Self::IdleOnly if session.execution != SessionExecutionState::Idle => {
                Some("only an idle Session may be archived")
            }
            Self::TerminalOnly if !session.execution.is_terminal() => {
                Some("terminal Session release requires already-terminal execution")
            }
            Self::UnattemptedProfiledPreparation
                if !crate::state::session_is_unattempted_profiled_preparation(session) =>
            {
                Some("profiled preparation release requires an unattempted frozen Session")
            }
            Self::FailedProfiledExecution
                if session.execution != SessionExecutionState::Running
                    || session.frozen_baseline().is_none_or(|baseline| {
                        baseline.mutation_policy
                            != awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen
                            || !baseline.delegate_ids.is_empty()
                    }) =>
            {
                Some("failed profiled release requires a Frozen primary execution")
            }
            _ => None,
        };
        refusal.map_or(Ok(()), |message| {
            Err(awaken_runtime_contract::runs::RunError::bad_request(
                message,
            ))
        })
    }
}

/// One Session Application command-identity law for every archive entrance.
/// Route observation time belongs only to the candidate that wins the root
/// CAS. It cannot distinguish two requests for the same business intent.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionArchiveBusinessIntent<'a> {
    Ordinary {
        admission: &'static str,
    },
    RepositoryPublication {
        source_revision: awaken_session_types::SessionRevision,
        binding_id: &'a BindingId,
        expectation: &'a RepositoryPublicationExpectation,
        resolved: &'a SessionRepositoryPublicationIntent,
    },
}

pub fn session_archive_idempotency_record(
    owner_scope: &str,
    session_id: &awaken_session_types::SessionId,
    fact: &ManagedLifecycleFact,
    business: SessionArchiveBusinessIntent<'_>,
) -> crate::state::IdempotencyRecord {
    let key = match &business {
        SessionArchiveBusinessIntent::Ordinary { .. } => {
            format!("session:archive:{session_id}:{}", fact.id)
        }
        SessionArchiveBusinessIntent::RepositoryPublication {
            source_revision, ..
        } => format!(
            "session:archive-with-repository-publication:{session_id}:{}:{}",
            source_revision.0, fact.id
        ),
    };
    let payload_hash = awaken_agent_contract::stable_fingerprint(&(
        "session-archive-intent-v2",
        owner_scope,
        session_id,
        &fact.id,
        &fact.object_id,
        &fact.workspace_id,
        &fact.event_type,
        &fact.runtime_interval,
        &business,
    ));
    crate::state::IdempotencyRecord { key, payload_hash }
}

/// The expected existing archive receipt for an archived root. This derives
/// identity only; the repository receipt and root CAS still prove admission.
#[must_use]
pub fn session_unattempted_profiled_archive_record(
    owner_scope: &str,
    session: &PersistedSession,
) -> Option<crate::state::IdempotencyRecord> {
    if !matches!(session.disposition, SessionDisposition::Archived { .. }) {
        return None;
    }
    let archive_time = i64::try_from(session.archived_at_unix_ms()?).ok()?;
    let fact = ManagedLifecycleFact {
        id: crate::event::session_terminated_fact_id(session.session_id.as_str()),
        object_id: session.session_id.to_string(),
        workspace_id: Some(owner_scope.to_owned()),
        event_type: crate::event::SESSION_TERMINATED.to_owned(),
        timestamp: archive_time,
        runtime_interval: None,
    };
    Some(session_archive_idempotency_record(
        owner_scope,
        &session.session_id,
        &fact,
        SessionArchiveBusinessIntent::Ordinary {
            admission: SessionArchiveAdmission::UnattemptedProfiledPreparation.as_identity_str(),
        },
    ))
}

/// Pure affinity admission, not an assignment or a receipt. The application
/// must obtain the required archive witness and recheck the root CAS. A logical
/// Worker name alone does not identify its original process installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTerminalCleanupAffinity {
    Unchanged,
    Retained,
    RequiresUnattemptedArchiveReceipt,
    RequiresPredecessorExit,
}

#[must_use]
pub fn session_terminal_cleanup_affinity_for(
    session: &PersistedSession,
    target_owner: &str,
    target_runtime_incarnation: &str,
    now_unix_ms: u64,
) -> SessionTerminalCleanupAffinity {
    let Ok(cleanup) = session.verified_terminal_cleanup() else {
        return SessionTerminalCleanupAffinity::Retained;
    };
    let Some(current) = &session.realization else {
        return SessionTerminalCleanupAffinity::Unchanged;
    };
    if current.owner == target_owner && current.runtime_incarnation == target_runtime_incarnation {
        return SessionTerminalCleanupAffinity::Unchanged;
    }
    let expired = !awaken_session_configuration_contract::resource::realization_lease_is_live_at(
        current.expires_at_unix_ms,
        now_unix_ms,
    );
    if expired
        && session.is_terminal()
        && (matches!(
            session.environment,
            crate::state::SessionEnvironmentState::Hibernated { .. }
        ) || session
            .environment_materialization()
            .is_some_and(|facts| facts.target_not_admitted(&session.environment)))
    {
        // Known filesystem absence is not process/dispatch closure. A new
        // actor still needs the existing exact predecessor-exit observation.
        return SessionTerminalCleanupAffinity::RequiresPredecessorExit;
    }
    if expired
        && session.is_terminal()
        && session.environment.binding().is_some()
        && cleanup.thread_ids().is_some()
    {
        return SessionTerminalCleanupAffinity::RequiresPredecessorExit;
    }
    if expired
        && matches!(
            session.environment,
            crate::state::SessionEnvironmentState::Unmaterialized
        )
        && cleanup.has_unprepared_root_only_request(&session.session_id)
    {
        SessionTerminalCleanupAffinity::RequiresUnattemptedArchiveReceipt
    } else {
        SessionTerminalCleanupAffinity::Retained
    }
}

/// Select only the immutable unsuccessful current primary Run from its owner
/// prefix. This is logical archive admission, never independent effect closure.
pub fn failed_profiled_run_for_archive<'a>(
    session: &PersistedSession,
    snapshot: &'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
) -> Option<&'a awaken_agent_contract::agent::run::Record> {
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    let run = snapshot.current_run().ok()??;
    (snapshot.thread_id.0 == session.session_id.as_str()
        && matches!(
            run.state,
            RunState::Ended(EndCause::Error(_) | EndCause::Cancelled)
        ))
    .then_some(run)
}

/// Exact original activity membership, rechecked against the root being archived.
/// A different root revision needs another read/CAS; later or concurrent activity
/// can never borrow the old Run's terminal outcome.
pub fn failed_profiled_activity_matches(
    session: &PersistedSession,
    observed: &PersistedSession,
    epoch: awaken_lease_contract::fence::Epoch,
) -> bool {
    observed.revision == session.revision
        && epoch.get() == session.activity_epoch
        && session
            .active_activity_epochs
            .iter()
            .all(|active| *active == epoch.get())
}

#[cfg(test)]
mod archive_identity_tests {
    use super::*;

    #[test]
    fn sampled_time_is_not_identity_but_interval_and_admission_are() {
        // Causes: I1 same Session/owner/fact id/type/admission, different
        // Route timestamps; I2 same id/type but a different Runtime interval;
        // I3 same fact but a different admission business rule. Effects: I1
        // exact key+hash replay; I2/I3 same key but different payload hashes,
        // so the Repository refuses an accidental different-intent replay.
        let id = awaken_session_types::SessionId::from("session");
        let mut fact = ManagedLifecycleFact {
            id: "session:session:terminated".into(),
            object_id: id.to_string(),
            workspace_id: Some("workspace".into()),
            event_type: "session.terminated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        let record = |fact: &ManagedLifecycleFact, admission| {
            session_archive_idempotency_record(
                "workspace",
                &id,
                fact,
                SessionArchiveBusinessIntent::Ordinary { admission },
            )
        };
        let original = record(&fact, "idle_only");
        fact.timestamp = 2;
        assert_eq!(record(&fact, "idle_only"), original, "I1");
        fact.runtime_interval = Some(
            serde_json::from_value(serde_json::json!({
                "interval_id": "interval", "activity_epoch": 1,
                "started_at_unix_ms": 1, "ended_at_unix_ms": 2
            }))
            .unwrap(),
        );
        let interval = record(&fact, "idle_only");
        assert_eq!(interval.key, original.key, "I2 same public fact id");
        assert_ne!(interval.payload_hash, original.payload_hash, "I2 conflict");
        let force = record(&fact, "force_terminal");
        assert_eq!(force.key, original.key, "I3 same public fact id");
        assert_ne!(force.payload_hash, interval.payload_hash, "I3 conflict");
    }
}
