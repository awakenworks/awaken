// @formal evidence session_execution.terminal_state_never_reopens
//! One archive command identity shared by mutation and read-only recovery.
//! Observation uses the original atomic command receipt, without mutation or
//! a dependency on realization/resource orchestration.

use awaken_provisioning_contract::RepositoryPublicationExpectation;
use awaken_resource_contract::BindingId;
use awaken_session_state_contract::{
    ManagedLifecycleFact, PersistedSession, SessionDisposition, SessionRepositoryPublicationIntent,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveAdmission {
    IdleOnly,
    TerminalOnly,
    UnattemptedProfiledPreparation,
    ForceTerminal,
}

impl ArchiveAdmission {
    pub(crate) const fn as_identity_str(self) -> &'static str {
        match self {
            Self::IdleOnly => "idle_only",
            Self::TerminalOnly => "terminal_only",
            Self::UnattemptedProfiledPreparation => "unattempted_profiled_preparation",
            Self::ForceTerminal => "force_terminal",
        }
    }
}

/// One Session Application command-identity law for every archive entrance.
/// Route observation time belongs only to the candidate that wins the root
/// CAS. It cannot distinguish two requests for the same business intent.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ArchiveBusinessIntent<'a> {
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

pub(crate) fn archive_idempotency_record(
    owner_scope: &str,
    session_id: &awaken_session_types::SessionId,
    fact: &ManagedLifecycleFact,
    business: ArchiveBusinessIntent<'_>,
) -> awaken_session_state_contract::IdempotencyRecord {
    let key = match &business {
        ArchiveBusinessIntent::Ordinary { .. } => {
            format!("session:archive:{session_id}:{}", fact.id)
        }
        ArchiveBusinessIntent::RepositoryPublication {
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
    awaken_session_state_contract::IdempotencyRecord { key, payload_hash }
}

/// Observe the existing atomic archive receipt, never a sampled empty handle.
/// The same command fingerprint proves its narrow no-Create admission happened
/// before the archive CAS. A generic archive receipt cannot supply this fact.
pub(crate) async fn unattempted_profiled_archive_witness(
    repository: &dyn awaken_session_repository_contract::SessionRepository,
    owner_scope: &str,
    session: &PersistedSession,
) -> Result<bool, awaken_session_state_contract::SessionRepositoryError> {
    if !matches!(session.disposition, SessionDisposition::Archived { .. }) {
        return Ok(false);
    }
    let Some(archive_time) = session
        .archived_at_unix_ms()
        .and_then(|value| i64::try_from(value).ok())
    else {
        return Ok(false);
    };
    let fact = ManagedLifecycleFact {
        id: awaken_session_event_contract::session_terminated_fact_id(session.session_id.as_str()),
        object_id: session.session_id.to_string(),
        workspace_id: Some(owner_scope.to_owned()),
        event_type: awaken_session_event_contract::SESSION_TERMINATED.to_owned(),
        timestamp: archive_time, // Recorded Root time; excluded from command identity.
        runtime_interval: None,
    };
    let expected = archive_idempotency_record(
        owner_scope,
        &session.session_id,
        &fact,
        ArchiveBusinessIntent::Ordinary {
            admission: ArchiveAdmission::UnattemptedProfiledPreparation.as_identity_str(),
        },
    );
    Ok(repository
        .idempotency_receipt(&session.session_id, &expected.key)
        .await?
        .is_some_and(|receipt| {
            receipt.payload_hash == expected.payload_hash
                && receipt.committed_revision.0 <= session.revision.0
        }))
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
            archive_idempotency_record(
                "workspace",
                &id,
                fact,
                ArchiveBusinessIntent::Ordinary { admission },
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
