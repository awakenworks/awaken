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
//! `access_token`, `refresh_token`, `client_secret`) is write-only: sealed (or,
//! for `client_secret`, consumed — see [`TokenEndpointAuthParams`]) on the way in,
//! never present in any response. The MCP-OAuth validate route reports
//! `has_refresh_token` truthfully but does not live-probe the server yet, so its
//! `status` stays `unknown` for every credential type (never a false `valid`).
//! [`VaultState::mcp_credential_source_for_url`] is the seam a session uses to
//! bind an MCP server to a vault credential by URL.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_credential_vault::repo::{CredentialRepo, enter_credential};
use awaken_credential_vault::{CredentialSourceId, SecretRef, SecretStore};
use awaken_managed_bridge::{
    WireEnvVarCreate, WireMcpOauthCreate, WireStaticBearerCreate, env_var_to_create_params,
    mcp_oauth_to_create_params, static_bearer_to_create_params,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::dto::ErrorResponse;
use crate::router::ManagedJson;

/// Deterministic timestamp stamped on every vault/credential object, matching the
/// session surface's `PROCESSED_AT` convention (no wall-clock/uuid dependency, so
/// the wire is reproducible under test).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// Anthropic's per-vault credential cap.
const MAX_CREDENTIALS_PER_VAULT: usize = 20;

// ---- Wire DTOs (mirror @anthropic-ai/sdk beta.vaults.*) --------------------

/// `BetaManagedAgentsVault` — a credential container.
#[derive(Debug, Clone, Serialize)]
pub struct Vault {
    pub id: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub display_name: String,
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub updated_at: String,
}

/// `VaultCreateParams` body.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultCreateParams {
    pub display_name: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// `BetaManagedAgentsDeletedVault`.
#[derive(Debug, Clone, Serialize)]
pub struct DeletedVault {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
}

/// The outbound-host substitution scope of an env-var credential
/// (`BetaManagedAgentsCredentialNetworking*`), tagged by `type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialNetworking {
    Unrestricted,
    Limited { allowed_hosts: Vec<String> },
}

/// The token-endpoint auth scheme as it arrives on the wire
/// (`BetaManagedAgentsTokenEndpointAuth{None,Basic,Post}Param`). The
/// `client_secret` is write-only and — since this slice performs no live refresh
/// exchange — consumed and dropped here: it is never stored and never echoed. A
/// future refresh-exchange slice must seal it like the other secrets.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenEndpointAuthParams {
    None,
    ClientSecretBasic { client_secret: String },
    ClientSecretPost { client_secret: String },
}

/// The secret-free token-endpoint auth projection
/// (`BetaManagedAgentsTokenEndpointAuth{None,Basic,Post}Response`): the scheme
/// tag only — the SDK response types carry no `client_secret`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TokenEndpointAuthResponse {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

/// `BetaManagedAgentsMCPOAuthRefreshParams` — refresh configuration on create.
/// `refresh_token` is write-only: sealed under its own `SecretRef` next to the
/// credential row, never echoed back.
#[derive(Debug, Clone, Deserialize)]
pub struct McpOauthRefreshParams {
    pub client_id: String,
    /// Write-only: sealed into the `SecretStore`, never echoed back.
    pub refresh_token: String,
    pub token_endpoint: String,
    /// The SDK requires this, but we tolerate its absence (defaults to `none`) —
    /// nothing in this slice exchanges tokens, so nothing can misfire.
    #[serde(default)]
    pub token_endpoint_auth: Option<TokenEndpointAuthParams>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// `BetaManagedAgentsMCPOAuthRefreshResponse` — the secret-free refresh
/// projection: configuration only, never the refresh token or client secret.
#[derive(Debug, Clone, Serialize)]
pub struct McpOauthRefreshResponse {
    pub client_id: String,
    pub token_endpoint: String,
    pub token_endpoint_auth: TokenEndpointAuthResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// The `auth` projection of a credential, one variant per SDK `*AuthResponse`
/// shape. None of them ever carries secret material: env-var projects the
/// variable name + networking scope, `static_bearer` only the server URL, and
/// `mcp_oauth` the server URL + secret-free refresh configuration.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialAuth {
    EnvironmentVariable {
        secret_name: String,
        networking: CredentialNetworking,
    },
    StaticBearer {
        mcp_server_url: String,
    },
    McpOauth {
        mcp_server_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh: Option<McpOauthRefreshResponse>,
    },
}

/// `BetaManagedAgentsCredential` — the secret-free projection.
#[derive(Debug, Clone, Serialize)]
pub struct Credential {
    pub id: String,
    pub archived_at: Option<String>,
    pub auth: CredentialAuth,
    pub created_at: String,
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub updated_at: String,
    pub vault_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Credential create params, discriminated by `type`, one variant per SDK
/// `*CreateParams` shape. An unknown tag deserialize-fails into a clean `400`.
/// All secret fields are write-only.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialCreateParams {
    EnvironmentVariable {
        secret_name: String,
        /// Write-only: sealed into the `SecretStore`, never echoed back.
        secret_value: String,
        networking: CredentialNetworking,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
    /// `BetaManagedAgentsStaticBearerCreateParams`.
    StaticBearer {
        mcp_server_url: String,
        /// Write-only: sealed into the `SecretStore`, never echoed back.
        token: String,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
    /// `BetaManagedAgentsMCPOAuthCreateParams`.
    McpOauth {
        mcp_server_url: String,
        /// Write-only: sealed into the `SecretStore`, never echoed back.
        access_token: String,
        #[serde(default)]
        expires_at: Option<String>,
        #[serde(default)]
        refresh: Option<McpOauthRefreshParams>,
        #[serde(default)]
        metadata: BTreeMap<String, String>,
        #[serde(default)]
        display_name: Option<String>,
    },
}

/// `BetaManagedAgentsCredentialValidationStatus`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialValidationStatus {
    Valid,
    Invalid,
    Unknown,
}

/// `BetaManagedAgentsCredentialValidation`.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialValidation {
    pub credential_id: String,
    pub has_refresh_token: bool,
    pub mcp_probe: Option<serde_json::Value>,
    pub refresh: Option<serde_json::Value>,
    pub status: CredentialValidationStatus,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub validated_at: String,
    pub vault_id: String,
}

// ---- State ------------------------------------------------------------------

/// A stored vault (control-plane fields only; the display projection adds tags).
#[derive(Clone)]
struct VaultRecord {
    display_name: String,
    metadata: BTreeMap<String, String>,
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
/// projection plus the ref the sealed refresh token lives under (the future
/// refresh-exchange slice reads it; this slice only reports its presence).
#[derive(Clone)]
struct McpOauthRefreshRecord {
    projection: McpOauthRefreshResponse,
    #[allow(dead_code)] // Read by the refresh-exchange slice; sealed-proof today.
    refresh_token_ref: SecretRef,
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
            inner: std::sync::Mutex::new(Store::default()),
            vault_seq: AtomicU64::new(0),
            cred_seq: AtomicU64::new(0),
        }
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
            .filter(|(_, c)| vault_ids.iter().any(|v| *v == c.vault_id))
            .filter(|(_, c)| {
                matches!(&c.auth, AuthRecord::McpOauth { mcp_server_url, .. } if mcp_server_url == url)
            })
            .min_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, c)| c.source_id.clone())
    }

    fn project_vault(id: &str, record: &VaultRecord) -> Vault {
        Vault {
            id: id.to_string(),
            archived_at: None,
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
            archived_at: None,
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
        .route("/v1/vaults", post(create_vault))
        .route("/v1/vaults/:id", get(retrieve_vault).delete(delete_vault))
        .route("/v1/vaults/:vault_id/credentials", post(create_credential))
        .route(
            "/v1/vaults/:vault_id/credentials/:id",
            get(retrieve_credential),
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
            // Split the wire refresh object: the token goes to the bridge to be
            // sealed; the rest is secret-free configuration for the projection
            // (the client_secret inside token_endpoint_auth is dropped — see
            // `TokenEndpointAuthParams`).
            let (refresh_config, refresh_token) = match refresh {
                Some(r) => {
                    let projection = McpOauthRefreshResponse {
                        client_id: r.client_id,
                        token_endpoint: r.token_endpoint,
                        token_endpoint_auth: match r.token_endpoint_auth {
                            None | Some(TokenEndpointAuthParams::None) => {
                                TokenEndpointAuthResponse::None
                            }
                            Some(TokenEndpointAuthParams::ClientSecretBasic { .. }) => {
                                TokenEndpointAuthResponse::ClientSecretBasic
                            }
                            Some(TokenEndpointAuthParams::ClientSecretPost { .. }) => {
                                TokenEndpointAuthResponse::ClientSecretPost
                            }
                        },
                        resource: r.resource,
                        scope: r.scope,
                    };
                    (Some(projection), Some(r.refresh_token))
                }
                None => (None, None),
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
            // the row (the row's own material_ref holds the access token).
            let refresh_record = match (refresh_config, bridged.refresh_secret) {
                (Some(projection), Some(secret)) => {
                    let r = SecretRef(format!("sec:refresh:{}", source.id.0));
                    state
                        .secrets
                        .put(&r, secret)
                        .await
                        .map_err(|e| bad_request(e.to_string()))?;
                    Some(McpOauthRefreshRecord {
                        projection,
                        refresh_token_ref: r,
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
    };
    let credential = VaultState::project_credential(&id, &record);
    state.inner.lock().unwrap().credentials.insert(id, record);
    Ok((StatusCode::OK, Json(credential)))
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

async fn validate_credential(
    State(state): State<Arc<VaultState>>,
    Path((vault_id, id)): Path<(String, String)>,
) -> Result<Json<CredentialValidation>, WireError> {
    let store = state.inner.lock().unwrap();
    let record = store
        .credentials
        .get(&id)
        .filter(|c| c.vault_id == vault_id)
        .ok_or_else(|| not_found("credential"))?;
    // No live MCP probe in this slice, so `status` stays `unknown` for every
    // credential type (never a false `valid`). Only the refresh-token presence of
    // an mcp_oauth credential is reported truthfully — it is a stored fact, not a
    // probe result.
    let has_refresh_token = matches!(
        &record.auth,
        AuthRecord::McpOauth {
            refresh: Some(_),
            ..
        }
    );
    Ok(Json(CredentialValidation {
        credential_id: id.clone(),
        has_refresh_token,
        mcp_probe: None,
        refresh: None,
        status: CredentialValidationStatus::Unknown,
        object_type: "vault_credential_validation",
        validated_at: OBJECT_AT.to_string(),
        vault_id: record.vault_id.clone(),
    }))
}
