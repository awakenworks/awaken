// @formal entrypoint credential.managed_rollout_abstract_ack_requires_every_target_adoption
// @formal entrypoint credential.managed_rollout_failure_or_busy_session_remains_pending
//! The Coordinator's adoption of Vault credential rollouts by its Sessions:
//! the target the private rollout route (and an all-in-one Control) delivers
//! to. It reads and updates Sessions only through the Session application's
//! ports, and publishes each committed root into the protocol projection
//! through one narrow port, so no protocol owns this orchestration.

use awaken_agent_contract::stable_fingerprint;
use awaken_credential_contract::{
    ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress,
    ManagedCredentialAdoptionUnavailable, ManagedCredentialRolloutTarget,
};
use std::sync::Arc;

use awaken_credential_contract::{ManagedCredentialOperation, ManagedCredentialRollout};
use awaken_session_application::{
    McpAttachmentCandidate, McpAttachmentCandidateTarget, SessionApplication, SessionMcpUpdate,
    SessionRootReads, SessionUpdateCommand, SessionUpdateError, SessionUpdates,
};
use awaken_session_contract::PersistedSession;

/// The disposable protocol projection a committed Session root is published
/// into (the Managed wire cache). Durable truth is already committed when it
/// is called; `false` leaves the rollout pending so it is redelivered.
pub trait SessionProjectionPublication: Send + Sync {
    fn publish_committed_session(&self, session: &PersistedSession) -> bool;
}

impl SessionProjectionPublication for awaken_protocol_managed::ManagedState {
    fn publish_committed_session(&self, session: &PersistedSession) -> bool {
        self.publish_persisted_session(session).is_ok()
    }
}

/// The Sessions an event names could not be read: log the store's own detail
/// here, and answer the closed cause.
fn sessions_unreadable(
    event: &ManagedCredentialRollout,
    error: &dyn std::fmt::Display,
) -> ManagedCredentialAdoptionError {
    tracing::warn!(rollout_id = %event.id, %error,
        "Managed credential rollout cannot read the Sessions it names");
    ManagedCredentialAdoptionError::Unavailable(ManagedCredentialAdoptionUnavailable::SessionRead)
}

/// Current active publication is adoption evidence even during another Run.
/// A pending/failed generation or any older pin still needs the idle update.
fn source_already_adopted(session: &PersistedSession, event: &ManagedCredentialRollout) -> bool {
    if matches!(
        event.operation,
        ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete
    ) {
        let references_source =
            |attachment: &awaken_session_configuration_contract::mcp::SessionMcpAttachment| {
                attachment
                    .credential
                    .as_ref()
                    .is_some_and(|access| access.credential.id == event.source_id.0)
            };
        return !session
            .mcp
            .desired_attachments()
            .into_iter()
            .any(references_source)
            && !session.mcp.attachments.iter().any(|attachment| {
                !attachment.state.is_terminal() && references_source(attachment)
            });
    }
    let mut matched = false;
    for attachment in session.mcp.desired_attachments() {
        if let Some(access) = attachment.credential.as_ref()
            && access.credential.id == event.source_id.0
        {
            matched = true;
            if access.credential.revision < event.source_version
                || attachment.state
                    != awaken_session_configuration_contract::mcp::McpAttachmentState::Active
                || !attachment.publication_acknowledged
            {
                return false;
            }
        }
    }
    matched
}

// Candidate construction is the existing frozen MCP transformation. Session
// mutation, idempotency and publication stay in the adoption use case below.
fn rollout_candidates(
    session: &PersistedSession,
    event: &ManagedCredentialRollout,
) -> Vec<McpAttachmentCandidate> {
    let desired = session.mcp.desired_attachments();
    let mut candidates = Vec::with_capacity(desired.len());
    for attachment in desired {
        let uses_changed_source = attachment
            .credential
            .as_ref()
            .is_some_and(|access| access.credential.id == event.source_id.0);
        if uses_changed_source
            && matches!(
                event.operation,
                ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete
            )
        {
            // Revocation is fail-closed: drain the authenticated MCP
            // generation instead of silently publishing it anonymous.
            continue;
        }
        let published_credential = if uses_changed_source {
            match event.operation {
                ManagedCredentialOperation::Update => {
                    let access = attachment
                        .credential
                        .as_ref()
                        .expect("a matching credential source has an access descriptor");
                    let target_revision =
                        awaken_credential_contract::managed_credential_rollout_adoption_revision(
                            access.credential.revision,
                            event.source_version,
                        );
                    if target_revision == access.credential.revision {
                        // Duplicate or stale delivery cannot roll a Session
                        // back from a newer credential revision.
                        Some((access.credential.id.clone(), access.credential.revision))
                    } else {
                        // Resolve the exact durable revision named by this
                        // event. A revoked revision fails closed instead of
                        // silently publishing an anonymous attachment.
                        Some((event.source_id.0.clone(), target_revision))
                    }
                }
                ManagedCredentialOperation::Create => attachment
                    .credential
                    .as_ref()
                    .map(|access| (access.credential.id.clone(), access.credential.revision)),
                ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete => {
                    unreachable!("revoked attachments are removed before candidate construction")
                }
            }
        } else {
            attachment
                .credential
                .as_ref()
                .map(|access| (access.credential.id.clone(), access.credential.revision))
        };
        candidates.push(McpAttachmentCandidate {
            name: attachment.name.clone(),
            target: McpAttachmentCandidateTarget::Normalized(attachment.target.clone()),
            prompts_as_skills: attachment.prompts_as_skills,
            published_credential,
            origin: attachment.origin,
        });
    }
    candidates
}

/// Session adoption of credential rollouts over the Session application and
/// one projection publication.
pub struct SessionCredentialRolloutAdoption {
    roots: Arc<dyn SessionRootReads>,
    updates: Arc<dyn SessionUpdates>,
    publication: Arc<dyn SessionProjectionPublication>,
}

impl SessionCredentialRolloutAdoption {
    #[must_use]
    pub fn new(
        sessions: Arc<SessionApplication>,
        publication: Arc<dyn SessionProjectionPublication>,
    ) -> Self {
        Self {
            roots: sessions.queries(),
            updates: sessions.update_commands(),
            publication,
        }
    }

    /// The adoption over the Session application a Managed state projects,
    /// publishing into that state's wire cache.
    #[must_use]
    pub fn over_managed(
        sessions: &Arc<SessionApplication>,
        managed: &Arc<awaken_protocol_managed::ManagedState>,
    ) -> Arc<Self> {
        Arc::new(Self::new(sessions.clone(), managed.clone()))
    }

    async fn apply_vault_rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        let sessions = self
            .roots
            .sessions_referencing_credential_source(&event.workspace_id, &event.source_id)
            .await
            .map_err(|error| sessions_unreadable(event, &error))?;
        let mut pending = false;
        for session in sessions {
            if source_already_adopted(&session, event) {
                if !self.publication.publish_committed_session(&session) {
                    pending = true;
                }
                continue;
            }
            if !awaken_session_contract::session_can_adopt_mcp_credential_lifecycle(&session) {
                tracing::info!(rollout_id = %event.id, session_id = %session.session_id,
                    phase = "session_adoption", failure = "not_idle",
                    "Managed credential rollout remains pending");
                pending = true;
                continue;
            }
            let candidates = rollout_candidates(&session, event);
            let request_fingerprint = stable_fingerprint(&(
                "managed-credential-rollout-v1",
                &event.id,
                &session.session_id,
                event.source_version,
                event.credential_revision,
            ));
            let result = self
                .updates
                .update_session(
                    &session.session_id,
                    SessionUpdateCommand {
                        title: None,
                        metadata: None,
                        budget: None,
                        tools: None,
                        mcp_update: Some(SessionMcpUpdate::CredentialLifecycle {
                            source_id: event.source_id.0.clone(),
                            revoked: matches!(
                                event.operation,
                                ManagedCredentialOperation::Archive
                                    | ManagedCredentialOperation::Delete
                            ),
                            candidates,
                        }),
                        idempotency_key: Some(event.id.clone()),
                        request_fingerprint,
                        if_match: None,
                    },
                )
                .await;
            match result {
                Ok(outcome) => {
                    if !source_already_adopted(&outcome.session, event) {
                        pending = true;
                    }
                    if !self.publication.publish_committed_session(&outcome.session) {
                        tracing::warn!(rollout_id = %event.id, session_id = %session.session_id,
                            phase = "projection_after_commit", failure = "publication",
                            "Managed credential rollout remains pending");
                        pending = true;
                    }
                }
                Err(error) => {
                    let (phase, failure) = match error {
                        SessionUpdateError::ProjectionAfterCommit { .. } => {
                            ("projection_after_commit", "projection")
                        }
                        SessionUpdateError::NotFound => ("session_adoption", "not_found"),
                        SessionUpdateError::NotIdle => ("session_adoption", "not_idle"),
                        SessionUpdateError::NotFrozen => ("session_adoption", "not_frozen"),
                        SessionUpdateError::Conflict => ("session_adoption", "conflict"),
                        SessionUpdateError::IdempotencyMismatch => {
                            ("session_adoption", "idempotency_mismatch")
                        }
                        SessionUpdateError::Rejected(_) => ("session_adoption", "rejected"),
                        SessionUpdateError::Realization(_) => ("session_adoption", "realization"),
                        SessionUpdateError::Unavailable(_) => ("session_adoption", "unavailable"),
                    };
                    tracing::warn!(rollout_id = %event.id, session_id = %session.session_id,
                        phase, failure, "Managed credential rollout remains pending");
                    pending = true;
                }
            }
        }
        if !pending {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        } else {
            Ok(ManagedCredentialAdoptionProgress::Pending)
        }
    }
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for SessionCredentialRolloutAdoption {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        self.apply_vault_rollout(event).await
    }
}

/// The private rollout route over this Coordinator's Session adoption.
pub(crate) fn router(
    sessions: &Arc<SessionApplication>,
    managed: &Arc<awaken_protocol_managed::ManagedState>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
) -> axum::Router {
    crate::credential_rollout_router_with_authenticator(
        SessionCredentialRolloutAdoption::over_managed(sessions, managed),
        authenticator,
    )
}
