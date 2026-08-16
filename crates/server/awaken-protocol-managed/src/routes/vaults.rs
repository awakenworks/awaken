//! The Managed Agents **vault / credential** front door (ADR-0043).
//!
//! These are the public `/v1/vaults...` routes the official `@anthropic-ai/sdk`
//! `beta.vaults.*` client calls. The DTOs mirror the SDK types exactly
//! (`BetaManagedAgentsVault`, `BetaManagedAgentsCredential`, the per-type
//! auth/create shapes, and `BetaManagedAgentsCredentialValidation`); the wire
//! keeps Anthropic's snake_case tags (`vault` / `vault_credential` /
//! `environment_variable` / `static_bearer` / `mcp_oauth`).
//!
//! Storage is neutral: a credential's secret is sealed into the credential
//! domain's [`SecretStore`](awaken_credential_vault::SecretStore) via the
//! this adapter's vault ACL (secret-in), and every response is secret-free
//! (secret-out never happens). The credential rows land in a
//! [`CredentialRepo`](awaken_credential_vault::repo::CredentialRepo), so a vault
//! credential entered here is the same row the resolver binds a run to.
//!
//! Scope (Phase 3): all three credential types — `environment_variable`,
//! `static_bearer`, and `mcp_oauth`. Every wire secret (`secret_value`, `token`,
//! `access_token`, `refresh_token`, `client_secret`) is write-only: sealed into
//! the `SecretStore` on the way in as one revisioned credential material set,
//! never present in any response. The MCP-OAuth validate route live-probes the
//! MCP server when the process startup wires an [`McpProbe`]
//! ([`VaultState::with_probe`]): the credential's access token is materialized
//! here and the port receives the resolved secret, never a vault ref. Without a
//! probe — and always for `environment_variable` / `static_bearer`, which have no
//! MCP handshake to probe — `status` stays `unknown` (never a false `valid`).
//! [`VaultState::mcp_credential_source_for_url`] is the seam a session uses to
//! bind an MCP server to a vault credential by URL. Once selected,
//! [`VaultState::mcp_access_for_source`] compiles the sole exact credential and
//! refresh/reseal execution value.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::control::vault_acl::{
    WireEnvVarCreate, WireMcpOauthCreate, WireStaticBearerCreate, env_var_to_create_params,
    mcp_oauth_to_create_params, static_bearer_to_create_params,
};
use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_contract::{CredentialEnvelopeIssuance, CredentialEnvelopeIssuer};
use awaken_credential_vault::catalog::{
    ManagedCredentialAdmissionError, ManagedCredentialAuth as AuthRecord,
    ManagedCredentialNetworking, ManagedMcpOauthRefresh as McpOauthRefreshRecord,
    ManagedVault as VaultRecord, ManagedVaultCredential as CredentialRecord,
    ManagedVaultMutationError, ManagedVaultRepo,
};
use awaken_credential_vault::repo::{
    ApplicationMcpBearerCommand, CredentialMaterialPatch, CredentialRepo, CredentialRetirement,
    advance_credential_revision, enter_credential, enter_credential_with_materials,
    enter_or_rotate_application_mcp_bearer, revoke_credential, rotate_credential_materials,
};
use awaken_credential_vault::{
    CredentialCreateParams as DomainCredentialCreateParams, CredentialKind,
    OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore, StructuredCredentialMaterial,
};
use awaken_session_application::{RepositoryCredentialIngress, SessionCredentialSource};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::routes::{ManagedJson, WorkspaceScope, sha256_identity};
use crate::types::vault::{
    Credential, CredentialAuth, CredentialCreateParams, CredentialCreateWire, CredentialNetworking,
    CredentialUpdateAuth, CredentialUpdateParams, CredentialValidation, CredentialValidationStatus,
    DeletedCredential, DeletedVault, ListQuery, McpOauthRefreshResponse, McpProbeResult,
    TokenEndpointAuthParams, TokenEndpointAuthResponse, TokenEndpointAuthUpdate, Vault,
    VaultCreateParams, VaultUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};

/// Deterministic timestamp stamped on every vault/credential object, matching the
/// session surface's `PROCESSED_AT` convention (no wall-clock/uuid dependency, so
/// the wire is reproducible under test).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

fn application_mcp_target_fingerprint(
    url: &str,
) -> Result<String, awaken_session_contract::McpTargetError> {
    let identity = awaken_session_contract::McpTarget::identity(url)?;
    let port = identity.port.map(|port| port.to_string());
    Ok(sha256_identity(
        "application-mcp-normalized-target-v1",
        &[
            &identity.scheme,
            &identity.host,
            port.as_deref().unwrap_or(""),
            &identity.path,
            if identity.query.is_some() {
                "query"
            } else {
                ""
            },
            identity.query.as_deref().unwrap_or(""),
        ],
    ))
}

fn application_mcp_vault_id(workspace_id: &str, authority_id: &str) -> String {
    format!(
        "vlt_app_{}",
        sha256_identity("application-mcp-vault-v1", &[workspace_id, authority_id])
    )
}

fn application_mcp_source_id(vault_id: &str, target_fingerprint: &str) -> CredentialSourceId {
    CredentialSourceId(format!(
        "cred:app-mcp:{vault_id}:{}",
        sha256_identity("application-mcp-target-v1", &[target_fingerprint])
    ))
}

// Wire DTOs live in `crate::types::vault` (1:1 with @anthropic-ai/sdk
// beta.vaults.*). This module owns the store, the secret sealing, the record→wire
// projection, and the internal binding vocabulary below.

/// Apply a metadata patch in place: a `Some(v)` upserts the key, a `None` (JSON
/// `null`) deletes it, and any key absent from the patch is preserved. Shared by
/// vault + credential update.
fn apply_metadata_patch(
    target: &mut BTreeMap<String, String>,
    patch: BTreeMap<String, Option<String>>,
) {
    for (key, value) in patch {
        match value {
            Some(v) => {
                target.insert(key, v);
            }
            None => {
                target.remove(&key);
            }
        }
    }
}

pub(crate) use awaken_session_contract::{McpProbe, McpProbeStatus};

// ---- State ------------------------------------------------------------------

fn auth_mcp_server_url(auth: &AuthRecord) -> Option<&str> {
    match auth {
        AuthRecord::StaticBearer { mcp_server_url }
        | AuthRecord::McpOauth { mcp_server_url, .. } => Some(mcp_server_url),
        AuthRecord::EnvironmentVariable { .. } => None,
    }
}

/// The vault surface's state: the neutral credential domain stores plus the
/// durable, secret-free Managed projection.
pub struct VaultState {
    secrets: Arc<dyn SecretStore>,
    credentials: Arc<dyn CredentialRepo>,
    vaults: Arc<dyn ManagedVaultRepo>,
    /// The live MCP probe the validate route consults for `mcp_oauth`
    /// credentials, when the process startup wires one. `None` keeps every
    /// validation `unknown` (never a false `valid`).
    probe: Option<Arc<dyn McpProbe>>,
    envelope_issuer: Option<Arc<dyn CredentialEnvelopeIssuer>>,
}

impl VaultState {
    /// Build the vault surface over the credential domain's secret store + repo.
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        credentials: Arc<dyn CredentialRepo>,
        vaults: Arc<dyn ManagedVaultRepo>,
    ) -> Self {
        Self {
            secrets,
            credentials,
            vaults,
            probe: None,
            envelope_issuer: None,
        }
    }

    /// Create or rotate one hosted application's stable MCP bearer in the
    /// authoritative credential aggregate. HTTP ownership stays with Awaken
    /// Control; this method returns only secret-free receipt facts.
    pub async fn enter_application_mcp_bearer(
        &self,
        workspace_id: &str,
        application_authority_id: &str,
        mcp_server_url: &str,
        idempotency_key: &str,
        bearer: RedactedString,
    ) -> Result<(String, CredentialSourceId, u64), awaken_credential_vault::CredentialError> {
        let target_fingerprint =
            application_mcp_target_fingerprint(mcp_server_url).map_err(|_| {
                awaken_credential_vault::CredentialError::InvalidSource(
                    "MCP server URL must be an absolute HTTP(S) URL".into(),
                )
            })?;
        let vault_id = application_mcp_vault_id(workspace_id, application_authority_id);
        let source_id = application_mcp_source_id(&vault_id, &target_fingerprint);
        let command_key_fingerprint = sha256_identity(
            "application-mcp-command-key-v1",
            &[
                workspace_id,
                application_authority_id,
                &target_fingerprint,
                idempotency_key,
            ],
        );
        let source = enter_or_rotate_application_mcp_bearer(
            ApplicationMcpBearerCommand {
                source_id,
                workspace_id: workspace_id.to_owned(),
                target_fingerprint,
                command_key_fingerprint,
                bearer,
            },
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await?;
        self.vaults
            .put_vault(
                workspace_id,
                VaultRecord {
                    id: vault_id.clone(),
                    workspace_id: workspace_id.to_owned(),
                    display_name: application_authority_id.to_owned(),
                    metadata: BTreeMap::new(),
                    archived_at: None,
                    revision: 1,
                },
            )
            .await?;
        self.vaults
            .put_vault_credential(
                workspace_id,
                CredentialRecord {
                    id: format!(
                        "crd_app_{}",
                        sha256_identity("application-mcp-credential-v1", &[&source.id.0])
                    ),
                    vault_id: vault_id.clone(),
                    workspace_id: workspace_id.to_owned(),
                    source_id: source.id.clone(),
                    auth: AuthRecord::StaticBearer {
                        mcp_server_url: mcp_server_url.to_owned(),
                    },
                    metadata: BTreeMap::new(),
                    display_name: None,
                    archived_at: None,
                },
            )
            .await?;
        let revision = u64::try_from(source.version).map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "application MCP credential revision is invalid".into(),
            )
        })?;
        Ok((vault_id, source.id, revision))
    }

    /// Wire the live MCP probe, so `POST .../mcp_oauth_validate` reports a real
    /// `valid`/`invalid` verdict for an `mcp_oauth` credential instead of
    /// `unknown`.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn McpProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Install the deployment-owned cryptographic transport adapter. Selection,
    /// revision, holder, usage and target binding remain Vault/Session facts;
    /// the adapter can only seal that exact request.
    #[must_use]
    pub fn with_envelope_issuer(mut self, issuer: Arc<dyn CredentialEnvelopeIssuer>) -> Self {
        self.envelope_issuer = Some(issuer);
        self
    }

    /// Whether `id` names an active vault that may be attached to a new Session.
    /// Archived vaults remain retrievable for audit but are not executable.
    pub async fn has_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<bool, awaken_credential_vault::CredentialError> {
        Ok(self
            .vaults
            .get_vault(workspace_id, id)
            .await?
            .is_some_and(|vault| vault.archived_at.is_none()))
    }

    /// Seal a write-only compatibility token and return only its neutral source
    /// id. Used when a Managed repository resource carries an inline token; the
    /// Session manifest and Resource Catalog never receive the token value.
    pub async fn enter_session_bearer(
        &self,
        workspace_id: &str,
        token: String,
    ) -> Result<CredentialSourceId, awaken_credential_vault::CredentialError> {
        let material = StructuredCredentialMaterial {
            type_id: awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE.to_string(),
            fields: BTreeMap::from([
                ("username".into(), RedactedString::new("x-access-token")),
                ("password".into(), RedactedString::from(token)),
            ]),
        };
        let material = awaken_credential_vault::encode_structured_material(material)?;
        enter_credential(
            DomainCredentialCreateParams {
                workspace_id: workspace_id.to_string(),
                kind: CredentialKind::Vault,
                provider_id: Some("git".into()),
                env_key: None,
                secret: Some(material),
                oauth_command: None,
            },
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await
        .map(|source| source.id)
    }

    /// The neutral credential-domain row id for a wire credential id, if it lives
    /// in `vault_id`. This is the seam a session uses to bind a vault credential to
    /// a run: the resolver takes this `CredentialSourceId`, never the wire id.
    pub async fn credential_source_id(
        &self,
        vault_id: &str,
        credential_id: &str,
    ) -> Option<CredentialSourceId> {
        self.credential_source_id_in_workspace(crate::state::DEFAULT_SCOPE, vault_id, credential_id)
            .await
    }

    /// Workspace-authoritative form used by hosted/session callers. The
    /// compatibility helper above is limited to the standalone installation
    /// Workspace and cannot discover another Workspace by id.
    async fn credential_source_id_in_workspace(
        &self,
        workspace_id: &str,
        vault_id: &str,
        credential_id: &str,
    ) -> Option<CredentialSourceId> {
        self.vaults
            .get_vault_credential(workspace_id, credential_id)
            .await
            .ok()
            .flatten()
            .filter(|credential| credential.vault_id == vault_id)
            .map(|credential| credential.source_id)
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
    ) -> Result<Option<CredentialSourceId>, awaken_credential_vault::CredentialError> {
        let Ok(requested) = awaken_session_contract::McpTarget::identity(url) else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let vault_is_active = self
                .vaults
                .get_vault(workspace_id, vault_id)
                .await?
                .is_some_and(|vault| vault.archived_at.is_none());
            if !vault_is_active {
                continue;
            }
            let mut credentials = self
                .vaults
                .list_vault_credentials(workspace_id, vault_id)
                .await?;
            credentials.sort_by(|left, right| left.id.cmp(&right.id));
            if let Some(credential) = credentials
                .into_iter()
                .filter(|credential| credential.archived_at.is_none())
                .find(|credential| {
                    auth_mcp_server_url(&credential.auth)
                        .and_then(|url| awaken_session_contract::McpTarget::identity(url).ok())
                        .is_some_and(|candidate| candidate == requested)
                })
            {
                return Ok(Some(credential.source_id));
            }
        }
        Ok(None)
    }

    async fn exact_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: Option<&str>,
        usage: awaken_credential_contract::CredentialUsage,
        policy: awaken_credential_contract::CredentialExecutionPolicy,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<
        (
            awaken_credential_vault::CredentialSource,
            awaken_credential_contract::CredentialAccess,
        ),
        awaken_credential_vault::CredentialError,
    > {
        use awaken_credential_contract::{
            CredentialAccess, CredentialMaterialSource, CredentialRef,
        };

        let source = self.credentials.get(source_id).await?;
        if source.status != awaken_credential_vault::CredentialStatus::Active {
            return Err(awaken_credential_vault::CredentialError::NotActive(
                source_id.0.clone(),
            ));
        }
        if workspace_id.is_some_and(|workspace_id| source.workspace_id != workspace_id) {
            return Err(awaken_credential_vault::CredentialError::InvalidSource(
                "credential source belongs to another Workspace".into(),
            ));
        }
        binding.validate().map_err(|error| {
            awaken_credential_vault::CredentialError::InvalidSource(error.to_string())
        })?;
        if workspace_id.is_some_and(|workspace_id| binding.workspace_id != workspace_id) {
            return Err(awaken_credential_vault::CredentialError::InvalidSource(
                "credential material binding belongs to another Workspace".into(),
            ));
        }
        if !policy.allowed_plaintext_holders.contains(selected_holder) {
            return Err(awaken_credential_vault::CredentialError::InvalidSource(
                "selected plaintext holder is not authorized by credential policy".into(),
            ));
        }
        let revision = u64::try_from(source.version).map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "credential revision is negative".into(),
            )
        })?;
        let mut access = CredentialAccess::new(
            CredentialRef {
                id: source_id.0.clone(),
                revision,
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            policy,
        );
        if let Some(issuer) = &self.envelope_issuer {
            let material =
                awaken_credential_vault::materialize(&source, self.secrets.as_ref()).await?;
            let envelope = issuer
                .issue(CredentialEnvelopeIssuance {
                    access: access.clone(),
                    selected_holder: selected_holder.clone(),
                    binding: binding.clone(),
                    material,
                })
                .await
                .map_err(awaken_credential_vault::CredentialError::InvalidSource)?;
            envelope
                .validate_issuance(&access, selected_holder, binding)
                .map_err(|error| {
                    awaken_credential_vault::CredentialError::InvalidSource(error.to_string())
                })?;
            access = access.with_envelope(envelope);
        }
        Ok((source, access))
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// credential source. The open deployment reads only the active revision;
    /// an installed hosted issuer may open that exact material solely to attach
    /// the existing recipient-bound envelope to the returned secret-free pin.
    pub async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        usage: awaken_credential_contract::CredentialUsage,
        policy: awaken_credential_contract::CredentialExecutionPolicy,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        self.exact_access_for_source(
            source_id,
            Some(workspace_id),
            usage,
            policy,
            selected_holder,
            binding,
        )
        .await
        .map(|(_, access)| access)
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// MCP credential. Without a hosted issuer this reads only the revision and
    /// opaque references; with one it seals that exact revision for the holder.
    pub async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        let holder = awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
            .mcp_holder;
        let usage = awaken_credential_contract::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        };
        let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
            "unscoped-control",
            &source_id.0,
            &usage,
        );
        self.compile_mcp_access_for_source(source_id, None, &holder, &binding)
            .await
    }

    /// The single MCP execution-pin compiler. `workspace_id` narrows admission
    /// at the existing exact row read; refresh projection is identical for
    /// scoped and unscoped trusted callers.
    async fn compile_mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: Option<&str>,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        use awaken_credential_contract::{
            CredentialExecutionPolicy, CredentialRefreshAccess, CredentialUsage,
        };

        let (source, mut access) = self
            .exact_access_for_source(
                source_id,
                workspace_id,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::self_hosted_mcp(),
                selected_holder,
                binding,
            )
            .await?;
        let revision = access.credential.revision;
        let refresh = self
            .vaults
            .get_vault_credential_by_source(&source.workspace_id, source_id)
            .await?
            .and_then(|record| match record.auth {
                AuthRecord::McpOauth { refresh, .. } => refresh,
                _ => None,
            });
        if let Some(refresh) = refresh {
            let access_token_ref = source
                .material_ref
                .as_ref()
                .ok_or_else(|| {
                    awaken_credential_vault::CredentialError::MissingMaterialRef(
                        source_id.0.clone(),
                    )
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
            access = access.with_refresh(CredentialRefreshAccess::new(
                revision,
                refresh.token_endpoint,
                refresh.client_id,
                refresh.token_endpoint_auth,
                client_secret_ref,
                refresh_token_ref,
                access_token_ref,
                refresh.scope,
                refresh.resource,
            ));
        }
        Ok(access)
    }

    fn project_vault(record: &VaultRecord) -> Vault {
        Vault {
            id: record.id.clone(),
            archived_at: record.archived_at.clone(),
            created_at: OBJECT_AT.to_string(),
            display_name: record.display_name.clone(),
            metadata: record.metadata.clone(),
            object_type: "vault",
            updated_at: OBJECT_AT.to_string(),
        }
    }

    fn project_credential(record: &CredentialRecord) -> Credential {
        let auth = match &record.auth {
            AuthRecord::EnvironmentVariable {
                secret_name,
                networking,
            } => CredentialAuth::EnvironmentVariable {
                secret_name: secret_name.clone(),
                networking: match networking {
                    ManagedCredentialNetworking::Unrestricted => CredentialNetworking::Unrestricted,
                    ManagedCredentialNetworking::Limited { allowed_hosts } => {
                        CredentialNetworking::Limited {
                            allowed_hosts: allowed_hosts.clone(),
                        }
                    }
                },
            },
            AuthRecord::StaticBearer { mcp_server_url } => CredentialAuth::StaticBearer {
                mcp_server_url: mcp_server_url.clone(),
            },
            AuthRecord::McpOauth {
                mcp_server_url,
                expires_at,
                refresh,
            } => CredentialAuth::McpOauth {
                mcp_server_url: mcp_server_url.clone(),
                expires_at: expires_at.clone(),
                refresh: refresh.as_ref().map(|r| McpOauthRefreshResponse {
                    client_id: r.client_id.clone(),
                    token_endpoint: r.token_endpoint.clone(),
                    token_endpoint_auth: match r.token_endpoint_auth {
                        awaken_credential_contract::TokenEndpointAuth::None => {
                            TokenEndpointAuthResponse::None
                        }
                        awaken_credential_contract::TokenEndpointAuth::ClientSecretBasic => {
                            TokenEndpointAuthResponse::ClientSecretBasic
                        }
                        awaken_credential_contract::TokenEndpointAuth::ClientSecretPost => {
                            TokenEndpointAuthResponse::ClientSecretPost
                        }
                    },
                    resource: r.resource.clone(),
                    scope: r.scope.clone(),
                }),
            },
        };
        Credential {
            id: record.id.clone(),
            archived_at: record.archived_at.clone(),
            auth,
            created_at: OBJECT_AT.to_string(),
            metadata: record.metadata.clone(),
            object_type: "vault_credential",
            updated_at: OBJECT_AT.to_string(),
            vault_id: record.vault_id.clone(),
            display_name: record.display_name.clone(),
        }
    }
}

#[async_trait::async_trait]
impl SessionCredentialSource for VaultState {
    async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, String> {
        VaultState::has_vault(self, workspace_id, id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn mcp_credential_source_for_url(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String> {
        VaultState::mcp_credential_source_for_url(self, workspace_id, vault_ids, url)
            .await
            .map_err(|error| error.to_string())
    }

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        self.compile_mcp_access_for_source(source_id, Some(workspace_id), selected_holder, binding)
            .await
            .map_err(|error| error.to_string())
    }

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        usage: awaken_credential_contract::CredentialUsage,
        policy: awaken_credential_contract::CredentialExecutionPolicy,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        VaultState::credential_access_for_source(
            self,
            source_id,
            workspace_id,
            usage,
            policy,
            selected_holder,
            binding,
        )
        .await
        .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl RepositoryCredentialIngress for VaultState {
    async fn enter_repository_token(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        token: RedactedString,
    ) -> Result<CredentialSourceId, String> {
        let entry = awaken_credential_vault::repo::enter_credential_idempotent(
            source_id,
            DomainCredentialCreateParams {
                workspace_id: workspace_id.to_string(),
                kind: CredentialKind::Vault,
                provider_id: Some("github_repository".into()),
                env_key: None,
                secret: Some(token),
                oauth_command: None,
            },
            None,
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(entry.source.id)
    }

    async fn rotate_repository_token(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        token: RedactedString,
    ) -> Result<(), String> {
        let current = self
            .credentials
            .get(source_id)
            .await
            .map_err(|error| error.to_string())?;
        if current.workspace_id != workspace_id
            || current.provider_id.as_deref() != Some("github_repository")
        {
            return Err("repository credential binding is unavailable in this Workspace".into());
        }
        rotate_credential_materials(
            source_id,
            CredentialMaterialPatch {
                primary: Some(token),
                auxiliary: BTreeMap::new(),
            },
            self.secrets.as_ref(),
            self.credentials.as_ref(),
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }
}

// ---- Router -----------------------------------------------------------------

/// The Managed vault/credential routes. Mount alongside the session router.
pub fn vault_router(state: Arc<VaultState>) -> Router {
    Router::new()
        .route("/v1/vaults", post(create_vault).get(list_vaults))
        .route(
            "/v1/vaults/{id}",
            get(retrieve_vault).post(update_vault).delete(delete_vault),
        )
        .route("/v1/vaults/{id}/archive", post(archive_vault))
        .route(
            "/v1/vaults/{vault_id}/credentials",
            post(create_credential).get(list_credentials),
        )
        .route(
            "/v1/vaults/{vault_id}/credentials/{id}",
            get(retrieve_credential)
                .post(update_credential)
                .delete(delete_credential),
        )
        .route(
            "/v1/vaults/{vault_id}/credentials/{id}/archive",
            post(archive_credential),
        )
        .route(
            "/v1/vaults/{vault_id}/credentials/{id}/mcp_oauth_validate",
            post(validate_credential),
        )
        .with_state(state)
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn not_found(what: &str) -> WireError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "not_found_error",
            format!("{what} not found"),
        )),
    )
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

fn storage_error(error: impl std::fmt::Display) -> WireError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new("api_error", error.to_string())),
    )
}

fn vault_mutation_error(error: ManagedVaultMutationError) -> WireError {
    match error {
        ManagedVaultMutationError::NotFound => not_found("vault"),
        ManagedVaultMutationError::RevisionConflict => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "conflict_error",
                "vault changed concurrently; retry the request",
            )),
        ),
        ManagedVaultMutationError::RevisionExhausted => storage_error(error),
        ManagedVaultMutationError::Store(error) => storage_error(error),
    }
}

/// Resolve the trusted Workspace stamped by the product edge. Standalone/local
/// composition preserves its documented installation Workspace fallback here,
/// before any Vault repository operation receives authority.
fn request_workspace(scope: Option<Extension<WorkspaceScope>>) -> String {
    scope.map_or_else(
        || crate::state::DEFAULT_SCOPE.to_string(),
        |Extension(scope)| scope.0,
    )
}

async fn create_vault(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<VaultCreateParams>,
) -> Result<(StatusCode, Json<Vault>), WireError> {
    if params.display_name.is_empty() || params.display_name.len() > 255 {
        return Err(bad_request("display_name must be 1-255 characters"));
    }
    let id = format!("vlt_{}", uuid::Uuid::now_v7().simple());
    let workspace_id = request_workspace(scope);
    let record = VaultRecord {
        id,
        workspace_id: workspace_id.clone(),
        display_name: params.display_name,
        metadata: params.metadata,
        archived_at: None,
        revision: 1,
    };
    let vault = VaultState::project_vault(&record);
    state
        .vaults
        .put_vault(&workspace_id, record)
        .await
        .map_err(storage_error)?;
    Ok((StatusCode::OK, Json(vault)))
}

async fn retrieve_vault(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let workspace_id = request_workspace(scope);
    let record = state
        .vaults
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| not_found("vault"))?;
    Ok(Json(VaultState::project_vault(&record)))
}

/// `GET /v1/vaults` — one full page of vaults (the SDK `beta.vaults.list`).
/// Deterministic order: ascending wire id (`vlt_…` is zero-padded, so
/// lexicographic == creation order). Archived vaults are excluded unless
/// `?include_archived=true`.
async fn list_vaults(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Query(query): Query<ListQuery>,
    Query(page): Query<PageQuery>,
) -> Result<Json<PageCursor<Vault>>, WireError> {
    let workspace_id = request_workspace(scope);
    let mut records = state
        .vaults
        .list_vaults(&workspace_id)
        .await
        .map_err(storage_error)?;
    records.retain(|record| query.include_archived || record.archived_at.is_none());
    records.sort_by(|left, right| left.id.cmp(&right.id));
    let data = records.iter().map(VaultState::project_vault).collect();
    Ok(Json(paginate(data, &page, |v| v.id.as_str())))
}

async fn delete_vault(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedVault>, WireError> {
    let workspace_id = request_workspace(scope);
    if !state
        .vaults
        .delete_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
    {
        return Err(not_found("vault"));
    }
    Ok(Json(DeletedVault {
        id,
        object_type: "vault_deleted",
    }))
}

/// `POST /v1/vaults/:id/archive` — soft-delete (the SDK `beta.vaults.archive`).
/// Stamps `archived_at` and returns the vault; an already-archived vault is
/// re-stamped idempotently. The vault's credentials are unaffected (archiving a
/// vault does not cascade — only `DELETE` cascades).
async fn archive_vault(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let workspace_id = request_workspace(scope);
    let mut record = state
        .vaults
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| not_found("vault"))?;
    let expected_revision = record.revision;
    record.archived_at = Some(OBJECT_AT.to_string());
    record.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| vault_mutation_error(ManagedVaultMutationError::RevisionExhausted))?;
    state
        .vaults
        .replace_vault(&workspace_id, expected_revision, record.clone())
        .await
        .map_err(vault_mutation_error)?;
    Ok(Json(VaultState::project_vault(&record)))
}

/// `POST /v1/vaults/:id` — partial update (the SDK `beta.vaults.update`).
/// Replaces `display_name` when present and PATCHes `metadata`; returns the
/// updated vault. A `404` for an unknown vault. An empty body is a no-op update
/// that echoes the vault.
async fn update_vault(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<VaultUpdateParams>,
) -> Result<Json<Vault>, WireError> {
    let workspace_id = request_workspace(scope);
    if let Some(name) = &params.display_name
        && (name.is_empty() || name.len() > 255)
    {
        return Err(bad_request("display_name must be 1-255 characters"));
    }
    let mut record = state
        .vaults
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| not_found("vault"))?;
    let expected_revision = record.revision;
    if let Some(name) = params.display_name {
        record.display_name = name;
    }
    if let Some(patch) = params.metadata {
        apply_metadata_patch(&mut record.metadata, patch);
    }
    record.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| vault_mutation_error(ManagedVaultMutationError::RevisionExhausted))?;
    state
        .vaults
        .replace_vault(&workspace_id, expected_revision, record.clone())
        .await
        .map_err(vault_mutation_error)?;
    Ok(Json(VaultState::project_vault(&record)))
}

async fn create_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(vault_id): Path<String>,
    ManagedJson(params): ManagedJson<CredentialCreateWire>,
) -> Result<(StatusCode, Json<Credential>), WireError> {
    let params = params.into_params();
    // The vault id is a wire-side container id, not an authorization scope. The
    // platform-resolved workspace stamped at the startup edge owns the durable
    // credential row. Standalone embeddings that omit that edge use the documented
    // local/default workspace; tenancy is never derived from a resource id.
    let resource_workspace = request_workspace(scope);
    // Secret-in through the ACL: every raw secret crosses into the domain here and
    // is sealed by the SecretStore; the returned row is secret-free, and only the
    // kind-specific wire projection is kept on the record.
    let enter = |create| enter_credential(create, &*state.secrets, &*state.credentials);
    let (source, auth, metadata, display_name) = match params {
        CredentialCreateParams::EnvironmentVariable {
            secret_name,
            secret_value,
            networking,
            metadata,
            display_name,
        } => {
            let create = env_var_to_create_params(
                resource_workspace.clone(),
                None,
                WireEnvVarCreate {
                    secret_name: secret_name.clone(),
                    secret_value,
                },
            );
            let source = enter(create)
                .await
                .map_err(|e| bad_request(e.to_string()))?;
            let auth = AuthRecord::EnvironmentVariable {
                secret_name,
                networking: match networking {
                    CredentialNetworking::Unrestricted => ManagedCredentialNetworking::Unrestricted,
                    CredentialNetworking::Limited { allowed_hosts } => {
                        ManagedCredentialNetworking::Limited { allowed_hosts }
                    }
                },
            };
            (source, auth, metadata, display_name)
        }
        CredentialCreateParams::StaticBearer {
            mcp_server_url,
            token,
            metadata,
            display_name,
        } => {
            let create = static_bearer_to_create_params(
                resource_workspace.clone(),
                WireStaticBearerCreate { token },
            );
            let source = enter(create)
                .await
                .map_err(|e| bad_request(e.to_string()))?;
            (
                source,
                AuthRecord::StaticBearer { mcp_server_url },
                metadata,
                display_name,
            )
        }
        CredentialCreateParams::McpOauth {
            mcp_server_url,
            access_token,
            expires_at,
            refresh,
            metadata,
            display_name,
        } => {
            // Split the wire refresh object into write-only material and the
            // secret-free projection. All material is then entered as one
            // revisioned credential aggregate.
            let (refresh_config, refresh_token, client_secret) = match refresh {
                Some(r) => {
                    let (auth_tag, client_secret) = match r.token_endpoint_auth {
                        None | Some(TokenEndpointAuthParams::None) => {
                            (TokenEndpointAuthResponse::None, Option::None)
                        }
                        Some(TokenEndpointAuthParams::ClientSecretBasic { client_secret }) => (
                            TokenEndpointAuthResponse::ClientSecretBasic,
                            Some(client_secret),
                        ),
                        Some(TokenEndpointAuthParams::ClientSecretPost { client_secret }) => (
                            TokenEndpointAuthResponse::ClientSecretPost,
                            Some(client_secret),
                        ),
                    };
                    let projection = McpOauthRefreshResponse {
                        client_id: r.client_id,
                        token_endpoint: r.token_endpoint,
                        token_endpoint_auth: auth_tag,
                        resource: r.resource,
                        scope: r.scope,
                    };
                    (Some(projection), Some(r.refresh_token), client_secret)
                }
                None => (None, None, None),
            };
            let bridged = mcp_oauth_to_create_params(
                resource_workspace.clone(),
                WireMcpOauthCreate {
                    access_token,
                    refresh_token,
                },
            );
            let mut auxiliary = BTreeMap::new();
            let refresh_record = match (refresh_config, bridged.refresh_secret) {
                (Some(projection), Some(secret)) => {
                    auxiliary.insert(OAUTH_REFRESH_TOKEN_SLOT.to_string(), secret);
                    let token_endpoint_auth = match (projection.token_endpoint_auth, client_secret)
                    {
                        (TokenEndpointAuthResponse::ClientSecretBasic, Some(cs)) => {
                            auxiliary.insert(
                                OAUTH_CLIENT_SECRET_SLOT.to_string(),
                                RedactedString::new(cs),
                            );
                            awaken_credential_contract::TokenEndpointAuth::ClientSecretBasic
                        }
                        (TokenEndpointAuthResponse::ClientSecretPost, Some(cs)) => {
                            auxiliary.insert(
                                OAUTH_CLIENT_SECRET_SLOT.to_string(),
                                RedactedString::new(cs),
                            );
                            awaken_credential_contract::TokenEndpointAuth::ClientSecretPost
                        }
                        _ => awaken_credential_contract::TokenEndpointAuth::None,
                    };
                    Some(McpOauthRefreshRecord {
                        client_id: projection.client_id,
                        token_endpoint: projection.token_endpoint,
                        token_endpoint_auth,
                        resource: projection.resource,
                        scope: projection.scope,
                    })
                }
                _ => None,
            };
            let source = enter_credential_with_materials(
                bridged.params,
                auxiliary,
                &*state.secrets,
                &*state.credentials,
            )
            .await
            .map_err(|e| bad_request(e.to_string()))?;
            let auth = AuthRecord::McpOauth {
                mcp_server_url,
                expires_at,
                refresh: refresh_record,
            };
            (source, auth, metadata, display_name)
        }
    };

    let id = format!("crd_{}", uuid::Uuid::now_v7().simple());
    let record = CredentialRecord {
        id,
        vault_id,
        workspace_id: resource_workspace.clone(),
        source_id: source.id.clone(),
        auth,
        metadata,
        display_name,
        archived_at: None,
    };
    let credential = VaultState::project_credential(&record);
    let insertion = state
        .vaults
        .insert_vault_credential(&resource_workspace, record)
        .await;
    if let Err(error) = insertion {
        // The source is already a durable, tracked credential aggregate. Retire
        // it before returning an aggregate-admission error so a rejected child
        // never leaves executable orphan material.
        if let Err(retirement_error) = revoke_credential(
            &source.id,
            CredentialRetirement::Archive,
            state.secrets.as_ref(),
            state.credentials.as_ref(),
        )
        .await
        {
            return Err(storage_error(retirement_error));
        }
        return Err(match error {
            ManagedCredentialAdmissionError::VaultUnavailable => not_found("vault"),
            ManagedCredentialAdmissionError::LimitReached => {
                bad_request("vault credential limit reached (max 20)")
            }
            ManagedCredentialAdmissionError::DuplicateEnvironmentKey(secret_name) => bad_request(
                format!("credential key `{secret_name}` already exists in this vault"),
            ),
            ManagedCredentialAdmissionError::InvalidMcpUrl => {
                bad_request("mcp_server_url must be an absolute HTTP(S) URL")
            }
            ManagedCredentialAdmissionError::WorkspaceMismatch => {
                bad_request("credential workspace does not match request authority")
            }
            ManagedCredentialAdmissionError::Store(error) => storage_error(error),
        });
    }
    Ok((StatusCode::OK, Json(credential)))
}

/// `GET /v1/vaults/:vault_id/credentials` — one full page of a vault's
/// credentials (the SDK `beta.vaults.credentials.list`). An unknown vault is a
/// `404` (not an empty page), matching retrieve. Deterministic order: ascending
/// wire id. Archived credentials are excluded unless `?include_archived=true`.
async fn list_credentials(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(vault_id): Path<String>,
    Query(query): Query<ListQuery>,
    Query(page): Query<PageQuery>,
) -> Result<Json<PageCursor<Credential>>, WireError> {
    let workspace_id = request_workspace(scope);
    if state
        .vaults
        .get_vault(&workspace_id, &vault_id)
        .await
        .map_err(storage_error)?
        .is_none()
    {
        return Err(not_found("vault"));
    }
    let mut records = state
        .vaults
        .list_vault_credentials(&workspace_id, &vault_id)
        .await
        .map_err(storage_error)?;
    records.retain(|record| query.include_archived || record.archived_at.is_none());
    records.sort_by(|left, right| left.id.cmp(&right.id));
    let data = records.iter().map(VaultState::project_credential).collect();
    Ok(Json(paginate(data, &page, |c| c.id.as_str())))
}

async fn retrieve_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let workspace_id = request_workspace(scope);
    let record = state
        .vaults
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    Ok(Json(VaultState::project_credential(&record)))
}

/// `DELETE /v1/vaults/:vault_id/credentials/:id` — hard-delete one credential
/// (the SDK `beta.vaults.credentials.delete`). Drops the wire bookkeeping (the
/// sealed secrets go inert, as in `delete_vault`); the domain row is orphaned,
/// never re-referenced. Scoped by `vault_id`: a credential under another vault
/// 404s rather than deleting across the path scope.
async fn delete_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<DeletedCredential>, WireError> {
    let workspace_id = request_workspace(scope);
    let record = state
        .vaults
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|record| record.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    retire_record(&state, &record).await?;
    state
        .vaults
        .delete_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?;
    Ok(Json(DeletedCredential {
        id,
        object_type: "vault_credential_deleted",
    }))
}

/// `POST /v1/vaults/:vault_id/credentials/:id/archive` — soft-delete (the SDK
/// `beta.vaults.credentials.archive`). Stamps `archived_at` and returns the
/// secret-free credential; scoped by `vault_id`.
async fn archive_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let workspace_id = request_workspace(scope);
    let mut record = state
        .vaults
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|record| record.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    retire_record(&state, &record).await?;
    record.archived_at = Some(OBJECT_AT.to_string());
    state
        .vaults
        .put_vault_credential(&workspace_id, record.clone())
        .await
        .map_err(storage_error)?;
    Ok(Json(VaultState::project_credential(&record)))
}

async fn retire_record(state: &VaultState, record: &CredentialRecord) -> Result<(), WireError> {
    // Cause/effect lifecycle rule L1: a valid wire record owns one domain source
    // whose aggregate owns every named material slot. Retirement publishes a
    // non-materializable higher revision and reclaims the complete material set
    // before the wire projection changes.
    revoke_credential(
        &record.source_id,
        CredentialRetirement::Archive,
        state.secrets.as_ref(),
        state.credentials.as_ref(),
    )
    .await
    .map_err(|error| bad_request(error.to_string()))?;
    Ok(())
}

/// `POST /v1/vaults/:vault_id/credentials/:id` — partial update (the SDK
/// `beta.vaults.credentials.update`). The credential kind is immutable: an `auth`
/// patch must carry the credential's own `type` or the request is a `400`. Secret
/// fields (`secret_value` / `token` / `access_token` / `refresh_token` /
/// confidential `client_secret`) are write-only and rotated as one exact
/// credential revision — never echoed. `display_name` may be cleared
/// (JSON `null`); `metadata` is a patch. Three phases keep the `SecretStore`
/// `await`s off the state mutex: validate + snapshot, re-seal, then apply.
async fn update_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((vault_id, id)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<CredentialUpdateParams>,
) -> Result<Json<Credential>, WireError> {
    let workspace_id = request_workspace(scope);
    // Phase 1 — validate the kind match + refresh precondition, snapshot the id.
    let mut record = state
        .vaults
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    let source_id = {
        if let Some(auth) = &params.auth {
            let kind_matches = matches!(
                (auth, &record.auth),
                (
                    CredentialUpdateAuth::EnvironmentVariable { .. },
                    AuthRecord::EnvironmentVariable { .. }
                ) | (
                    CredentialUpdateAuth::StaticBearer { .. },
                    AuthRecord::StaticBearer { .. }
                ) | (
                    CredentialUpdateAuth::McpOauth { .. },
                    AuthRecord::McpOauth { .. }
                )
            );
            if !kind_matches {
                return Err(bad_request(
                    "auth.type does not match the credential's type",
                ));
            }
            if let CredentialUpdateAuth::McpOauth {
                refresh: Some(_), ..
            } = auth
            {
                let has_refresh = matches!(
                    &record.auth,
                    AuthRecord::McpOauth {
                        refresh: Some(_),
                        ..
                    }
                );
                if !has_refresh {
                    return Err(bad_request(
                        "credential has no refresh configuration to update",
                    ));
                }
            }
        }
        record.source_id.clone()
    };

    // Phase 2 — rotate primary material through the domain lifecycle. A higher
    // source revision means an old exact pin fails before any new material opens.
    if let Some(auth) = &params.auth {
        let refresh_config_changed = matches!(
            auth,
            CredentialUpdateAuth::McpOauth {
                refresh: Some(update),
                ..
            } if update.scope.is_some() || update.token_endpoint_auth.is_some()
        );
        let mut patch = CredentialMaterialPatch {
            primary: match auth {
                CredentialUpdateAuth::EnvironmentVariable { secret_value, .. } => {
                    secret_value.clone()
                }
                CredentialUpdateAuth::StaticBearer { token } => token.clone(),
                CredentialUpdateAuth::McpOauth { access_token, .. } => access_token.clone(),
            }
            .map(RedactedString::new),
            auxiliary: BTreeMap::new(),
        };
        if let CredentialUpdateAuth::McpOauth {
            refresh: Some(update),
            ..
        } = auth
        {
            if let Some(rt) = &update.refresh_token {
                patch.auxiliary.insert(
                    OAUTH_REFRESH_TOKEN_SLOT.to_string(),
                    Some(RedactedString::new(rt.clone())),
                );
            }
            match &update.token_endpoint_auth {
                Some(TokenEndpointAuthUpdate::None) => {
                    patch
                        .auxiliary
                        .insert(OAUTH_CLIENT_SECRET_SLOT.to_string(), None);
                }
                Some(
                    TokenEndpointAuthUpdate::ClientSecretBasic {
                        client_secret: Some(cs),
                    }
                    | TokenEndpointAuthUpdate::ClientSecretPost {
                        client_secret: Some(cs),
                    },
                ) => {
                    patch.auxiliary.insert(
                        OAUTH_CLIENT_SECRET_SLOT.to_string(),
                        Some(RedactedString::new(cs.clone())),
                    );
                }
                Some(
                    TokenEndpointAuthUpdate::ClientSecretBasic {
                        client_secret: None,
                    }
                    | TokenEndpointAuthUpdate::ClientSecretPost {
                        client_secret: None,
                    },
                )
                | None => {}
            }
        }
        let material_changed = patch.primary.is_some() || !patch.auxiliary.is_empty();
        if material_changed {
            rotate_credential_materials(
                &source_id,
                patch,
                state.secrets.as_ref(),
                state.credentials.as_ref(),
            )
            .await
            .map_err(|e| bad_request(e.to_string()))?;
        } else if refresh_config_changed {
            advance_credential_revision(&source_id, state.credentials.as_ref())
                .await
                .map_err(|e| bad_request(e.to_string()))?;
        }
    }

    // Phase 3 — publish the secret-free projection through the same durable repo.
    if let Some(auth) = params.auth {
        match auth {
            CredentialUpdateAuth::EnvironmentVariable { networking, .. } => {
                if let (
                    Some(nw),
                    AuthRecord::EnvironmentVariable {
                        networking: cur, ..
                    },
                ) = (networking, &mut record.auth)
                {
                    *cur = match nw {
                        CredentialNetworking::Unrestricted => {
                            ManagedCredentialNetworking::Unrestricted
                        }
                        CredentialNetworking::Limited { allowed_hosts } => {
                            ManagedCredentialNetworking::Limited { allowed_hosts }
                        }
                    };
                }
            }
            CredentialUpdateAuth::StaticBearer { .. } => {}
            CredentialUpdateAuth::McpOauth {
                expires_at,
                refresh,
                ..
            } => {
                if let AuthRecord::McpOauth {
                    expires_at: cur_ex,
                    refresh: cur_refresh,
                    ..
                } = &mut record.auth
                {
                    if let Some(new_ex) = expires_at {
                        *cur_ex = Some(new_ex);
                    }
                    if let (Some(update), Some(r)) = (refresh, cur_refresh.as_mut()) {
                        if let Some(scope) = update.scope {
                            r.scope = Some(scope);
                        }
                        if let Some(tea) = update.token_endpoint_auth {
                            let auth = match tea {
                                TokenEndpointAuthUpdate::None => {
                                    awaken_credential_contract::TokenEndpointAuth::None
                                }
                                TokenEndpointAuthUpdate::ClientSecretBasic { .. } => {
                                    awaken_credential_contract::TokenEndpointAuth::ClientSecretBasic
                                }
                                TokenEndpointAuthUpdate::ClientSecretPost { .. } => {
                                    awaken_credential_contract::TokenEndpointAuth::ClientSecretPost
                                }
                            };
                            r.token_endpoint_auth = auth;
                        }
                    }
                }
            }
        }
    }
    if let Some(display_name) = params.display_name {
        record.display_name = display_name;
    }
    if let Some(patch) = params.metadata {
        apply_metadata_patch(&mut record.metadata, patch);
    }
    state
        .vaults
        .put_vault_credential(&workspace_id, record.clone())
        .await
        .map_err(storage_error)?;
    Ok(Json(VaultState::project_credential(&record)))
}

async fn validate_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<CredentialValidation>, WireError> {
    let workspace_id = request_workspace(scope);
    let (source_id, mcp_server_url, has_refresh_token) = {
        let record = state
            .vaults
            .get_vault_credential(&workspace_id, &id)
            .await
            .map_err(storage_error)?
            .filter(|c| c.vault_id == vault_id)
            .ok_or_else(|| not_found("credential"))?;
        let (url, has_refresh) = match &record.auth {
            AuthRecord::McpOauth {
                mcp_server_url,
                refresh,
                ..
            } => (Some(mcp_server_url.clone()), refresh.is_some()),
            _ => (None, false),
        };
        (record.source_id, url, has_refresh)
    };
    // Live probe: only an `mcp_oauth` credential (it names an MCP server to
    // handshake with) and only when the process startup wired an `McpProbe`.
    // The access token is materialized HERE and the port receives the resolved
    // secret — never a vault ref (its signature enforces that). Any gap — no
    // probe, env-var/static_bearer, a broken row, an inconclusive probe — keeps
    // `status` `unknown`: never a false verdict. The `mcp_probe` detail is
    // secret-free by construction (a handshake flag or an HTTP status).
    let mut status = CredentialValidationStatus::Unknown;
    let mut mcp_probe = None;
    if let (Some(probe), Some(url)) = (&state.probe, &mcp_server_url) {
        let bearer = match state.credentials.get(&source_id).await {
            Ok(row) => awaken_credential_vault::materialize(&row, &*state.secrets)
                .await
                .ok(),
            Err(_) => None,
        };
        if let Some(bearer) = bearer {
            match probe.probe(url, &bearer).await {
                McpProbeStatus::Valid => {
                    status = CredentialValidationStatus::Valid;
                    mcp_probe = Some(McpProbeResult::ok());
                }
                McpProbeStatus::Invalid { http_status } => {
                    status = CredentialValidationStatus::Invalid;
                    mcp_probe = Some(McpProbeResult::invalid(http_status));
                }
                McpProbeStatus::Unknown => {}
            }
        }
    }
    Ok(Json(CredentialValidation {
        credential_id: id,
        has_refresh_token,
        mcp_probe,
        refresh: None,
        status,
        object_type: "vault_credential_validation",
        validated_at: OBJECT_AT.to_string(),
        vault_id,
    }))
}
