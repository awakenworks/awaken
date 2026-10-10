// @formal entrypoint credential.injection_location_has_exactly_three_usable_states_and_partial_updates_preserve_validity
// @formal entrypoint credential.managed_creation_begin_rejects_every_published_source_or_child_identity
// @formal entrypoint credential.managed_creation_pair_requires_exact_workspace_source_and_initial_lifecycle
// @formal entrypoint credential.managed_vault_child_admission_requires_active_parent_capacity_and_unique_key
// @formal entrypoint credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing
// @formal entrypoint credential.managed_vault_replacement_rejects_stale_and_non_successor_revisions
// @formal entrypoint credential_envelope.issuance_requires_exact_claim
//! The Managed Agents **vault / credential** front door (ADR-0043).
//!
//! These are the public `/v1/vaults...` routes the official `@anthropic-ai/sdk`
//! `beta.vaults.*` client calls. The DTOs mirror the SDK types exactly
//! (`BetaManagedAgentsVault`, `BetaManagedAgentsCredential`, the per-type
//! auth/create shapes, and `BetaManagedAgentsCredentialValidation`); the wire
//! keeps Anthropic's snake_case tags (`vault` / `vault_credential` /
//! `environment_variable` / `static_bearer` / `mcp_oauth`).
//!
//! Orchestration is the credential context's: every command below maps the
//! wire onto [`VaultApplication`], which owns the stores, the clock, rollout
//! delivery and exact access; this module only translates and projects.
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
use awaken_credential_contract::{CredentialDescriptor, CredentialSourceId};
use awaken_credential_vault::catalog::{
    ManagedCredentialAdmissionError, ManagedCredentialAuth as AuthRecord,
    ManagedCredentialInjectionLocation, ManagedCredentialMutationError,
    ManagedCredentialNetworking, ManagedMcpOauthRefresh as McpOauthRefreshRecord,
    ManagedVault as VaultRecord, ManagedVaultCredential as CredentialRecord,
    ManagedVaultMutationError,
};
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, ManagedCredentialAdoptionProgress, ManagedCredentialCreationError,
    ManagedCredentialRepository,
};
use awaken_credential_vault::{
    ApplicationMcpBearerEntry, McpCredentialTarget, OAUTH_CLIENT_SECRET_SLOT,
    OAUTH_REFRESH_TOKEN_SLOT, SecretStore, VaultApplication, VaultCommandError,
    VaultCredentialChange, VaultCredentialDraft, VaultObject,
};
use awaken_session_application::{SessionCredentialAccessRequest, SessionCredentialSource};
use awaken_time::WallClock;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::common::headers::parse_idempotency_key_header;
use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::{ManagedJson, ManagedQuery, sha256_identity};
use crate::types::vault::{
    Credential, CredentialAuth, CredentialCreateParams, CredentialCreateWire,
    CredentialInjectionLocation, CredentialInjectionLocationParams, CredentialMcpProbe,
    CredentialNetworking, CredentialRefreshResult, CredentialUpdateAuth, CredentialUpdateParams,
    CredentialValidation, CredentialValidationStatus, DeletedCredential, DeletedVault, ListQuery,
    McpOauthRefreshResponse, TokenEndpointAuthParams, TokenEndpointAuthResponse,
    TokenEndpointAuthUpdate, Vault, VaultCreateParams, VaultUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};

mod credential_material_ingress;

fn application_mcp_target_fingerprint(
    url: &str,
) -> Result<String, awaken_session_mcp_contract::McpTargetError> {
    let identity = awaken_session_mcp_contract::McpTarget::identity(url)?;
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

/// The canonical MCP audience of `url`.
fn mcp_audience(url: &str) -> Result<String, awaken_credential_vault::CredentialError> {
    awaken_session_mcp_contract::McpTarget::identity(url)
        .map(|identity| identity.canonical_url())
        .map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "MCP credential target must be an absolute HTTP(S) URL".into(),
            )
        })
}

fn mcp_credential_descriptor(
    url: &str,
) -> Result<(CredentialDescriptor, String), awaken_credential_vault::CredentialError> {
    let audience = mcp_audience(url)?;
    Ok((
        awaken_credential_vault::mcp_bearer_descriptor(&audience),
        audience,
    ))
}

use awaken_session_configuration_contract::{McpProbe, McpProbeStatus};

// ---- State ------------------------------------------------------------------

fn auth_mcp_server_url(auth: &AuthRecord) -> Option<&str> {
    match auth {
        AuthRecord::StaticBearer { mcp_server_url }
        | AuthRecord::McpOauth { mcp_server_url, .. } => Some(mcp_server_url),
        AuthRecord::EnvironmentVariable { .. } => None,
    }
}

fn create_injection_location(
    requested: Option<CredentialInjectionLocationParams>,
) -> Result<ManagedCredentialInjectionLocation, WireError> {
    let (header, body) = requested.map_or((true, true), |requested| {
        (
            requested.header.unwrap_or(false),
            requested.body.unwrap_or(false),
        )
    });
    ManagedCredentialInjectionLocation::from_flags(header, body)
        .ok_or_else(|| bad_request("injection_location must enable header, body, or both"))
}

/// The vault surface's state: the credential context's Vault application plus
/// the MCP probe and refresh exchange the validation route consults.
mod mcp_inspection;

use mcp_inspection::validate_credential;
pub use mcp_inspection::{McpCredentialChoice, McpInspectionView};

pub struct VaultState {
    /// The Vault use cases: every store, clock and orchestration step.
    application: VaultApplication,
    /// The live MCP probe the validate route consults for `mcp_oauth`
    /// credentials, when the process startup wires one. `None` keeps every
    /// validation `unknown` (never a false `valid`).
    probe: Option<Arc<dyn McpProbe>>,
    /// The OAuth refresh exchange the validate route attempts when the probe
    /// is refused and the credential holds a refresh token. `None` reports no
    /// attempt (`refresh: null`), never a fabricated outcome.
    refresh_exchange:
        Option<Arc<dyn awaken_session_configuration_contract::CredentialRefreshExchange>>,
}

impl VaultState {
    /// Build the vault surface over the credential domain's secret store + repo.
    /// `clock` is the composing process's one wall clock.
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        repository: Arc<dyn ManagedCredentialRepository>,
        clock: Arc<dyn WallClock>,
    ) -> Self {
        Self {
            application: VaultApplication::new(secrets, repository, clock),
            probe: None,
            refresh_exchange: None,
        }
    }

    /// The Vault application this surface translates for; process
    /// composition installs the rollout target and runs rollout delivery on it.
    #[must_use]
    pub fn application(&self) -> &VaultApplication {
        &self.application
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
        let fingerprint = application_mcp_target_fingerprint(mcp_server_url).map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "MCP server URL must be an absolute HTTP(S) URL".into(),
            )
        })?;
        let audience = mcp_audience(mcp_server_url)?;
        let receipt = self
            .application
            .enter_application_mcp_bearer(ApplicationMcpBearerEntry {
                workspace_id: workspace_id.to_owned(),
                application_authority_id: application_authority_id.to_owned(),
                mcp_server_url: mcp_server_url.to_owned(),
                target: McpCredentialTarget {
                    audience,
                    fingerprint,
                },
                idempotency_key: idempotency_key.to_owned(),
                credential_generation,
                bearer,
            })
            .await?;
        Ok((
            receipt.vault_id,
            receipt.source_id,
            receipt.revision,
            receipt.adoption,
        ))
    }

    /// Wire the live MCP probe, so `POST .../mcp_oauth_validate` reports a real
    /// `valid`/`invalid` verdict for an `mcp_oauth` credential instead of
    /// `unknown`.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn McpProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Wire the OAuth refresh exchange, so validating an auth-refused
    /// `mcp_oauth` credential that holds a refresh token attempts the exchange,
    /// reseals the issued token, re-probes, and reports the exchange outcome.
    #[must_use]
    pub fn with_refresh_exchange(
        mut self,
        exchange: Arc<dyn awaken_session_configuration_contract::CredentialRefreshExchange>,
    ) -> Self {
        self.refresh_exchange = Some(exchange);
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
        self.application = self.application.with_material_delivery(delivery);
        self
    }

    /// The neutral credential-domain row id for a wire credential id, if it lives
    /// in `vault_id`. This is the seam a session uses to bind a vault credential to
    /// a run: the resolver takes this `CredentialSourceId`, never the wire id.
    /// Limited to the standalone installation Workspace; it cannot discover
    /// another Workspace by id.
    pub async fn credential_source_id(
        &self,
        vault_id: &str,
        credential_id: &str,
    ) -> Option<CredentialSourceId> {
        self.application
            .credential_source_id(crate::state::DEFAULT_SCOPE, vault_id, credential_id)
            .await
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
        let Ok(requested) = awaken_session_mcp_contract::McpTarget::identity(url) else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let Some(credentials) = self
                .application
                .executable_credentials(workspace_id, vault_id)
                .await?
            else {
                continue;
            };
            if let Some(credential) = credentials.into_iter().find(|credential| {
                auth_mcp_server_url(&credential.auth)
                    .and_then(|url| awaken_session_mcp_contract::McpTarget::identity(url).ok())
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
        self.application
            .exact_access(source_id, |now_unix_ms| {
                awaken_credential_vault::ExactCredentialAccessRequest {
                    workspace_id: Some(workspace_id),
                    target: Some(target),
                    usage,
                    policy,
                    selected_holder: &selected_holder,
                    binding: &binding,
                    now_unix_ms,
                }
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
        let source = self.application.source(source_id).await?;
        let record = self
            .application
            .credential_of_source(&source.workspace_id, source_id)
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
        let target = awaken_session_mcp_contract::McpTarget::parse_http(&url).map_err(|_| {
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
        target: &awaken_session_mcp_contract::McpTarget,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        let target_url = target.http_url().ok_or_else(|| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "MCP credentials require an HTTP target".into(),
            )
        })?;
        let target_audience = awaken_session_mcp_contract::McpTarget::identity(target_url)
            .map_err(|_| {
                awaken_credential_vault::CredentialError::InvalidSource(
                    "MCP credential target is invalid".into(),
                )
            })?
            .canonical_url();
        self.application
            .mcp_access(
                source_id,
                workspace_id,
                target_audience,
                selected_holder,
                binding,
            )
            .await
    }

    fn project_vault(record: &VaultRecord) -> Vault {
        Vault {
            id: record.id.clone(),
            archived_at: record.archived_at.clone(),
            created_at: record.created_at.clone(),
            display_name: record.display_name.clone(),
            metadata: record.metadata.clone(),
            object_type: "vault",
            updated_at: record.updated_at.clone(),
        }
    }

    fn project_credential(record: &CredentialRecord) -> Credential {
        let auth = match &record.auth {
            AuthRecord::EnvironmentVariable {
                secret_name,
                networking,
                injection_location,
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
                injection_location: CredentialInjectionLocation {
                    body: injection_location.body(),
                    header: injection_location.header(),
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
            created_at: record.created_at.clone(),
            metadata: record.metadata.clone(),
            object_type: "vault_credential",
            updated_at: record.updated_at.clone(),
            vault_id: record.vault_id.clone(),
            display_name: record.display_name.clone(),
        }
    }
}

#[async_trait::async_trait]
impl SessionCredentialSource for VaultState {
    async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, String> {
        self.application
            .has_vault(workspace_id, id)
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
        target: &awaken_session_mcp_contract::McpTarget,
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

/// The wire envelope of a Vault command outcome: each domain error keeps the
/// mapping its route always had.
fn command_error(error: VaultCommandError) -> WireError {
    match error {
        VaultCommandError::NotFound(VaultObject::Vault) => not_found("vault"),
        VaultCommandError::NotFound(VaultObject::Credential) => not_found("credential"),
        VaultCommandError::Store(error) => storage_error(error),
        VaultCommandError::Vault(error) => vault_mutation_error(error),
        VaultCommandError::Creation(error) => managed_creation_error(error),
        VaultCommandError::Credential(error) => managed_mutation_error(error),
    }
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
    let record = state
        .application
        .create_vault(&workspace_id, params.display_name, params.metadata)
        .await
        .map_err(command_error)?;
    Ok((StatusCode::OK, Json(VaultState::project_vault(&record))))
}

async fn retrieve_vault(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let record = state
        .application
        .visible_vault(&workspace_id, &id)
        .await
        .map_err(command_error)?;
    Ok(Json(VaultState::project_vault(&record)))
}

/// `GET /v1/vaults` — one full page of vaults (the SDK `beta.vaults.list`).
/// Deterministic order: ascending wire id (`vlt_…` is zero-padded, so
/// lexicographic == creation order). Archived vaults are excluded unless
/// `?include_archived=true`.
async fn list_vaults(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    ManagedQuery(query): ManagedQuery<ListQuery>,
    ManagedQuery(page): ManagedQuery<PageQuery>,
) -> Result<Json<PageCursor<Vault>>, WireError> {
    let records = state
        .application
        .visible_vaults(&workspace_id, query.include_archived)
        .await
        .map_err(storage_error)?;
    let data = records.iter().map(VaultState::project_vault).collect();
    Ok(Json(paginate(data, &page, |v| v.id.as_str())))
}

async fn delete_vault(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> Result<Json<DeletedVault>, WireError> {
    state
        .application
        .delete_vault(&workspace_id, &id)
        .await
        .map_err(command_error)?;
    Ok(Json(DeletedVault {
        id,
        object_type: "vault_deleted",
    }))
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
    let record = state
        .application
        .archive_vault(&workspace_id, &id)
        .await
        .map_err(command_error)?;
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
    let record = state
        .application
        .update_vault(&workspace_id, &id, params.display_name, params.metadata)
        .await
        .map_err(command_error)?;
    Ok(Json(VaultState::project_vault(&record)))
}

async fn create_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(resource_workspace): RequiredWorkspaceScope,
    Path(vault_id): Path<String>,
    headers: HeaderMap,
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
            injection_location,
            metadata,
            display_name,
        } => {
            let injection_location = create_injection_location(injection_location)?;
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
                injection_location,
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

    let idempotency_key = parse_idempotency_key_header(&headers).map_err(bad_request)?;
    let record = state
        .application
        .create_credential(
            &resource_workspace,
            vault_id,
            idempotency_key.as_deref(),
            VaultCredentialDraft {
                source,
                descriptor,
                auxiliary_materials,
                auth,
                metadata,
                display_name,
            },
        )
        .await
        .map_err(command_error)?;
    Ok((
        StatusCode::OK,
        Json(VaultState::project_credential(&record)),
    ))
}

/// `GET /v1/vaults/:vault_id/credentials` — one full page of a vault's
/// credentials (the SDK `beta.vaults.credentials.list`). An unknown vault is a
/// `404` (not an empty page), matching retrieve. Deterministic order: ascending
/// wire id. Archived credentials are excluded unless `?include_archived=true`.
async fn list_credentials(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path(vault_id): Path<String>,
    ManagedQuery(query): ManagedQuery<ListQuery>,
    ManagedQuery(page): ManagedQuery<PageQuery>,
) -> Result<Json<PageCursor<Credential>>, WireError> {
    let records = state
        .application
        .visible_credentials(&workspace_id, &vault_id, query.include_archived)
        .await
        .map_err(command_error)?;
    let data = records.iter().map(VaultState::project_credential).collect();
    Ok(Json(paginate(data, &page, |c| c.id.as_str())))
}

async fn retrieve_credential(
    State(state): State<Arc<VaultState>>,
    RequiredWorkspaceScope(workspace_id): RequiredWorkspaceScope,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let record = state
        .application
        .visible_credential(&workspace_id, &vault_id, &id)
        .await
        .map_err(command_error)?;
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
    state
        .application
        .delete_credential(&workspace_id, &vault_id, &id)
        .await
        .map_err(command_error)?;
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
    let record = state
        .application
        .archive_credential(&workspace_id, &vault_id, &id)
        .await
        .map_err(command_error)?;
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
        .application
        .active_credential(&workspace_id, &vault_id, &id)
        .await
        .map_err(command_error)?;
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
    let injection_location_update =
        match (&params.auth, &record.auth) {
            (
                Some(CredentialUpdateAuth::EnvironmentVariable {
                    injection_location: Some(update),
                    ..
                }),
                AuthRecord::EnvironmentVariable {
                    injection_location: current,
                    ..
                },
            ) => Some(current.merge(update.header, update.body).ok_or_else(|| {
                bad_request("injection_location must enable header, body, or both")
            })?),
            _ => None,
        };
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
                if let AuthRecord::EnvironmentVariable {
                    networking: current_networking,
                    injection_location: current_location,
                    ..
                } = &mut record.auth
                {
                    if let Some(nw) = networking {
                        *current_networking = match nw {
                            CredentialNetworking::Unrestricted => {
                                ManagedCredentialNetworking::Unrestricted
                            }
                            CredentialNetworking::Limited { allowed_hosts } => {
                                ManagedCredentialNetworking::Limited { allowed_hosts }
                            }
                        };
                    }
                    if let Some(location) = injection_location_update {
                        *current_location = location;
                    }
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
        awaken_credential_vault::apply_metadata_patch(&mut record.metadata, patch);
    }
    let record = state
        .application
        .change_credential(VaultCredentialChange {
            before: before_record,
            after: record,
            material: material_patch,
            advance_source_without_material,
        })
        .await
        .map_err(command_error)?;
    Ok(Json(VaultState::project_credential(&record)))
}
