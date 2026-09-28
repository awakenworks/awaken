// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: the provider-neutral durable Sandbox handle envelope. Dynamic path: the
// @formal reason: historical wire decodes into an open provider schema, an order-preserving opaque
// @formal reason: provider payload and typed neutral realization evidence; Resource path growth, Memory
// @formal reason: evidence and substrate identity are decided only from that evidence and the exact opaque
// @formal reason: locator. It adds no provider selection, credential custody or lifecycle authority, and
// @formal reason: historical-wire decision tables qualify the behavior; this is not a direct formal-proof
// @formal reason: claim.
//! The durable Sandbox handle: neutral identity and realization evidence around
//! a provider-owned payload the contract never interprets.
//!
//! The contract knows no backend. A provider crate owns its payload types and
//! codec, and chooses a [`SandboxPayloadSchema`] (an open provider name plus a
//! payload version). The contract keeps only what its own checks need:
//! restoration and Memory evidence beside the payload, and the neutral
//! [`HandleEvidence`] every current provider payload carries.
//!
//! # Wire
//!
//! The encoding is the historical one, unchanged:
//! `{"sandbox_id", "restoration"?, "payload": {"schema": "<provider>_v<N>", ..},
//! "memory_materializations"?}`, or `{"schema": "unmanaged", "provider_kind"}`
//! for a non-resumable handle. Provider members keep their exact order across
//! decode and re-encode, because durable Session bindings compare the encoded
//! handle byte-for-byte.

mod payload;

use serde::de::DeserializeOwned;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use super::foundation::MemoryMaterializationEvidence;
use super::restore_wire::SandboxRestorationEvidence;
use super::{SandboxEffectFence, SandboxError};
use payload::{PayloadObject, PayloadValue};

/// Open identity of the provider that owns a handle payload, plus the version
/// of that provider's payload schema. Encoded as the `<provider>_v<version>`
/// schema tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxPayloadSchema {
    provider: String,
    version: u32,
}

impl SandboxPayloadSchema {
    /// `provider` is a lowercase identifier (`[a-z][a-z0-9_]*`); `version`
    /// starts at 1.
    pub fn new(provider: impl Into<String>, version: u32) -> Result<Self, SandboxError> {
        let provider = provider.into();
        let valid_provider = provider
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
            && provider
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid_provider || version == 0 {
            return Err(SandboxError::invalid(format!(
                "sandbox handle payload schema `{provider}` v{version} is not a lowercase \
                 provider identifier with a positive version"
            )));
        }
        Ok(Self { provider, version })
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    fn tag(&self) -> String {
        format!("{}_v{}", self.provider, self.version)
    }

    fn parse(tag: &str) -> Result<Self, SandboxError> {
        let malformed = || {
            SandboxError::invalid(format!(
                "sandbox handle payload schema `{tag}` is malformed"
            ))
        };
        let (provider, version) = tag.rsplit_once("_v").ok_or_else(malformed)?;
        if version.is_empty()
            || version.starts_with('0')
            || !version.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(malformed());
        }
        let version = version.parse().map_err(|_| malformed())?;
        Self::new(provider, version).map_err(|_| malformed())
    }
}

/// Neutral realization evidence carried by every current provider payload.
///
/// A provider records these facts as top-level payload members named
/// `realization_fingerprint`, `owned_paths` and (for a filesystem realization
/// frozen under an aggregate fence) `effect_fence`. The contract reads only
/// these members; everything else in the payload is the provider's locator.
/// A payload without them is a legacy handle: it stays decodable and
/// adoptable where no current proof is needed, but never mints one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleEvidence {
    realization_fingerprint: crate::SandboxRealizationFingerprint,
    effect_fence: Option<SandboxEffectFence>,
    owned_paths: Vec<String>,
}

const REALIZATION_FINGERPRINT: &str = "realization_fingerprint";
const EFFECT_FENCE: &str = "effect_fence";
const OWNED_PATHS: &str = "owned_paths";

impl HandleEvidence {
    /// Exact provider-visible creation identity (crash-safe create-or-adopt).
    #[must_use]
    pub const fn realization_fingerprint(&self) -> &crate::SandboxRealizationFingerprint {
        &self.realization_fingerprint
    }

    /// Aggregate fence frozen into the realization, when the provider persists
    /// one. Compared by effect identity, never by its renewable expiry.
    #[must_use]
    pub const fn effect_fence(&self) -> Option<&SandboxEffectFence> {
        self.effect_fence.as_ref()
    }

    /// Complete provider-observed owned path set; it may only grow.
    #[must_use]
    pub fn owned_paths(&self) -> &[String] {
        &self.owned_paths
    }

    fn from_payload(payload: &PayloadObject) -> Result<Option<Self>, SandboxError> {
        let member = |name: &str| payload.get(name);
        let (fingerprint, owned_paths, effect_fence) = (
            member(REALIZATION_FINGERPRINT),
            member(OWNED_PATHS),
            member(EFFECT_FENCE),
        );
        let (fingerprint, owned_paths) = match (fingerprint, owned_paths) {
            (None, None) if effect_fence.is_none() => return Ok(None),
            (Some(fingerprint), Some(owned_paths)) => (fingerprint, owned_paths),
            _ => {
                return Err(SandboxError::invalid(
                    "sandbox handle payload carries incomplete realization evidence",
                ));
            }
        };
        let invalid = |name: &str, error: serde_json::Error| {
            SandboxError::invalid(format!(
                "sandbox handle evidence `{name}` is invalid: {error}"
            ))
        };
        Ok(Some(Self {
            realization_fingerprint: fingerprint
                .to_typed()
                .map_err(|error| invalid(REALIZATION_FINGERPRINT, error))?,
            effect_fence: effect_fence
                .map(PayloadValue::to_typed)
                .transpose()
                .map_err(|error| invalid(EFFECT_FENCE, error))?,
            owned_paths: owned_paths
                .to_typed()
                .map_err(|error| invalid(OWNED_PATHS, error))?,
        }))
    }

    fn is_member(name: &str) -> bool {
        matches!(name, REALIZATION_FINGERPRINT | EFFECT_FENCE | OWNED_PATHS)
    }

    /// Immutable evidence identity; owned paths and fence expiry excluded.
    fn same_realization(&self, other: &Self) -> bool {
        self.realization_fingerprint == other.realization_fingerprint
            && match (&self.effect_fence, &other.effect_fence) {
                (None, None) => true,
                (Some(left), Some(right)) => left.same_effect_identity(right),
                _ => false,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HandlePayload {
    /// Deliberately non-resumable handle of an ephemeral provider or double.
    Unmanaged { provider_kind: String },
    Provider {
        schema: SandboxPayloadSchema,
        payload: PayloadObject,
        evidence: Option<Box<HandleEvidence>>,
    },
}

/// A serializable, **durable** reference to a realized sandbox. Persist it the
/// moment a sandbox is created; a live `Box<dyn Sandbox>` cannot survive a host
/// restart, but the handle can be stored and later passed to
/// [`SandboxProvider::adopt_for_effect`](super::SandboxProvider::adopt_for_effect)
/// to reconnect to a still-running sandbox. The payload is owned and decoded by
/// the provider crate named in its [`SandboxPayloadSchema`]; a provider rejects
/// a payload of another schema or shape before any backend effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHandle {
    pub sandbox_id: String,
    /// Exact restore effect which owns this physical binding. This remains
    /// provider evidence on the canonical handle, never a second lifecycle state.
    restoration: Option<SandboxRestorationEvidence>,
    payload: HandlePayload,
    /// Original CAS bases for copy-backed Memory mounts. This lives beside the
    /// provider payload because every current provider can carry the same
    /// evidence and the serialized handle is already the Session root binding.
    memory_materializations: Vec<MemoryMaterializationEvidence>,
}

impl SandboxHandle {
    /// Construct a deliberately non-resumable handle for ephemeral providers and
    /// test doubles. Durable providers use [`Self::from_provider_payload`].
    pub fn new(provider_kind: impl Into<String>, sandbox_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: HandlePayload::Unmanaged {
                provider_kind: provider_kind.into(),
            },
            memory_materializations: Vec::new(),
        }
    }

    /// Wrap a provider-owned typed payload. The payload must encode as a JSON
    /// object without a `schema` member; any neutral evidence members it
    /// carries must be complete and well-formed.
    pub fn from_provider_payload<P: Serialize>(
        sandbox_id: impl Into<String>,
        schema: SandboxPayloadSchema,
        payload: &P,
    ) -> Result<Self, SandboxError> {
        let payload = PayloadObject::from_typed(payload)?;
        Ok(Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: provider_payload(schema, payload)?,
            memory_materializations: Vec::new(),
        })
    }

    /// Open identity of the provider that owns this handle.
    #[must_use]
    pub fn provider_kind(&self) -> &str {
        match &self.payload {
            HandlePayload::Unmanaged { provider_kind } => provider_kind,
            HandlePayload::Provider { schema, .. } => schema.provider(),
        }
    }

    /// Provider payload schema; `None` for an unmanaged handle.
    #[must_use]
    pub fn payload_schema(&self) -> Option<&SandboxPayloadSchema> {
        match &self.payload {
            HandlePayload::Unmanaged { .. } => None,
            HandlePayload::Provider { schema, .. } => Some(schema),
        }
    }

    /// Decode the complete provider payload into the provider's own type. The
    /// caller must first match [`Self::payload_schema`]; a shape error is an
    /// invalid durable handle, never a reason to fall back to another type.
    pub fn decode_provider_payload<P: DeserializeOwned>(&self) -> Result<P, SandboxError> {
        match &self.payload {
            HandlePayload::Unmanaged { provider_kind } => Err(SandboxError::invalid(format!(
                "unmanaged {provider_kind:?} sandbox handle has no provider payload"
            ))),
            HandlePayload::Provider {
                schema, payload, ..
            } => payload.to_typed().map_err(|error| {
                SandboxError::invalid(format!(
                    "{} sandbox handle payload is malformed: {error}",
                    schema.tag()
                ))
            }),
        }
    }

    /// The typed rejection a provider returns for a handle it does not own.
    #[must_use]
    pub fn provider_mismatch(&self, expected_provider: &str) -> SandboxError {
        SandboxError::invalid(format!(
            "{expected_provider} provider cannot adopt {:?} handle payload",
            self.provider_kind()
        ))
    }

    /// Neutral realization evidence of a current handle; `None` for legacy
    /// and unmanaged handles, which callers must not upgrade by inference.
    #[must_use]
    pub fn evidence(&self) -> Option<&HandleEvidence> {
        match &self.payload {
            HandlePayload::Provider { evidence, .. } => evidence.as_deref(),
            HandlePayload::Unmanaged { .. } => None,
        }
    }

    /// Exact restore evidence carried by this provider locator, when any.
    #[must_use]
    pub const fn restoration(&self) -> Option<&SandboxRestorationEvidence> {
        self.restoration.as_ref()
    }

    /// Bind exact restore evidence through the handle's sole internal mutation
    /// seam. Rebinding is idempotent only for the same complete tuple.
    pub(super) fn with_restoration_evidence(
        mut self,
        evidence: SandboxRestorationEvidence,
    ) -> Result<Self, SandboxError> {
        if self
            .restoration
            .as_ref()
            .is_some_and(|current| current != &evidence)
        {
            return Err(SandboxError::conflict(
                "sandbox handle already belongs to a different restore effect",
            ));
        }
        self.restoration = Some(evidence);
        Ok(self)
    }

    /// Attach the original durable heads captured by current copy-backed
    /// Memory mounts. Legacy/unmanaged handles cannot be upgraded by attaching
    /// evidence after the fact.
    pub fn with_memory_materializations(
        mut self,
        mut evidence: Vec<MemoryMaterializationEvidence>,
    ) -> Result<Self, SandboxError> {
        if self.evidence().is_none() {
            return Err(SandboxError::unsupported(
                "legacy sandbox handle cannot carry Memory materialization evidence",
            ));
        }
        MemoryMaterializationEvidence::canonicalize_all(evidence.as_mut_slice())?;
        self.memory_materializations = evidence;
        Ok(self)
    }

    /// Validate and return durable copy bases for a current handle. `None`
    /// identifies a legacy/unmanaged payload; callers must not synthesize an
    /// empty current snapshot from that absence.
    pub fn memory_materializations(
        &self,
    ) -> Result<Option<&[MemoryMaterializationEvidence]>, SandboxError> {
        if self.evidence().is_none() {
            return Ok(None);
        }
        MemoryMaterializationEvidence::validate_all(&self.memory_materializations)?;
        Ok(Some(&self.memory_materializations))
    }

    /// Complete provider-observed path evidence for current handles. `None`
    /// identifies a legacy/unmanaged payload and must fail closed when a
    /// Repository adoption needs proof of non-overlap.
    #[must_use]
    pub fn owned_paths(&self) -> Option<&[String]> {
        self.evidence().map(HandleEvidence::owned_paths)
    }

    /// Exact provider-visible creation identity carried by current handles.
    /// Legacy handles predate crash-safe create-or-adopt evidence and return
    /// `None`; callers may preserve their compatibility but must not synthesize
    /// this proof from a partial payload.
    #[must_use]
    pub fn realization_fingerprint(&self) -> Option<&crate::SandboxRealizationFingerprint> {
        self.evidence().map(HandleEvidence::realization_fingerprint)
    }

    /// Whether two current handles name the same physical substrate: same
    /// Sandbox, restore effect, original Memory heads, payload schema, and
    /// realization evidence, and an identical provider locator (every payload
    /// member other than the neutral evidence). Owned paths are deliberately
    /// excluded: a Resource reservation extends them without creating a second
    /// Sandbox identity. A legacy handle carries no realization evidence and so
    /// is never proven the same substrate as anything.
    fn same_substrate(&self, other: &Self) -> bool {
        if self.sandbox_id != other.sandbox_id
            || self.restoration != other.restoration
            || self.memory_materializations != other.memory_materializations
        {
            return false;
        }
        match (&self.payload, &other.payload) {
            (
                HandlePayload::Provider {
                    schema: left_schema,
                    payload: left,
                    evidence: Some(left_evidence),
                },
                HandlePayload::Provider {
                    schema: right_schema,
                    payload: right,
                    evidence: Some(right_evidence),
                },
            ) => {
                left_schema == right_schema
                    && left_evidence.same_realization(right_evidence)
                    && same_locator(left, right)
            }
            _ => false,
        }
    }

    /// Admit the one durable-handle update allowed by a Resource path WAL:
    /// both handles already carry current realization evidence, immutable
    /// substrate identity is unchanged, and complete owned paths grow
    /// monotonically. Legacy handles remain adoptable for non-Repository
    /// compatibility but can never mint ownership evidence.
    #[must_use]
    pub fn owned_paths_are_monotonic_to(&self, next: &Self) -> bool {
        if !self.same_substrate(next) {
            return false;
        }
        let (Some(current_paths), Some(next_paths)) = (self.owned_paths(), next.owned_paths())
        else {
            return false;
        };
        current_paths
            .iter()
            .all(|path| next_paths.iter().any(|candidate| candidate == path))
    }
}

fn provider_payload(
    schema: SandboxPayloadSchema,
    payload: PayloadObject,
) -> Result<HandlePayload, SandboxError> {
    if payload.get(SCHEMA).is_some() {
        return Err(SandboxError::invalid(
            "sandbox handle provider payload cannot define the reserved `schema` member",
        ));
    }
    let evidence = HandleEvidence::from_payload(&payload)?.map(Box::new);
    Ok(HandlePayload::Provider {
        schema,
        payload,
        evidence,
    })
}

/// Provider locator equality: every member except the neutral evidence.
fn same_locator(left: &PayloadObject, right: &PayloadObject) -> bool {
    let locator = |payload: &PayloadObject| {
        payload
            .members()
            .filter(|(name, _)| !HandleEvidence::is_member(name))
            .count()
    };
    locator(left) == locator(right)
        && left
            .members()
            .filter(|(name, _)| !HandleEvidence::is_member(name))
            .all(|(name, value)| right.get(name) == Some(value))
}

const SCHEMA: &str = "schema";
const UNMANAGED: &str = "unmanaged";
const PROVIDER_KIND: &str = "provider_kind";

/// Historical handle envelope; payload members are decoded opaquely.
#[derive(Deserialize)]
#[serde(rename = "SandboxHandle", deny_unknown_fields)]
struct WireHandle {
    sandbox_id: String,
    #[serde(default)]
    restoration: Option<SandboxRestorationEvidence>,
    payload: PayloadObject,
    #[serde(default)]
    memory_materializations: Vec<MemoryMaterializationEvidence>,
}

impl TryFrom<WireHandle> for SandboxHandle {
    type Error = SandboxError;

    fn try_from(wire: WireHandle) -> Result<Self, Self::Error> {
        let mut payload = wire.payload;
        let tag = match payload.remove(SCHEMA) {
            Some(PayloadValue::String(tag)) => tag,
            _ => {
                return Err(SandboxError::invalid(
                    "sandbox handle payload requires a string `schema` tag",
                ));
            }
        };
        let payload = if tag == UNMANAGED {
            let provider_kind = match payload.remove(PROVIDER_KIND) {
                Some(PayloadValue::String(provider_kind)) if payload.is_empty() => provider_kind,
                _ => {
                    return Err(SandboxError::invalid(
                        "unmanaged sandbox handle payload carries exactly one string \
                         `provider_kind`",
                    ));
                }
            };
            HandlePayload::Unmanaged { provider_kind }
        } else {
            provider_payload(SandboxPayloadSchema::parse(&tag)?, payload)?
        };
        Ok(Self {
            sandbox_id: wire.sandbox_id,
            restoration: wire.restoration,
            payload,
            memory_materializations: wire.memory_materializations,
        })
    }
}

impl<'de> Deserialize<'de> for SandboxHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = WireHandle::deserialize(deserializer)?;
        Self::try_from(wire).map_err(serde::de::Error::custom)
    }
}

struct WirePayload<'a>(&'a HandlePayload);

impl Serialize for WirePayload<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self.0 {
            HandlePayload::Unmanaged { provider_kind } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry(SCHEMA, UNMANAGED)?;
                map.serialize_entry(PROVIDER_KIND, provider_kind)?;
                map.end()
            }
            HandlePayload::Provider {
                schema, payload, ..
            } => {
                let members = payload.members();
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry(SCHEMA, &schema.tag())?;
                for (name, value) in members {
                    map.serialize_entry(name, value)?;
                }
                map.end()
            }
        }
    }
}

impl Serialize for SandboxHandle {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut handle = serializer.serialize_struct("SandboxHandle", 4)?;
        handle.serialize_field("sandbox_id", &self.sandbox_id)?;
        match &self.restoration {
            Some(restoration) => handle.serialize_field("restoration", restoration)?,
            None => handle.skip_field("restoration")?,
        }
        handle.serialize_field("payload", &WirePayload(&self.payload))?;
        if self.memory_materializations.is_empty() {
            handle.skip_field("memory_materializations")?;
        } else {
            handle.serialize_field("memory_materializations", &self.memory_materializations)?;
        }
        handle.end()
    }
}

#[cfg(test)]
mod tests;
