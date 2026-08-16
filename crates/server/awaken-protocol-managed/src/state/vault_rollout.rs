//! Credential-outbox consumer for online Managed Sessions.

use std::collections::BTreeMap;

use awaken_credential_vault::repo::{
    ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress, ManagedCredentialOperation,
    ManagedCredentialRollout, ManagedCredentialRolloutTarget,
};
use awaken_session_application::{
    McpAttachmentCandidate, McpAttachmentCandidateTarget, SessionUpdateCommand,
};
use awaken_session_contract::{McpAttachmentState, SessionExecutionState, stable_fingerprint};

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
            .sessions_referencing_vault(&event.workspace_id, &event.vault_id)
            .await
            .map_err(|error| ManagedCredentialAdoptionError::Unavailable(error.to_string()))?;
        let mut pending = Vec::new();
        for session in sessions {
            if session.execution != SessionExecutionState::Idle {
                pending.push(session.session_id);
                continue;
            }
            let desired_names = session.mcp.desired_names.as_ref();
            let mut desired = BTreeMap::new();
            for attachment in &session.mcp.attachments {
                let is_desired = desired_names.map_or_else(
                    || !attachment.state.is_terminal(),
                    |names| names.contains(&attachment.name),
                );
                if !is_desired || attachment.state == McpAttachmentState::Removed {
                    continue;
                }
                let replace = desired.get(&attachment.name).is_none_or(
                    |current: &&awaken_session_contract::SessionMcpAttachment| {
                        current.generation < attachment.generation
                    },
                );
                if replace {
                    desired.insert(attachment.name.clone(), attachment);
                }
            }
            let mut candidates = Vec::with_capacity(desired.len());
            for attachment in desired.into_values() {
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
                        mcp_candidates: Some(candidates),
                        idempotency_key: Some(event.id.clone()),
                        request_fingerprint,
                        if_match: None,
                    },
                )
                .await;
            match result {
                Ok(outcome) => {
                    self.refresh_cached_projection(&outcome.session)
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
