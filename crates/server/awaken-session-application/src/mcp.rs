// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: This source implements the repository-controlled mcp product boundary and is classified
// @formal reason: against its product requirement; this link is not a formal-proof claim.
//! Canonical Session MCP authoring normalization and credential selection.

use awaken_runtime_contract::runs::RunError;
use {
    awaken_agent_contract::McpTarget,
    awaken_session_configuration_contract::mcp::{McpAttachmentDraft, McpAttachmentOrigin},
};

use super::SessionCredentials;

#[derive(Clone, Debug)]
pub struct McpAttachmentCandidate {
    pub name: String,
    pub target: McpAttachmentCandidateTarget,
    pub prompts_as_skills: bool,
    pub published_credential: Option<(String, u64)>,
    pub origin: McpAttachmentOrigin,
}

#[derive(Clone, Debug)]
pub enum McpAttachmentCandidateTarget {
    HttpUrl(String),
    SandboxStdio { command: String, args: Vec<String> },
    Normalized(McpTarget),
}

impl SessionCredentials {
    /// Normalize endpoint identity without selecting credentials. Environment
    /// resolution consumes these targets first; its frozen MCP holder then owns
    /// the one credential compilation decision.
    pub fn normalize_mcp_candidate_targets(
        candidates: Vec<McpAttachmentCandidate>,
    ) -> Result<(Vec<McpAttachmentCandidate>, Vec<McpTarget>), RunError> {
        let mut normalized = Vec::with_capacity(candidates.len());
        let mut targets = Vec::with_capacity(candidates.len());
        for mut candidate in candidates {
            let target = match candidate.target {
                McpAttachmentCandidateTarget::HttpUrl(url) => {
                    McpTarget::parse_http(&url).map_err(|_| {
                        RunError::bad_request(format!(
                            "invalid MCP server URL for `{}`",
                            candidate.name
                        ))
                    })?
                }
                McpAttachmentCandidateTarget::SandboxStdio { command, args } => {
                    McpTarget::sandbox_stdio(&command, args).map_err(|_| {
                        RunError::bad_request(format!(
                            "invalid sandbox stdio MCP command for `{}`",
                            candidate.name
                        ))
                    })?
                }
                McpAttachmentCandidateTarget::Normalized(target) => target,
            };
            targets.push(target.clone());
            candidate.target = McpAttachmentCandidateTarget::Normalized(target);
            normalized.push(candidate);
        }
        Ok((normalized, targets))
    }

    /// Normalize URL/stdio identity, ordered Vault selection, and exact
    /// credential revision pinning once for every Session authoring path.
    pub async fn normalize_mcp_drafts(
        &self,
        workspace_id: &str,
        candidates: Vec<McpAttachmentCandidate>,
        ordered_vault_ids: &[String],
        selected_holder: &awaken_agent_contract::credential::PlaintextHolder,
    ) -> Result<Vec<McpAttachmentDraft>, RunError> {
        let (candidates, _) = Self::normalize_mcp_candidate_targets(candidates)?;
        let mut drafts = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let name = candidate.name;
            let McpAttachmentCandidateTarget::Normalized(target) = candidate.target else {
                unreachable!("MCP targets are normalized above")
            };
            let usage = awaken_agent_contract::credential::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            };
            let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
                workspace_id,
                &target,
                &usage,
            );
            let selected = match candidate.published_credential {
                Some((id, revision)) => {
                    let credentials = self.credential_source().ok_or_else(|| {
                        RunError::bad_request(
                            "published MCP credential requires the canonical credential authority",
                        )
                    })?;
                    Some((
                        credentials,
                        awaken_credential_contract::CredentialSourceId(id),
                        Some(revision),
                    ))
                }
                None => match self.credential_source() {
                    Some(credentials) => {
                        let source_id = match target.http_url() {
                            Some(url) => credentials
                                .mcp_credential_source_for_url(workspace_id, ordered_vault_ids, url)
                                .await
                                .map_err(|error| {
                                    RunError::bad_request(format!(
                                        "MCP credential selection failed: {error}"
                                    ))
                                })?,
                            None => None,
                        };
                        source_id.map(|source_id| (credentials, source_id, None))
                    }
                    None => None,
                },
            };
            let credential = if let Some((credentials, source_id, published_revision)) = selected {
                let access = credentials
                    .mcp_access_for_source(
                        &source_id,
                        workspace_id,
                        &target,
                        selected_holder,
                        &binding,
                    )
                    .await
                    .map_err(|error| {
                        RunError::bad_request(format!(
                            "MCP credential could not be pinned exactly: {error}"
                        ))
                    })?;
                if published_revision.is_some_and(|revision| access.credential.revision != revision)
                {
                    return Err(RunError::bad_request(
                        "published MCP credential revision no longer matches",
                    ));
                }
                Some(access)
            } else {
                None
            };
            drafts.push(McpAttachmentDraft {
                name,
                target,
                prompts_as_skills: candidate.prompts_as_skills,
                credential,
                origin: candidate.origin,
            });
        }
        Ok(drafts)
    }
}
