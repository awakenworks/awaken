//! Credential / vault domain (ADR-0043, agents bucket). A **config store** whose
//! aggregates are *secret-free*: a [`CredentialSource`] holds only a reference to
//! secret material, never the material itself. The plaintext lives behind the
//! [`SecretStore`] port and only ever surfaces as an already-resolved
//! [`RedactedString`](awaken_agent_contract::RedactedString) at the injection seam
//! (D6/D9 — the runtime never sees the store, the ref, or a resolver).
//!
//! Two orthogonal axes (oversight-next / awaken-management-contract):
//! *materialization* ([`CredentialKind`]: where the secret lives) and *selection*
//! ([`CredentialBinding`]: which source a run uses). P0 wires `Vault`/`Env` kinds
//! and the `Exact` binding; pools/identities land in P1.

#![forbid(unsafe_code)]

pub mod availability;
#[cfg(feature = "oauth-command")]
pub mod oauth;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod repo;
pub mod schema;
#[cfg(feature = "sealed-aead")]
pub mod sealed;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use availability::{AvailabilityLedger, AvailabilityState};
#[cfg(feature = "oauth-command")]
pub use oauth::{CommandTokenSource, TokenSource};
#[cfg(feature = "postgres")]
pub use postgres::{PostgresCredentialRepo, PostgresSealedBlobStore};
#[cfg(feature = "sealed-aead")]
pub use sealed::{SealedAeadSecretStore, parse_seal_key, resolve_seal_key_hex};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore};

use std::collections::HashMap;
use std::sync::Mutex;

use awaken_agent_contract::RedactedString;

/// Opaque handle into a [`SecretStore`]. Never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct SecretRef(pub String);

/// Stable id of a [`CredentialSource`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct CredentialSourceId(pub String);

/// Where a secret physically lives (the materialization axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// Secret material sealed in the vault (a [`SecretRef`] into [`SecretStore`]).
    Vault,
    /// A host environment variable named by `env_key`; nothing is stored here.
    Env,
    /// An OAuth-backed provider credential (#5): the secret is a short-lived
    /// Bearer token minted on demand by running `oauth_command`, never stored. The
    /// long-lived grant lives inside the helper (e.g. `gcloud`), so nothing secret
    /// crosses the control plane.
    Oauth,
}

/// Server-owned OAuth token helper. The API carries this allowlisted id, never
/// an operator-supplied command line; the credential bounded context owns how
/// it becomes a token source for model, MCP, and A2A consumers alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum OAuthHelper {
    /// Mint a Google access token from the active gcloud account.
    Gcloud,
}

impl OAuthHelper {
    /// Fixed argv for the helper. This preserves the existing persisted
    /// `oauth_command` representation while authoring stays safe and stable.
    #[must_use]
    pub fn command(self) -> Vec<String> {
        match self {
            Self::Gcloud => vec!["gcloud".into(), "auth".into(), "print-access-token".into()],
        }
    }

    /// Recover the public helper id from a stored legacy argv. Unknown commands
    /// remain internal and are never projected as an operator-selectable helper.
    #[must_use]
    pub fn from_command(command: &[String]) -> Option<Self> {
        (command == Self::Gcloud.command()).then_some(Self::Gcloud)
    }
}

/// Lifecycle of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Active,
    Disabled,
    Archived,
}

/// The stored credential row — **secret-free** (the read/serde projection). The
/// secret is reachable only via `material_ref` through the [`SecretStore`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialSource {
    pub id: CredentialSourceId,
    pub workspace_id: String,
    pub kind: CredentialKind,
    /// Provider namespace this credential authenticates (`anthropic`, `openai`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Environment-variable name the secret is injected under (all kinds may set it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// Vault reference; `None` for `Env` (the secret never crosses the control plane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material_ref: Option<SecretRef>,
    /// The refresh helper for a [`CredentialKind::Oauth`] source: `[program,
    /// args…]`, whose trimmed stdout is a fresh access token. `None` for every
    /// other kind. Only a *reference to a command* travels — never a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_command: Option<Vec<String>>,
    pub status: CredentialStatus,
    pub version: i64,
}

/// The "which credential" axis (oversight-next / awaken-management-contract).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialBinding {
    /// No credential is needed.
    None,
    /// Use exactly one source.
    Exact {
        credential_source_id: CredentialSourceId,
    },
    /// Use one eligible member of a pool; the resolver selects by policy and may
    /// fail over to the next member if the chosen one cannot be materialized.
    OneOfCredentialPool {
        credential_pool_id: CredentialPoolId,
    },
}

/// A credential pool identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialPoolId(pub String);

/// How a pool picks among its eligible members — the *intent* behind selection,
/// consumed by the runtime selector (an availability-aware picker, per ADR-0043).
/// The pool's [`selection_order`](CredentialPool::selection_order) always yields the
/// eligible members in ordinal order; this policy decides which of them the picker
/// commits to.
///
/// Names align with awaken-next's `SelectionPolicy`. `#[non_exhaustive]` so adding a
/// policy is not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SelectionPolicy {
    /// Pin the first eligible member in ordinal order (the default; the historical
    /// behavior). Deterministic — the same pool state always picks the same member.
    #[default]
    FirstHealthy,
    /// Round-robin across the eligible members to spread load and delay quota
    /// exhaustion. Rotation state lives in the selector, not the pool.
    RotateSpread,
    /// Reuse the member a prior run in the same scope committed to, when it is still
    /// eligible, for prompt-cache / session affinity.
    StickyResume,
}

/// A pool of interchangeable credential sources for one provider principal
/// (oversight-next account grouping). The resolver picks one eligible member per
/// run; members are tried in policy order so a disabled/exhausted member fails
/// over to the next rather than failing the run (fail-closed only when none work).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialPool {
    pub id: CredentialPoolId,
    pub workspace_id: String,
    pub members: Vec<CredentialPoolMember>,
    /// How the selector picks among eligible members. `#[serde(default)]` keeps
    /// existing pool rows (persisted as JSON) loadable as [`FirstHealthy`], the
    /// historical behavior.
    ///
    /// [`FirstHealthy`]: SelectionPolicy::FirstHealthy
    #[serde(default)]
    pub policy: SelectionPolicy,
}

/// One source's membership in a pool. `ordinal` is the default selection order
/// (ascending); a disabled member is skipped. `selection_weight` is reserved for a
/// future weighted policy and does not affect the default ordinal order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialPoolMember {
    pub credential_source_id: CredentialSourceId,
    pub ordinal: u32,
    pub enabled: bool,
    #[serde(default)]
    pub selection_weight: u32,
}

impl CredentialPool {
    /// The enabled members in selection order (ascending ordinal, then stable by
    /// source id). This is the order the resolver tries for failover.
    #[must_use]
    pub fn selection_order(&self) -> Vec<&CredentialPoolMember> {
        let mut members: Vec<&CredentialPoolMember> =
            self.members.iter().filter(|m| m.enabled).collect();
        members.sort_by(|a, b| member_ordering(a, b));
        members
    }

    /// [`selection_order`](Self::selection_order) with cooled/exhausted members
    /// dropped per the availability ledger at `now_ms` — the members a selector may
    /// actually pick right now. A cooled source rotates out until its deadline; this
    /// is the mid-run credential rotation the engine's candidate loop rides.
    #[must_use]
    pub fn eligible_order(
        &self,
        ledger: &crate::availability::AvailabilityLedger,
        now_ms: u64,
    ) -> Vec<&CredentialPoolMember> {
        self.selection_order()
            .into_iter()
            .filter(|m| {
                member_is_eligible(m.enabled, ledger.state(&m.credential_source_id, now_ms))
            })
            .collect()
    }
}

#[must_use]
pub const fn member_is_eligible(
    enabled: bool,
    availability: crate::availability::AvailabilityState,
) -> bool {
    enabled && availability.is_available()
}

fn member_ordering(a: &CredentialPoolMember, b: &CredentialPoolMember) -> std::cmp::Ordering {
    a.ordinal
        .cmp(&b.ordinal)
        .then_with(|| a.credential_source_id.0.cmp(&b.credential_source_id.0))
}

#[cfg(kani)]
mod verification {
    use super::*;
    use crate::availability::{AvailabilityState, availability_at};

    #[kani::proof]
    fn disabled_credential_pool_members_are_never_eligible() {
        let state = match kani::any::<u8>() % 3 {
            0 => AvailabilityState::Available,
            1 => AvailabilityState::CooledDown {
                retry_at_ms: kani::any(),
            },
            _ => AvailabilityState::Exhausted,
        };
        assert!(!member_is_eligible(false, state));
    }

    #[kani::proof]
    fn credential_cooldown_boundary_is_exact_and_inclusive() {
        let deadline = kani::any::<u64>();
        let now = kani::any::<u64>();
        let state = availability_at(false, Some(deadline), now);
        assert_eq!(state.is_available(), now >= deadline);
    }

    #[kani::proof]
    fn exhausted_credentials_are_unavailable_at_every_time() {
        let state = availability_at(true, Some(kani::any()), kani::any());
        assert_eq!(state, AvailabilityState::Exhausted);
        assert!(!state.is_available());
    }

    #[kani::proof]
    fn a_pool_with_no_enabled_available_member_fails_closed() {
        let enabled = [kani::any::<bool>(), kani::any(), kani::any()];
        let available = [kani::any::<bool>(), kani::any(), kani::any()];
        let mut selected = None;
        for index in 0..3 {
            let state = if available[index] {
                AvailabilityState::Available
            } else {
                AvailabilityState::Exhausted
            };
            if selected.is_none() && member_is_eligible(enabled[index], state) {
                selected = Some(index);
            }
        }
        if (0..3).all(|i| !enabled[i] || !available[i]) {
            assert!(selected.is_none());
        }
    }
}

/// Parameters to create a credential, **carrying the secret** (write-only, only at
/// the create seam — never stored on the aggregate, never serialized out).
pub struct CredentialCreateParams {
    pub workspace_id: String,
    pub kind: CredentialKind,
    pub provider_id: Option<String>,
    pub env_key: Option<String>,
    /// The secret; consumed into the [`SecretStore`], never onto the row.
    pub secret: Option<RedactedString>,
    /// The refresh helper `[program, args…]` for a [`CredentialKind::Oauth`]
    /// source (#5). `None` for every other kind.
    pub oauth_command: Option<Vec<String>>,
}

/// Persistence of raw secret bytes, keyed by [`SecretRef`]. Encryption-at-rest is
/// an adapter concern (ADR-0043): `inmem` here, `plaintext-file`/`sealed-aead`
/// later. The domain and runtime never see ciphertext.
#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    async fn put(&self, r: &SecretRef, secret: RedactedString) -> Result<(), CredentialError>;
    async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError>;
    /// Idempotently remove material. Credential creation uses this as its
    /// compensation edge when the secret write succeeds but the secret-free row
    /// cannot be committed.
    async fn delete(&self, r: &SecretRef) -> Result<(), CredentialError>;
    /// Enumerate opaque references for reconciliation. Production stores must
    /// implement this; adapters without inventory support fail closed.
    async fn inventory(&self) -> Result<Vec<SecretRef>, CredentialError> {
        Err(CredentialError::Storage(
            "secret inventory is not supported by this store".to_string(),
        ))
    }
}

/// A credential-domain failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("secret ref `{0}` not found")]
    SecretNotFound(String),
    #[error("credential source `{0}` not found")]
    SourceNotFound(String),
    #[error("credential pool `{0}` not found")]
    PoolNotFound(String),
    #[error("binding resolves to no credential")]
    NoCredential,
    #[error("credential source `{0}` is not active")]
    NotActive(String),
    #[error("vault source `{0}` has no material_ref")]
    MissingMaterialRef(String),
    #[error("env source `{0}` has no env_key / value")]
    MissingEnv(String),
    #[error("secret seal/open failed (wrong key or corrupt ciphertext)")]
    Seal,
    #[error("oauth token refresh failed: {0}")]
    OAuth(String),
    /// A durable-backend failure (I/O, serde, poisoned lock) surfaced by a
    /// persistent [`repo::CredentialRepo`] / [`SealedBlobStore`] adapter.
    #[error("credential storage: {0}")]
    Storage(String),
}

/// Persistence of opaque sealed blobs (`nonce ‖ ciphertext`), keyed by
/// [`SecretRef`] — the seam that makes the AEAD layer engine-agnostic. The
/// sealing `sealed::SealedAeadSecretStore` writes through this port;
/// `sqlite::SqliteSealedBlobStore` persists the same blobs durably. A blob is
/// ciphertext to everyone but the AEAD layer holding the key.
#[async_trait::async_trait]
pub trait SealedBlobStore: Send + Sync {
    async fn put_blob(&self, r: &SecretRef, blob: Vec<u8>) -> Result<(), CredentialError>;
    async fn get_blob(&self, r: &SecretRef) -> Result<Vec<u8>, CredentialError>;
    async fn delete_blob(&self, r: &SecretRef) -> Result<(), CredentialError>;
    async fn inventory_blobs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        Err(CredentialError::Storage(
            "sealed-blob inventory is not supported by this store".to_string(),
        ))
    }
}

/// In-memory [`SealedBlobStore`] — the default behind
/// `SealedAeadSecretStore::with_key` (dev / tests; a process restart forgets
/// the blobs).
#[derive(Default)]
pub struct InMemorySealedBlobStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemorySealedBlobStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SealedBlobStore for InMemorySealedBlobStore {
    async fn put_blob(&self, r: &SecretRef, blob: Vec<u8>) -> Result<(), CredentialError> {
        self.blobs
            .lock()
            .expect("sealed blob mutex")
            .insert(r.0.clone(), blob);
        Ok(())
    }

    async fn get_blob(&self, r: &SecretRef) -> Result<Vec<u8>, CredentialError> {
        self.blobs
            .lock()
            .expect("sealed blob mutex")
            .get(&r.0)
            .cloned()
            .ok_or_else(|| CredentialError::SecretNotFound(r.0.clone()))
    }

    async fn delete_blob(&self, r: &SecretRef) -> Result<(), CredentialError> {
        self.blobs.lock().expect("sealed blob mutex").remove(&r.0);
        Ok(())
    }

    async fn inventory_blobs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        Ok(self
            .blobs
            .lock()
            .expect("sealed blob mutex")
            .keys()
            .cloned()
            .map(SecretRef)
            .collect())
    }
}

/// In-memory [`SecretStore`] (dev / tests). Real backends encrypt at rest.
#[derive(Default)]
pub struct InMemorySecretStore {
    map: Mutex<HashMap<String, String>>,
}

impl InMemorySecretStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SecretStore for InMemorySecretStore {
    async fn put(&self, r: &SecretRef, secret: RedactedString) -> Result<(), CredentialError> {
        self.map
            .lock()
            .expect("secret store mutex")
            .insert(r.0.clone(), secret.expose_secret().to_string());
        Ok(())
    }

    async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError> {
        self.map
            .lock()
            .expect("secret store mutex")
            .get(&r.0)
            .map(|v| RedactedString::new(v.clone()))
            .ok_or_else(|| CredentialError::SecretNotFound(r.0.clone()))
    }

    async fn delete(&self, r: &SecretRef) -> Result<(), CredentialError> {
        self.map.lock().expect("secret store mutex").remove(&r.0);
        Ok(())
    }

    async fn inventory(&self) -> Result<Vec<SecretRef>, CredentialError> {
        Ok(self
            .map
            .lock()
            .expect("secret store mutex")
            .keys()
            .cloned()
            .map(SecretRef)
            .collect())
    }
}

/// Create a credential **secret-in / secret-free-out**: the secret is sealed into
/// the store (for `Vault`) and the returned [`CredentialSource`] carries only a
/// reference. Nothing serialized out of this function contains plaintext.
pub async fn create_source(
    params: CredentialCreateParams,
    store: &dyn SecretStore,
) -> Result<CredentialSource, CredentialError> {
    let (source, secret) = prepare_source(params);
    if let (Some(material_ref), Some(secret)) = (&source.material_ref, secret) {
        store.put(material_ref, secret).await?;
    }
    Ok(source)
}

/// Mint the secret-free source and retain material separately so the repository
/// can durably journal the reference before the first secret-store effect.
pub(crate) fn prepare_source(
    params: CredentialCreateParams,
) -> (CredentialSource, Option<RedactedString>) {
    let id = CredentialSourceId(format!("cred:{}:{}", params.workspace_id, next_id()));
    let (material_ref, secret) = match (params.kind, params.secret) {
        (CredentialKind::Vault, Some(secret)) => {
            (Some(SecretRef(format!("sec:{}", id.0))), Some(secret))
        }
        // `Env` never stores material; a stray secret is dropped with the params.
        _ => (None, None),
    };
    (
        CredentialSource {
            id,
            workspace_id: params.workspace_id,
            kind: params.kind,
            provider_id: params.provider_id,
            env_key: params.env_key,
            material_ref,
            oauth_command: params.oauth_command,
            status: CredentialStatus::Active,
            version: 1,
        },
        secret,
    )
}

/// Materialize a source into an already-resolved [`RedactedString`] at the
/// injection seam. `Vault` reads the [`SecretStore`]; `Env` reads the host
/// environment via `env_key`. Fail-closed on any gap.
pub async fn materialize(
    source: &CredentialSource,
    store: &dyn SecretStore,
) -> Result<RedactedString, CredentialError> {
    if source.status != CredentialStatus::Active {
        return Err(CredentialError::NotActive(source.id.0.clone()));
    }
    match source.kind {
        CredentialKind::Vault => {
            let r = source
                .material_ref
                .as_ref()
                .ok_or_else(|| CredentialError::MissingMaterialRef(source.id.0.clone()))?;
            store.get(r).await
        }
        CredentialKind::Env => {
            let key = source
                .env_key
                .as_deref()
                .ok_or_else(|| CredentialError::MissingEnv(source.id.0.clone()))?;
            std::env::var(key)
                .map(RedactedString::new)
                .map_err(|_| CredentialError::MissingEnv(source.id.0.clone()))
        }
        CredentialKind::Oauth => {
            // The secret is minted on demand by the helper; the store is not
            // consulted (nothing is sealed for an OAuth source). A per-source
            // cache reuses one token across a hot loop (#5). Requires the
            // `oauth-command` feature (the helper-spawning path).
            #[cfg(feature = "oauth-command")]
            {
                let command = source.oauth_command.as_deref().ok_or_else(|| {
                    CredentialError::OAuth(format!(
                        "source {} is kind oauth but has no oauth_command",
                        source.id.0
                    ))
                })?;
                crate::oauth::oauth_access_token(&source.id, command).await
            }
            #[cfg(not(feature = "oauth-command"))]
            {
                Err(CredentialError::OAuth(format!(
                    "source {} is kind oauth but the `oauth-command` feature is disabled",
                    source.id.0
                )))
            }
        }
    }
}

fn next_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{timestamp}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_is_secret_in_secret_free_out() {
        let store = InMemorySecretStore::new();
        let source = create_source(
            CredentialCreateParams {
                workspace_id: "ws1".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-super-secret-value")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();

        // The serialized row carries a ref, never the plaintext.
        let json = serde_json::to_string(&source).unwrap();
        assert!(!json.contains("sk-super-secret-value"));
        assert!(json.contains("material_ref"));

        // But it materializes back to the plaintext at the seam.
        let secret = materialize(&source, &store).await.unwrap();
        assert_eq!(secret.expose_secret(), "sk-super-secret-value");
    }

    #[tokio::test]
    async fn disabled_source_fails_closed() {
        let store = InMemorySecretStore::new();
        let mut source = create_source(
            CredentialCreateParams {
                workspace_id: "ws1".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("x")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        source.status = CredentialStatus::Disabled;
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::NotActive(_))
        ));
    }

    fn bare_source(kind: CredentialKind) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId("cred:ws1:test".into()),
            workspace_id: "ws1".into(),
            kind,
            provider_id: None,
            env_key: None,
            material_ref: None,
            oauth_command: None,
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    #[cfg(feature = "oauth-command")]
    #[tokio::test]
    async fn oauth_source_materializes_through_its_command() {
        // An OAuth source mints its token by running the helper; nothing sealed.
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Oauth);
        source.oauth_command = Some(vec!["printf".into(), "ya29.materialized".into()]);
        let token = materialize(&source, &store).await.unwrap();
        assert_eq!(token.expose_secret(), "ya29.materialized");
    }

    #[cfg(feature = "oauth-command")]
    #[tokio::test]
    async fn oauth_source_without_a_command_fails_closed() {
        let store = InMemorySecretStore::new();
        let source = bare_source(CredentialKind::Oauth);
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::OAuth(_))
        ));
    }

    #[cfg(feature = "oauth-command")]
    #[tokio::test]
    async fn create_source_builds_a_materializable_oauth_source() {
        // The standard create seam accepts an OAuth source with its helper command,
        // so it round-trips and materializes without direct construction (#5v2).
        let store = InMemorySecretStore::new();
        let source = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Oauth,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: None,
                oauth_command: Some(vec!["printf".into(), "ya29.created".into()]),
            },
            &store,
        )
        .await
        .unwrap();
        assert_eq!(source.kind, CredentialKind::Oauth);
        assert_eq!(
            source.oauth_command.as_deref(),
            Some(&["printf".to_string(), "ya29.created".to_string()][..])
        );
        let token = materialize(&source, &store).await.unwrap();
        assert_eq!(token.expose_secret(), "ya29.created");
    }

    #[tokio::test]
    async fn a_vault_source_without_material_ref_fails_closed() {
        let store = InMemorySecretStore::new();
        let source = bare_source(CredentialKind::Vault);
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::MissingMaterialRef(id)) if id == source.id.0
        ));
    }

    #[tokio::test]
    async fn an_env_source_without_env_key_fails_closed() {
        let store = InMemorySecretStore::new();
        let source = bare_source(CredentialKind::Env);
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::MissingEnv(id)) if id == source.id.0
        ));
    }

    #[tokio::test]
    async fn an_unset_env_var_is_missing_env() {
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Env);
        // A name no test or host would ever set; reading it is side-effect free.
        source.env_key = Some("AWAKEN_CREDENTIAL_VAULT_TEST_UNSET_VAR_7F3A".into());
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::MissingEnv(id)) if id == source.id.0
        ));
    }

    #[test]
    fn selection_order_skips_disabled_and_is_stable() {
        let member = |id: &str, ordinal: u32, enabled: bool| CredentialPoolMember {
            credential_source_id: CredentialSourceId(id.into()),
            ordinal,
            enabled,
            selection_weight: 0,
        };
        let pool = CredentialPool {
            id: CredentialPoolId("pool:1".into()),
            workspace_id: "ws1".into(),
            members: vec![
                member("cred:d", 2, true),
                member("cred:b", 1, true),
                member("cred:c", 1, true),
                member("cred:a", 0, false),
            ],
            policy: SelectionPolicy::FirstHealthy,
        };
        let order: Vec<&str> = pool
            .selection_order()
            .iter()
            .map(|m| m.credential_source_id.0.as_str())
            .collect();
        // The disabled ordinal-0 member is skipped; the ordinal-1 tie breaks by id.
        assert_eq!(order, ["cred:b", "cred:c", "cred:d"]);
    }

    #[test]
    fn selection_policy_defaults_to_first_healthy() {
        assert_eq!(SelectionPolicy::default(), SelectionPolicy::FirstHealthy);
    }

    /// CredentialBinding is the internally-tagged "which credential" wire vocab
    /// (`type` discriminant, snake_case). Pin every arm's wire shape and its
    /// round-trip so a persisted/wire binding stays loadable — a silent tag drift
    /// would fail-open a run onto the wrong (or no) credential.
    #[test]
    fn credential_binding_round_trips_with_its_tagged_wire_shape() {
        let cases = [
            (CredentialBinding::None, r#"{"type":"none"}"#),
            (
                CredentialBinding::Exact {
                    credential_source_id: CredentialSourceId("cred:1".into()),
                },
                r#"{"type":"exact","credential_source_id":"cred:1"}"#,
            ),
            (
                CredentialBinding::OneOfCredentialPool {
                    credential_pool_id: CredentialPoolId("pool:1".into()),
                },
                r#"{"type":"one_of_credential_pool","credential_pool_id":"pool:1"}"#,
            ),
        ];
        for (binding, wire) in cases {
            assert_eq!(serde_json::to_string(&binding).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<CredentialBinding>(wire).unwrap(),
                binding
            );
        }
    }

    /// CredentialKind is the materialization wire vocab (snake_case, all kinds). A
    /// drift here would mis-route materialization (e.g. read a vault ref as env).
    #[test]
    fn credential_kind_wire_form_is_snake_case_for_every_kind() {
        for (kind, wire) in [
            (CredentialKind::Vault, "\"vault\""),
            (CredentialKind::Env, "\"env\""),
            (CredentialKind::Oauth, "\"oauth\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<CredentialKind>(wire).unwrap(), kind);
        }
    }

    #[test]
    fn oauth_helper_is_allowlisted_and_maps_to_fixed_argv() {
        let helper: OAuthHelper = serde_json::from_str(r#""gcloud""#).unwrap();
        assert_eq!(helper, OAuthHelper::Gcloud);
        assert_eq!(
            helper.command(),
            vec!["gcloud", "auth", "print-access-token"]
        );
        assert_eq!(
            OAuthHelper::from_command(&helper.command()),
            Some(OAuthHelper::Gcloud)
        );
        assert!(serde_json::from_str::<OAuthHelper>(r#""operator_command""#).is_err());
    }

    #[test]
    fn a_pool_json_without_policy_loads_as_first_healthy() {
        // A row written before the `policy` field existed must still load — the
        // #[serde(default)] keeps historical pools readable, unchanged behavior.
        let legacy = r#"{"id":"pool:1","workspace_id":"ws","members":[]}"#;
        let pool: CredentialPool = serde_json::from_str(legacy).unwrap();
        assert_eq!(pool.policy, SelectionPolicy::FirstHealthy);
    }

    #[test]
    fn selection_policy_round_trips_through_serde() {
        for p in [
            SelectionPolicy::FirstHealthy,
            SelectionPolicy::RotateSpread,
            SelectionPolicy::StickyResume,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(serde_json::from_str::<SelectionPolicy>(&json).unwrap(), p);
        }
        // snake_case wire form.
        assert_eq!(
            serde_json::to_string(&SelectionPolicy::RotateSpread).unwrap(),
            "\"rotate_spread\""
        );
    }

    #[test]
    fn eligible_order_rotates_past_a_cooled_member() {
        use crate::availability::AvailabilityLedger;
        let member = |id: &str, ordinal: u32| CredentialPoolMember {
            credential_source_id: CredentialSourceId(id.into()),
            ordinal,
            enabled: true,
            selection_weight: 0,
        };
        let pool = CredentialPool {
            id: CredentialPoolId("pool".into()),
            workspace_id: "ws".into(),
            members: vec![member("cred:a", 0), member("cred:b", 1)],
            policy: SelectionPolicy::FirstHealthy,
        };
        let ledger = AvailabilityLedger::new();
        ledger.cool_down(&CredentialSourceId("cred:a".into()), 1_000);

        // While cred:a is cooled, only cred:b is eligible — the selection rotates.
        let eligible: Vec<&str> = pool
            .eligible_order(&ledger, 500)
            .iter()
            .map(|m| m.credential_source_id.0.as_str())
            .collect();
        assert_eq!(eligible, ["cred:b"]);

        // Past the deadline cred:a is back at the head of the order.
        let resumed: Vec<&str> = pool
            .eligible_order(&ledger, 1_000)
            .iter()
            .map(|m| m.credential_source_id.0.as_str())
            .collect();
        assert_eq!(resumed, ["cred:a", "cred:b"]);
    }

    // ---- CEG 05: materialize (F2) ----

    /// M4: a Vault source whose `material_ref` points at a key the store does not
    /// hold fails closed with the store's own `SecretNotFound` (not swallowed).
    #[tokio::test]
    async fn a_vault_ref_absent_from_the_store_is_secret_not_found() {
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Vault);
        source.material_ref = Some(SecretRef("sec:dangling".into()));
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::SecretNotFound(r)) if r == "sec:dangling"
        ));
    }

    /// M5: an `Env` source whose `env_key` names a variable that *is* set reads the
    /// host value at the seam. `PATH` is reliably present in the test process, so no
    /// env mutation (forbidden here — `unsafe_code = "forbid"`) is needed.
    #[tokio::test]
    async fn an_env_source_reads_a_set_host_variable() {
        let Ok(expected) = std::env::var("PATH") else {
            // No PATH in this environment — the read path is exercised elsewhere.
            return;
        };
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Env);
        source.env_key = Some("PATH".into());
        let value = materialize(&source, &store).await.unwrap();
        assert_eq!(value.expose_secret(), expected);
    }

    /// M9: with the `oauth-command` feature *off*, an OAuth source cannot mint a
    /// token and fails closed with an OAuth error naming the disabled feature — the
    /// cfg gate never silently degrades to a stored/empty secret.
    #[cfg(not(feature = "oauth-command"))]
    #[tokio::test]
    async fn oauth_without_the_feature_fails_closed() {
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Oauth);
        source.oauth_command = Some(vec!["printf".into(), "tok".into()]);
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::OAuth(msg)) if msg.contains("feature is disabled")
        ));
    }

    // ---- CEG 05: eligible_order (F1/F10) ----

    /// AV6: `eligible_order` drops a *disabled* member even when the availability
    /// ledger has no cooldown for it (disabled-ness is a pool fact, not a ledger
    /// fact) — the disabled member never reaches a selector.
    #[test]
    fn eligible_order_drops_a_disabled_member() {
        use crate::availability::AvailabilityLedger;
        let member = |id: &str, ordinal: u32, enabled: bool| CredentialPoolMember {
            credential_source_id: CredentialSourceId(id.into()),
            ordinal,
            enabled,
            selection_weight: 0,
        };
        let pool = CredentialPool {
            id: CredentialPoolId("pool".into()),
            workspace_id: "ws".into(),
            members: vec![member("cred:a", 0, false), member("cred:b", 1, true)],
            policy: SelectionPolicy::FirstHealthy,
        };
        let ledger = AvailabilityLedger::new(); // nothing cooled
        let eligible: Vec<&str> = pool
            .eligible_order(&ledger, 0)
            .iter()
            .map(|m| m.credential_source_id.0.as_str())
            .collect();
        assert_eq!(eligible, ["cred:b"]);
    }

    // ---- CEG 05: create_source (F3) ----

    /// A `SecretStore` whose `put` always fails, so `create_source` surfaces the
    /// seal/storage error rather than persisting a row that points at nothing.
    struct FailingPutStore;
    #[async_trait::async_trait]
    impl SecretStore for FailingPutStore {
        async fn put(&self, _r: &SecretRef, _s: RedactedString) -> Result<(), CredentialError> {
            Err(CredentialError::Storage("put boom".into()))
        }
        async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError> {
            Err(CredentialError::SecretNotFound(r.0.clone()))
        }
        async fn delete(&self, _r: &SecretRef) -> Result<(), CredentialError> {
            Ok(())
        }
    }

    /// create_source(b): a `Vault` create whose seal (`put`) fails returns the error
    /// — no half-created secret-free row escapes with a dangling ref.
    #[tokio::test]
    async fn create_vault_propagates_a_put_failure() {
        let store = FailingPutStore;
        let err = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("sk")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CredentialError::Storage(_)));
    }

    /// create_source(c): every kind that stores nothing (`Env`, and `Vault` with no
    /// secret) yields a row with `material_ref = None` — the store is never touched.
    #[tokio::test]
    async fn create_without_sealed_material_has_no_ref() {
        let store = InMemorySecretStore::new();
        let env = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Env,
                provider_id: None,
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("dropped")), // Env drops any stray secret.
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        assert_eq!(env.material_ref, None);

        let vault_no_secret = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: None,
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap();
        assert_eq!(vault_no_secret.material_ref, None);
    }
}
