//! Credential-outbox consumer for online Managed Sessions.

use awaken_credential_vault::repo::{
    ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress, ManagedCredentialOperation,
    ManagedCredentialRollout, ManagedCredentialRolloutTarget,
};
use awaken_session_application::{
    McpAttachmentCandidate, McpAttachmentCandidateTarget, SessionMcpUpdate, SessionUpdateCommand,
};
use awaken_session_contract::{SessionExecutionState, stable_fingerprint};

use super::ManagedState;

#[must_use]
const fn rollout_update_target_revision(current: u64, event: u64) -> u64 {
    if current >= event { current } else { event }
}

#[cfg(kani)]
#[kani::proof]
fn rollout_update_revision_is_monotonic_and_covers_event() {
    let current = kani::any::<u64>();
    let event = kani::any::<u64>();
    let target = rollout_update_target_revision(current, event);
    assert!(target >= current);
    assert!(target >= event);
    assert_eq!(target, current.max(event));
}

impl ManagedState {
    async fn apply_vault_rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        let sessions = self
            .application
            .session_repository_handle()
            .sessions_referencing_credential_source(&event.workspace_id, &event.source_id)
            .await
            .map_err(|error| ManagedCredentialAdoptionError::Unavailable(error.to_string()))?;
        let mut pending = Vec::new();
        for session in sessions {
            if session.execution != SessionExecutionState::Idle {
                pending.push(session.session_id);
                continue;
            }
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
                let published_credential =
                    if uses_changed_source {
                        match event.operation {
                            ManagedCredentialOperation::Update => {
                                let access = attachment.credential.as_ref().expect(
                                    "a matching credential source has an access descriptor",
                                );
                                let target_revision = rollout_update_target_revision(
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
                            ManagedCredentialOperation::Create => {
                                attachment.credential.as_ref().map(|access| {
                                    (access.credential.id.clone(), access.credential.revision)
                                })
                            }
                            ManagedCredentialOperation::Archive
                            | ManagedCredentialOperation::Delete => unreachable!(
                                "revoked attachments are removed before candidate construction"
                            ),
                        }
                    } else {
                        attachment.credential.as_ref().map(|access| {
                            (access.credential.id.clone(), access.credential.revision)
                        })
                    };
                candidates.push(McpAttachmentCandidate {
                    name: attachment.name.clone(),
                    target: McpAttachmentCandidateTarget::Normalized(attachment.target.clone()),
                    prompts_as_skills: attachment.prompts_as_skills,
                    published_credential,
                    origin: attachment.origin,
                });
            }
            let request_fingerprint = stable_fingerprint(&(
                "managed-credential-rollout-v1",
                &event.id,
                &session.session_id,
                event.source_version,
                event.credential_revision,
            ));
            let result = self
                .application
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
                    self.publish_persisted_session(&outcome.session)
                        .map_err(|error| {
                            ManagedCredentialAdoptionError::Unavailable(error.to_string())
                        })?;
                }
                Err(error) => pending.push(format!("{} ({error})", session.session_id)),
            }
        }
        if pending.is_empty() {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        } else {
            Ok(ManagedCredentialAdoptionProgress::Pending)
        }
    }
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for ManagedState {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        self.apply_vault_rollout(event).await
    }
}
