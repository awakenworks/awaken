//! Durable handle payloads owned by the Workdir (`local`) and Namespace
//! (`bubblewrap` / `seatbelt`) providers.
//!
//! The neutral [`pc::SandboxHandle`] carries these payloads opaquely; this
//! module is their only codec. The wire shapes are the historical ones: the
//! schema tag is `local_v1`/`local_v2`, `bubblewrap_v1`/`bubblewrap_v2` or
//! `seatbelt_v1`/`seatbelt_v2`, and V2 records the neutral realization evidence
//! (`realization_fingerprint`, `effect_fence`, `owned_paths`) at the top level
//! of the payload beside this provider's own locator.

use std::collections::BTreeSet;

use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServiceKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<pc::EnvVar>,
    pub continuation_excluded_paths: Vec<String>,
    /// Workdir egress posture is part of the durable projection.
    #[serde(default)]
    pub deny_tool_egress: bool,
}

/// Current local durable handle. V1 remains decodable for non-Repository
/// compatibility; V2 adds the complete provider-observed path set needed to
/// prove safe Repository adoption after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSandboxHandleV2 {
    pub previous: LocalSandboxHandleV1,
    pub realization_fingerprint: pc::SandboxRealizationFingerprint,
    /// Aggregate-projected fence persisted by the provider that created this
    /// exact filesystem realization. The provider never mints this value.
    pub effect_fence: pc::SandboxEffectFence,
    /// Opaque identity of one physical root incarnation. A same-spec rebuild
    /// receives a new value so a stale handle cannot delete or adopt it.
    pub physical_incarnation: String,
    pub owned_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<pc::EnvVar>,
    pub network: pc::NetworkPolicy,
    /// Exact provider control topology realized for this handle.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub control_services: BTreeSet<SandboxControlServiceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceSandboxHandleV2 {
    pub previous: NamespaceSandboxHandleV1,
    pub realization_fingerprint: pc::SandboxRealizationFingerprint,
    pub effect_fence: pc::SandboxEffectFence,
    pub physical_incarnation: String,
    pub owned_paths: Vec<String>,
}

/// The platform namespace mechanism that owns a Namespace handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceProviderKind {
    Bubblewrap,
    Seatbelt,
}

impl NamespaceProviderKind {
    /// Operator-facing mechanism name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bwrap",
            Self::Seatbelt => "seatbelt",
        }
    }

    /// Durable payload schema provider (`<provider>_v<N>` tag prefix).
    #[must_use]
    pub const fn schema_provider(self) -> &'static str {
        match self {
            Self::Bubblewrap => BUBBLEWRAP,
            Self::Seatbelt => SEATBELT,
        }
    }
}

const LOCAL: &str = "local";
const BUBBLEWRAP: &str = "bubblewrap";
const SEATBELT: &str = "seatbelt";

mod sealed {
    pub trait Sealed {}
    impl Sealed for awaken_provisioning_contract::SandboxHandle {}
}

/// Typed codec for the filesystem providers' durable handle payloads.
///
/// Implemented only for [`pc::SandboxHandle`]; import it to construct or read
/// a Local/Namespace handle. Every accessor rejects another provider's schema
/// and any payload that does not decode into this provider's exact type.
pub trait FilesystemHandleCodec: sealed::Sealed + Sized {
    fn local(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV1) -> Self;

    fn local_v2(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV2) -> Self;

    fn namespace(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV1,
    ) -> Self;

    fn namespace_v2(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV2,
    ) -> Self;

    /// The Workdir locator of a `local_v1`/`local_v2` handle.
    fn local_payload(&self) -> Result<LocalSandboxHandleV1, pc::SandboxError>;

    /// The Namespace locator of a handle owned by `expected_provider`.
    fn namespace_payload(
        &self,
        expected_provider: NamespaceProviderKind,
    ) -> Result<NamespaceSandboxHandleV1, pc::SandboxError>;

    /// Aggregate fence frozen into a current Local/Namespace filesystem handle.
    /// Legacy V1 returns `None`; another provider shape is a type mismatch.
    fn filesystem_effect_fence(&self) -> Result<Option<&pc::SandboxEffectFence>, pc::SandboxError>;

    /// Immutable physical identity frozen into a current Local/Namespace
    /// filesystem handle. Empty current evidence is rejected rather than being
    /// reclassified as legacy absence.
    fn filesystem_physical_incarnation(&self) -> Result<Option<String>, pc::SandboxError>;
}

fn encode(
    sandbox_id: impl Into<String>,
    provider: &str,
    version: u32,
    payload: &impl Serialize,
) -> pc::SandboxHandle {
    let schema = pc::SandboxPayloadSchema::new(provider, version)
        .expect("filesystem payload schemas are lowercase identifiers with positive versions");
    pc::SandboxHandle::from_provider_payload(sandbox_id, schema, payload)
        .expect("typed filesystem payloads always encode one object with complete evidence")
}

/// A filesystem handle's decoded generation. V1 is validated but carries no
/// realization evidence.
enum FilesystemGeneration {
    V1,
    V2 { physical_incarnation: String },
}

fn filesystem_generation(
    handle: &pc::SandboxHandle,
) -> Result<FilesystemGeneration, pc::SandboxError> {
    let unsupported = || {
        pc::SandboxError::unsupported(format!(
            "filesystem provider cannot inspect {:?} handle evidence",
            handle.provider_kind()
        ))
    };
    let schema = handle.payload_schema().ok_or_else(unsupported)?;
    let local = match schema.provider() {
        LOCAL => true,
        BUBBLEWRAP | SEATBELT => false,
        _ => return Err(unsupported()),
    };
    match (local, schema.version()) {
        (true, 1) => handle
            .decode_provider_payload::<LocalSandboxHandleV1>()
            .map(|_| FilesystemGeneration::V1),
        (false, 1) => handle
            .decode_provider_payload::<NamespaceSandboxHandleV1>()
            .map(|_| FilesystemGeneration::V1),
        (true, 2) => handle
            .decode_provider_payload::<LocalSandboxHandleV2>()
            .map(|payload| FilesystemGeneration::V2 {
                physical_incarnation: payload.physical_incarnation,
            }),
        (false, 2) => handle
            .decode_provider_payload::<NamespaceSandboxHandleV2>()
            .map(|payload| FilesystemGeneration::V2 {
                physical_incarnation: payload.physical_incarnation,
            }),
        _ => Err(unsupported()),
    }
}

fn version_of(handle: &pc::SandboxHandle, provider: &str) -> Option<u32> {
    handle
        .payload_schema()
        .filter(|schema| schema.provider() == provider)
        .map(pc::SandboxPayloadSchema::version)
}

impl FilesystemHandleCodec for pc::SandboxHandle {
    fn local(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV1) -> Self {
        encode(sandbox_id, LOCAL, 1, &payload)
    }

    fn local_v2(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV2) -> Self {
        encode(sandbox_id, LOCAL, 2, &payload)
    }

    fn namespace(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV1,
    ) -> Self {
        encode(sandbox_id, provider.schema_provider(), 1, &payload)
    }

    fn namespace_v2(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV2,
    ) -> Self {
        encode(sandbox_id, provider.schema_provider(), 2, &payload)
    }

    fn local_payload(&self) -> Result<LocalSandboxHandleV1, pc::SandboxError> {
        match version_of(self, LOCAL) {
            Some(1) => self.decode_provider_payload(),
            Some(2) => self
                .decode_provider_payload::<LocalSandboxHandleV2>()
                .map(|payload| payload.previous),
            _ => Err(self.provider_mismatch(LOCAL)),
        }
    }

    fn namespace_payload(
        &self,
        expected_provider: NamespaceProviderKind,
    ) -> Result<NamespaceSandboxHandleV1, pc::SandboxError> {
        match version_of(self, expected_provider.schema_provider()) {
            Some(1) => self.decode_provider_payload(),
            Some(2) => self
                .decode_provider_payload::<NamespaceSandboxHandleV2>()
                .map(|payload| payload.previous),
            _ => Err(self.provider_mismatch(expected_provider.as_str())),
        }
    }

    fn filesystem_effect_fence(&self) -> Result<Option<&pc::SandboxEffectFence>, pc::SandboxError> {
        match filesystem_generation(self)? {
            FilesystemGeneration::V1 => Ok(None),
            FilesystemGeneration::V2 { .. } => self
                .evidence()
                .and_then(pc::HandleEvidence::effect_fence)
                .map(Some)
                .ok_or_else(|| {
                    pc::SandboxError::invalid(
                        "current filesystem sandbox handle has no effect fence",
                    )
                }),
        }
    }

    fn filesystem_physical_incarnation(&self) -> Result<Option<String>, pc::SandboxError> {
        match filesystem_generation(self)? {
            FilesystemGeneration::V1 => Ok(None),
            FilesystemGeneration::V2 {
                physical_incarnation,
            } if physical_incarnation.trim().is_empty() => Err(pc::SandboxError::invalid(
                "current filesystem sandbox handle has an empty physical incarnation",
            )),
            FilesystemGeneration::V2 {
                physical_incarnation,
            } => Ok(Some(physical_incarnation)),
        }
    }
}

#[cfg(test)]
mod tests;
