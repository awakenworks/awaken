// @formal surface exclude=application_adapter
// @formal owner crates/platform/session/awaken-session-application/src/resource_reconciliation.rs
// @formal owner crates/platform/session/awaken-session-application/src/creation/request.rs
// @formal owner crates/platform/session/awaken-session-application/src/runtime_commands.rs
// @formal reason: The original validated plan feeds the same Repository configuration and compensation;
// @formal reason: this moved lowering loop makes no path/replacement, root CAS or cleanup decision.
//! Consume the selected Inputs' validated plan through existing Repository effects.

use super::PreparedSessionInputs;
use crate::{SessionInputTarget, SessionRepositoryParticipants};
use awaken_runtime_contract::runs::RunError;

impl PreparedSessionInputs {
    /// Configure the Repository/Vault participants for one already-validated
    /// input plan. No path or replacement decision is recomputed here; a
    /// failure compensates every participant configured before it.
    pub(crate) async fn lower(
        &self,
        participants: &SessionRepositoryParticipants,
        session_id: &str,
        owner_scope: &str,
    ) -> Result<
        (
            Vec<crate::session_resource::SessionInputAttachment>,
            Vec<crate::ConfiguredSessionRepository>,
        ),
        RunError,
    > {
        let mut attachments = Vec::with_capacity(self.requests.len());
        let mut repository_configurations = Vec::new();
        for (index, request) in self.requests.iter().enumerate() {
            let repository_id = if let SessionInputTarget::Repository {
                remote_url,
                credential_material,
                initial_branch,
                initial_commit,
            } = &request.target
            {
                let configured = match participants
                    .configure(crate::SessionRepositoryResourceInput {
                        id: format!("managed:{session_id}:repository:{index}"),
                        workspace_id: owner_scope.to_string(),
                        name: format!("Session repository {index}"),
                        description: "Managed compatibility Session input".into(),
                        remote_url: remote_url.clone(),
                        credential_material: credential_material.clone(),
                        credential: None,
                        mount_path: request.mount_path.clone(),
                        initial_branch: initial_branch.clone(),
                        initial_commit: initial_commit.clone(),
                    })
                    .await
                {
                    Ok(configured) => configured,
                    Err(first) => {
                        participants.compensate(
                            session_id,
                            &repository_configurations,
                            "Managed Session Repository compensation remains pending after lowering failure",
                        )
                        .await;
                        return Err(first);
                    }
                };
                let repository_id = configured.repository_id.clone();
                repository_configurations.push(configured);
                Some(repository_id)
            } else {
                None
            };
            let mut attachment = self.attachments[index].clone();
            if let Some(repository_id) = repository_id {
                attachment.binding.target =
                    awaken_resource_contract::InputResourceId::Repository(repository_id);
            }
            // A replacement changes the resource occupying one logical Agent
            // slot; it does not invent a new slot identity. This keeps published
            // extension configuration (such as memory.binding_id) stable.
            attachments.push(attachment);
        }
        Ok((attachments, repository_configurations))
    }
}
