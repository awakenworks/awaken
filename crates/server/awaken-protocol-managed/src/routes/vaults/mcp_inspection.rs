// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Static owner: config-page credential choices and MCP inspection over the existing Vault
// @formal reason: owner. Dynamic path: a route selects or probes a credential through that owner's
// @formal reason: admission, so inspection returns status rather than material and cannot become a second
// @formal reason: credential store or a path that reveals a secret the Vault would refuse. Evidence:
// @formal reason: credential.lifecycle.credential_lifecycle plus the vault route tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Config-page credential choices, MCP inspection, and the
//! `mcp_oauth_validate` route over the existing Vault owner.
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
    pub tools: Vec<awaken_session_configuration_contract::McpProbeTool>,
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
        let target = awaken_session_mcp_contract::McpTarget::identity(url)
            .map_err(|e| CredentialError::InvalidSource(e.to_string()))?;
        Ok(self
            .mcp_credential_inventory(workspace)
            .await?
            .into_iter()
            .filter(|choice| {
                awaken_session_mcp_contract::McpTarget::identity(&choice.mcp_server_url)
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
                let target = awaken_session_mcp_contract::McpTarget::parse_http(credential_url)
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
        let (choice, inspection) = self
            .run_mcp_inspection(workspace, url, credential_id)
            .await?;
        let (status, http_status) = match inspection.status {
            McpProbeStatus::Valid => ("valid", None),
            McpProbeStatus::Invalid { http_status } => ("invalid", Some(http_status)),
            McpProbeStatus::Unknown => ("unknown", None),
        };
        let checked_at = self.now_rfc3339()?;
        Ok(McpInspectionView {
            status,
            http_status,
            tools: inspection.tools,
            credential: choice,
            location: "control",
            checked_at,
        })
    }

    /// The credential-validation view of one probe: verdict, discovered tools
    /// and the failing step, all scrubbed of the credential's own material.
    pub(crate) async fn inspect_mcp_evidence(
        &self,
        workspace: &str,
        url: &str,
        credential_id: &str,
    ) -> Result<awaken_session_configuration_contract::McpInspection, CredentialError> {
        Ok(self
            .run_mcp_inspection(workspace, url, Some(credential_id))
            .await?
            .1)
    }

    /// Run one exact refresh-token exchange for the credential's current
    /// revision. `None` when the persisted row cannot compile an exact
    /// instruction, so no exchange was attempted.
    pub(crate) async fn exchange_refresh(
        &self,
        exchange: &dyn awaken_session_configuration_contract::CredentialRefreshExchange,
        source_id: &CredentialSourceId,
        config: awaken_credential_vault::catalog::ManagedMcpOauthRefresh,
    ) -> Option<awaken_session_configuration_contract::RefreshExchangeOutcome> {
        let source = self.repository.get(source_id).await.ok()?;
        let revision = u64::try_from(source.version).ok()?;
        let access = oauth_refresh_access(&source, revision, config).ok()?;
        Some(exchange.exchange(source.id.clone(), access).await)
    }

    async fn run_mcp_inspection(
        &self,
        workspace: &str,
        url: &str,
        credential_id: Option<&str>,
    ) -> Result<
        (
            Option<McpCredentialChoice>,
            awaken_session_configuration_contract::McpInspection,
        ),
        CredentialError,
    > {
        awaken_session_mcp_contract::McpTarget::parse_http(url)
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
            None => awaken_session_configuration_contract::McpInspection {
                status: McpProbeStatus::Unknown,
                tools: Vec::new(),
                failure: None,
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
            // The port already scrubs its own bearer; the owner of the
            // material does not rely on that.
            if let Some(response) = inspection
                .failure
                .as_mut()
                .and_then(|failure| failure.http_response.as_mut())
            {
                response.body = response.body.replace(secret, "[REDACTED]");
            }
        }
        Ok((choice, inspection))
    }
}

/// Compile the exact OAuth refresh/reseal instruction for one credential
/// revision from its persisted refresh configuration and sealed material refs.
pub(super) fn oauth_refresh_access(
    source: &awaken_credential_vault::CredentialSource,
    revision: u64,
    refresh: McpOauthRefreshRecord,
) -> Result<
    awaken_credential_contract::CredentialRefreshAccess,
    awaken_credential_vault::CredentialError,
> {
    let source_id = &source.id;
    let access_token_ref = source
        .material_ref
        .as_ref()
        .ok_or_else(|| {
            awaken_credential_vault::CredentialError::MissingMaterialRef(source_id.0.clone())
        })?
        .0
        .clone();
    let refresh_token_ref = source
        .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
        .ok_or_else(|| {
            awaken_credential_vault::CredentialError::MissingMaterialRef(format!(
                "{}:{OAUTH_REFRESH_TOKEN_SLOT}",
                source_id.0
            ))
        })?
        .0
        .clone();
    let client_secret_ref = match refresh.token_endpoint_auth {
        awaken_credential_contract::TokenEndpointAuth::None => None,
        awaken_credential_contract::TokenEndpointAuth::ClientSecretBasic
        | awaken_credential_contract::TokenEndpointAuth::ClientSecretPost => Some(
            source
                .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                .ok_or_else(|| {
                    awaken_credential_vault::CredentialError::MissingMaterialRef(format!(
                        "{}:{OAUTH_CLIENT_SECRET_SLOT}",
                        source_id.0
                    ))
                })?
                .0
                .clone(),
        ),
    };
    Ok(awaken_credential_contract::CredentialRefreshAccess::new(
        revision,
        refresh.token_endpoint,
        refresh.client_id,
        refresh.token_endpoint_auth,
        client_secret_ref,
        refresh_token_ref,
        access_token_ref,
        refresh.scope,
        refresh.resource,
    ))
}

pub(super) async fn validate_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<CredentialValidation>, WireError> {
    let record = state
        .repository
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|c| c.vault_id == vault_id && c.lifecycle.is_active())
        .ok_or_else(|| not_found("credential"))?;
    let (mcp_server_url, refresh_config) = match &record.auth {
        AuthRecord::McpOauth {
            mcp_server_url,
            refresh,
            ..
        } => (Some(mcp_server_url.clone()), refresh.clone()),
        _ => (None, None),
    };
    let has_refresh_token = refresh_config.is_some();
    // Live probe: only an `mcp_oauth` credential (it names an MCP server to
    // handshake with) and only when the process startup wired an `McpProbe`.
    // The access token is materialized inside `inspect_mcp` and the port
    // receives the resolved secret — never a vault ref. Any gap — no probe,
    // env-var/static_bearer, a broken row, an inconclusive probe — keeps
    // `status` `unknown`: never a false verdict. `mcp_probe` is the failing
    // step with its scrubbed HTTP response, `null` when the probe completed.
    let mut status = CredentialValidationStatus::Unknown;
    let mut mcp_probe = None;
    let mut refresh = None;
    if let Some(url) = &mcp_server_url
        && let Ok(mut evidence) = state.inspect_mcp_evidence(&workspace_id, url, &id).await
    {
        // An auth-refused access token is where a refresh token matters:
        // without one the outcome is `no_refresh_token`; with one and a wired
        // exchange, the exact exchange runs, reseals, and the probe repeats
        // against the resealed token.
        if matches!(evidence.status, McpProbeStatus::Invalid { .. }) {
            match refresh_config {
                None => refresh = Some(CredentialRefreshResult::no_refresh_token()),
                Some(config) => {
                    if let Some(exchange) = &state.refresh_exchange
                        && let Some(outcome) = state
                            .exchange_refresh(exchange.as_ref(), &record.source_id, config)
                            .await
                    {
                        let succeeded = outcome.status
                            == awaken_session_configuration_contract::RefreshExchangeStatus::Succeeded;
                        refresh = Some(CredentialRefreshResult::from(outcome));
                        if succeeded
                            && let Ok(after) =
                                state.inspect_mcp_evidence(&workspace_id, url, &id).await
                        {
                            evidence = after;
                        }
                    }
                }
            }
        }
        match evidence.status {
            McpProbeStatus::Valid => status = CredentialValidationStatus::Valid,
            McpProbeStatus::Invalid { .. } => status = CredentialValidationStatus::Invalid,
            McpProbeStatus::Unknown => {}
        }
        mcp_probe = evidence.failure.map(CredentialMcpProbe::from);
    }
    // The validation is performed now: stamp the instant its verdict settled.
    let validated_at = state.now_rfc3339().map_err(storage_error)?;
    Ok(Json(CredentialValidation {
        credential_id: id,
        has_refresh_token,
        mcp_probe,
        refresh,
        status,
        object_type: "vault_credential_validation",
        validated_at,
        vault_id,
    }))
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
    let audience = awaken_session_mcp_contract::McpTarget::identity(url)
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
