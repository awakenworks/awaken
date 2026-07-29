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
//! [`awaken_managed_bridge`] ACL (secret-in), and every response is secret-free
//! (secret-out never happens). The credential rows land in a
//! [`CredentialRepo`](awaken_credential_vault::repo::CredentialRepo), so a vault
//! credential entered here is the same row the resolver binds a run to.
//!
//! Scope (Phase 3): all three credential types — `environment_variable`,
//! `static_bearer`, and `mcp_oauth`. Every wire secret (`secret_value`, `token`,
//! `access_token`, `refresh_token`, `client_secret`) is write-only: sealed into
//! the `SecretStore` on the way in as one revisioned credential material set,
//! never present in any response. The MCP-OAuth validate route live-probes the
//! MCP server when the composition root wires an [`McpProbe`]
//! ([`VaultState::with_probe`]): the credential's access token is materialized
//! here and the port receives the resolved secret, never a vault ref. Without a
//! probe — and always for `environment_variable` / `static_bearer`, which have no
//! MCP handshake to probe — `status` stays `unknown` (never a false `valid`).
//! [`VaultState::mcp_credential_source_for_url`] is the seam a session uses to
//! bind an MCP server to a vault credential by URL. Once selected,
//! [`VaultState::mcp_access_for_source`] compiles the sole exact credential and
//! refresh/reseal execution value.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{
    CredentialMaterialPatch, CredentialRepo, CredentialRetirement, advance_credential_revision,
    enter_credential, enter_credential_with_materials, revoke_credential,
    rotate_credential_materials,
};
use awaken_credential_vault::{
    CredentialCreateParams as DomainCredentialCreateParams, CredentialKind, CredentialSourceId,
    OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore, StructuredCredentialMaterial,
};
use awaken_managed_bridge::{
    WireEnvVarCreate, WireMcpOauthCreate, WireStaticBearerCreate, env_var_to_create_params,
    mcp_oauth_to_create_params, static_bearer_to_create_params,
};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::routes::{ManagedJson, WorkspaceScope};
use crate::types::vault::{
    Credential, CredentialAuth, CredentialCreateParams, CredentialCreateWire, CredentialNetworking,
    CredentialUpdateAuth, CredentialUpdateParams, CredentialValidation, CredentialValidationStatus,
    DeletedCredential, DeletedVault, ListQuery, McpOauthRefreshResponse, McpProbeResult,
    TokenEndpointAuthParams, TokenEndpointAuthResponse, TokenEndpointAuthUpdate, Vault,
    VaultCreateParams, VaultUpdateParams,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};

/// Deterministic timestamp stamped on every vault/credential object, matching the
/// session surface's `PROCESSED_AT` convention (no wall-clock/uuid dependency, so
/// the wire is reproducible under test).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// Anthropic's per-vault credential cap.
const MAX_CREDENTIALS_PER_VAULT: usize = 20;

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

// The live MCP-probe port + its status now live in `awaken-session-contract`
// (a contract/ leaf), re-exported here so existing `awaken_protocol_managed::…` paths
// keep resolving until consumers flip to the contract directly.
pub use awaken_session_contract::{McpProbe, McpProbeStatus};

// ---- State ------------------------------------------------------------------

/// A stored vault (control-plane fields only; the display projection adds tags).
#[derive(Clone)]
struct VaultRecord {
    display_name: String,
    metadata: BTreeMap<String, String>,
    /// `Some(ts)` once archived (soft-delete); excluded from a default list.
    archived_at: Option<String>,
}

/// The kind-specific wire-only fields of a stored credential — everything the
/// neutral domain row does not carry (names, networking, MCP server URL, refresh
/// configuration). Secret-free by construction: it holds `SecretRef`s, never
/// material.
#[derive(Clone)]
enum AuthRecord {
    EnvironmentVariable {
        secret_name: String,
        networking: CredentialNetworking,
    },
    StaticBearer {
        mcp_server_url: String,
    },
    McpOauth {
        mcp_server_url: String,
        expires_at: Option<String>,
        refresh: Option<McpOauthRefreshRecord>,
    },
}

fn auth_mcp_server_url(auth: &AuthRecord) -> Option<&str> {
    match auth {
        AuthRecord::StaticBearer { mcp_server_url }
        | AuthRecord::McpOauth { mcp_server_url, .. } => Some(mcp_server_url),
        AuthRecord::EnvironmentVariable { .. } => None,
    }
}

/// Stored refresh configuration for an `mcp_oauth` credential: the secret-free
/// projection plus its authentication mode. Material references belong only to
/// the credential aggregate; execution receives the canonical
/// `CredentialRefreshAccess` compiled from that aggregate.
#[derive(Clone)]
struct McpOauthRefreshRecord {
    projection: McpOauthRefreshResponse,
    token_endpoint_auth: awaken_credential_contract::TokenEndpointAuth,
}

/// A stored credential: its neutral domain row id plus the wire-only projection
/// fields the domain row does not carry.
#[derive(Clone)]
struct CredentialRecord {
    vault_id: String,
    source_id: CredentialSourceId,
    auth: AuthRecord,
    metadata: BTreeMap<String, String>,
    display_name: Option<String>,
    /// `Some(ts)` once archived (soft-delete); excluded from a default list.
    archived_at: Option<String>,
}

#[derive(Default)]
struct Store {
    vaults: HashMap<String, VaultRecord>,
    credentials: HashMap<String, CredentialRecord>,
}

/// The vault surface's state: the neutral credential domain stores plus the
/// wire-only vault/credential bookkeeping.
pub struct VaultState {
    secrets: Arc<dyn SecretStore>,
    credentials: Arc<dyn CredentialRepo>,
    /// The live MCP probe the validate route consults for `mcp_oauth`
    /// credentials, when the composition root wires one. `None` keeps every
    /// validation `unknown` (never a false `valid`).
    probe: Option<Arc<dyn McpProbe>>,
    inner: std::sync::Mutex<Store>,
    vault_seq: AtomicU64,
    cred_seq: AtomicU64,
}

impl VaultState {
    /// Build the vault surface over the credential domain's secret store + repo.
    pub fn new(secrets: Arc<dyn SecretStore>, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            secrets,
            credentials,
            probe: None,
            inner: std::sync::Mutex::new(Store::default()),
            vault_seq: AtomicU64::new(0),
            cred_seq: AtomicU64::new(0),
        }
    }

    /// Wire the live MCP probe, so `POST .../mcp_oauth_validate` reports a real
    /// `valid`/`invalid` verdict for an `mcp_oauth` credential instead of
    /// `unknown`.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn McpProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Whether `id` names an active vault that may be attached to a new Session.
    /// Archived vaults remain retrievable for audit but are not executable.
    #[must_use]
    pub fn has_vault(&self, id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .vaults
            .get(id)
            .is_some_and(|vault| vault.archived_at.is_none())
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
    #[must_use]
    pub fn credential_source_id(
        &self,
        vault_id: &str,
        credential_id: &str,
    ) -> Option<CredentialSourceId> {
        let store = self.inner.lock().unwrap();
        store
            .credentials
            .get(credential_id)
            .filter(|c| c.vault_id == vault_id)
            .map(|c| c.source_id.clone())
    }

    /// The vault→MCP binding seam. Scan active vaults in caller-supplied order
    /// for an active `mcp_oauth` or `static_bearer` credential whose normalized
    /// server URL equals `url`. Vault order is the precedence contract; id order
    /// is only a defensive tie-breaker inside one vault.
    #[must_use]
    pub fn mcp_credential_source_for_url(
        &self,
        vault_ids: &[String],
        url: &str,
    ) -> Option<CredentialSourceId> {
        let store = self.inner.lock().unwrap();
        let requested = awaken_session_contract::McpTarget::identity(url).ok()?;
        for vault_id in vault_ids {
            let vault_is_active = store
                .vaults
                .get(vault_id)
                .is_some_and(|vault| vault.archived_at.is_none());
            if !vault_is_active {
                continue;
            }
            if let Some((_, credential)) = store
                .credentials
                .iter()
                .filter(|(_, credential)| {
                    credential.vault_id == *vault_id && credential.archived_at.is_none()
                })
                .filter(|(_, credential)| {
                    auth_mcp_server_url(&credential.auth)
                        .and_then(|url| awaken_session_contract::McpTarget::identity(url).ok())
                        .is_some_and(|candidate| candidate == requested)
                })
                .min_by(|(left, _), (right, _)| left.cmp(right))
            {
                return Some(credential.source_id.clone());
            }
        }
        None
    }

    async fn exact_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: Option<&str>,
        usage: awaken_credential_contract::CredentialUsage,
        policy: awaken_credential_contract::CredentialExecutionPolicy,
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
        let revision = u64::try_from(source.version).map_err(|_| {
            awaken_credential_vault::CredentialError::InvalidSource(
                "credential revision is negative".into(),
            )
        })?;
        let access = CredentialAccess::new(
            CredentialRef {
                id: source_id.0.clone(),
                revision,
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            policy,
        );
        Ok((source, access))
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// credential source. Selection and material opening remain separate: this
    /// reads only the active source revision and never opens secret material.
    pub async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        usage: awaken_credential_contract::CredentialUsage,
        policy: awaken_credential_contract::CredentialExecutionPolicy,
    ) -> Result<
        awaken_credential_contract::CredentialAccess,
        awaken_credential_vault::CredentialError,
    > {
        self.exact_access_for_source(source_id, Some(workspace_id), usage, policy)
            .await
            .map(|(_, access)| access)
    }

    /// Compile one exact, secret-free execution pin for a previously selected
    /// MCP credential. Selection and material opening remain separate: this
    /// reads only the credential row revision and opaque material references.
    pub async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
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
                None,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::self_hosted_mcp(),
            )
            .await?;
        let revision = access.credential.revision;
        let refresh = self
            .inner
            .lock()
            .unwrap()
            .credentials
            .values()
            .find(|record| &record.source_id == source_id)
            .and_then(|record| match &record.auth {
                AuthRecord::McpOauth {
                    refresh: Some(refresh),
                    ..
                } => Some(refresh.clone()),
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
                refresh.projection.token_endpoint,
                refresh.projection.client_id,
                refresh.token_endpoint_auth,
                client_secret_ref,
                refresh_token_ref,
                access_token_ref,
                refresh.projection.scope,
                refresh.projection.resource,
            ));
        }
        Ok(access)
    }

    fn project_vault(id: &str, record: &VaultRecord) -> Vault {
        Vault {
            id: id.to_string(),
            archived_at: record.archived_at.clone(),
            created_at: OBJECT_AT.to_string(),
            display_name: record.display_name.clone(),
            metadata: record.metadata.clone(),
            object_type: "vault",
            updated_at: OBJECT_AT.to_string(),
        }
    }

    fn project_credential(id: &str, record: &CredentialRecord) -> Credential {
        let auth = match &record.auth {
            AuthRecord::EnvironmentVariable {
                secret_name,
                networking,
            } => CredentialAuth::EnvironmentVariable {
                secret_name: secret_name.clone(),
                networking: networking.clone(),
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
                refresh: refresh.as_ref().map(|r| r.projection.clone()),
            },
        };
        Credential {
            id: id.to_string(),
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

async fn create_vault(
    State(state): State<Arc<VaultState>>,
    ManagedJson(params): ManagedJson<VaultCreateParams>,
) -> Result<(StatusCode, Json<Vault>), WireError> {
    if params.display_name.is_empty() || params.display_name.len() > 255 {
        return Err(bad_request("display_name must be 1-255 characters"));
    }
    let n = state.vault_seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("vlt_{n:016}");
    let record = VaultRecord {
        display_name: params.display_name,
        metadata: params.metadata,
        archived_at: None,
    };
    let vault = VaultState::project_vault(&id, &record);
    state.inner.lock().unwrap().vaults.insert(id, record);
    Ok((StatusCode::OK, Json(vault)))
}

async fn retrieve_vault(
    State(state): State<Arc<VaultState>>,
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let store = state.inner.lock().unwrap();
    let record = store.vaults.get(&id).ok_or_else(|| not_found("vault"))?;
    Ok(Json(VaultState::project_vault(&id, record)))
}

/// `GET /v1/vaults` — one full page of vaults (the SDK `beta.vaults.list`).
/// Deterministic order: ascending wire id (`vlt_…` is zero-padded, so
/// lexicographic == creation order). Archived vaults are excluded unless
/// `?include_archived=true`.
async fn list_vaults(
    State(state): State<Arc<VaultState>>,
    Query(query): Query<ListQuery>,
    Query(page): Query<PageQuery>,
) -> Json<Page<Vault>> {
    let store = state.inner.lock().unwrap();
    let mut ids: Vec<&String> = store
        .vaults
        .iter()
        .filter(|(_, r)| query.include_archived || r.archived_at.is_none())
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    let data: Vec<Vault> = ids
        .into_iter()
        .map(|id| VaultState::project_vault(id, &store.vaults[id]))
        .collect();
    Json(paginate(data, &page, |v| v.id.as_str()))
}

async fn delete_vault(
    State(state): State<Arc<VaultState>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedVault>, WireError> {
    let mut store = state.inner.lock().unwrap();
    if store.vaults.remove(&id).is_none() {
        return Err(not_found("vault"));
    }
    // Drop the vault's credential bookkeeping too (the sealed secrets are inert).
    store.credentials.retain(|_, c| c.vault_id != id);
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
    Path(id): Path<String>,
) -> Result<Json<Vault>, WireError> {
    let mut store = state.inner.lock().unwrap();
    let record = store
        .vaults
        .get_mut(&id)
        .ok_or_else(|| not_found("vault"))?;
    record.archived_at = Some(OBJECT_AT.to_string());
    Ok(Json(VaultState::project_vault(&id, record)))
}

/// `POST /v1/vaults/:id` — partial update (the SDK `beta.vaults.update`).
/// Replaces `display_name` when present and PATCHes `metadata`; returns the
/// updated vault. A `404` for an unknown vault. An empty body is a no-op update
/// that echoes the vault.
async fn update_vault(
    State(state): State<Arc<VaultState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<VaultUpdateParams>,
) -> Result<Json<Vault>, WireError> {
    if let Some(name) = &params.display_name
        && (name.is_empty() || name.len() > 255)
    {
        return Err(bad_request("display_name must be 1-255 characters"));
    }
    let mut store = state.inner.lock().unwrap();
    let record = store
        .vaults
        .get_mut(&id)
        .ok_or_else(|| not_found("vault"))?;
    if let Some(name) = params.display_name {
        record.display_name = name;
    }
    if let Some(patch) = params.metadata {
        apply_metadata_patch(&mut record.metadata, patch);
    }
    Ok(Json(VaultState::project_vault(&id, record)))
}

async fn create_credential(
    State(state): State<Arc<VaultState>>,
    scope: Option<Extension<WorkspaceScope>>,
    Path(vault_id): Path<String>,
    ManagedJson(params): ManagedJson<CredentialCreateWire>,
) -> Result<(StatusCode, Json<Credential>), WireError> {
    let params = params.into_params();
    // The vault id is a wire-side container id, not an authorization scope. The
    // platform-resolved workspace stamped at the composition edge owns the durable
    // credential row. Standalone embeddings that omit that edge use the documented
    // local/default workspace; tenancy is never derived from a resource id.
    let resource_workspace = scope.map_or_else(
        || crate::state::DEFAULT_SCOPE.to_string(),
        |Extension(scope)| scope.0,
    );
    // Enforce the vault exists and the per-vault constraints up front. The 20-cap
    // spans all credential types. Active env-var names are unique keys; MCP URLs
    // are validated here but may have multiple credentials, with deterministic
    // selection performed only by the Session binding normalizer.
    {
        let store = state.inner.lock().unwrap();
        if !store.vaults.contains_key(&vault_id) {
            return Err(not_found("vault"));
        }
        let in_vault: Vec<&CredentialRecord> = store
            .credentials
            .values()
            .filter(|c| c.vault_id == vault_id)
            .collect();
        if in_vault.len() >= MAX_CREDENTIALS_PER_VAULT {
            return Err(bad_request("vault credential limit reached (max 20)"));
        }
        match &params {
            CredentialCreateParams::EnvironmentVariable { secret_name, .. } => {
                let duplicate = in_vault.iter().any(|credential| {
                    credential.archived_at.is_none()
                        && matches!(
                            &credential.auth,
                            AuthRecord::EnvironmentVariable {
                                secret_name: existing,
                                ..
                            } if existing == secret_name
                        )
                });
                if duplicate {
                    return Err(bad_request(format!(
                        "credential key `{secret_name}` already exists in this vault"
                    )));
                }
            }
            CredentialCreateParams::StaticBearer { mcp_server_url, .. }
            | CredentialCreateParams::McpOauth { mcp_server_url, .. } => {
                if awaken_session_contract::McpTarget::identity(mcp_server_url).is_err() {
                    return Err(bad_request(
                        "mcp_server_url must be an absolute HTTP(S) URL",
                    ));
                }
            }
        }
    }

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
                networking,
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
                WireStaticBearerCreate {
                    mcp_server_url: mcp_server_url.clone(),
                    token,
                },
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
                resource_workspace,
                WireMcpOauthCreate {
                    mcp_server_url: mcp_server_url.clone(),
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
                        projection,
                        token_endpoint_auth,
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

    let n = state.cred_seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("crd_{n:016}");
    let record = CredentialRecord {
        vault_id,
        source_id: source.id,
        auth,
        metadata,
        display_name,
        archived_at: None,
    };
    let credential = VaultState::project_credential(&id, &record);
    state.inner.lock().unwrap().credentials.insert(id, record);
    Ok((StatusCode::OK, Json(credential)))
}

/// `GET /v1/vaults/:vault_id/credentials` — one full page of a vault's
/// credentials (the SDK `beta.vaults.credentials.list`). An unknown vault is a
/// `404` (not an empty page), matching retrieve. Deterministic order: ascending
/// wire id. Archived credentials are excluded unless `?include_archived=true`.
async fn list_credentials(
    State(state): State<Arc<VaultState>>,
    Path(vault_id): Path<String>,
    Query(query): Query<ListQuery>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Credential>>, WireError> {
    let store = state.inner.lock().unwrap();
    if !store.vaults.contains_key(&vault_id) {
        return Err(not_found("vault"));
    }
    let mut ids: Vec<&String> = store
        .credentials
        .iter()
        .filter(|(_, c)| c.vault_id == vault_id)
        .filter(|(_, c)| query.include_archived || c.archived_at.is_none())
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    let data: Vec<Credential> = ids
        .into_iter()
        .map(|id| VaultState::project_credential(id, &store.credentials[id]))
        .collect();
    Ok(Json(paginate(data, &page, |c| c.id.as_str())))
}

async fn retrieve_credential(
    State(state): State<Arc<VaultState>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let store = state.inner.lock().unwrap();
    let record = store
        .credentials
        .get(&id)
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    Ok(Json(VaultState::project_credential(&id, record)))
}

/// `DELETE /v1/vaults/:vault_id/credentials/:id` — hard-delete one credential
/// (the SDK `beta.vaults.credentials.delete`). Drops the wire bookkeeping (the
/// sealed secrets go inert, as in `delete_vault`); the domain row is orphaned,
/// never re-referenced. Scoped by `vault_id`: a credential under another vault
/// 404s rather than deleting across the path scope.
async fn delete_credential(
    State(state): State<Arc<VaultState>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<DeletedCredential>, WireError> {
    let record = state
        .inner
        .lock()
        .unwrap()
        .credentials
        .get(&id)
        .filter(|record| record.vault_id == vault_id)
        .cloned()
        .ok_or_else(|| not_found("credential"))?;
    retire_record(&state, &record).await?;
    state.inner.lock().unwrap().credentials.remove(&id);
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
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<Credential>, WireError> {
    let record = state
        .inner
        .lock()
        .unwrap()
        .credentials
        .get(&id)
        .filter(|record| record.vault_id == vault_id)
        .cloned()
        .ok_or_else(|| not_found("credential"))?;
    retire_record(&state, &record).await?;
    let mut store = state.inner.lock().unwrap();
    let record = store
        .credentials
        .get_mut(&id)
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    record.archived_at = Some(OBJECT_AT.to_string());
    Ok(Json(VaultState::project_credential(&id, record)))
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
    Path((vault_id, id)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<CredentialUpdateParams>,
) -> Result<Json<Credential>, WireError> {
    // Phase 1 — validate the kind match + refresh precondition, snapshot the id.
    let source_id = {
        let store = state.inner.lock().unwrap();
        let record = store
            .credentials
            .get(&id)
            .filter(|c| c.vault_id == vault_id)
            .ok_or_else(|| not_found("credential"))?;
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

    // Phase 3 — apply the secret-free wire mutations under the lock, then project.
    let mut store = state.inner.lock().unwrap();
    let record = store
        .credentials
        .get_mut(&id)
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
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
                    *cur = nw;
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
                            r.projection.scope = Some(scope);
                        }
                        if let Some(tea) = update.token_endpoint_auth {
                            let (tag, auth) = match tea {
                                TokenEndpointAuthUpdate::None => (
                                    TokenEndpointAuthResponse::None,
                                    awaken_credential_contract::TokenEndpointAuth::None,
                                ),
                                TokenEndpointAuthUpdate::ClientSecretBasic { .. } => (
                                    TokenEndpointAuthResponse::ClientSecretBasic,
                                    awaken_credential_contract::TokenEndpointAuth::ClientSecretBasic,
                                ),
                                TokenEndpointAuthUpdate::ClientSecretPost { .. } => (
                                    TokenEndpointAuthResponse::ClientSecretPost,
                                    awaken_credential_contract::TokenEndpointAuth::ClientSecretPost,
                                ),
                            };
                            r.projection.token_endpoint_auth = tag;
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
    Ok(Json(VaultState::project_credential(&id, record)))
}

async fn validate_credential(
    State(state): State<Arc<VaultState>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<CredentialValidation>, WireError> {
    // Snapshot the record under the lock; the live probe (if any) awaits after.
    let (source_id, mcp_server_url, has_refresh_token) = {
        let store = state.inner.lock().unwrap();
        let record = store
            .credentials
            .get(&id)
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
        (record.source_id.clone(), url, has_refresh)
    };
    // Live probe: only an `mcp_oauth` credential (it names an MCP server to
    // handshake with) and only when the composition root wired an `McpProbe`.
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
