//! Existing root-lease and terminal-claim admission helpers.

use super::*;

/// Selector for the one terminal-assignment claim algorithm. External Workers
/// scan only Worker-owned Sessions; the co-located recovery driver asks for one
/// exact local Session. Keeping the selection fact beside the shared CAS loop
/// prevents local cleanup from growing a second lease state machine.
#[derive(Clone, Copy)]
pub(super) enum TerminalCleanupClaimScope<'a> {
    External,
    LocalSession(&'a str),
}

/// One uncertain root-CAS attempt. Retaining the attempted and predecessor
/// leases lets a retry compensate only its own exact write when the Worker
/// Registry withdraws authority; it is not a second lease record.
pub(super) struct TerminalCleanupClaimConflict {
    pub(super) owner_scope: String,
    pub(super) session_id: String,
    pub(super) attempted_lease: SessionRealizationLease,
    pub(super) previous_realization: Option<SessionRealizationLease>,
}

pub(super) enum TerminalCleanupRootClaim {
    Assignment(Box<SessionTerminalCleanupAssignment>),
    Skip,
    Conflict(Box<TerminalCleanupClaimConflict>),
    Abort(SessionRealizationControlFailure),
    Failed(SessionRealizationControlFailure),
}

impl<'a> TerminalCleanupClaimScope<'a> {
    pub(super) fn admits(
        self,
        application: &SessionApplication,
        session: &PersistedSession,
    ) -> bool {
        match self {
            Self::External => application.requires_external_realization(session),
            Self::LocalSession(session_id) => {
                session.session_id == session_id
                    && !application.requires_external_realization(session)
            }
        }
    }

    pub(super) fn is_exact_local(self) -> bool {
        matches!(self, Self::LocalSession(_))
    }

    pub(super) const fn is_external(self) -> bool {
        matches!(self, Self::External)
    }
}

/// Lower one frozen terminal root into the static capabilities needed by the
/// current Worker for every remaining source-dependent phase. Model execution,
/// ACP, tool recovery, and inference credentials are deliberately absent: the
/// cleanup driver never starts a new Run.
pub(super) fn terminal_cleanup_worker_requirements(
    owner_scope: &str,
    session: &PersistedSession,
    action: &awaken_session_contract::SessionTerminalCleanupAction,
) -> Result<awaken_worker_contract::PlacementRequirements, SessionRealizationControlFailure> {
    if matches!(
        action,
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { .. }
    ) {
        return Ok(
            awaken_worker_contract::PlacementRequirements::terminal_cleanup(false, false, None),
        );
    }

    let resource_facts = session
        .resources
        .active_generation()
        .1
        .compatibility_facts();
    let publication_outcome_is_durable = session
        .terminal_cleanup
        .repository_publication_receipt(&session.session_id)
        .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
        .is_some()
        || session
            .terminal_cleanup
            .repository_publication_rejection(&session.session_id)
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
            .is_some();
    let pending_publication = (!publication_outcome_is_durable)
        .then(|| session.terminal_cleanup.repository_publication_intent())
        .flatten();
    if pending_publication.is_some()
        && session
            .environment
            .terminal_repository_publication_binding()
            .is_none()
    {
        return Err(SessionRealizationControlFailure::Invalid(
            "terminal Repository publication has no existing live Environment source".into(),
        ));
    }
    let publication_has_credential = pending_publication.is_some_and(|intent| {
        matches!(
            &intent.input.source,
            awaken_session_contract::ResolvedInputSource::Repository { config, .. }
                if config.credential_binding.is_some()
        )
    });

    let baseline = session.frozen_baseline().ok_or_else(|| {
        SessionRealizationControlFailure::Invalid(
            "terminal cleanup requires one frozen Session baseline".into(),
        )
    })?;
    let checkpoint_format = if let Some(checkpoint) = session.environment.checkpoint() {
        Some(checkpoint.format.clone())
    } else {
        session
            .environment
            .checkpoint_request(
                owner_scope,
                &session.session_id,
                &baseline.environment.idle_retention,
            )
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
            .map(|request| request.format)
    };

    Ok(
        awaken_worker_contract::PlacementRequirements::terminal_cleanup(
            resource_facts.has_resources() || pending_publication.is_some(),
            resource_facts.has_credentialed_repository() || publication_has_credential,
            checkpoint_format,
        ),
    )
}

pub(super) fn verify_lease(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
) -> Result<(), SessionRealizationControlFailure> {
    if !session.realization.as_ref().is_some_and(|current| {
        awaken_session_contract::realization_lease_authorizes(current, asserted, now_unix_ms())
    }) {
        return Err(SessionRealizationControlFailure::StaleOwnership);
    }
    Ok(())
}

pub(super) fn exact_generation_key(generation: &McpGenerationRef) -> String {
    awaken_session_contract::stable_fingerprint(generation)
}

pub(super) fn generation_set_is_renewed_successor(
    current: &[McpGenerationRef],
    asserted: &[McpGenerationRef],
) -> bool {
    current.len() == asserted.len()
        && current.iter().all(|expected| {
            asserted.iter().any(|actual| {
                awaken_session_contract::realization_generation_authorizes(expected, actual)
            })
        })
        && current.iter().any(|expected| {
            asserted.iter().any(|actual| {
                awaken_session_contract::realization_generation_authorizes(expected, actual)
                    && expected.lease_expires_at_unix_ms > actual.lease_expires_at_unix_ms
            })
        })
}
