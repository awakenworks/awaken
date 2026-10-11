// @formal evidence session_execution.terminal_state_never_reopens
//! Pure identity of the existing Session archive command and its recovery observation.
//!
//! Archive identity, execution admission and cleanup-affinity queries belong to
//! the aggregate's state owner. The application still obtains receipts and
//! rechecks its root CAS; these queries grant no command or physical absence.
//! Identity and receipt decision tables are executable evidence, not a new
//! proof of repository delivery or Sandbox termination. The original evidence
//! link above is this file's sole formal surface classification.

use crate::{
    ManagedLifecycleFact, PersistedSession, SessionDisposition, SessionRepositoryPublicationIntent,
};
use awaken_provisioning_contract::RepositoryPublicationExpectation;
use awaken_resource_contract::BindingId;

/// The existing archive entrances, selected and rechecked by the root mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionArchiveAdmission {
    IdleOnly,
    TerminalOnly,
    UnattemptedProfiledPreparation,
    ForceTerminal,
}

impl SessionArchiveAdmission {
    pub const fn as_identity_str(self) -> &'static str {
        match self {
            Self::IdleOnly => "idle_only",
            Self::TerminalOnly => "terminal_only",
            Self::UnattemptedProfiledPreparation => "unattempted_profiled_preparation",
            Self::ForceTerminal => "force_terminal",
        }
    }

    /// The existing execution-state admission. Hidden roots, pending Events,
    /// receipt reads and the archive CAS remain separate root/application gates.
    pub fn validate_execution(
        self,
        session: &PersistedSession,
    ) -> Result<(), awaken_session_types::RunError> {
        use crate::SessionExecutionState;
        let refusal = match self {
            Self::IdleOnly if session.execution != SessionExecutionState::Idle => {
                Some("only an idle Session may be archived")
            }
            Self::TerminalOnly if !session.execution.is_terminal() => {
                Some("terminal Session release requires already-terminal execution")
            }
            Self::UnattemptedProfiledPreparation
                if !crate::session_is_unattempted_profiled_preparation(session) =>
            {
                Some("profiled preparation release requires an unattempted frozen Session")
            }
            _ => None,
        };
        refusal.map_or(Ok(()), |message| {
            Err(awaken_session_types::RunError::bad_request(message))
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
) -> crate::IdempotencyRecord {
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
    crate::IdempotencyRecord { key, payload_hash }
}

/// The expected existing archive receipt for an archived root. This derives
/// identity only; the repository receipt and root CAS still prove admission.
#[must_use]
pub fn session_unattempted_profiled_archive_record(
    owner_scope: &str,
    session: &PersistedSession,
) -> Option<crate::IdempotencyRecord> {
    if !matches!(session.disposition, SessionDisposition::Archived { .. }) {
        return None;
    }
    let archive_time = i64::try_from(session.archived_at_unix_ms()?).ok()?;
    let fact = ManagedLifecycleFact {
        id: awaken_session_event_contract::session_terminated_fact_id(session.session_id.as_str()),
        object_id: session.session_id.to_string(),
        workspace_id: Some(owner_scope.to_owned()),
        event_type: awaken_session_event_contract::SESSION_TERMINATED.to_owned(),
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
/// must obtain the required archive witness and recheck the root CAS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTerminalCleanupAffinity {
    Unchanged,
    Retained,
    RequiresUnattemptedArchiveReceipt,
}

#[must_use]
pub fn session_terminal_cleanup_affinity_for(
    session: &PersistedSession,
    target_owner: &str,
    now_unix_ms: u64,
) -> SessionTerminalCleanupAffinity {
    let Some(current) = &session.realization else {
        return SessionTerminalCleanupAffinity::Unchanged;
    };
    if current.owner == target_owner {
        return SessionTerminalCleanupAffinity::Unchanged;
    }
    if !awaken_session_resource_contract::realization_lease_is_live_at(
        current.expires_at_unix_ms,
        now_unix_ms,
    ) && matches!(
        session.environment,
        crate::SessionEnvironmentState::Unmaterialized
    ) && session
        .terminal_cleanup
        .has_unprepared_root_only_request(&session.session_id)
    {
        SessionTerminalCleanupAffinity::RequiresUnattemptedArchiveReceipt
    } else {
        SessionTerminalCleanupAffinity::Retained
    }
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
