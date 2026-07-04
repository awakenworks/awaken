//! The Managed Agents **vault / credential** front door (ADR-0043).
//!
//! These are the public `/v1/vaults...` routes the official `@anthropic-ai/sdk`
//! `beta.vaults.*` client calls. The DTOs mirror the SDK types exactly
//! (`BetaManagedAgentsVault`, `BetaManagedAgentsCredential`, the
//! `environment_variable` auth/create shapes, and `BetaManagedAgentsCredentialValidation`);
//! the wire keeps Anthropic's snake_case tags (`vault` / `vault_credential` /
//! `environment_variable`).
//!
//! Storage is neutral: a credential's secret is sealed into the credential
//! domain's [`SecretStore`](awaken_credential_vault::SecretStore) via the
//! [`awaken_managed_bridge`] ACL (secret-in), and every response is secret-free
//! (secret-out never happens). The credential rows land in a
//! [`CredentialRepo`](awaken_credential_vault::repo::CredentialRepo), so a vault
//! credential entered here is the same row the resolver binds a run to.
//!
//! Scope: the `environment_variable` credential type end to end. `static_bearer`
//! and `mcp_oauth` create params are rejected as unknown variants (a clean 400)
//! until Phase 3 fills them in; the MCP-OAuth validate route answers `unknown`
//! for an env-var credential (no live probe yet).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_credential_vault::repo::{CredentialRepo, enter_credential};
use awaken_credential_vault::{CredentialSourceId, SecretStore};
use awaken_managed_bridge::{WireEnvVarCreate, env_var_to_create_params};
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

/// The `auth` projection of a credential. Env-var is the only resolved shape for
/// now (`BetaManagedAgentsEnvironmentVariableAuthResponse`); it never carries the
/// secret value, only the variable name and its networking scope.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialAuth {
    EnvironmentVariable {
        secret_name: String,
        networking: CredentialNetworking,
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

/// Credential create params, discriminated by `type`. Only `environment_variable`
/// is accepted today; other tags deserialize-fail into a clean `400`.
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

/// A stored credential: its neutral domain row id plus the wire-only projection
/// fields (networking / display name) the domain row does not carry.
#[derive(Clone)]
struct CredentialRecord {
    vault_id: String,
    source_id: CredentialSourceId,
    secret_name: String,
    networking: CredentialNetworking,
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
        Credential {
            id: id.to_string(),
            archived_at: None,
            auth: CredentialAuth::EnvironmentVariable {
                secret_name: record.secret_name.clone(),
                networking: record.networking.clone(),
            },
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
    let CredentialCreateParams::EnvironmentVariable {
        secret_name,
        secret_value,
        networking,
        metadata,
        display_name,
    } = params;

    // Enforce the vault exists and the per-vault key/count constraints up front.
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
        if in_vault.clone().any(|c| c.secret_name == secret_name) {
            return Err(bad_request(format!(
                "credential key `{secret_name}` already exists in this vault"
            )));
        }
    }

    // Secret-in through the ACL: the raw value crosses into the domain here and is
    // sealed by the SecretStore; the returned row is secret-free.
    let create = env_var_to_create_params(
        vault_id.clone(),
        None,
        WireEnvVarCreate {
            secret_name: secret_name.clone(),
            secret_value,
        },
    );
    let source = enter_credential(create, &*state.secrets, &*state.credentials)
        .await
        .map_err(|e| bad_request(e.to_string()))?;

    let n = state.cred_seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("crd_{n:016}");
    let record = CredentialRecord {
        vault_id,
        source_id: source.id,
        secret_name,
        networking,
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
    // MCP-OAuth live probing is Phase 3; an env-var credential has no upstream to
    // probe, so its validation is `unknown` (never a false `valid`).
    Ok(Json(CredentialValidation {
        credential_id: id.clone(),
        has_refresh_token: false,
        mcp_probe: None,
        refresh: None,
        status: CredentialValidationStatus::Unknown,
        object_type: "vault_credential_validation",
        validated_at: OBJECT_AT.to_string(),
        vault_id: record.vault_id.clone(),
    }))
}
