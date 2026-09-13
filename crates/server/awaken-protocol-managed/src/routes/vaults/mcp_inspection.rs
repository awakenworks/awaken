//! Config-page credential choices and inspection over the existing Vault owner.
use super::*;
use awaken_credential_vault::{CredentialError, CredentialStatus};
use awaken_runtime_contract::CredentialRef;

#[derive(Clone, Debug, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpCredentialChoice {
    pub id: String,
    pub vault_id: String,
    pub vault_name: String,
    pub display_name: String,
    pub auth_type: &'static str,
    pub source: CredentialRef,
    pub mcp_server_url: String,
    pub target_fingerprint: String,
}
#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpInspectionView {
    pub status: &'static str,
    pub http_status: Option<u16>,
    pub tools: Vec<awaken_session_contract::McpProbeTool>,
    pub credential: Option<McpCredentialChoice>,
    pub location: &'static str,
    pub checked_at: String,
}
impl VaultState {
    pub async fn mcp_credential_choices(
        &self,
        workspace: &str,
        url: &str,
    ) -> Result<Vec<McpCredentialChoice>, CredentialError> {
        let target = awaken_session_contract::McpTarget::identity(url)
            .map_err(|e| CredentialError::InvalidSource(e.to_string()))?;
        Ok(self
            .mcp_credential_inventory(workspace)
            .await?
            .into_iter()
            .filter(|choice| {
                awaken_session_contract::McpTarget::identity(&choice.mcp_server_url)
                    .ok()
                    .as_ref()
                    == Some(&target)
            })
            .collect())
    }
    pub async fn mcp_credential_inventory(
        &self,
        workspace: &str,
    ) -> Result<Vec<McpCredentialChoice>, CredentialError> {
        let mut choices = Vec::new();
        for vault in self.repository.list_vaults(workspace).await? {
            if !vault.accepts_child_mutation() {
                continue;
            }
            for credential in self
                .repository
                .list_vault_credentials(workspace, &vault.id)
                .await?
            {
                if !credential.lifecycle.is_active() {
                    continue;
                }
                let (credential_url, auth_type) = match &credential.auth {
                    AuthRecord::StaticBearer { mcp_server_url } => {
                        (mcp_server_url, "static_bearer")
                    }
                    AuthRecord::McpOauth { mcp_server_url, .. } => (mcp_server_url, "mcp_oauth"),
                    AuthRecord::EnvironmentVariable { .. } => continue,
                };
                let target = awaken_session_contract::McpTarget::parse_http(credential_url)
                    .map_err(|e| CredentialError::InvalidSource(e.to_string()))?;
                let source = self.repository.get(&credential.source_id).await?;
                if inspection_source_admitted(&source, workspace, credential_url).is_err() {
                    continue;
                }
                let Ok(revision) = u64::try_from(source.version) else {
                    continue;
                };
                if revision == 0 {
                    continue;
                }
                choices.push(McpCredentialChoice {
                    id: credential.id,
                    vault_id: vault.id.clone(),
                    vault_name: vault.display_name.clone(),
                    display_name: credential
                        .display_name
                        .unwrap_or_else(|| credential_url.clone()),
                    auth_type,
                    source: CredentialRef {
                        id: source.id.0,
                        revision,
                    },
                    mcp_server_url: credential_url.clone(),
                    target_fingerprint: target.fingerprint().to_owned(),
                });
            }
        }
        choices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(choices)
    }
    pub async fn inspect_mcp(
        &self,
        workspace: &str,
        url: &str,
        credential_id: Option<&str>,
    ) -> Result<McpInspectionView, CredentialError> {
        awaken_session_contract::McpTarget::parse_http(url)
            .map_err(|e| CredentialError::InvalidSource(e.to_string()))?;
        let choice = if let Some(id) = credential_id {
            Some(
                self.mcp_credential_choices(workspace, url)
                    .await?
                    .into_iter()
                    .find(|c| c.id == id)
                    .ok_or_else(|| {
                        CredentialError::InvalidSource(
                            "selected credential is unavailable or targets another MCP endpoint"
                                .into(),
                        )
                    })?,
            )
        } else {
            None
        };
        let bearer = if let Some(choice) = &choice {
            let source = self
                .repository
                .get(&CredentialSourceId(choice.source.id.clone()))
                .await?;
            if u64::try_from(source.version).ok() != Some(choice.source.revision) {
                return Err(CredentialError::MutationConflict(
                    "credential changed before inspection".into(),
                ));
            }
            inspection_source_admitted(&source, workspace, url)?;
            let record = self
                .repository
                .get_vault_credential(workspace, &choice.id)
                .await?
                .filter(|record| {
                    record.lifecycle.is_active()
                        && record.source_id.0 == choice.source.id
                        && record.vault_id == choice.vault_id
                })
                .ok_or_else(|| CredentialError::NotActive(choice.id.clone()))?;
            let _ = record;
            if !self
                .repository
                .get_vault(workspace, &choice.vault_id)
                .await?
                .is_some_and(|vault| vault.accepts_child_mutation())
            {
                return Err(CredentialError::NotActive(choice.vault_id.clone()));
            }
            Some(awaken_credential_vault::materialize(&source, self.secrets.as_ref()).await?)
        } else {
            None
        };
        let mut inspection = match &self.probe {
            Some(probe) => probe.inspect(url, bearer.as_ref()).await,
            None => awaken_session_contract::McpInspection {
                status: McpProbeStatus::Unknown,
                tools: Vec::new(),
            },
        };
        if let Some(secret) = bearer.as_ref().map(RedactedString::expose_secret) {
            for tool in &mut inspection.tools {
                tool.name = tool.name.replace(secret, "[REDACTED]");
                tool.description = tool
                    .description
                    .as_ref()
                    .map(|text| text.replace(secret, "[REDACTED]"));
                redact_probe_value(&mut tool.input_schema, secret);
            }
        }
        let (status, http_status) = match inspection.status {
            McpProbeStatus::Valid => ("valid", None),
            McpProbeStatus::Invalid { http_status } => ("invalid", Some(http_status)),
            McpProbeStatus::Unknown => ("unknown", None),
        };
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| CredentialError::Storage(e.to_string()))?
            .as_millis();
        let millis = u64::try_from(millis).map_err(|e| CredentialError::Storage(e.to_string()))?;
        Ok(McpInspectionView {
            status,
            http_status,
            tools: inspection.tools,
            credential: choice,
            location: "control",
            checked_at: awaken_session_contract::epoch_millis_to_rfc3339(millis),
        })
    }
}

fn redact_probe_value(value: &mut serde_json::Value, secret: &str) {
    match value {
        serde_json::Value::String(text) => *text = text.replace(secret, "[REDACTED]"),
        serde_json::Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_probe_value(item, secret)),
        serde_json::Value::Object(object) => {
            let previous = std::mem::take(object);
            for (key, mut value) in previous {
                redact_probe_value(&mut value, secret);
                object.insert(key.replace(secret, "[REDACTED]"), value);
            }
        }
        _ => {}
    }
}

fn inspection_source_admitted(
    source: &awaken_credential_vault::CredentialSource,
    workspace: &str,
    url: &str,
) -> Result<(), CredentialError> {
    if source.workspace_id != workspace
        || source.status != CredentialStatus::Active
        || source.version <= 0
        || !source.is_executable_origin()
    {
        return Err(CredentialError::NotActive(source.id.0.clone()));
    }
    let audience = awaken_session_contract::McpTarget::identity(url)
        .map_err(|e| CredentialError::InvalidSource(e.to_string()))?
        .canonical_url();
    let target = awaken_credential_contract::CredentialTarget::new(
        awaken_credential_contract::CredentialPurpose::McpAuthorization,
        audience,
    );
    source.validate_access_target(
        Some(&target),
        &awaken_credential_contract::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inspection_admission_requires_every_source_authority_condition() {
        // Graph: Workspace AND active AND positive revision AND executable
        // origin AND exact purpose/audience/usage -> eligible. R0 all true;
        // R1..R10 independently negate one input -> reject before material I/O.
        // URL canonicalization is exercised by the public two-account test.
        let url = "https://mcp.example.test/mcp";
        let source = awaken_credential_vault::CredentialSource {
            id: CredentialSourceId("test-source".into()),
            replacement_of: None,
            workspace_id: "w".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            descriptor: Some(mcp_credential_descriptor(url).unwrap().0),
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        for rule in 0..=10 {
            let mut candidate = source.clone();
            match rule {
                1 => candidate.workspace_id = "other".into(),
                2 => candidate.status = CredentialStatus::Disabled,
                3 => candidate.status = CredentialStatus::Archived,
                4 => candidate.version = 0,
                5 => candidate.version = -1,
                6 => candidate.kind = awaken_credential_vault::CredentialKind::Env,
                7 => candidate.descriptor = None,
                8 => {
                    candidate.descriptor.as_mut().unwrap().targets[0]
                        .target
                        .purpose = CredentialPurpose::ProviderAdapter
                }
                9 => {
                    candidate.descriptor = Some(
                        mcp_credential_descriptor("https://mcp.example.test/other")
                            .unwrap()
                            .0,
                    )
                }
                10 => {
                    candidate.descriptor.as_mut().unwrap().targets[0].usage =
                        CredentialUsage::HttpHeader {
                            name: "x-api-key".into(),
                            scheme: None,
                        }
                }
                _ => {}
            }
            assert_eq!(
                inspection_source_admitted(&candidate, "w", url).is_ok(),
                rule == 0,
                "R{rule}"
            );
        }
    }
}
