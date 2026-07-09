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
//! the `SecretStore` on the way in (a confidential-client `client_secret` under
//! its own `sec:client:{source_id}` ref — see [`TokenEndpointAuthParams`]),
//! never present in any response. The MCP-OAuth validate route live-probes the
//! MCP server when the composition root wires an [`McpProbe`]
//! ([`VaultState::with_probe`]): the credential's access token is materialized
//! here and the port receives the resolved secret, never a vault ref. Without a
//! probe — and always for `environment_variable` / `static_bearer`, which have no
//! MCP handshake to probe — `status` stays `unknown` (never a false `valid`).
//! [`VaultState::mcp_credential_source_for_url`] is the seam a session uses to
//! bind an MCP server to a vault credential by URL, and
//! [`VaultState::mcp_refresh_for_source`] exposes that credential's stored
//! refresh configuration for the session's transport-level token refresh.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{CredentialRepo, enter_credential};
use awaken_credential_vault::{CredentialSourceId, SecretRef, SecretStore};
use awaken_managed_bridge::{
    WireEnvVarCreate, WireMcpOauthCreate, WireStaticBearerCreate, env_var_to_create_params,
    mcp_oauth_to_create_params, static_bearer_to_create_params,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::router::ManagedJson;
use crate::types::vault::{
    Credential, CredentialAuth, CredentialCreateParams, CredentialNetworking, CredentialUpdateAuth,
    CredentialUpdateParams, CredentialValidation, CredentialValidationStatus, DeletedCredential,
    DeletedVault, ListQuery, McpOauthRefreshResponse, TokenEndpointAuthParams,
    TokenEndpointAuthResponse, TokenEndpointAuthUpdate, Vault, VaultCreateParams,
    VaultUpdateParams,
};
use crate::types::{ErrorResponse, Page};

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

/// The stored refresh configuration of an `mcp_oauth` credential, exposed for a
/// session's transport-level token refresh (consumer: `ManagedState::create_session`
/// → [`crate::McpServerBinding`], which the server's `ManagedHost::prepare_session`
/// turns into a live refresher on the MCP transport). Secret-free by construction:
/// it carries the sealed refresh token's [`SecretRef`] (and, for a
/// confidential-client scheme, the sealed client secret's ref via
/// [`TokenEndpointAuthBinding`]), never material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRefreshBinding {
    pub token_endpoint: String,
    pub client_id: String,
    /// The ref the sealed refresh token lives under (`sec:refresh:{source_id}`).
    pub refresh_token_ref: SecretRef,
    /// How the refresher must authenticate the grant at the token endpoint.
    pub token_endpoint_auth: TokenEndpointAuthBinding,
    pub scope: Option<String>,
    pub resource: Option<String>,
}

/// The client-authentication method of a refresh grant, as the session's
/// refresher must apply it (consumer: the server's `VaultRefresher` grant
/// construction — RFC 6749 §2.3.1 `Basic` header for `client_secret_basic`,
/// `client_secret` form field for `client_secret_post`). A confidential scheme
/// carries the sealed client secret's ref (`sec:client:{source_id}`), never
/// material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenEndpointAuthBinding {
    None,
    ClientSecretBasic { secret_ref: SecretRef },
    ClientSecretPost { secret_ref: SecretRef },
}

/// The status a live MCP probe reports for an `mcp_oauth` credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProbeStatus {
    /// Connect + MCP `initialize` handshake succeeded with the bearer.
    Valid,
    /// The server refused the bearer with an auth challenge (401/403).
    Invalid { http_status: u16 },
    /// No verdict: unreachable, protocol error, or otherwise inconclusive.
    Unknown,
}

/// A port that live-probes an MCP server with an already-materialized bearer.
/// The implementation (server-local, backed by `awaken-ext-mcp`) is the only
/// place the MCP client is named — this crate stays wire-client-free. The
/// signature takes the resolved secret, never a vault ref: materialization
/// happens on this side of the port.
#[async_trait::async_trait]
pub trait McpProbe: Send + Sync {
    async fn probe(&self, mcp_server_url: &str, bearer: &RedactedString) -> McpProbeStatus;
}

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

/// Stored refresh configuration for an `mcp_oauth` credential: the secret-free
/// projection plus the refs the sealed refresh token (and, for a confidential
/// client, the sealed client secret) live under, which
/// [`VaultState::mcp_refresh_for_source`] hands to the session's refresher.
#[derive(Clone)]
struct McpOauthRefreshRecord {
    projection: McpOauthRefreshResponse,
    refresh_token_ref: SecretRef,
    token_endpoint_auth: TokenEndpointAuthBinding,
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

    /// Whether `id` names an existing vault. Consumer: `ManagedState::create_session`,
    /// which fails a create closed (404, naming the vault id) when a
    /// `vault_ids` entry names no vault — instead of silently binding nothing
    /// and only surfacing a 401 at the first turn.
    #[must_use]
    pub fn has_vault(&self, id: &str) -> bool {
        self.inner.lock().unwrap().vaults.contains_key(id)
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

    /// The vault→MCP binding seam (SDK model: a credential binds to an MCP server
    /// by `mcp_server_url`; a session binds to vaults by `vault_ids`). Scan the
    /// given vaults for an `mcp_oauth` credential whose server URL equals `url`
    /// (exact string match) and return its neutral domain source id — the
    /// session-ingress slice hands that to the resolver, never the wire id.
    /// Env-var and `static_bearer` credentials never match. On ties the lowest
    /// wire id (earliest created) wins, so the pick is deterministic.
    #[must_use]
    pub fn mcp_credential_source_for_url(
        &self,
        vault_ids: &[String],
        url: &str,
    ) -> Option<CredentialSourceId> {
        let store = self.inner.lock().unwrap();
        store
            .credentials
            .iter()
            .filter(|(_, c)| vault_ids.contains(&c.vault_id))
            .filter(|(_, c)| {
                matches!(&c.auth, AuthRecord::McpOauth { mcp_server_url, .. } if mcp_server_url == url)
            })
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, c)| c.source_id.clone())
    }

    /// The stored refresh configuration of the `mcp_oauth` credential backing
    /// `source_id`, if it is refreshable. This is the second half of the
    /// vault→MCP binding seam (consumer: `ManagedState::create_session`, which
    /// carries it on [`crate::McpServerBinding`] next to the source id): the
    /// server's session build turns it into a transport-level refresher. Both
    /// public (`token_endpoint_auth: none`) and confidential
    /// (`client_secret_basic` / `client_secret_post`) clients are exposed — a
    /// confidential scheme's binding carries its sealed client secret's ref
    /// ([`TokenEndpointAuthBinding`]). `None` for env-var / `static_bearer`
    /// credentials and for an `mcp_oauth` credential entered without a refresh
    /// object.
    #[must_use]
    pub fn mcp_refresh_for_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Option<McpRefreshBinding> {
        let store = self.inner.lock().unwrap();
        store
            .credentials
            .values()
            .filter(|c| &c.source_id == source_id)
            .find_map(|c| match &c.auth {
                AuthRecord::McpOauth {
                    refresh: Some(r), ..
                } => Some(McpRefreshBinding {
                    token_endpoint: r.projection.token_endpoint.clone(),
                    client_id: r.projection.client_id.clone(),
                    refresh_token_ref: r.refresh_token_ref.clone(),
                    token_endpoint_auth: r.token_endpoint_auth.clone(),
                    scope: r.projection.scope.clone(),
                    resource: r.projection.resource.clone(),
                }),
                _ => None,
            })
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
            "/v1/vaults/:id",
            get(retrieve_vault).post(update_vault).delete(delete_vault),
        )
        .route("/v1/vaults/:id/archive", post(archive_vault))
        .route(
            "/v1/vaults/:vault_id/credentials",
            post(create_credential).get(list_credentials),
        )
        .route(
            "/v1/vaults/:vault_id/credentials/:id",
            get(retrieve_credential)
                .post(update_credential)
                .delete(delete_credential),
        )
        .route(
            "/v1/vaults/:vault_id/credentials/:id/archive",
            post(archive_credential),
        )
        .route(
            "/v1/vaults/:vault_id/credentials/:id/mcp_oauth_validate",
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
) -> Json<Page<Vault>> {
    let store = state.inner.lock().unwrap();
    let mut ids: Vec<&String> = store
        .vaults
        .iter()
        .filter(|(_, r)| query.include_archived || r.archived_at.is_none())
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    let data = ids
        .into_iter()
        .map(|id| VaultState::project_vault(id, &store.vaults[id]))
        .collect();
    Json(Page::single(data))
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
    if let Some(name) = &params.display_name {
        if name.is_empty() || name.len() > 255 {
            return Err(bad_request("display_name must be 1-255 characters"));
        }
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
    Path(vault_id): Path<String>,
    ManagedJson(params): ManagedJson<CredentialCreateParams>,
) -> Result<(StatusCode, Json<Credential>), WireError> {
    // Enforce the vault exists and the per-vault constraints up front. The 20-cap
    // spans all credential types; the duplicate-name check only applies among
    // env-var credentials (static_bearer / mcp_oauth have no secret_name to
    // collide on — the SDK allows several credentials against one server).
    {
        let store = state.inner.lock().unwrap();
        if !store.vaults.contains_key(&vault_id) {
            return Err(not_found("vault"));
        }
        let in_vault = store
            .credentials
            .values()
            .filter(|c| c.vault_id == vault_id);
        if in_vault.clone().count() >= MAX_CREDENTIALS_PER_VAULT {
            return Err(bad_request("vault credential limit reached (max 20)"));
        }
        if let CredentialCreateParams::EnvironmentVariable { secret_name, .. } = &params {
            let dup = in_vault.clone().any(|c| {
                matches!(&c.auth, AuthRecord::EnvironmentVariable { secret_name: existing, .. }
                    if existing == secret_name)
            });
            if dup {
                return Err(bad_request(format!(
                    "credential key `{secret_name}` already exists in this vault"
                )));
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
                vault_id.clone(),
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
                vault_id.clone(),
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
            // Split the wire refresh object: the refresh token goes to the
            // bridge to be sealed and a confidential-client `client_secret` is
            // sealed below under `sec:client:{source_id}`; the rest is
            // secret-free configuration for the projection (see
            // `TokenEndpointAuthParams`).
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
                vault_id.clone(),
                WireMcpOauthCreate {
                    mcp_server_url: mcp_server_url.clone(),
                    access_token,
                    refresh_token,
                },
            );
            let source = enter(bridged.params)
                .await
                .map_err(|e| bad_request(e.to_string()))?;
            // The refresh token is a second secret: sealed under a sibling ref of
            // the row (the row's own material_ref holds the access token). A
            // confidential-client `client_secret` is a third, sealed under its
            // own deterministic sibling ref so the refresh grant can
            // authenticate later.
            let refresh_record = match (refresh_config, bridged.refresh_secret) {
                (Some(projection), Some(secret)) => {
                    let r = SecretRef(format!("sec:refresh:{}", source.id.0));
                    state
                        .secrets
                        .put(&r, secret)
                        .await
                        .map_err(|e| bad_request(e.to_string()))?;
                    let token_endpoint_auth = match (projection.token_endpoint_auth, client_secret)
                    {
                        (TokenEndpointAuthResponse::ClientSecretBasic, Some(cs)) => {
                            let secret_ref = SecretRef(format!("sec:client:{}", source.id.0));
                            state
                                .secrets
                                .put(&secret_ref, RedactedString::new(cs))
                                .await
                                .map_err(|e| bad_request(e.to_string()))?;
                            TokenEndpointAuthBinding::ClientSecretBasic { secret_ref }
                        }
                        (TokenEndpointAuthResponse::ClientSecretPost, Some(cs)) => {
                            let secret_ref = SecretRef(format!("sec:client:{}", source.id.0));
                            state
                                .secrets
                                .put(&secret_ref, RedactedString::new(cs))
                                .await
                                .map_err(|e| bad_request(e.to_string()))?;
                            TokenEndpointAuthBinding::ClientSecretPost { secret_ref }
                        }
                        _ => TokenEndpointAuthBinding::None,
                    };
                    Some(McpOauthRefreshRecord {
                        projection,
                        refresh_token_ref: r,
                        token_endpoint_auth,
                    })
                }
                _ => None,
            };
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
    let data = ids
        .into_iter()
        .map(|id| VaultState::project_credential(id, &store.credentials[id]))
        .collect();
    Ok(Json(Page::single(data)))
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
    let mut store = state.inner.lock().unwrap();
    let matches_vault = store
        .credentials
        .get(&id)
        .is_some_and(|c| c.vault_id == vault_id);
    if !matches_vault {
        return Err(not_found("credential"));
    }
    store.credentials.remove(&id);
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
    let mut store = state.inner.lock().unwrap();
    let record = store
        .credentials
        .get_mut(&id)
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    record.archived_at = Some(OBJECT_AT.to_string());
    Ok(Json(VaultState::project_credential(&id, record)))
}

/// `POST /v1/vaults/:vault_id/credentials/:id` — partial update (the SDK
/// `beta.vaults.credentials.update`). The credential kind is immutable: an `auth`
/// patch must carry the credential's own `type` or the request is a `400`. Secret
/// fields (`secret_value` / `token` / `access_token` / `refresh_token` /
/// confidential `client_secret`) are write-only and re-sealed under the
/// credential's *existing* refs — never echoed. `display_name` may be cleared
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

    // Phase 2 — re-seal every supplied secret under its existing ref (no lock).
    if let Some(auth) = &params.auth {
        let primary = match auth {
            CredentialUpdateAuth::EnvironmentVariable { secret_value, .. } => secret_value.clone(),
            CredentialUpdateAuth::StaticBearer { token } => token.clone(),
            CredentialUpdateAuth::McpOauth { access_token, .. } => access_token.clone(),
        };
        if let Some(secret) = primary {
            let row = state
                .credentials
                .get(&source_id)
                .await
                .map_err(|e| bad_request(e.to_string()))?;
            let material_ref = row
                .material_ref
                .ok_or_else(|| bad_request("credential row has no material_ref"))?;
            state
                .secrets
                .put(&material_ref, RedactedString::new(secret))
                .await
                .map_err(|e| bad_request(e.to_string()))?;
        }
        if let CredentialUpdateAuth::McpOauth {
            refresh: Some(update),
            ..
        } = auth
        {
            if let Some(rt) = &update.refresh_token {
                let rref = SecretRef(format!("sec:refresh:{}", source_id.0));
                state
                    .secrets
                    .put(&rref, RedactedString::new(rt.clone()))
                    .await
                    .map_err(|e| bad_request(e.to_string()))?;
            }
            // Re-seal the client secret only when the update actually carries one;
            // an omitted `client_secret` keeps the currently-sealed value.
            if let Some(
                TokenEndpointAuthUpdate::ClientSecretBasic {
                    client_secret: Some(cs),
                }
                | TokenEndpointAuthUpdate::ClientSecretPost {
                    client_secret: Some(cs),
                },
            ) = &update.token_endpoint_auth
            {
                let cref = SecretRef(format!("sec:client:{}", source_id.0));
                state
                    .secrets
                    .put(&cref, RedactedString::new(cs.clone()))
                    .await
                    .map_err(|e| bad_request(e.to_string()))?;
            }
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
                            // The confidential binding always points at the stable
                            // `sec:client:{source_id}` ref: an omitted secret keeps
                            // the sealed value, a supplied one re-sealed it above.
                            let client_ref = || SecretRef(format!("sec:client:{}", source_id.0));
                            let (tag, binding) = match tea {
                                TokenEndpointAuthUpdate::None => (
                                    TokenEndpointAuthResponse::None,
                                    TokenEndpointAuthBinding::None,
                                ),
                                TokenEndpointAuthUpdate::ClientSecretBasic { .. } => (
                                    TokenEndpointAuthResponse::ClientSecretBasic,
                                    TokenEndpointAuthBinding::ClientSecretBasic {
                                        secret_ref: client_ref(),
                                    },
                                ),
                                TokenEndpointAuthUpdate::ClientSecretPost { .. } => (
                                    TokenEndpointAuthResponse::ClientSecretPost,
                                    TokenEndpointAuthBinding::ClientSecretPost {
                                        secret_ref: client_ref(),
                                    },
                                ),
                            };
                            r.projection.token_endpoint_auth = tag;
                            r.token_endpoint_auth = binding;
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
                    mcp_probe = Some(serde_json::json!({ "handshake": "ok" }));
                }
                McpProbeStatus::Invalid { http_status } => {
                    status = CredentialValidationStatus::Invalid;
                    mcp_probe = Some(serde_json::json!({ "http_status": http_status }));
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
