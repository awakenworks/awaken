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
use std::sync::{Arc, RwLock};

use crate::control::vault_acl::{
    WireEnvVarCreate, WireMcpOauthCreate, WireStaticBearerCreate, env_var_to_create_params,
    mcp_oauth_to_create_params, static_bearer_to_create_params,
};
use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialCustodyPublication, CredentialDescriptor, CredentialEnvelopeIssuance,
    CredentialMaterialDescriptor, CredentialPurpose, CredentialSourceId, CredentialTarget,
    CredentialTargetContract, CredentialUsage, OPAQUE_SECRET_MATERIAL_TYPE,
};
use awaken_credential_vault::catalog::{
    ManagedCredentialAdmissionError, ManagedCredentialAuth as AuthRecord,
    ManagedCredentialMutationError, ManagedCredentialNetworking,
    ManagedMcpOauthRefresh as McpOauthRefreshRecord, ManagedVault as VaultRecord,
    ManagedVaultCredential as CredentialRecord, ManagedVaultMutationError,
    request_managed_vault_deletion,
};
use awaken_credential_vault::repo::{
    ApplicationMcpBearerCommand, CredentialMaterialPatch, ManagedCredentialAdoptionProgress,
    ManagedCredentialCreateCommand, ManagedCredentialCreationError, ManagedCredentialOperation,
    ManagedCredentialRepository, application_mcp_material_ref, application_mcp_operation_id,
    create_managed_credential, prepare_application_mcp_bearer_rotation,
    reconcile_managed_credential_rollout, reconcile_managed_vault_deletion,
    retire_managed_credential, update_managed_credential, update_managed_credential_prepared,
};
use awaken_credential_vault::{
    CredentialCreateParams as DomainCredentialCreateParams, CredentialKind,
    OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore,
};
use awaken_session_application::{SessionCredentialAccessRequest, SessionCredentialSource};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::{ManagedJson, sha256_identity};
use crate::types::vault::{
    Credential, CredentialAuth, CredentialCreateParams, CredentialCreateWire, CredentialNetworking,
    CredentialUpdateAuth, CredentialUpdateParams, CredentialValidation, CredentialValidationStatus,
    DeletedCredential, DeletedVault, ListQuery, McpOauthRefreshResponse, McpProbeResult,
    TokenEndpointAuthParams, TokenEndpointAuthResponse, TokenEndpointAuthUpdate, Vault,
    VaultCreateParams, VaultUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};

mod repository_credentials;

/// Deterministic timestamp stamped on every vault/credential object, matching the
/// session surface's `PROCESSED_AT` convention (no wall-clock/uuid dependency, so
/// the wire is reproducible under test).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

fn credential_clock_unix_ms() -> Result<u64, awaken_credential_vault::CredentialError> {
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            awaken_credential_vault::CredentialError::InvalidSource(error.to_string())
        })?
        .as_millis();
    u64::try_from(now_unix_ms).map_err(|_| {
        awaken_credential_vault::CredentialError::InvalidSource(
            "credential clock exceeds supported range".into(),
        )
    })
}

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

fn mcp_credential_descriptor(
    url: &str,
) -> Result<(CredentialDescriptor, String), awaken_credential_vault::CredentialError> {
    let identity = awaken_session_contract::McpTarget::identity(url).map_err(|_| {
        awaken_credential_vault::CredentialError::InvalidSource(
            "MCP credential target must be an absolute HTTP(S) URL".into(),
        )
    })?;
    let audience = identity.canonical_url();
    Ok((
        CredentialDescriptor::new(
            audience.clone(),
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [CredentialTargetContract::new(
                CredentialTarget::new(CredentialPurpose::McpAuthorization, audience.clone()),
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
            )],
        ),
        audience,
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
    repository: Arc<dyn ManagedCredentialRepository>,
    /// The live MCP probe the validate route consults for `mcp_oauth`
    /// credentials, when the process startup wires one. `None` keeps every
    /// validation `unknown` (never a false `valid`).
    probe: Option<Arc<dyn McpProbe>>,
    material_delivery: Option<awaken_credential_contract::CredentialMaterialDelivery>,
    rollout_target:
        RwLock<Option<Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>>>,
}

impl VaultState {
    /// Build the vault surface over the credential domain's secret store + repo.
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        repository: Arc<dyn ManagedCredentialRepository>,
    ) -> Self {
        Self {
            secrets,
            repository,
            probe: None,
            material_delivery: None,
            rollout_target: RwLock::new(None),
        }
    }

    /// Install the service-owned rolling replacement edge after process
    /// composition has constructed both Control and Session applications.
    pub fn set_rollout_target(
        &self,
        target: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>,
    ) {
        *self.rollout_target.write().expect("Vault rollout target") = Some(target);
    }

    /// Best-effort one delivery pass. Durable events remain pending when no
    /// target is available (split-service startup) or a Session is still busy.
    pub async fn reconcile_rollouts(
        &self,
    ) -> Result<usize, awaken_credential_vault::CredentialError> {
        let target = self
            .rollout_target
            .read()
            .expect("Vault rollout target")
            .clone();
        let Some(target) = target else {
            return Ok(0);
        };
        awaken_credential_vault::repo::reconcile_managed_credential_rollouts(
            self.repository.as_ref(),
            target.as_ref(),
        )
        .await
    }

    async fn reconcile_application_mcp_rollout(
        &self,
        event_id: Option<&str>,
        workspace_id: &str,
        vault_id: &str,
        credential_id: &str,
        source_id: &CredentialSourceId,
        source_revision: u64,
    ) -> Result<ManagedCredentialAdoptionProgress, awaken_credential_vault::CredentialError> {
        let Some(event_id) = event_id else {
            // Managed creation has no predecessor to replace and therefore no
            // rollout.
            return Ok(ManagedCredentialAdoptionProgress::Converged);
        };
        let Some(event) = self.repository.managed_rollout(event_id).await? else {
            // An absent exact update event has already been exactly
            // acknowledged by this same repository.
            return Ok(ManagedCredentialAdoptionProgress::Converged);
        };
        if event.workspace_id != workspace_id
            || event.vault_id != vault_id
            || event.credential_id != credential_id
            || event.source_id != *source_id
            || event.source_version != source_revision
            || event.operation != ManagedCredentialOperation::Update
        {
            return Err(awaken_credential_vault::CredentialError::MutationConflict(
                "application MCP rollout identity conflicts with committed credential".into(),
            ));
        }
        let target = self
            .rollout_target
            .read()
            .expect("Vault rollout target")
            .clone();
        let Some(target) = target else {
            return Ok(ManagedCredentialAdoptionProgress::Pending);
        };
        reconcile_managed_credential_rollout(&event, self.repository.as_ref(), target.as_ref())
            .await
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
        credential_generation: u64,
        bearer: RedactedString,
    ) -> Result<
        (
            String,
            CredentialSourceId,
            u64,
            ManagedCredentialAdoptionProgress,
        ),
        awaken_credential_vault::CredentialError,
    > {
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
        let material_ref = application_mcp_material_ref(
            &source_id,
            &command_key_fingerprint,
            credential_generation,
        )?;
        let (descriptor, target_audience) = mcp_credential_descriptor(mcp_server_url)?;
        self.repository
            .ensure_vault(
                workspace_id,
                VaultRecord {
                    id: vault_id.clone(),
                    workspace_id: workspace_id.to_owned(),
                    display_name: application_authority_id.to_owned(),
                    metadata: BTreeMap::new(),
                    archived_at: None,
                    deletion: None,
                    revision: 1,
                },
            )
            .await?;
        let credential_id = format!(
            "crd_app_{}",
            sha256_identity("application-mcp-credential-v1", &[&source_id.0])
        );
        let atomically_created = match self.repository.get(&source_id).await {
            Err(awaken_credential_vault::CredentialError::SourceNotFound(_)) => {
                match create_managed_credential(
                    ManagedCredentialCreateCommand {
                        source: DomainCredentialCreateParams {
                            workspace_id: workspace_id.to_owned(),
                            kind: CredentialKind::Vault,
                            provider_id: None,
                            env_key: None,
                            secret: Some(bearer.clone()),
                            oauth_command: None,
                        },
                        descriptor: Some(descriptor),
                        source_id: Some(source_id.clone()),
                        protocol_endpoint_id: Some(target_fingerprint.clone()),
                        primary_material_ref: Some(material_ref),
                        auxiliary_materials: BTreeMap::new(),
                        credential_id: credential_id.clone(),
                        vault_id: vault_id.clone(),
                        auth: AuthRecord::StaticBearer {
                            mcp_server_url: mcp_server_url.to_owned(),
                        },
                        metadata: BTreeMap::new(),
                        display_name: None,
                    },
                    self.secrets.as_ref(),
                    self.repository.as_ref(),
                )
                .await
                {
                    Ok((source, _)) => Some(source),
                    Err(ManagedCredentialCreationError::Credential(
                        awaken_credential_vault::CredentialError::MutationConflict(_),
                    ))
                    | Err(ManagedCredentialCreationError::Admission(
                        ManagedCredentialAdmissionError::Store(
                            awaken_credential_vault::CredentialError::MutationConflict(_),
                        ),
                    )) => None,
                    Err(ManagedCredentialCreationError::Credential(error))
                    | Err(ManagedCredentialCreationError::Admission(
                        ManagedCredentialAdmissionError::Store(error),
                    )) => return Err(error),
                    Err(ManagedCredentialCreationError::Admission(error)) => {
                        return Err(awaken_credential_vault::CredentialError::InvalidSource(
                            error.to_string(),
                        ));
                    }
                }
            }
            Ok(_) => None,
            Err(error) => return Err(error),
        };
        let (source, rollout_id) = if let Some(source) = atomically_created {
            (source, None)
        } else {
            let before_source = self.repository.get(&source_id).await?;
            let before_credential = self
                .repository
                .get_vault_credential_by_source(workspace_id, &source_id)
                .await?
                .ok_or_else(|| {
                    awaken_credential_vault::CredentialError::MutationConflict(
                        "application MCP Source has no matching Managed child".into(),
                    )
                })?;
            let replay_command_key_fingerprint = command_key_fingerprint.clone();
            let rotation = prepare_application_mcp_bearer_rotation(
                ApplicationMcpBearerCommand {
                    source_id: source_id.clone(),
                    workspace_id: workspace_id.to_owned(),
                    target_fingerprint,
                    target_audience,
                    command_key_fingerprint,
                    credential_generation,
                    bearer,
                },
                &before_source,
                self.secrets.as_ref(),
            )
            .await?;
            match rotation {
                None => {
                    let rollout_id = before_source
                        .version
                        .checked_sub(1)
                        .filter(|prior_version| *prior_version > 0)
                        .map(|prior_version| {
                            application_mcp_operation_id(
                                &source_id,
                                prior_version,
                                &replay_command_key_fingerprint,
                                credential_generation,
                            )
                        })
                        .transpose()?;
                    (before_source, rollout_id)
                }
                Some(rotation) => {
                    let rollout_id = rotation.operation_id.clone();
                    let source = update_managed_credential_prepared(
                        before_credential.clone(),
                        before_credential,
                        before_source,
                        rotation,
                        self.secrets.as_ref(),
                        self.repository.as_ref(),
                    )
                    .await
                    .map_err(|error| {
                        awaken_credential_vault::CredentialError::MutationConflict(
                            error.to_string(),
                        )
                    })?
                    .0;
                    (source, Some(rollout_id))
                }
            }
        };
        let revision = u64::try_from(source.version).map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "application MCP credential revision is invalid".into(),
            )
        })?;
        let adoption = self
            .reconcile_application_mcp_rollout(
                rollout_id.as_deref(),
                workspace_id,
                &vault_id,
                &credential_id,
                &source.id,
                revision,
            )
            .await?;
        Ok((vault_id, source.id, revision, adoption))
    }

    /// Wire the live MCP probe, so `POST .../mcp_oauth_validate` reports a real
    /// `valid`/`invalid` verdict for an `mcp_oauth` credential instead of
    /// `unknown`.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn McpProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Install the one deployment-owned plaintext delivery mechanism.
    /// Selection, revision, holder, usage and target binding remain
    /// Vault/Session facts.
    #[must_use]
    pub fn with_material_delivery(
        mut self,
        delivery: awaken_credential_contract::CredentialMaterialDelivery,
    ) -> Self {
        self.material_delivery = Some(delivery);
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
            .repository
            .get_vault(workspace_id, id)
            .await?
            .is_some_and(|vault| vault.accepts_child_mutation()))
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
                .repository
                .get_vault(workspace_id, vault_id)
                .await?
                .is_some_and(|vault| vault.accepts_child_mutation());
            if !vault_is_active {
                continue;
            }
            let mut credentials = self
                .repository
                .list_vault_credentials(workspace_id, vault_id)
                .await?;
            credentials.sort_by(|left, right| left.id.cmp(&right.id));
            if let Some(credential) = credentials
                .into_iter()
                .filter(|credential| credential.lifecycle.is_active())
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
        request: awaken_credential_vault::ExactCredentialAccessRequest<'_>,
    ) -> Result<
        (
            awaken_credential_vault::CredentialSource,
            awaken_credential_contract::CredentialAccess,
        ),
        awaken_credential_vault::CredentialError,
    > {
        let source = self.repository.get(source_id).await?;
        let selected_holder = request.selected_holder;
        let binding = request.binding;
        let mut access =
            awaken_credential_vault::compile_exact_credential_access(&source, request)?;
        match &self.material_delivery {
            Some(awaken_credential_contract::CredentialMaterialDelivery::ExternalCustody(
                custodian,
            )) if custodian.handles(selected_holder, &access.usage) => {
                let material =
                    awaken_credential_vault::materialize(&source, self.secrets.as_ref()).await?;
                if let Some(descriptor) = &source.descriptor {
                    awaken_credential_vault::validate_described_material(descriptor, &material)?;
                }
                custodian
                    .publish(CredentialCustodyPublication {
                        access: access.clone(),
                        selected_holder: selected_holder.clone(),
                        binding: binding.clone(),
                        material,
                    })
                    .await
                    .map_err(awaken_credential_vault::CredentialError::InvalidSource)?;
            }
            Some(awaken_credential_contract::CredentialMaterialDelivery::RecipientEnvelope(
                issuer,
            )) if selected_holder.boundary
                != awaken_credential_contract::PlaintextBoundary::Platform =>
            {
                let material =
                    awaken_credential_vault::materialize(&source, self.secrets.as_ref()).await?;
                if let Some(descriptor) = &source.descriptor {
                    awaken_credential_vault::validate_described_material(descriptor, &material)?;
                }
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
            _ => {}
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
        request: SessionCredentialAccessRequest,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        let SessionCredentialAccessRequest {
            target,
            usage,
            policy,
            selected_holder,
            binding,
        } = request;
        let now_unix_ms = credential_clock_unix_ms()?;
        self.exact_access_for_source(
            source_id,
            awaken_credential_vault::ExactCredentialAccessRequest {
                workspace_id: Some(workspace_id),
                target: Some(target),
                usage,
                policy,
                selected_holder: &selected_holder,
                binding: &binding,
                now_unix_ms,
            },
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
        let source = self.repository.get(source_id).await?;
        let record = self
            .repository
            .get_vault_credential_by_source(&source.workspace_id, source_id)
            .await?
            .ok_or_else(|| {
                awaken_credential_vault::CredentialError::InvalidSource(
                    "MCP credential source has no authoritative Managed child".into(),
                )
            })?;
        let url = match record.auth {
            AuthRecord::StaticBearer { mcp_server_url }
            | AuthRecord::McpOauth { mcp_server_url, .. } => mcp_server_url,
            AuthRecord::EnvironmentVariable { .. } => {
                return Err(awaken_credential_vault::CredentialError::InvalidSource(
                    "credential source is not owned by an MCP authorization".into(),
                ));
            }
        };
        let target = awaken_session_contract::McpTarget::parse_http(&url).map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "MCP credential target is invalid".into(),
            )
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
        target: &awaken_session_contract::McpTarget,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        use awaken_credential_contract::{
            CredentialExecutionPolicy, CredentialPurpose, CredentialRefreshAccess,
            CredentialTarget, CredentialUsage,
        };

        let target_url = target.http_url().ok_or_else(|| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "MCP credentials require an HTTP target".into(),
            )
        })?;
        let target_audience = awaken_session_contract::McpTarget::identity(target_url)
            .map_err(|_| {
                awaken_credential_vault::CredentialError::InvalidSource(
                    "MCP credential target is invalid".into(),
                )
            })?
            .canonical_url();

        let (source, mut access) = self
            .exact_access_for_source(
                source_id,
                awaken_credential_vault::ExactCredentialAccessRequest {
                    workspace_id,
                    target: Some(CredentialTarget::new(
                        CredentialPurpose::McpAuthorization,
                        target_audience,
                    )),
                    usage: CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                    policy: CredentialExecutionPolicy::self_hosted_mcp(),
                    selected_holder,
                    binding,
                    now_unix_ms: credential_clock_unix_ms()?,
                },
            )
            .await?;
        let revision = access.credential.revision;
        let refresh = self
            .repository
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
            archived_at: record.lifecycle.archived_at().map(str::to_owned),
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
        target: &awaken_session_contract::McpTarget,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
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
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        VaultState::credential_access_for_source(self, source_id, workspace_id, request)
            .await
            .map_err(|error| error.to_string())
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
        ManagedVaultMutationError::InvalidLifecycle => not_found("vault"),
        ManagedVaultMutationError::Store(error) => storage_error(error),
    }
}

fn managed_creation_error(error: ManagedCredentialCreationError) -> WireError {
    match error {
        ManagedCredentialCreationError::Credential(
            error @ awaken_credential_vault::CredentialError::Storage(_),
        ) => storage_error(error),
        ManagedCredentialCreationError::Credential(error) => bad_request(error.to_string()),
        ManagedCredentialCreationError::Admission(error) => match error {
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
        },
    }
}

fn managed_mutation_error(error: ManagedCredentialMutationError) -> WireError {
    match error {
        ManagedCredentialMutationError::NotFound => not_found("credential"),
        ManagedCredentialMutationError::RevisionConflict => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "conflict_error",
                "credential changed concurrently; retry the request",
            )),
        ),
        ManagedCredentialMutationError::RevisionExhausted => storage_error(error),
        ManagedCredentialMutationError::InvalidLifecycle => {
            bad_request("credential lifecycle does not allow this operation")
        }
        ManagedCredentialMutationError::Admission(error) => {
            managed_creation_error(ManagedCredentialCreationError::Admission(error))
        }
        ManagedCredentialMutationError::Store(error) => storage_error(error),
        error @ ManagedCredentialMutationError::Compensation { .. } => storage_error(
            awaken_credential_vault::CredentialError::Storage(error.to_string()),
        ),
    }
}

async fn create_vault(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<VaultCreateParams>,
) -> Result<(StatusCode, Json<Vault>), WireError> {
    if params.display_name.is_empty() || params.display_name.len() > 255 {
        return Err(bad_request("display_name must be 1-255 characters"));
    }
    let id = format!("vlt_{}", uuid::Uuid::now_v7().simple());
    let record = VaultRecord {
        id,
        workspace_id: workspace_id.clone(),
        display_name: params.display_name,
        metadata: params.metadata,
        archived_at: None,
        deletion: None,
        revision: 1,
    };
    let vault = VaultState::project_vault(&record);
    state
        .repository
        .insert_vault(&workspace_id, record)
        .await
        .map_err(storage_error)?;
    Ok((StatusCode::OK, Json(vault)))
}

async fn retrieve_vault(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let record = state
        .repository
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|record| record.deletion.is_none())
        .ok_or_else(|| not_found("vault"))?;
    Ok(Json(VaultState::project_vault(&record)))
}

/// `GET /v1/vaults` — one full page of vaults (the SDK `beta.vaults.list`).
/// Deterministic order: ascending wire id (`vlt_…` is zero-padded, so
/// lexicographic == creation order). Archived vaults are excluded unless
/// `?include_archived=true`.
async fn list_vaults(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Query(query): Query<ListQuery>,
    Query(page): Query<PageQuery>,
) -> Result<Json<PageCursor<Vault>>, WireError> {
    let mut records = state
        .repository
        .list_vaults(&workspace_id)
        .await
        .map_err(storage_error)?;
    records.retain(|record| {
        record.deletion.is_none() && (query.include_archived || record.archived_at.is_none())
    });
    records.sort_by(|left, right| left.id.cmp(&right.id));
    let data = records.iter().map(VaultState::project_vault).collect();
    Ok(Json(paginate(data, &page, |v| v.id.as_str())))
}

async fn delete_vault(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> Result<Json<DeletedVault>, WireError> {
    let current = state
        .repository
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| not_found("vault"))?;
    let (requested, changed) = request_managed_vault_deletion(&current, OBJECT_AT.to_string())
        .map_err(vault_mutation_error)?;
    let requested = persist_vault_deletion_request(
        state.repository.as_ref(),
        &workspace_id,
        &current,
        requested,
        changed,
    )
    .await?;
    // The request is durable before child retirement starts. Cancellation or a
    // transient failure therefore leaves a supervised, retryable root fact.
    let _completed = reconcile_managed_vault_deletion(
        &requested,
        state.secrets.as_ref(),
        state.repository.as_ref(),
    )
    .await
    .map_err(managed_mutation_error)?;
    Ok(Json(DeletedVault {
        id,
        object_type: "vault_deleted",
    }))
}

/// Publish the root delete fence, or resume the durable winner when another
/// request committed the fence after this handler read its snapshot.
async fn persist_vault_deletion_request(
    repository: &dyn ManagedCredentialRepository,
    workspace_id: &str,
    current: &VaultRecord,
    requested: VaultRecord,
    changed: bool,
) -> Result<VaultRecord, WireError> {
    if !changed {
        return Ok(requested);
    }
    match repository
        .replace_vault(workspace_id, current.revision, requested.clone())
        .await
    {
        Ok(()) => Ok(requested),
        Err(error)
            if matches!(
                error,
                ManagedVaultMutationError::RevisionConflict
                    | ManagedVaultMutationError::InvalidLifecycle
            ) =>
        {
            let durable = repository
                .get_vault(workspace_id, &current.id)
                .await
                .map_err(storage_error)?;
            match durable {
                Some(durable) if durable.deletion.is_some() => Ok(durable),
                _ => Err(vault_mutation_error(error)),
            }
        }
        Err(error) => Err(vault_mutation_error(error)),
    }
}

/// `POST /v1/vaults/:id/archive` — soft-delete (the SDK `beta.vaults.archive`).
/// Stamps `archived_at` and returns the vault; an already-archived vault is
/// returned unchanged. The vault's credentials are unaffected (archiving a
/// vault does not cascade — only `DELETE` cascades).
async fn archive_vault(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let mut record = state
        .repository
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|record| record.deletion.is_none())
        .ok_or_else(|| not_found("vault"))?;
    if record.archived_at.is_some() {
        return Ok(Json(VaultState::project_vault(&record)));
    }
    let expected_revision = record.revision;
    record.archived_at = Some(OBJECT_AT.to_string());
    record.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| vault_mutation_error(ManagedVaultMutationError::RevisionExhausted))?;
    state
        .repository
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
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<VaultUpdateParams>,
) -> Result<Json<Vault>, WireError> {
    if let Some(name) = &params.display_name
        && (name.is_empty() || name.len() > 255)
    {
        return Err(bad_request("display_name must be 1-255 characters"));
    }
    let mut record = state
        .repository
        .get_vault(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|record| record.deletion.is_none())
        .ok_or_else(|| not_found("vault"))?;
    let before = record.clone();
    let expected_revision = record.revision;
    if let Some(name) = params.display_name {
        record.display_name = name;
    }
    if let Some(patch) = params.metadata {
        apply_metadata_patch(&mut record.metadata, patch);
    }
    if record == before {
        return Ok(Json(VaultState::project_vault(&record)));
    }
    record.revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| vault_mutation_error(ManagedVaultMutationError::RevisionExhausted))?;
    state
        .repository
        .replace_vault(&workspace_id, expected_revision, record.clone())
        .await
        .map_err(vault_mutation_error)?;
    Ok(Json(VaultState::project_vault(&record)))
}

async fn create_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(resource_workspace): RequiredWorkspaceScope,
    Path(vault_id): Path<String>,
    ManagedJson(params): ManagedJson<CredentialCreateWire>,
) -> Result<(StatusCode, Json<Credential>), WireError> {
    let params = params.into_params();
    // The vault id is a wire-side container id, not an authorization scope. The
    // platform-resolved workspace stamped at the startup edge owns the durable
    // credential row. Standalone embeddings that omit that edge use the documented
    // local/default workspace; tenancy is never derived from a resource id.
    // Secret-in through the ACL: every raw secret crosses into one domain
    // command here. Source and Managed child remain invisible until the
    // Credential repository atomically publishes both.
    let (source, descriptor, auxiliary_materials, auth, metadata, display_name) = match params {
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
            let auth = AuthRecord::EnvironmentVariable {
                secret_name,
                networking: match networking {
                    CredentialNetworking::Unrestricted => ManagedCredentialNetworking::Unrestricted,
                    CredentialNetworking::Limited { allowed_hosts } => {
                        ManagedCredentialNetworking::Limited { allowed_hosts }
                    }
                },
            };
            (create, None, BTreeMap::new(), auth, metadata, display_name)
        }
        CredentialCreateParams::StaticBearer {
            mcp_server_url,
            token,
            metadata,
            display_name,
        } => {
            let (descriptor, _) = mcp_credential_descriptor(&mcp_server_url)
                .map_err(|error| bad_request(error.to_string()))?;
            let create = static_bearer_to_create_params(
                resource_workspace.clone(),
                WireStaticBearerCreate { token },
            );
            (
                create,
                Some(descriptor),
                BTreeMap::new(),
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
            let (descriptor, _) = mcp_credential_descriptor(&mcp_server_url)
                .map_err(|error| bad_request(error.to_string()))?;
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
            (
                bridged.params,
                Some(descriptor),
                auxiliary,
                AuthRecord::McpOauth {
                    mcp_server_url,
                    expires_at,
                    refresh: refresh_record,
                },
                metadata,
                display_name,
            )
        }
    };

    let id = format!("crd_{}", uuid::Uuid::now_v7().simple());
    let (_, record) = create_managed_credential(
        ManagedCredentialCreateCommand {
            source,
            descriptor,
            source_id: None,
            protocol_endpoint_id: None,
            primary_material_ref: None,
            auxiliary_materials,
            credential_id: id,
            vault_id,
            auth,
            metadata,
            display_name,
        },
        state.secrets.as_ref(),
        state.repository.as_ref(),
    )
    .await
    .map_err(managed_creation_error)?;
    let credential = VaultState::project_credential(&record);
    Ok((StatusCode::OK, Json(credential)))
}

/// `GET /v1/vaults/:vault_id/credentials` — one full page of a vault's
/// credentials (the SDK `beta.vaults.credentials.list`). An unknown vault is a
/// `404` (not an empty page), matching retrieve. Deterministic order: ascending
/// wire id. Archived credentials are excluded unless `?include_archived=true`.
async fn list_credentials(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(vault_id): Path<String>,
    Query(query): Query<ListQuery>,
    Query(page): Query<PageQuery>,
) -> Result<Json<PageCursor<Credential>>, WireError> {
    if state
        .repository
        .get_vault(&workspace_id, &vault_id)
        .await
        .map_err(storage_error)?
        .is_none()
    {
        return Err(not_found("vault"));
    }
    let mut records = state
        .repository
        .list_vault_credentials(&workspace_id, &vault_id)
        .await
        .map_err(storage_error)?;
    records.retain(|record| {
        !record.lifecycle.is_deleted() && (query.include_archived || record.lifecycle.is_active())
    });
    records.sort_by(|left, right| left.id.cmp(&right.id));
    let data = records.iter().map(VaultState::project_credential).collect();
    Ok(Json(paginate(data, &page, |c| c.id.as_str())))
}

async fn retrieve_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let record = state
        .repository
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|c| c.vault_id == vault_id && !c.lifecycle.is_deleted())
        .ok_or_else(|| not_found("credential"))?;
    Ok(Json(VaultState::project_credential(&record)))
}

/// `DELETE /v1/vaults/:vault_id/credentials/:id` — logically delete one
/// credential (the SDK `beta.vaults.credentials.delete`). The absorbing domain
/// tombstone remains as a stale-writer fence while sealed material is reclaimed.
/// Scoped by `vault_id`: a credential under another vault 404s rather than
/// deleting across the path scope.
async fn delete_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<DeletedCredential>, WireError> {
    let visible = state
        .repository
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .is_some_and(|credential| {
            credential.vault_id == vault_id && !credential.lifecycle.is_deleted()
        });
    if !visible {
        return Err(not_found("credential"));
    }
    retire_managed_credential(
        &workspace_id,
        &vault_id,
        &id,
        ManagedCredentialOperation::Delete,
        OBJECT_AT.to_string(),
        state.secrets.as_ref(),
        state.repository.as_ref(),
    )
    .await
    .map_err(managed_mutation_error)?;
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
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let record = retire_managed_credential(
        &workspace_id,
        &vault_id,
        &id,
        ManagedCredentialOperation::Archive,
        OBJECT_AT.to_string(),
        state.secrets.as_ref(),
        state.repository.as_ref(),
    )
    .await
    .map_err(managed_mutation_error)?;
    Ok(Json(VaultState::project_credential(&record)))
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
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<CredentialUpdateParams>,
) -> Result<Json<Credential>, WireError> {
    // Phase 1 — validate the kind match + refresh precondition, snapshot the id.
    let mut record = state
        .repository
        .get_vault_credential(&workspace_id, &id)
        .await
        .map_err(storage_error)?
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    if !record.lifecycle.is_active() {
        return Err(not_found("credential"));
    }
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
    let before_record = record.clone();
    let mut material_patch = CredentialMaterialPatch::default();
    let mut advance_source_without_material = false;

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
            descriptor: None,
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
        advance_source_without_material = refresh_config_changed && !material_changed;
        material_patch = patch;
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
    if record == before_record
        && material_patch.primary.is_none()
        && material_patch.auxiliary.is_empty()
        && !advance_source_without_material
    {
        return Ok(Json(VaultState::project_credential(&record)));
    }
    let record = update_managed_credential(
        before_record,
        record,
        material_patch,
        advance_source_without_material,
        state.secrets.as_ref(),
        state.repository.as_ref(),
    )
    .await
    .map_err(managed_mutation_error)?;
    Ok(Json(VaultState::project_credential(&record)))
}

async fn validate_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<CredentialValidation>, WireError> {
    let (source_id, mcp_server_url, has_refresh_token) = {
        let record = state
            .repository
            .get_vault_credential(&workspace_id, &id)
            .await
            .map_err(storage_error)?
            .filter(|c| c.vault_id == vault_id && c.lifecycle.is_active())
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
        let bearer = match state.repository.get(&source_id).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::catalog::ManagedVaultRepo;
    use awaken_credential_vault::repo::InMemoryCredentialRepo;

    #[tokio::test]
    async fn stale_delete_writer_resumes_the_durable_winner() {
        let repository = InMemoryCredentialRepo::new();
        let current = VaultRecord {
            id: "vault-delete-race".into(),
            workspace_id: "workspace".into(),
            display_name: "Vault".into(),
            metadata: BTreeMap::new(),
            archived_at: None,
            deletion: None,
            revision: 1,
        };
        repository
            .insert_vault("workspace", current.clone())
            .await
            .unwrap();
        let (requested, changed) =
            request_managed_vault_deletion(&current, OBJECT_AT.to_owned()).unwrap();
        assert!(changed);

        // Deterministically reproduce the interleaving of two DELETE handlers:
        // both read `current`, then the first publishes `requested` before the
        // second attempts the same stale CAS.
        repository
            .replace_vault("workspace", current.revision, requested.clone())
            .await
            .unwrap();
        let resumed = persist_vault_deletion_request(
            &repository,
            "workspace",
            &current,
            requested.clone(),
            changed,
        )
        .await
        .expect("the stale loser must resume the durable delete request");

        assert_eq!(resumed, requested);
        assert!(resumed.deletion_requested());
    }
}
