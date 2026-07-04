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

pub mod repo;
pub mod schema;

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
        members.sort_by(|a, b| {
            a.ordinal
                .cmp(&b.ordinal)
                .then_with(|| a.credential_source_id.0.cmp(&b.credential_source_id.0))
        });
        members
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
}

/// Persistence of raw secret bytes, keyed by [`SecretRef`]. Encryption-at-rest is
/// an adapter concern (ADR-0043): `inmem` here, `plaintext-file`/`sealed-aead`
/// later. The domain and runtime never see ciphertext.
#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    async fn put(&self, r: &SecretRef, secret: RedactedString) -> Result<(), CredentialError>;
    async fn get(&self, r: &SecretRef) -> Result<RedactedString, CredentialError>;
}

/// A credential-domain failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    #[error("secret ref `{0}` not found")]
    SecretNotFound(String),
    #[error("credential source `{0}` not found")]
    SourceNotFound(String),
    #[error("binding resolves to no credential")]
    NoCredential,
    #[error("credential source `{0}` is not active")]
    NotActive(String),
    #[error("vault source `{0}` has no material_ref")]
    MissingMaterialRef(String),
    #[error("env source `{0}` has no env_key / value")]
    MissingEnv(String),
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
}

/// Create a credential **secret-in / secret-free-out**: the secret is sealed into
/// the store (for `Vault`) and the returned [`CredentialSource`] carries only a
/// reference. Nothing serialized out of this function contains plaintext.
pub async fn create_source(
    params: CredentialCreateParams,
    store: &dyn SecretStore,
) -> Result<CredentialSource, CredentialError> {
    let id = CredentialSourceId(format!("cred:{}:{}", params.workspace_id, next_seq()));
    let material_ref = match (params.kind, params.secret) {
        (CredentialKind::Vault, Some(secret)) => {
            let r = SecretRef(format!("sec:{}", id.0));
            store.put(&r, secret).await?;
            Some(r)
        }
        // `Env` never stores material; a stray secret is simply dropped (zeroized).
        _ => None,
    };
    Ok(CredentialSource {
        id,
        workspace_id: params.workspace_id,
        kind: params.kind,
        provider_id: params.provider_id,
        env_key: params.env_key,
        material_ref,
        status: CredentialStatus::Active,
        version: 1,
    })
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
    }
}

fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
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
}
