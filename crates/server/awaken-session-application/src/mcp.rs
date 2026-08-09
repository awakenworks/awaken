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
    /// Normalize URL/stdio identity, ordered Vault selection, and exact
    /// credential revision pinning once for every Session authoring path.
    pub async fn normalize_mcp_drafts(
        &self,
        candidates: Vec<McpAttachmentCandidate>,
        ordered_vault_ids: &[String],
    ) -> Result<Vec<McpAttachmentDraft>, RunError> {
        let mut drafts = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let name = candidate.name;
            let target = match candidate.target {
                McpAttachmentCandidateTarget::HttpUrl(url) => {
                    McpTarget::parse_http(&url).map_err(|_| {
                        RunError::bad_request(format!("invalid MCP server URL for `{name}`"))
                    })?
                }
                McpAttachmentCandidateTarget::SandboxStdio { command, args } => {
                    McpTarget::sandbox_stdio(&command, args).map_err(|_| {
                        RunError::bad_request(format!(
                            "invalid sandbox stdio MCP command for `{name}`"
                        ))
                    })?
                }
                McpAttachmentCandidateTarget::Normalized(target) => target,
            };
            let credential = match candidate.published_credential {
                Some((id, revision)) => {
                    let source_id = awaken_credential_contract::CredentialSourceId(id.clone());
                    let access = if let Some(credentials) = self.credential_source() {
                        let access = credentials
                            .mcp_access_for_source(&source_id)
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
                        awaken_credential_contract::CredentialAccess::new(
                            awaken_credential_contract::CredentialRef { id, revision },
                            awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
                            awaken_credential_contract::CredentialUsage::HttpHeader {
                                name: "authorization".into(),
                                scheme: Some("Bearer".into()),
                            },
                            awaken_credential_contract::CredentialExecutionPolicy::self_hosted_provider(),
                        )
                    };
                    Some(access)
                }
                None => match self.credential_source() {
                    Some(credentials) => {
                        let source_id = match target.http_url() {
                            Some(url) => credentials
                                .mcp_credential_source_for_url(ordered_vault_ids, url)
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
                                    .mcp_access_for_source(&source_id)
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
