//! Credential / vault domain (ADR-0043, agents bucket). A **config store** whose
//! aggregates are *secret-free*: a [`CredentialSource`] holds only a reference to
//! secret material, never the material itself. The plaintext lives behind the
//! [`SecretStore`] port and only ever surfaces as an already-resolved
//! [`RedactedString`](awaken_agent_contract::RedactedString) at the injection seam
//! (D6/D9 — the runtime never sees the store, the ref, or a resolver).
//!
//! Two orthogonal axes (oversight-next / awaken-management-contract):
//! *materialization* ([`CredentialKind`]: where the secret lives) and *selection*
//! ([`CredentialBinding`]: which source a run uses). Executable credentials are
//! persisted (`Vault`) or minted by an explicitly persisted helper (`Oauth`);
//! ambient process environment is never execution configuration.

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
pub use sealed::{SealedAeadSecretStore, generate_seal_key_hex, parse_seal_key};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore};

use std::collections::BTreeMap;
#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;

use awaken_agent_contract::RedactedString;
pub use awaken_agent_contract::StructuredCredentialMaterial;

/// Opaque handle into a [`SecretStore`]. Never the secret itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct SecretRef(pub String);

/// Conventional slots used by OAuth refresh consumers. They are ordinary
/// extension-defined slots and receive no special lifecycle treatment.
pub const OAUTH_REFRESH_TOKEN_SLOT: &str = "oauth_refresh_token";
pub const OAUTH_CLIENT_SECRET_SLOT: &str = "oauth_client_secret";

/// Stable id of a [`CredentialSource`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct CredentialSourceId(pub String);

/// Non-secret identity of material owned by one Worker-local driver. The
/// credential source id is derived from this tuple for idempotent registration;
/// callers never recover the tuple by parsing the id.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkerLocalBinding {
    pub driver_id: String,
    pub subject_id: String,
}

impl WorkerLocalBinding {
    #[must_use]
    pub fn new(driver_id: impl Into<String>, subject_id: impl Into<String>) -> Self {
        Self {
            driver_id: driver_id.into(),
            subject_id: subject_id.into(),
        }
    }
}

/// Where a secret physically lives (the materialization axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// Secret material sealed in the vault (a [`SecretRef`] into [`SecretStore`]).
    Vault,
    /// Legacy persisted value. New sources of this kind are rejected and existing
    /// rows cannot materialize: environment discovery may propose configuration,
    /// but an operator must persist it as `Vault` before publication/execution.
    Env,
    /// An OAuth-backed provider credential (#5): the secret is a short-lived
    /// Bearer token minted on demand by running `oauth_command`, never stored. The
    /// long-lived grant lives inside the helper (e.g. `gcloud`), so nothing secret
    /// crosses the control plane.
    Oauth,
    /// Secret material is installed and retained on an eligible worker. The
    /// persisted source id and revision are the only cross-plane handle; worker
    /// heartbeats advertise whether that exact handle is currently available.
    WorkerLocal,
}

/// Canonical material-location projection over the retained [`CredentialKind`]
/// wire. New domain decisions use this axis instead of interpreting `kind`
/// themselves; the legacy enum remains only for storage/API compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMaterialOrigin {
    Vault,
    WorkerLocal,
    ExternalHelper,
    LegacyEnvironment,
}

/// Canonical acquisition projection, orthogonal to material location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialAcquisition {
    Static,
    HelperMinted,
}

/// Claude Code's documented long-lived `setup-token` process-secret channel.
/// It is a CLI credential, not an Anthropic Messages API key, so generic
/// provider resolution must never treat it as native provider material.
pub const CLAUDE_CODE_SETUP_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

const STRUCTURED_MATERIAL_PREFIX: &str = "awaken-credential-material-v1:";

#[derive(serde::Serialize, serde::Deserialize)]
struct StructuredCredentialMaterialWire {
    type_id: String,
    fields: std::collections::BTreeMap<String, String>,
}

/// Encode canonical typed material as one versioned document for sealing by the
/// existing [`SecretStore`]. The codec is a Vault concern; the material type is
/// owned only by `awaken-credential-contract`.
pub fn encode_structured_material(
    material: StructuredCredentialMaterial,
) -> Result<RedactedString, CredentialError> {
    if material.type_id.trim().is_empty() || material.fields.is_empty() {
        return Err(CredentialError::InvalidSource(
            "structured credential requires a type_id and fields".into(),
        ));
    };
    let wire = StructuredCredentialMaterialWire {
        type_id: material.type_id,
        fields: material
            .fields
            .into_iter()
            .map(|(name, value)| (name, value.expose_secret().to_string()))
            .collect(),
    };
    serde_json::to_string(&wire)
        .map(|json| RedactedString::new(format!("{STRUCTURED_MATERIAL_PREFIX}{json}")))
        .map_err(|error| CredentialError::InvalidSource(error.to_string()))
}

/// Decode only the reserved versioned envelope. Ordinary legacy secrets are
/// returned unchanged; malformed reserved documents fail closed.
pub fn decode_structured_material(
    value: RedactedString,
) -> Result<Result<StructuredCredentialMaterial, RedactedString>, CredentialError> {
    let Some(json) = value
        .expose_secret()
        .strip_prefix(STRUCTURED_MATERIAL_PREFIX)
    else {
        return Ok(Err(value));
    };
    let wire: StructuredCredentialMaterialWire = serde_json::from_str(json)
        .map_err(|error| CredentialError::InvalidSource(error.to_string()))?;
    if wire.type_id.trim().is_empty() || wire.fields.is_empty() {
        return Err(CredentialError::InvalidSource(
            "structured credential requires a type_id and fields".into(),
        ));
    }
    Ok(Ok(StructuredCredentialMaterial {
        type_id: wire.type_id,
        fields: wire
            .fields
            .into_iter()
            .map(|(name, value)| (name, RedactedString::new(value)))
            .collect(),
    }))
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
    /// Optional exact Provider protocol endpoint this credential may
    /// authenticate. `None` means provider-wide material. This is an access
    /// scope only: dialect and credential delivery remain executor-owned facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_endpoint_id: Option<String>,
    /// Environment-variable name the secret is injected under (all kinds may set it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// Vault reference; `None` for `Env` (the secret never crosses the control plane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material_ref: Option<SecretRef>,
    /// Additional, named material owned by the same credential revision. Slot
    /// names are extension-defined; the credential domain owns only their
    /// lifecycle and never interprets their contents.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub auxiliary_material_refs: BTreeMap<String, SecretRef>,
    /// The refresh helper for a [`CredentialKind::Oauth`] source: `[program,
    /// args…]`, whose trimmed stdout is a fresh access token. `None` for every
    /// other kind. Only a *reference to a command* travels — never a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_command: Option<Vec<String>>,
    /// Present only for [`CredentialKind::WorkerLocal`]. It is a stable locator,
    /// never authentication material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_local_binding: Option<WorkerLocalBinding>,
    pub status: CredentialStatus,
    pub version: i64,
}

impl CredentialSource {
    /// Every sealed-material reference owned by this aggregate, including the
    /// compatibility primary slot and open extension-defined auxiliary slots.
    pub fn material_refs(&self) -> impl Iterator<Item = &SecretRef> {
        self.material_ref
            .iter()
            .chain(self.auxiliary_material_refs.values())
    }

    /// Resolve one extension-defined material slot without exposing storage
    /// layout to its consumer.
    #[must_use]
    pub fn auxiliary_material_ref(&self, slot: &str) -> Option<&SecretRef> {
        self.auxiliary_material_refs.get(slot)
    }

    /// Normalize the retained storage discriminator into one material-origin
    /// fact. This is the sole mapping from legacy `CredentialKind` semantics.
    #[must_use]
    pub fn material_origin(&self) -> CredentialMaterialOrigin {
        match self.kind {
            CredentialKind::Vault => CredentialMaterialOrigin::Vault,
            CredentialKind::WorkerLocal => CredentialMaterialOrigin::WorkerLocal,
            CredentialKind::Oauth => CredentialMaterialOrigin::ExternalHelper,
            CredentialKind::Env => CredentialMaterialOrigin::LegacyEnvironment,
        }
    }

    /// Normalize how executable material is acquired independently of location.
    #[must_use]
    pub fn acquisition(&self) -> CredentialAcquisition {
        match self.kind {
            CredentialKind::Oauth => CredentialAcquisition::HelperMinted,
            CredentialKind::Vault | CredentialKind::WorkerLocal | CredentialKind::Env => {
                CredentialAcquisition::Static
            }
        }
    }

    /// Retained authoring hint for compiling an explicit process-secret usage.
    /// Runtime adapters must consume only the resulting `CredentialAccess.usage`.
    #[must_use]
    pub fn process_secret_environment_hint(&self) -> Option<&str> {
        self.env_key.as_deref()
    }

    /// Claude setup tokens are an ACP workload credential, not provider API
    /// material. Centralizing the classification prevents selection, UI defaults,
    /// and publication from drifting in their string interpretation.
    #[must_use]
    pub fn is_claude_code_setup_token(&self) -> bool {
        self.process_secret_environment_hint() == Some(CLAUDE_CODE_SETUP_TOKEN_ENV)
    }

    /// Whether this retained source can participate in execution publication.
    #[must_use]
    pub fn is_executable_origin(&self) -> bool {
        self.material_origin() != CredentialMaterialOrigin::LegacyEnvironment
    }
}

/// The "which credential" axis (oversight-next / awaken-management-contract).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialBinding {
    /// No credential is needed.
    None,
    /// Obtain short-lived, exact-model access from the platform broker at
    /// execution time. No Provider credential or Cloud capability is persisted
    /// in the local profile or publication.
    Brokered,
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
        self.selection_order_at(0)
    }

    /// The enabled members in policy order for one resolver-owned selection
    /// sequence. `FirstHealthy` and `StickyResume` retain the stable authored
    /// order when no affinity key is available. `RotateSpread` moves the first
    /// candidate by `sequence`, while preserving deterministic failover order
    /// for the remainder.
    #[must_use]
    pub fn selection_order_at(&self, sequence: u64) -> Vec<&CredentialPoolMember> {
        let mut members: Vec<&CredentialPoolMember> =
            self.members.iter().filter(|m| m.enabled).collect();
        members.sort_by(|a, b| member_ordering(a, b));
        if matches!(self.policy, SelectionPolicy::RotateSpread) && !members.is_empty() {
            let offset = usize::try_from(sequence % members.len() as u64)
                .expect("rotation offset is bounded by the member count");
            members.rotate_left(offset);
        }
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
        self.eligible_order_at(ledger, now_ms, 0)
    }

    /// Availability-filtered [`selection_order_at`](Self::selection_order_at).
    #[must_use]
    pub fn eligible_order_at(
        &self,
        ledger: &crate::availability::AvailabilityLedger,
        now_ms: u64,
        sequence: u64,
    ) -> Vec<&CredentialPoolMember> {
        self.selection_order_at(sequence)
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
    #[error(
        "environment credential source `{0}` is not executable; persist the secret in the vault"
    )]
    EnvironmentSourceUnsupported(String),
    #[error("worker-local credential source `{0}` must be materialized by its assigned worker")]
    WorkerLocalSourceUnsupported(String),
    #[error("invalid credential source: {0}")]
    InvalidSource(String),
    #[error("credential mutation conflict: {0}")]
    MutationConflict(String),
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

/// In-memory [`SealedBlobStore`] for tests and scenario fixtures.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemorySealedBlobStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl InMemorySealedBlobStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(test, feature = "test-support"))]
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
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemorySecretStore {
    map: Mutex<HashMap<String, String>>,
}

#[cfg(any(test, feature = "test-support"))]
impl InMemorySecretStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(test, feature = "test-support"))]
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
    validate_create_params(&params)?;
    let (source, secret) = prepare_source(params);
    if let (Some(material_ref), Some(secret)) = (&source.material_ref, secret) {
        store.put(material_ref, secret).await?;
    }
    Ok(source)
}

/// Environment variables are discovery inputs, never durable or executable
/// credential sources. Kept at the domain entry seam so every HTTP/repository
/// composition receives the same fail-closed decision.
pub(crate) fn validate_create_params(
    params: &CredentialCreateParams,
) -> Result<(), CredentialError> {
    match params.kind {
        CredentialKind::Env => Err(CredentialError::EnvironmentSourceUnsupported(
            params
                .env_key
                .clone()
                .unwrap_or_else(|| "<unnamed>".to_string()),
        )),
        CredentialKind::Vault => {
            if params.secret.is_none() {
                Err(CredentialError::InvalidSource(
                    "vault credentials require a secret".into(),
                ))
            } else if params.oauth_command.is_some() {
                Err(CredentialError::InvalidSource(
                    "vault credentials cannot configure an OAuth helper".into(),
                ))
            } else if params.env_key.as_deref() == Some(CLAUDE_CODE_SETUP_TOKEN_ENV)
                && params.provider_id.as_deref() != Some("anthropic")
            {
                Err(CredentialError::InvalidSource(
                    "Claude Code setup tokens must be scoped to provider anthropic".into(),
                ))
            } else {
                Ok(())
            }
        }
        CredentialKind::Oauth => {
            if params.oauth_command.is_none() {
                Err(CredentialError::InvalidSource(
                    "oauth credentials require an allowlisted helper".into(),
                ))
            } else if params.secret.is_some() {
                Err(CredentialError::InvalidSource(
                    "oauth credentials cannot persist a supplied secret".into(),
                ))
            } else {
                Ok(())
            }
        }
        CredentialKind::WorkerLocal => Err(CredentialError::InvalidSource(
            "worker-local credentials must be registered through ensure_worker_local".into(),
        )),
    }
}

/// Mint the secret-free source and retain material separately so the repository
/// can durably journal the reference before the first secret-store effect.
pub(crate) fn prepare_source(
    params: CredentialCreateParams,
) -> (CredentialSource, Option<RedactedString>) {
    let id = CredentialSourceId(format!("cred:{}:{}", params.workspace_id, next_id()));
    prepare_source_with_id(id, params)
}

/// Mint a source at a caller-owned stable identity. Only the credential
/// application layer may expose this through an idempotent create operation;
/// transports must not construct material references themselves.
pub(crate) fn prepare_source_with_id(
    id: CredentialSourceId,
    params: CredentialCreateParams,
) -> (CredentialSource, Option<RedactedString>) {
    let (material_ref, secret) = match (params.kind, params.secret) {
        (CredentialKind::Vault, Some(secret)) => {
            (Some(SecretRef(format!("sec:{}", id.0))), Some(secret))
        }
        // OAuth mints short-lived material through its persisted helper. A
        // WorkerLocal source intentionally persists no material. The legacy Env
        // variant is rejected before this internal constructor is reached.
        _ => (None, None),
    };
    (
        CredentialSource {
            id,
            workspace_id: params.workspace_id,
            kind: params.kind,
            provider_id: params.provider_id,
            protocol_endpoint_id: None,
            env_key: params.env_key,
            material_ref,
            auxiliary_material_refs: BTreeMap::new(),
            oauth_command: params.oauth_command,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        },
        secret,
    )
}

/// Materialize a source into an already-resolved [`RedactedString`] at the
/// injection seam. `Vault` reads the [`SecretStore`]; `Oauth` invokes its persisted,
/// allowlisted helper. Legacy `Env` rows fail closed and never read process state.
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
        CredentialKind::Env => Err(CredentialError::EnvironmentSourceUnsupported(
            source.id.0.clone(),
        )),
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
        CredentialKind::WorkerLocal => Err(CredentialError::WorkerLocalSourceUnsupported(
            source.id.0.clone(),
        )),
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
    async fn claude_setup_token_is_an_anthropic_scoped_vault_source() {
        let store = InMemorySecretStore::new();
        let source = create_source(
            CredentialCreateParams {
                workspace_id: "ws1".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some(CLAUDE_CODE_SETUP_TOKEN_ENV.into()),
                secret: Some(RedactedString::new("setup-token")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .expect("Claude setup token");
        assert_eq!(source.env_key.as_deref(), Some(CLAUDE_CODE_SETUP_TOKEN_ENV));

        let error = create_source(
            CredentialCreateParams {
                workspace_id: "ws1".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some(CLAUDE_CODE_SETUP_TOKEN_ENV.into()),
                secret: Some(RedactedString::new("setup-token")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .expect_err("setup token cannot be scoped to another provider");
        assert!(matches!(error, CredentialError::InvalidSource(_)));
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
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    #[test]
    fn endpoint_scope_is_backward_compatible_and_round_trips_when_present() {
        // Cause/effect decision table: R1 a legacy serialized source omits the
        // endpoint field -> decode as provider-wide; R2 a new endpoint-scoped
        // source -> serialize and decode the exact immutable scope. No storage
        // migration or inferred endpoint is allowed in either rule.
        let legacy: CredentialSource = serde_json::from_value(serde_json::json!({
            "id": "cred:workspace:legacy",
            "workspace_id": "workspace",
            "kind": "vault",
            "provider_id": "openai",
            "status": "active",
            "version": 1
        }))
        .unwrap();
        assert!(legacy.protocol_endpoint_id.is_none(), "R1");

        let scoped = CredentialSource {
            protocol_endpoint_id: Some("openai.open_ai_chat.primary".into()),
            ..legacy
        };
        let decoded: CredentialSource =
            serde_json::from_value(serde_json::to_value(&scoped).unwrap()).unwrap();
        assert_eq!(
            decoded.protocol_endpoint_id.as_deref(),
            Some("openai.open_ai_chat.primary"),
            "R2"
        );
    }

    /// Cause-effect graph: the retained kind is a storage input; normalization
    /// emits independent origin/acquisition facts and executability. No caller is
    /// allowed to recreate this mapping.
    ///
    /// | Rule | retained kind | origin | acquisition | executable |
    /// |---|---|---|---|---|
    /// | N1 | vault | Vault | Static | yes |
    /// | N2 | oauth | ExternalHelper | HelperMinted | yes |
    /// | N3 | worker_local | WorkerLocal | Static | yes |
    /// | N4 | env | LegacyEnvironment | Static | no |
    #[test]
    fn retained_kinds_have_one_normalized_domain_projection() {
        let rules = [
            (
                "N1",
                CredentialKind::Vault,
                CredentialMaterialOrigin::Vault,
                CredentialAcquisition::Static,
                true,
            ),
            (
                "N2",
                CredentialKind::Oauth,
                CredentialMaterialOrigin::ExternalHelper,
                CredentialAcquisition::HelperMinted,
                true,
            ),
            (
                "N3",
                CredentialKind::WorkerLocal,
                CredentialMaterialOrigin::WorkerLocal,
                CredentialAcquisition::Static,
                true,
            ),
            (
                "N4",
                CredentialKind::Env,
                CredentialMaterialOrigin::LegacyEnvironment,
                CredentialAcquisition::Static,
                false,
            ),
        ];
        for (id, kind, origin, acquisition, executable) in rules {
            let source = bare_source(kind);
            assert_eq!(source.material_origin(), origin, "{id}");
            assert_eq!(source.acquisition(), acquisition, "{id}");
            assert_eq!(source.is_executable_origin(), executable, "{id}");
        }
    }

    /// Cause-effect graph: namespaced type + non-empty fields -> one sealed
    /// versioned document -> exact typed decode. Legacy scalars remain scalars;
    /// malformed reserved documents fail closed rather than becoming API keys.
    ///
    /// | Rule | input | effect |
    /// |---|---|---|
    /// | T1 | external SSH type + fields | exact typed round trip |
    /// | T2 | legacy scalar | unchanged scalar |
    /// | T3 | reserved malformed document | InvalidSource |
    #[test]
    fn structured_material_codec_is_open_versioned_and_fail_closed() {
        let material = StructuredCredentialMaterial {
            type_id: "acme.ssh-key/v1".into(),
            fields: std::collections::BTreeMap::from([
                ("private_key".into(), RedactedString::new("pem")),
                ("known_hosts".into(), RedactedString::new("host-key")),
            ]),
        };
        let encoded = encode_structured_material(material).expect("T1 encode");
        let Ok(decoded) = decode_structured_material(encoded).expect("T1 decode") else {
            panic!("T1 must remain typed")
        };
        assert_eq!(decoded.type_id, "acme.ssh-key/v1", "T1");
        assert_eq!(decoded.fields["private_key"].expose_secret(), "pem", "T1");

        let scalar = RedactedString::new("legacy-api-key");
        let Err(scalar) = decode_structured_material(scalar).expect("T2 decode") else {
            panic!("T2 must remain scalar")
        };
        assert_eq!(scalar.expose_secret(), "legacy-api-key", "T2");

        assert!(matches!(
            decode_structured_material(RedactedString::new(format!(
                "{STRUCTURED_MATERIAL_PREFIX}not-json"
            ))),
            Err(CredentialError::InvalidSource(_))
        ));
    }

    #[test]
    fn legacy_source_without_worker_locator_remains_readable() {
        // Cause graph: retained JSON row without the additive locator field
        // -> serde default -> the original non-WorkerLocal source remains valid.
        // Decision table: missing field => None; no implicit binding is invented.
        let legacy = r#"{
            "id":"cred:legacy",
            "workspace_id":"ws1",
            "kind":"vault",
            "status":"active",
            "version":1
        }"#;
        let source: CredentialSource = serde_json::from_str(legacy).unwrap();
        assert_eq!(source.id.0, "cred:legacy");
        assert_eq!(source.worker_local_binding, None);
    }

    #[cfg(feature = "oauth-command")]
    #[tokio::test]
    async fn oauth_source_materializes_through_its_command() {
        // An OAuth source mints its token by running the helper; nothing sealed.
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Oauth);
        source.oauth_command = Some(oauth::test_stdout_command("ya29.materialized"));
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
        let oauth_command = oauth::test_stdout_command("ya29.created");
        let source = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Oauth,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: None,
                oauth_command: Some(oauth_command.clone()),
            },
            &store,
        )
        .await
        .unwrap();
        assert_eq!(source.kind, CredentialKind::Oauth);
        assert_eq!(
            source.oauth_command.as_deref(),
            Some(oauth_command.as_slice())
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
    async fn a_legacy_env_source_fails_closed_without_reading_process_state() {
        let store = InMemorySecretStore::new();
        let source = bare_source(CredentialKind::Env);
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::EnvironmentSourceUnsupported(id)) if id == source.id.0
        ));
    }

    #[tokio::test]
    async fn a_legacy_env_source_is_rejected_even_when_it_names_a_variable() {
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Env);
        source.env_key = Some("PATH".into());
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::EnvironmentSourceUnsupported(id)) if id == source.id.0
        ));
    }

    #[tokio::test]
    async fn generic_create_rejects_worker_local_without_a_stable_locator() {
        let store = InMemorySecretStore::new();
        let result = create_source(
            CredentialCreateParams {
                workspace_id: "ws1".into(),
                kind: CredentialKind::WorkerLocal,
                provider_id: Some("openai".into()),
                env_key: None,
                secret: None,
                oauth_command: None,
            },
            &store,
        )
        .await;
        assert!(matches!(
            result,
            Err(CredentialError::InvalidSource(message))
                if message.contains("ensure_worker_local")
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

    // Cause/effect decision table for policy-owned candidate ordering:
    // R1 FirstHealthy + any sequence -> stable ordinal/id order;
    // R2 RotateSpread + sequence 0 -> stable order;
    // R3 RotateSpread + sequence 1..N -> rotate the first candidate modulo N;
    // R4 disabled member -> absent before rotation.
    #[test]
    fn selection_policy_is_applied_by_the_pool_without_a_second_selector() {
        let member = |id: &str, ordinal: u32, enabled: bool| CredentialPoolMember {
            credential_source_id: CredentialSourceId(id.into()),
            ordinal,
            enabled,
            selection_weight: 0,
        };
        let mut pool = CredentialPool {
            id: CredentialPoolId("pool:rotate".into()),
            workspace_id: "ws1".into(),
            members: vec![
                member("cred:a", 0, true),
                member("cred:disabled", 1, false),
                member("cred:b", 2, true),
            ],
            policy: SelectionPolicy::FirstHealthy,
        };
        let ids = |pool: &CredentialPool, sequence| {
            pool.selection_order_at(sequence)
                .into_iter()
                .map(|member| member.credential_source_id.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&pool, 1), ["cred:a", "cred:b"], "R1/R4");
        pool.policy = SelectionPolicy::RotateSpread;
        assert_eq!(ids(&pool, 0), ["cred:a", "cred:b"], "R2/R4");
        assert_eq!(ids(&pool, 1), ["cred:b", "cred:a"], "R3/R4");
        assert_eq!(ids(&pool, 2), ["cred:a", "cred:b"], "R3 modulo N");
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
            (CredentialBinding::Brokered, r#"{"type":"brokered"}"#),
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
            (CredentialKind::WorkerLocal, "\"worker_local\""),
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

    /// M5: process environment is not an execution credential source. Even a set,
    /// commonplace variable is never read by materialization.
    #[tokio::test]
    async fn materialization_never_reads_a_set_host_variable() {
        let store = InMemorySecretStore::new();
        let mut source = bare_source(CredentialKind::Env);
        source.env_key = Some("PATH".into());
        assert!(matches!(
            materialize(&source, &store).await,
            Err(CredentialError::EnvironmentSourceUnsupported(_))
        ));
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

    /// Environment rows and incomplete vault rows are both rejected at the only
    /// create seam, so an active source can never be born unmaterializable.
    #[tokio::test]
    async fn create_without_sealed_material_has_no_ref() {
        let store = InMemorySecretStore::new();
        let error = create_source(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Env,
                provider_id: None,
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("not-imported")),
                oauth_command: None,
            },
            &store,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            CredentialError::EnvironmentSourceUnsupported(_)
        ));

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
        .unwrap_err();
        assert!(matches!(vault_no_secret, CredentialError::InvalidSource(_)));
    }
}
