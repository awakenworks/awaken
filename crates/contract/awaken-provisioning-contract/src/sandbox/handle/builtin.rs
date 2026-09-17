//! Built-in provider payload codecs still hosted by the contract.

use serde::{Deserialize, Serialize};

use super::super::restore_wire::HostBindRestorationHandle;
use super::super::{SandboxEffectFence, SandboxError};
use super::{SandboxHandle, SandboxPayloadSchema};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
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
    pub realization_fingerprint: crate::SandboxRealizationFingerprint,
    /// Aggregate-projected fence persisted by the provider that created this
    /// exact filesystem realization. The provider never mints this value.
    pub effect_fence: SandboxEffectFence,
    /// Opaque identity of one physical root incarnation. A same-spec rebuild
    /// receives a new value so a stale handle cannot delete or adopt it.
    pub physical_incarnation: String,
    pub owned_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceSandboxHandleV1 {
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub network: crate::NetworkPolicy,
    /// Exact provider control topology realized for this handle.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceSandboxHandleV2 {
    pub previous: NamespaceSandboxHandleV1,
    pub realization_fingerprint: crate::SandboxRealizationFingerprint,
    pub effect_fence: SandboxEffectFence,
    pub physical_incarnation: String,
    pub owned_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceProviderKind {
    Bubblewrap,
    Seatbelt,
}

impl NamespaceProviderKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bwrap",
            Self::Seatbelt => "seatbelt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSandboxHandleV1 {
    pub container_id: String,
    pub outputs_path: String,
    pub base_env: Vec<crate::EnvVar>,
    pub live_input_projection: bool,
    /// Sandbox-absolute paths excluded from a later mutable-layer checkpoint.
    /// The creating provider freezes this evidence so adoption never has to
    /// rediscover independently governed mounts from ambient infrastructure.
    #[serde(default)]
    pub continuation_excluded_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_handle: Option<ContainerContinuationHandle>,
    /// Exact provider runtime incarnation owning published control services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_control_incarnation: Option<crate::SandboxControlIncarnation>,
    /// Exact provider control topology realized alongside the incarnation.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSandboxHandleV2 {
    pub previous: ContainerSandboxHandleV1,
    /// Pure provider-effective configuration identity recomputed before any
    /// backend observation or adoption I/O. The physical realization fingerprint
    /// below extends this value with facts resolved during creation (for example
    /// an immutable package-image digest).
    pub adoption_fingerprint: crate::SandboxRealizationFingerprint,
    pub realization_fingerprint: crate::SandboxRealizationFingerprint,
    pub owned_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerContinuationHandle {
    /// Legacy retained-volume evidence. It remains decodable, but has no Pod
    /// incarnation and therefore cannot authorize destructive replacement or
    /// terminal deletion after Worker replacement.
    KubernetesContinuation { claim_uid: String },
    /// Current Kubernetes continuation evidence. The Pod UID fences the
    /// physical object independently from the optional retained-volume UID.
    KubernetesContinuationV2 {
        pod_uid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        claim_uid: Option<String>,
    },
    /// Host-owned exact staging target for a checkpoint restore.
    HostBindRestoration(HostBindRestorationHandle),
}

impl ContainerContinuationHandle {
    #[must_use]
    pub const fn is_host_bind_restoration(&self) -> bool {
        matches!(self, Self::HostBindRestoration(_))
    }
}

fn schema(provider: &str, version: u32) -> SandboxPayloadSchema {
    SandboxPayloadSchema::new(provider, version).expect("built-in payload schema is well-formed")
}

fn built_in(
    sandbox_id: impl Into<String>,
    provider: &str,
    version: u32,
    payload: &impl Serialize,
) -> SandboxHandle {
    SandboxHandle::from_provider_payload(sandbox_id, schema(provider, version), payload)
        .expect("typed built-in payloads always encode complete neutral evidence")
}

const LOCAL: &str = "local";
const CONTAINER: &str = "container";

impl NamespaceProviderKind {
    const fn wire_provider(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap",
            Self::Seatbelt => "seatbelt",
        }
    }
}

enum FilesystemPayload {
    /// A validated legacy payload; it carries no realization evidence.
    V1,
    V2(Box<LocalOrNamespaceV2>),
}

enum LocalOrNamespaceV2 {
    Local(LocalSandboxHandleV2),
    Namespace(NamespaceSandboxHandleV2),
}

impl SandboxHandle {
    #[must_use]
    pub fn local(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV1) -> Self {
        built_in(sandbox_id, LOCAL, 1, &payload)
    }

    #[must_use]
    pub fn local_v2(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV2) -> Self {
        built_in(sandbox_id, LOCAL, 2, &payload)
    }

    #[must_use]
    pub fn namespace(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV1,
    ) -> Self {
        built_in(sandbox_id, provider.wire_provider(), 1, &payload)
    }

    #[must_use]
    pub fn namespace_v2(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV2,
    ) -> Self {
        built_in(sandbox_id, provider.wire_provider(), 2, &payload)
    }

    #[must_use]
    pub fn container(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV1) -> Self {
        built_in(sandbox_id, CONTAINER, 1, &payload)
    }

    #[must_use]
    pub fn container_v2(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV2) -> Self {
        built_in(sandbox_id, CONTAINER, 2, &payload)
    }

    fn schema_is(&self, provider: &str) -> Option<u32> {
        self.payload_schema()
            .filter(|schema| schema.provider() == provider)
            .map(SandboxPayloadSchema::version)
    }

    fn filesystem_payload(&self) -> Option<Result<FilesystemPayload, SandboxError>> {
        let schema = self.payload_schema()?;
        let local = match schema.provider() {
            LOCAL => true,
            "bubblewrap" | "seatbelt" => false,
            _ => return None,
        };
        Some(match (local, schema.version()) {
            (true, 1) => self
                .decode_provider_payload::<LocalSandboxHandleV1>()
                .map(|_| FilesystemPayload::V1),
            (false, 1) => self
                .decode_provider_payload::<NamespaceSandboxHandleV1>()
                .map(|_| FilesystemPayload::V1),
            (true, 2) => self
                .decode_provider_payload()
                .map(|payload| FilesystemPayload::V2(Box::new(LocalOrNamespaceV2::Local(payload)))),
            (false, 2) => self.decode_provider_payload().map(|payload| {
                FilesystemPayload::V2(Box::new(LocalOrNamespaceV2::Namespace(payload)))
            }),
            _ => return None,
        })
    }

    pub fn local_payload(&self) -> Result<LocalSandboxHandleV1, SandboxError> {
        match self.schema_is(LOCAL) {
            Some(1) => self.decode_provider_payload(),
            Some(2) => self
                .decode_provider_payload::<LocalSandboxHandleV2>()
                .map(|payload| payload.previous),
            _ => Err(self.provider_mismatch(LOCAL)),
        }
    }

    pub fn namespace_payload(
        &self,
        expected_provider: NamespaceProviderKind,
    ) -> Result<NamespaceSandboxHandleV1, SandboxError> {
        match self.schema_is(expected_provider.wire_provider()) {
            Some(1) => self.decode_provider_payload(),
            Some(2) => self
                .decode_provider_payload::<NamespaceSandboxHandleV2>()
                .map(|payload| payload.previous),
            _ => Err(self.provider_mismatch(expected_provider.as_str())),
        }
    }

    pub fn container_payload(&self) -> Result<ContainerSandboxHandleV1, SandboxError> {
        match self.schema_is(CONTAINER) {
            Some(1) => self.decode_provider_payload(),
            Some(2) => self
                .decode_provider_payload::<ContainerSandboxHandleV2>()
                .map(|payload| payload.previous),
            _ => Err(self.provider_mismatch(CONTAINER)),
        }
    }

    /// Exact immutable physical incarnation used by an authorized container
    /// rebuild. Docker and Podman use their immutable container id. Kubernetes
    /// uses the Pod UID persisted by the current continuation handle; the
    /// legacy claim-only shape deliberately fails closed.
    pub fn container_physical_incarnation(&self) -> Result<String, SandboxError> {
        let payload = match self.schema_is(CONTAINER) {
            Some(2) => {
                self.decode_provider_payload::<ContainerSandboxHandleV2>()?
                    .previous
            }
            Some(1) => {
                return Err(SandboxError::unsupported(
                    "legacy container handle has no current realization evidence",
                ));
            }
            _ => return Err(self.provider_mismatch(CONTAINER)),
        };
        match payload.runtime_handle {
            Some(ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, .. }) => {
                if pod_uid.trim().is_empty() {
                    Err(SandboxError::invalid(
                        "Kubernetes continuation handle has an empty Pod incarnation",
                    ))
                } else {
                    Ok(pod_uid)
                }
            }
            Some(ContainerContinuationHandle::KubernetesContinuation { .. }) => {
                Err(SandboxError::unsupported(
                    "legacy Kubernetes handle has no Pod incarnation evidence",
                ))
            }
            Some(ContainerContinuationHandle::HostBindRestoration(_)) => {
                Err(SandboxError::invalid(
                    "host-bind restore handle cannot authorize ordinary container incarnation effects",
                ))
            }
            None if payload.container_id.trim().is_empty() => Err(SandboxError::invalid(
                "container handle has an empty physical incarnation",
            )),
            None => Ok(payload.container_id),
        }
    }

    /// Aggregate fence frozen into a current Local/Namespace filesystem handle.
    /// Legacy V1 returns `None`; another provider shape is a type mismatch.
    pub fn filesystem_effect_fence(&self) -> Result<Option<&SandboxEffectFence>, SandboxError> {
        match self.filesystem_payload() {
            Some(Ok(FilesystemPayload::V1)) => Ok(None),
            Some(Ok(FilesystemPayload::V2(_))) => self
                .evidence()
                .and_then(super::HandleEvidence::effect_fence)
                .map(Some)
                .ok_or_else(|| {
                    SandboxError::invalid("current filesystem sandbox handle has no effect fence")
                }),
            Some(Err(error)) => Err(error),
            None => Err(SandboxError::unsupported(format!(
                "filesystem provider cannot inspect {:?} handle evidence",
                self.provider_kind()
            ))),
        }
    }

    /// Immutable physical identity frozen into a current Local/Namespace
    /// filesystem handle. Empty current evidence is rejected rather than being
    /// reclassified as legacy absence.
    pub fn filesystem_physical_incarnation(&self) -> Result<Option<String>, SandboxError> {
        let incarnation = match self.filesystem_payload() {
            Some(Ok(FilesystemPayload::V1)) => return Ok(None),
            Some(Ok(FilesystemPayload::V2(payload))) => match *payload {
                LocalOrNamespaceV2::Local(payload) => payload.physical_incarnation,
                LocalOrNamespaceV2::Namespace(payload) => payload.physical_incarnation,
            },
            Some(Err(error)) => return Err(error),
            None => {
                return Err(SandboxError::unsupported(format!(
                    "filesystem provider cannot inspect {:?} handle evidence",
                    self.provider_kind()
                )));
            }
        };
        if incarnation.trim().is_empty() {
            Err(SandboxError::invalid(
                "current filesystem sandbox handle has an empty physical incarnation",
            ))
        } else {
            Ok(Some(incarnation))
        }
    }

    /// Provider-effective configuration identity required to adopt a current
    /// container handle. V1 remains continuity-only and returns `None`; callers
    /// must not synthesize current evidence from its partial payload.
    pub fn container_adoption_fingerprint(
        &self,
    ) -> Result<Option<crate::SandboxRealizationFingerprint>, SandboxError> {
        match self.schema_is(CONTAINER) {
            Some(1) => Ok(None),
            Some(2) => self
                .decode_provider_payload::<ContainerSandboxHandleV2>()
                .map(|payload| Some(payload.adoption_fingerprint)),
            _ => Err(self.provider_mismatch(CONTAINER)),
        }
    }
}
