// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: The original exact credential access and generic material commands use the
// @formal reason: same repository, SecretStore, Clock and selected delivery. Rollout target
// @formal reason: ownership remains with VaultApplication; no return edge is stored here.
//! Credential selection, exact access and consumer-neutral material commands.

use crate::catalog::{
    ManagedCredentialAuth, ManagedMcpOauthRefresh, ManagedVault, ManagedVaultCredential,
};
use crate::credential_access::bearer_usage;
use crate::repo::{
    CredentialMaterialPatch, CredentialRetirement, ManagedCredentialRepository,
    revoke_credential_exact, rotate_credential_materials_exact,
};
use crate::{
    CredentialCreateParams, CredentialError, CredentialKind, CredentialSource, CredentialStatus,
    ExactCredentialAccessRequest, OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore,
};
use awaken_agent_contract::RedactedString;
use awaken_agent_contract::{
    credential::CredentialAccess, credential::CredentialPurpose, credential::CredentialRef,
    credential::CredentialRefreshAccess, credential::CredentialTarget,
    credential::TokenEndpointAuth,
};
use awaken_credential_contract::{
    CredentialDescriptor, CredentialMaterialDelivery, CredentialMaterialDescriptor,
    CredentialMaterialIngress, CredentialMaterialIngressCommand, CredentialMaterialInput,
    CredentialMaterialRetirementCommand, CredentialMaterialRotationCommand, CredentialPlaintext,
    CredentialSourceId, CredentialTargetContract, MaterialEntry, OPAQUE_SECRET_MATERIAL_TYPE,
    SessionCredentialAccessRequest, SessionCredentialSource,
};
use awaken_time::WallClock;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Vault credential access over the original backend identities.
///
/// This role deliberately owns no rollout target, VaultApplication or protocol
/// projection. Escaping either Source or Ingress retains only these backends.
///
/// I2 capability table: the concrete Access implements both original roles;
/// the protocol State cannot be installed as either role and return to its
/// rollout target. Each old-State rejection is checked independently.
///
/// ```
/// use awaken_credential_contract::{CredentialMaterialIngress, SessionCredentialSource};
/// use awaken_credential_vault::VaultCredentialAccess;
/// fn roles<T: SessionCredentialSource + CredentialMaterialIngress>() {}
/// roles::<VaultCredentialAccess>();
/// ```
pub struct VaultCredentialAccess {
    secrets: Arc<dyn SecretStore>,
    repository: Arc<dyn ManagedCredentialRepository>,
    clock: Arc<dyn WallClock>,
    material_delivery: Option<CredentialMaterialDelivery>,
}

impl VaultCredentialAccess {
    pub(crate) fn new(
        secrets: Arc<dyn SecretStore>,
        repository: Arc<dyn ManagedCredentialRepository>,
        clock: Arc<dyn WallClock>,
        material_delivery: Option<CredentialMaterialDelivery>,
    ) -> Self {
        Self {
            secrets,
            repository,
            clock,
            material_delivery,
        }
    }

    /// Whether `id` names an active vault that may be attached to a new Session.
    /// Archived vaults remain retrievable for audit but are not executable.
    pub async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, CredentialError> {
        Ok(self
            .repository
            .get_vault(workspace_id, id)
            .await?
            .is_some_and(|vault| vault.accepts_child_mutation()))
    }

    /// The Workspace's Vaults that accept credential use, in repository order.
    pub async fn executable_vaults(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ManagedVault>, CredentialError> {
        let mut vaults = self.repository.list_vaults(workspace_id).await?;
        vaults.retain(ManagedVault::accepts_child_mutation);
        Ok(vaults)
    }

    /// The active credentials of one Vault in ascending id order, or `None`
    /// when the Vault does not accept credential use.
    pub async fn executable_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Option<Vec<ManagedVaultCredential>>, CredentialError> {
        if !self.has_vault(workspace_id, vault_id).await? {
            return Ok(None);
        }
        let mut credentials = self
            .repository
            .list_vault_credentials(workspace_id, vault_id)
            .await?;
        credentials.retain(|credential| credential.lifecycle.is_active());
        credentials.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Some(credentials))
    }

    /// The neutral credential-domain row id of an active credential in
    /// `vault_id`: the resolver takes this `CredentialSourceId`, never the wire
    /// id.
    pub async fn credential_source_id(
        &self,
        workspace_id: &str,
        vault_id: &str,
        credential_id: &str,
    ) -> Option<CredentialSourceId> {
        self.repository
            .get_vault_credential(workspace_id, credential_id)
            .await
            .ok()
            .flatten()
            .filter(|credential| {
                credential.vault_id == vault_id && credential.lifecycle.is_active()
            })
            .map(|credential| credential.source_id)
    }

    /// The credential source row.
    pub async fn source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<CredentialSource, CredentialError> {
        self.repository.get(source_id).await
    }

    /// The authoritative Managed child of a credential source.
    pub async fn credential_of_source(
        &self,
        workspace_id: &str,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        self.repository
            .get_vault_credential_by_source(workspace_id, source_id)
            .await
    }

    /// Open the active material of one source for an exact, caller-owned use
    /// (an MCP inspection the Vault owner admitted). Never returned to a wire.
    pub async fn open_material(
        &self,
        source: &CredentialSource,
    ) -> Result<RedactedString, CredentialError> {
        crate::materialize(source, self.secrets.as_ref()).await
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// credential source. The open deployment reads only the active revision;
    /// an installed hosted issuer may open that exact material solely to attach
    /// the existing recipient-bound envelope to the returned secret-free pin.
    /// `now_unix_ms` is sampled here; the request carries everything else.
    pub async fn exact_access<'r>(
        &self,
        source_id: &CredentialSourceId,
        request: impl FnOnce(u64) -> ExactCredentialAccessRequest<'r>,
    ) -> Result<(CredentialSource, CredentialAccess), CredentialError> {
        let now_unix_ms = crate::repo::credential_material_now_unix_ms(self.clock.as_ref())?;
        let source = self.repository.get(source_id).await?;
        let access = crate::publish_exact_credential_access(
            &source,
            request(now_unix_ms),
            self.material_delivery
                .as_ref()
                .map(|delivery| (self.secrets.as_ref(), delivery)),
        )
        .await?;
        Ok((source, access))
    }

    /// The exact MCP execution pin for `audience`, with the OAuth refresh
    /// instruction of the same revision attached when the credential holds
    /// one. `workspace_id` narrows admission at the exact row read.
    pub async fn mcp_access(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: Option<&str>,
        audience: String,
        selected_holder: &awaken_agent_contract::credential::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<CredentialAccess, CredentialError> {
        let (source, mut access) = self
            .exact_access(source_id, |now_unix_ms| ExactCredentialAccessRequest {
                workspace_id,
                target: Some(CredentialTarget::new(
                    CredentialPurpose::McpAuthorization,
                    audience,
                )),
                usage: bearer_usage(),
                policy:
                    awaken_agent_contract::credential::CredentialExecutionPolicy::self_hosted_mcp(),
                selected_holder,
                binding,
                now_unix_ms,
            })
            .await?;
        let revision = access.credential.revision;
        let refresh = self
            .repository
            .get_vault_credential_by_source(&source.workspace_id, source_id)
            .await?
            .and_then(|record| match record.auth {
                ManagedCredentialAuth::McpOauth { refresh, .. } => refresh,
                _ => None,
            });
        if let Some(refresh) = refresh {
            access = access.with_refresh(oauth_refresh_access(&source, revision, refresh)?);
        }
        Ok(access)
    }

    /// The exact refresh/reseal instruction of a credential's current
    /// revision, or `None` when the persisted row cannot compile one.
    pub async fn refresh_access(
        &self,
        source_id: &CredentialSourceId,
        config: ManagedMcpOauthRefresh,
    ) -> Option<(CredentialSourceId, CredentialRefreshAccess)> {
        let source = self.repository.get(source_id).await.ok()?;
        let revision = u64::try_from(source.version).ok()?;
        let access = oauth_refresh_access(&source, revision, config).ok()?;
        Some((source.id, access))
    }

    /// Enter one described material idempotently under `source_id`.
    pub async fn enter_material(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        descriptor: CredentialDescriptor,
        material: RedactedString,
    ) -> Result<MaterialEntry, String> {
        let entry = crate::repo::enter_credential_idempotent_described(
            source_id,
            CredentialCreateParams {
                workspace_id: workspace_id.to_string(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(material),
                oauth_command: None,
            },
            None,
            descriptor,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let revision = u64::try_from(entry.source.version)
            .map_err(|_| "credential revision exceeds the Session range".to_string())?;
        Ok(MaterialEntry {
            credential: CredentialRef {
                id: entry.source.id.0,
                revision,
            },
            created: entry.created,
        })
    }

    /// Rotate the material of `source_id` from exactly `expected_revision`;
    /// a replay of the completed rotation with the same material succeeds.
    pub async fn rotate_material(
        &self,
        source_id: &CredentialSourceId,
        expected_revision: u64,
        workspace_id: &str,
        expected_descriptor: &CredentialDescriptor,
        material: RedactedString,
    ) -> Result<u64, String> {
        let current = self
            .repository
            .get(source_id)
            .await
            .map_err(|error| error.to_string())?;
        if current.workspace_id != workspace_id
            || current.descriptor.as_ref() != Some(expected_descriptor)
        {
            return Err("credential binding is unavailable in this Workspace".into());
        }
        let expected_version = i64::try_from(expected_revision)
            .map_err(|_| "credential revision exceeds the Vault range".to_string())?;
        let completed_version = expected_version
            .checked_add(1)
            .ok_or_else(|| "credential revision exceeds the Vault range".to_string())?;
        if current.version == completed_version {
            if current.status != CredentialStatus::Active {
                return Err("credential is not active".into());
            }
            let sealed = crate::materialize(&current, self.secrets.as_ref())
                .await
                .map_err(|error| error.to_string())?;
            if sealed.expose_secret() != material.expose_secret() {
                return Err("credential revision changed before material rotation".into());
            }
            return u64::try_from(current.version)
                .map_err(|_| "credential revision exceeds the Session range".into());
        }
        if current.version != expected_version {
            return Err("credential revision changed before material rotation".into());
        }
        let rotated = rotate_credential_materials_exact(
            source_id,
            expected_version,
            CredentialMaterialPatch {
                primary: Some(material),
                auxiliary: BTreeMap::new(),
                descriptor: None,
            },
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        u64::try_from(rotated.version)
            .map_err(|_| "credential revision exceeds the Session range".into())
    }

    /// Retire the exact credential revision `credential`; a replay of the
    /// completed retirement succeeds.
    pub async fn retire_material(
        &self,
        credential: CredentialRef,
        workspace_id: &str,
    ) -> Result<(), String> {
        let source_id = CredentialSourceId(credential.id);
        let source = self
            .repository
            .get(&source_id)
            .await
            .map_err(|error| error.to_string())?;
        if source.workspace_id != workspace_id {
            return Err("credential binding is unavailable in this Workspace".into());
        }
        let expected = i64::try_from(credential.revision)
            .map_err(|_| "credential revision exceeds the Vault range".to_string())?;
        let completed = expected
            .checked_add(1)
            .ok_or_else(|| "credential revision exceeds the Vault range".to_string())?;
        if source.version == completed
            && source.status == CredentialStatus::Archived
            && source.material_ref.is_none()
            && source.auxiliary_material_refs.is_empty()
        {
            return Ok(());
        }
        revoke_credential_exact(
            &source_id,
            expected,
            CredentialRetirement::Archive,
            self.secrets.as_ref(),
            self.repository.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    /// The vault→MCP binding seam. Scan active vaults in caller-supplied order
    /// for an active `mcp_oauth` or `static_bearer` credential whose normalized
    /// server URL equals `url`. Vault order is the precedence contract; id order
    /// is only a defensive tie-breaker inside one vault.
    pub async fn mcp_credential_source_for_url(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, CredentialError> {
        let Ok(requested) = awaken_agent_contract::McpTarget::identity(url) else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let Some(credentials) = self.executable_credentials(workspace_id, vault_id).await?
            else {
                continue;
            };
            if let Some(credential) = credentials.into_iter().find(|credential| {
                auth_mcp_server_url(&credential.auth)
                    .and_then(|url| awaken_agent_contract::McpTarget::identity(url).ok())
                    .is_some_and(|candidate| candidate == requested)
            }) {
                return Ok(Some(credential.source_id));
            }
        }
        Ok(None)
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// credential source. The open deployment reads only the active revision;
    /// an installed hosted issuer may open that exact material solely to attach
    /// the existing recipient-bound envelope to the returned secret-free pin.
    pub async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        request: SessionCredentialAccessRequest,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, CredentialError> {
        let SessionCredentialAccessRequest {
            target,
            usage,
            policy,
            selected_holder,
            binding,
        } = request;
        self.exact_access(source_id, |now_unix_ms| ExactCredentialAccessRequest {
            workspace_id: Some(workspace_id),
            target: Some(target),
            usage,
            policy,
            selected_holder: &selected_holder,
            binding: &binding,
            now_unix_ms,
        })
        .await
        .map(|(_, access)| access)
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// MCP credential. Without a hosted issuer this reads only the revision and
    /// opaque references; with one it seals that exact revision for the holder.
    pub async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, CredentialError> {
        let holder = awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
            .mcp_holder;
        let usage = awaken_agent_contract::credential::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        };
        let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
            "unscoped-control",
            &source_id.0,
            &usage,
        );
        let source = self.source(source_id).await?;
        let record = self
            .credential_of_source(&source.workspace_id, source_id)
            .await?
            .ok_or_else(|| {
                CredentialError::InvalidSource(
                    "MCP credential source has no authoritative Managed child".into(),
                )
            })?;
        let url = match record.auth {
            ManagedCredentialAuth::StaticBearer { mcp_server_url }
            | ManagedCredentialAuth::McpOauth { mcp_server_url, .. } => mcp_server_url,
            ManagedCredentialAuth::EnvironmentVariable { .. } => {
                return Err(CredentialError::InvalidSource(
                    "credential source is not owned by an MCP authorization".into(),
                ));
            }
        };
        let target = awaken_agent_contract::McpTarget::parse_http(&url).map_err(|_| {
            CredentialError::InvalidSource("MCP credential target is invalid".into())
        })?;
        self.compile_mcp_access_for_source(source_id, None, &target, &holder, &binding)
            .await
    }

    /// The single MCP execution-pin compiler. `workspace_id` narrows admission
    /// at the existing exact row read; refresh projection is identical for
    /// scoped and unscoped trusted callers.
    async fn compile_mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: Option<&str>,
        target: &awaken_agent_contract::McpTarget,
        selected_holder: &awaken_agent_contract::credential::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, CredentialError> {
        let target_url = target.http_url().ok_or_else(|| {
            CredentialError::InvalidSource("MCP credentials require an HTTP target".into())
        })?;
        let target_audience = awaken_agent_contract::McpTarget::identity(target_url)
            .map_err(|_| CredentialError::InvalidSource("MCP credential target is invalid".into()))?
            .canonical_url();
        self.mcp_access(
            source_id,
            workspace_id,
            target_audience,
            selected_holder,
            binding,
        )
        .await
    }
}

#[async_trait::async_trait]
impl SessionCredentialSource for VaultCredentialAccess {
    async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, String> {
        self.has_vault(workspace_id, id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn mcp_credential_source_for_url(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String> {
        VaultCredentialAccess::mcp_credential_source_for_url(self, workspace_id, vault_ids, url)
            .await
            .map_err(|error| error.to_string())
    }

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        target: &awaken_agent_contract::McpTarget,
        selected_holder: &awaken_agent_contract::credential::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, String> {
        self.compile_mcp_access_for_source(
            source_id,
            Some(workspace_id),
            target,
            selected_holder,
            binding,
        )
        .await
        .map_err(|error| error.to_string())
    }

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        request: SessionCredentialAccessRequest,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, String> {
        VaultCredentialAccess::credential_access_for_source(self, source_id, workspace_id, request)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl CredentialMaterialIngress for VaultCredentialAccess {
    async fn enter_material(
        &self,
        command: CredentialMaterialIngressCommand,
    ) -> Result<MaterialEntry, String> {
        let CredentialMaterialIngressCommand {
            source_id,
            workspace_id,
            target,
            usage,
            material,
        } = command;
        let (provider, material_descriptor, material) = encode_material(material)?;
        let descriptor = CredentialDescriptor::new(
            provider,
            material_descriptor,
            [CredentialTargetContract::new(target, usage)],
        );
        self.enter_material(source_id, &workspace_id, descriptor, material)
            .await
    }

    async fn rotate_material(
        &self,
        command: CredentialMaterialRotationCommand,
    ) -> Result<u64, String> {
        let CredentialMaterialRotationCommand {
            source_id,
            expected_revision,
            workspace_id,
            target,
            usage,
            material,
        } = command;
        let (provider, material_descriptor, encoded_material) = encode_material(material)?;
        let expected_descriptor = CredentialDescriptor::new(
            provider,
            material_descriptor,
            [CredentialTargetContract::new(target, usage)],
        );
        self.rotate_material(
            &source_id,
            expected_revision,
            &workspace_id,
            &expected_descriptor,
            encoded_material,
        )
        .await
    }

    async fn retire_material(
        &self,
        command: CredentialMaterialRetirementCommand,
    ) -> Result<(), String> {
        let CredentialMaterialRetirementCommand {
            credential,
            workspace_id,
        } = command;
        self.retire_material(credential, &workspace_id).await
    }
}

fn encode_material(
    material: CredentialMaterialInput,
) -> Result<
    (
        String,
        CredentialMaterialDescriptor,
        awaken_agent_contract::RedactedString,
    ),
    String,
> {
    let descriptor = match &material.plaintext {
        CredentialPlaintext::Opaque(_) => {
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE)
        }
        CredentialPlaintext::Structured(structured) => CredentialMaterialDescriptor::structured(
            structured.type_id.clone(),
            structured.fields.keys().cloned(),
        ),
    };
    let encoded = match material.plaintext {
        CredentialPlaintext::Opaque(value) => value,
        CredentialPlaintext::Structured(structured) => {
            crate::encode_structured_material(structured).map_err(|error| error.to_string())?
        }
    };
    Ok((material.provider, descriptor, encoded))
}

fn auth_mcp_server_url(auth: &ManagedCredentialAuth) -> Option<&str> {
    match auth {
        ManagedCredentialAuth::StaticBearer { mcp_server_url }
        | ManagedCredentialAuth::McpOauth { mcp_server_url, .. } => Some(mcp_server_url),
        ManagedCredentialAuth::EnvironmentVariable { .. } => None,
    }
}

/// Compile the exact OAuth refresh/reseal instruction for one credential
/// revision from its persisted refresh configuration and sealed material refs.
fn oauth_refresh_access(
    source: &CredentialSource,
    revision: u64,
    refresh: ManagedMcpOauthRefresh,
) -> Result<CredentialRefreshAccess, CredentialError> {
    let source_id = &source.id;
    let access_token_ref = source
        .material_ref
        .as_ref()
        .ok_or_else(|| CredentialError::MissingMaterialRef(source_id.0.clone()))?
        .0
        .clone();
    let refresh_token_ref = source
        .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
        .ok_or_else(|| {
            CredentialError::MissingMaterialRef(format!(
                "{}:{OAUTH_REFRESH_TOKEN_SLOT}",
                source_id.0
            ))
        })?
        .0
        .clone();
    let client_secret_ref = match refresh.token_endpoint_auth {
        TokenEndpointAuth::None => None,
        TokenEndpointAuth::ClientSecretBasic | TokenEndpointAuth::ClientSecretPost => Some(
            source
                .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                .ok_or_else(|| {
                    CredentialError::MissingMaterialRef(format!(
                        "{}:{OAUTH_CLIENT_SECRET_SLOT}",
                        source_id.0
                    ))
                })?
                .0
                .clone(),
        ),
    };
    Ok(CredentialRefreshAccess::new(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_access::tests::PublicationProbe;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn access_construction_freezes_the_original_backend_and_delivery_arcs() {
        // I1 cause/effect table: C1 selected delivery is absent, Envelope, or
        // Custody; C2 the same Repo/Secrets/Clock arrive at VaultApplication.
        // For every C1+C2 row, Access retains those exact allocations, repeated
        // composition reads return one Access, and construction does no material
        // read/delivery. This is identity/no-I/O evidence only: the original
        // Managed envelope/custody issuance cases still prove real publication.
        // The reused PublicationProbe rejects any accidental Envelope issue,
        // counts it, and cannot generate a fabricated successful envelope.
        for selection in 0..3 {
            let probe = Arc::new(PublicationProbe {
                reads: AtomicUsize::new(0),
                deliveries: AtomicUsize::new(0),
                handles: true,
                fail: true,
            });
            let secrets: Arc<dyn SecretStore> = probe.clone();
            let repository: Arc<dyn ManagedCredentialRepository> =
                Arc::new(crate::repo::InMemoryCredentialRepo::new());
            let clock = awaken_time::system_clock();
            let delivery = match selection {
                0 => None,
                1 => Some(CredentialMaterialDelivery::RecipientEnvelope(probe.clone())),
                _ => Some(CredentialMaterialDelivery::ExternalCustody(probe.clone())),
            };
            let application = crate::VaultApplication::new(
                secrets.clone(),
                repository.clone(),
                clock.clone(),
                delivery.clone(),
            );
            let access = application.credential_access();
            assert!(Arc::ptr_eq(&access, &application.credential_access()));
            assert!(Arc::ptr_eq(&secrets, &access.secrets));
            assert!(Arc::ptr_eq(&repository, &access.repository));
            assert!(Arc::ptr_eq(&clock, &access.clock));
            match (&delivery, &access.material_delivery) {
                (None, None) => {}
                (
                    Some(CredentialMaterialDelivery::RecipientEnvelope(expected)),
                    Some(CredentialMaterialDelivery::RecipientEnvelope(actual)),
                ) => assert!(Arc::ptr_eq(expected, actual)),
                (
                    Some(CredentialMaterialDelivery::ExternalCustody(expected)),
                    Some(CredentialMaterialDelivery::ExternalCustody(actual)),
                ) => assert!(Arc::ptr_eq(expected, actual)),
                _ => panic!("selected delivery must retain its variant and allocation"),
            }
            assert_eq!(probe.reads.load(Ordering::SeqCst), 0);
            assert_eq!(probe.deliveries.load(Ordering::SeqCst), 0);
        }
    }
}
