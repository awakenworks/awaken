//! Canonical Session MCP authoring normalization and credential selection.

use awaken_session_contract::{McpAttachmentDraft, McpAttachmentOrigin, McpTarget, RunError};

use super::SessionApplication;

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

impl SessionApplication {
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
        selected_holder: &awaken_credential_contract::PlaintextHolder,
    ) -> Result<Vec<McpAttachmentDraft>, RunError> {
        let (candidates, _) = Self::normalize_mcp_candidate_targets(candidates)?;
        let mut drafts = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let name = candidate.name;
            let McpAttachmentCandidateTarget::Normalized(target) = candidate.target else {
                unreachable!("MCP targets are normalized above")
            };
            let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            };
            let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
                workspace_id,
                &target,
                &usage,
            );
            let credential = match candidate.published_credential {
                Some((id, revision)) => {
                    let source_id = awaken_credential_contract::CredentialSourceId(id.clone());
                    let access = if let Some(credentials) = self.credential_source() {
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
                        if access.credential.revision != revision {
                            return Err(RunError::bad_request(
                                "published MCP credential revision no longer matches",
                            ));
                        }
                        access
                    } else {
                        return Err(RunError::bad_request(
                            "published MCP credential requires the canonical credential authority",
                        ));
                    };
                    Some(access)
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
                        match source_id {
                            Some(source_id) => Some(
                                credentials
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
                                    })?,
                            ),
                            None => None,
                        }
                    }
                    None => None,
                },
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
