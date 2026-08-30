//! Durable sandbox handles, provider selection, and live execution ports.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::spec::{Command, SandboxSpec};
use crate::vocab::{Artifact, MountAccess, MountRequirement, RealizedMount};

use super::foundation::*;
use super::restore_contract::{SandboxRestoreRequest, SandboxRestoreResult, SandboxRestoreTarget};
use super::restore_wire::{HostBindRestorationHandle, SandboxRestorationEvidence};

mod disposal;
mod memory_reconciliation;

pub use disposal::{SandboxDisposalAuthorization, SandboxDisposalPreparation};
pub use memory_reconciliation::MemoryReconciliationAck;
use memory_reconciliation::{
    default_memory_reconciliation_ack, validate_live_memory_reconciliation_fence,
};

/// A serializable, **durable** reference to a realized sandbox. Persist it the
/// moment a sandbox is created; a live `Box<dyn Sandbox>` cannot survive a host
/// restart, but the handle can be stored and later passed to
/// [`SandboxProvider::adopt`] to reconnect to a still-running remote sandbox
/// (k8s pod / container on another host). For a local sandbox it is just the
/// directory id. The closed, versioned payload enum makes every durable locator
/// explicit and rejects unknown or cross-provider shapes during deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxHandle {
    pub sandbox_id: String,
    /// Exact restore effect which owns this physical binding. This remains
    /// provider evidence on the canonical handle, never a second lifecycle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restoration: Option<SandboxRestorationEvidence>,
    payload: SandboxHandlePayload,
    /// Original CAS bases for copy-backed Memory mounts. This lives beside the
    /// provider payload because every current provider can carry the same
    /// evidence and the serialized handle is already the Session root binding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    memory_materializations: Vec<MemoryMaterializationEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "schema", rename_all = "snake_case", deny_unknown_fields)]
enum SandboxHandlePayload {
    Unmanaged { provider_kind: String },
    LocalV1(LocalSandboxHandleV1),
    LocalV2(LocalSandboxHandleV2),
    BubblewrapV1(NamespaceSandboxHandleV1),
    BubblewrapV2(NamespaceSandboxHandleV2),
    SeatbeltV1(NamespaceSandboxHandleV1),
    SeatbeltV2(NamespaceSandboxHandleV2),
    ContainerV1(ContainerSandboxHandleV1),
    ContainerV2(ContainerSandboxHandleV2),
}

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

impl SandboxHandle {
    /// Construct a deliberately non-resumable handle for ephemeral providers and
    /// test doubles. Durable built-in providers use one of the typed constructors.
    pub fn new(provider_kind: impl Into<String>, sandbox_id: impl Into<String>) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::Unmanaged {
                provider_kind: provider_kind.into(),
            },
            memory_materializations: Vec::new(),
        }
    }

    #[must_use]
    pub fn local(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV1) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::LocalV1(payload),
            memory_materializations: Vec::new(),
        }
    }

    #[must_use]
    pub fn local_v2(sandbox_id: impl Into<String>, payload: LocalSandboxHandleV2) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::LocalV2(payload),
            memory_materializations: Vec::new(),
        }
    }

    #[must_use]
    pub fn namespace(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV1,
    ) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: match provider {
                NamespaceProviderKind::Bubblewrap => SandboxHandlePayload::BubblewrapV1(payload),
                NamespaceProviderKind::Seatbelt => SandboxHandlePayload::SeatbeltV1(payload),
            },
            memory_materializations: Vec::new(),
        }
    }

    #[must_use]
    pub fn namespace_v2(
        provider: NamespaceProviderKind,
        sandbox_id: impl Into<String>,
        payload: NamespaceSandboxHandleV2,
    ) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: match provider {
                NamespaceProviderKind::Bubblewrap => SandboxHandlePayload::BubblewrapV2(payload),
                NamespaceProviderKind::Seatbelt => SandboxHandlePayload::SeatbeltV2(payload),
            },
            memory_materializations: Vec::new(),
        }
    }

    #[must_use]
    pub fn container(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV1) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::ContainerV1(payload),
            memory_materializations: Vec::new(),
        }
    }

    #[must_use]
    pub fn container_v2(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV2) -> Self {
        Self {
            sandbox_id: sandbox_id.into(),
            restoration: None,
            payload: SandboxHandlePayload::ContainerV2(payload),
            memory_materializations: Vec::new(),
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
            return Err(SandboxError::new(
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
        if !matches!(
            self.payload,
            SandboxHandlePayload::LocalV2(_)
                | SandboxHandlePayload::BubblewrapV2(_)
                | SandboxHandlePayload::SeatbeltV2(_)
                | SandboxHandlePayload::ContainerV2(_)
        ) {
            return Err(SandboxError::new(
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
        if matches!(
            self.payload,
            SandboxHandlePayload::Unmanaged { .. }
                | SandboxHandlePayload::LocalV1(_)
                | SandboxHandlePayload::BubblewrapV1(_)
                | SandboxHandlePayload::SeatbeltV1(_)
                | SandboxHandlePayload::ContainerV1(_)
        ) {
            return Ok(None);
        }
        MemoryMaterializationEvidence::validate_all(&self.memory_materializations)?;
        Ok(Some(&self.memory_materializations))
    }

    pub fn local_payload(&self) -> Result<&LocalSandboxHandleV1, SandboxError> {
        match &self.payload {
            SandboxHandlePayload::LocalV1(payload) => Ok(payload),
            SandboxHandlePayload::LocalV2(payload) => Ok(&payload.previous),
            _ => Err(self.payload_mismatch("local")),
        }
    }

    pub fn namespace_payload(
        &self,
        expected_provider: NamespaceProviderKind,
    ) -> Result<&NamespaceSandboxHandleV1, SandboxError> {
        match (expected_provider, &self.payload) {
            (NamespaceProviderKind::Bubblewrap, SandboxHandlePayload::BubblewrapV1(payload))
            | (NamespaceProviderKind::Seatbelt, SandboxHandlePayload::SeatbeltV1(payload)) => {
                Ok(payload)
            }
            (NamespaceProviderKind::Bubblewrap, SandboxHandlePayload::BubblewrapV2(payload))
            | (NamespaceProviderKind::Seatbelt, SandboxHandlePayload::SeatbeltV2(payload)) => {
                Ok(&payload.previous)
            }
            _ => Err(self.payload_mismatch(expected_provider.as_str())),
        }
    }

    pub fn container_payload(&self) -> Result<&ContainerSandboxHandleV1, SandboxError> {
        match &self.payload {
            SandboxHandlePayload::ContainerV1(payload) => Ok(payload),
            SandboxHandlePayload::ContainerV2(payload) => Ok(&payload.previous),
            _ => Err(self.payload_mismatch("container")),
        }
    }

    /// Exact immutable physical incarnation used by an authorized container
    /// rebuild. Docker and Podman use their immutable container id. Kubernetes
    /// uses the Pod UID persisted by the current continuation handle; the
    /// legacy claim-only shape deliberately fails closed.
    pub fn container_physical_incarnation(&self) -> Result<&str, SandboxError> {
        let payload = match &self.payload {
            SandboxHandlePayload::ContainerV2(payload) => &payload.previous,
            SandboxHandlePayload::ContainerV1(_) => {
                return Err(SandboxError::new(
                    "legacy container handle has no current realization evidence",
                ));
            }
            _ => return Err(self.payload_mismatch("container")),
        };
        match payload.runtime_handle.as_ref() {
            Some(ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, .. }) => {
                if pod_uid.trim().is_empty() {
                    Err(SandboxError::new(
                        "Kubernetes continuation handle has an empty Pod incarnation",
                    ))
                } else {
                    Ok(pod_uid)
                }
            }
            Some(ContainerContinuationHandle::KubernetesContinuation { .. }) => Err(
                SandboxError::new("legacy Kubernetes handle has no Pod incarnation evidence"),
            ),
            Some(ContainerContinuationHandle::HostBindRestoration(_)) => Err(SandboxError::new(
                "host-bind restore handle cannot authorize ordinary container incarnation effects",
            )),
            None if payload.container_id.trim().is_empty() => Err(SandboxError::new(
                "container handle has an empty physical incarnation",
            )),
            None => Ok(&payload.container_id),
        }
    }

    /// Complete provider-observed path evidence for current handles. `None`
    /// identifies a legacy/unmanaged payload and must fail closed when a
    /// Repository adoption needs proof of non-overlap.
    #[must_use]
    pub fn owned_paths(&self) -> Option<&[String]> {
        match &self.payload {
            SandboxHandlePayload::LocalV2(payload) => Some(&payload.owned_paths),
            SandboxHandlePayload::BubblewrapV2(payload)
            | SandboxHandlePayload::SeatbeltV2(payload) => Some(&payload.owned_paths),
            SandboxHandlePayload::ContainerV2(payload) => Some(&payload.owned_paths),
            SandboxHandlePayload::Unmanaged { .. }
            | SandboxHandlePayload::LocalV1(_)
            | SandboxHandlePayload::BubblewrapV1(_)
            | SandboxHandlePayload::SeatbeltV1(_)
            | SandboxHandlePayload::ContainerV1(_) => None,
        }
    }

    /// Exact provider-visible creation identity carried by current handles.
    /// Legacy handles predate crash-safe create-or-adopt evidence and return
    /// `None`; callers may preserve their compatibility but must not synthesize
    /// this proof from a partial payload.
    #[must_use]
    pub fn realization_fingerprint(&self) -> Option<&crate::SandboxRealizationFingerprint> {
        match &self.payload {
            SandboxHandlePayload::LocalV2(payload) => Some(&payload.realization_fingerprint),
            SandboxHandlePayload::BubblewrapV2(payload)
            | SandboxHandlePayload::SeatbeltV2(payload) => Some(&payload.realization_fingerprint),
            SandboxHandlePayload::ContainerV2(payload) => Some(&payload.realization_fingerprint),
            SandboxHandlePayload::Unmanaged { .. }
            | SandboxHandlePayload::LocalV1(_)
            | SandboxHandlePayload::BubblewrapV1(_)
            | SandboxHandlePayload::SeatbeltV1(_)
            | SandboxHandlePayload::ContainerV1(_) => None,
        }
    }

    /// Aggregate fence frozen into a current Local/Namespace filesystem handle.
    /// Legacy V1 returns `None`; another provider shape is a type mismatch.
    pub fn filesystem_effect_fence(&self) -> Result<Option<&SandboxEffectFence>, SandboxError> {
        match &self.payload {
            SandboxHandlePayload::LocalV1(_)
            | SandboxHandlePayload::BubblewrapV1(_)
            | SandboxHandlePayload::SeatbeltV1(_) => Ok(None),
            SandboxHandlePayload::LocalV2(payload) => Ok(Some(&payload.effect_fence)),
            SandboxHandlePayload::BubblewrapV2(payload)
            | SandboxHandlePayload::SeatbeltV2(payload) => Ok(Some(&payload.effect_fence)),
            _ => Err(SandboxError::new(format!(
                "filesystem provider cannot inspect {:?} handle evidence",
                self.provider_kind()
            ))),
        }
    }

    /// Immutable physical identity frozen into a current Local/Namespace
    /// filesystem handle. Empty current evidence is rejected rather than being
    /// reclassified as legacy absence.
    pub fn filesystem_physical_incarnation(&self) -> Result<Option<&str>, SandboxError> {
        let incarnation = match &self.payload {
            SandboxHandlePayload::LocalV1(_)
            | SandboxHandlePayload::BubblewrapV1(_)
            | SandboxHandlePayload::SeatbeltV1(_) => return Ok(None),
            SandboxHandlePayload::LocalV2(payload) => payload.physical_incarnation.as_str(),
            SandboxHandlePayload::BubblewrapV2(payload)
            | SandboxHandlePayload::SeatbeltV2(payload) => payload.physical_incarnation.as_str(),
            _ => {
                return Err(SandboxError::new(format!(
                    "filesystem provider cannot inspect {:?} handle evidence",
                    self.provider_kind()
                )));
            }
        };
        if incarnation.trim().is_empty() {
            Err(SandboxError::new(
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
    ) -> Result<Option<&crate::SandboxRealizationFingerprint>, SandboxError> {
        match &self.payload {
            SandboxHandlePayload::ContainerV1(_) => Ok(None),
            SandboxHandlePayload::ContainerV2(payload) => Ok(Some(&payload.adoption_fingerprint)),
            _ => Err(self.payload_mismatch("container")),
        }
    }

    /// Whether two durable handles name the same physical substrate and carry
    /// the same immutable provider locator and original Memory materialization
    /// heads. V2 path evidence is deliberately excluded: a Resource reservation
    /// extends that evidence without creating a second Sandbox identity, while
    /// changing a Memory base would change terminal CAS authority.
    #[must_use]
    pub fn same_substrate(&self, other: &Self) -> bool {
        if self.sandbox_id != other.sandbox_id
            || self.restoration != other.restoration
            || self.memory_materializations != other.memory_materializations
        {
            return false;
        }
        match (&self.payload, &other.payload) {
            (
                SandboxHandlePayload::Unmanaged {
                    provider_kind: left,
                },
                SandboxHandlePayload::Unmanaged {
                    provider_kind: right,
                },
            ) => left == right,
            (SandboxHandlePayload::LocalV1(_), SandboxHandlePayload::LocalV1(_)) => false,
            (SandboxHandlePayload::LocalV1(_), SandboxHandlePayload::LocalV2(_))
            | (SandboxHandlePayload::LocalV2(_), SandboxHandlePayload::LocalV1(_)) => false,
            (SandboxHandlePayload::LocalV2(left), SandboxHandlePayload::LocalV2(right)) => {
                left.previous == right.previous
                    && left.realization_fingerprint == right.realization_fingerprint
                    && left.effect_fence.same_effect_identity(&right.effect_fence)
                    && left.physical_incarnation == right.physical_incarnation
            }
            (SandboxHandlePayload::BubblewrapV1(_), SandboxHandlePayload::BubblewrapV1(_))
            | (SandboxHandlePayload::SeatbeltV1(_), SandboxHandlePayload::SeatbeltV1(_)) => false,
            (SandboxHandlePayload::BubblewrapV1(_), SandboxHandlePayload::BubblewrapV2(_))
            | (SandboxHandlePayload::BubblewrapV2(_), SandboxHandlePayload::BubblewrapV1(_))
            | (SandboxHandlePayload::SeatbeltV1(_), SandboxHandlePayload::SeatbeltV2(_))
            | (SandboxHandlePayload::SeatbeltV2(_), SandboxHandlePayload::SeatbeltV1(_)) => false,
            (
                SandboxHandlePayload::BubblewrapV2(left),
                SandboxHandlePayload::BubblewrapV2(right),
            )
            | (SandboxHandlePayload::SeatbeltV2(left), SandboxHandlePayload::SeatbeltV2(right)) => {
                left.previous == right.previous
                    && left.realization_fingerprint == right.realization_fingerprint
                    && left.effect_fence.same_effect_identity(&right.effect_fence)
                    && left.physical_incarnation == right.physical_incarnation
            }
            (SandboxHandlePayload::ContainerV1(left), SandboxHandlePayload::ContainerV1(right)) => {
                left == right
            }
            (SandboxHandlePayload::ContainerV1(left), SandboxHandlePayload::ContainerV2(right))
            | (SandboxHandlePayload::ContainerV2(right), SandboxHandlePayload::ContainerV1(left)) => {
                left == &right.previous
            }
            (SandboxHandlePayload::ContainerV2(left), SandboxHandlePayload::ContainerV2(right)) => {
                left.previous == right.previous
                    && left.adoption_fingerprint == right.adoption_fingerprint
                    && left.realization_fingerprint == right.realization_fingerprint
            }
            _ => false,
        }
    }

    /// Admit the one durable-handle update allowed by a Resource path WAL:
    /// both handles already carry current V2 realization evidence, immutable
    /// substrate identity is unchanged, and complete owned paths grow
    /// monotonically. Legacy V1 handles remain adoptable for non-Repository
    /// compatibility but can never mint V2 ownership evidence.
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

    #[must_use]
    pub fn provider_kind(&self) -> &str {
        match &self.payload {
            SandboxHandlePayload::Unmanaged { provider_kind } => provider_kind,
            SandboxHandlePayload::LocalV1(_) | SandboxHandlePayload::LocalV2(_) => "local",
            SandboxHandlePayload::BubblewrapV1(_) | SandboxHandlePayload::BubblewrapV2(_) => {
                "bwrap"
            }
            SandboxHandlePayload::SeatbeltV1(_) | SandboxHandlePayload::SeatbeltV2(_) => "seatbelt",
            SandboxHandlePayload::ContainerV1(_) | SandboxHandlePayload::ContainerV2(_) => {
                "container"
            }
        }
    }

    fn payload_mismatch(&self, expected_provider: &str) -> SandboxError {
        SandboxError::new(format!(
            "{expected_provider} provider cannot adopt {:?} handle payload",
            self.provider_kind()
        ))
    }
}

/// The lifecycle state of a sandbox, queryable idempotently (survives reconnect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    /// Being realized (image pull, mounts binding).
    Provisioning,
    /// Realized and usable — processes may be spawned.
    Ready,
    /// Torn down, reaped, or lease-expired; no longer usable.
    Terminated,
}

/// Typed observation of one exact durable Sandbox handle before adoption or
/// fenced physical cleanup. Provider errors are deliberately kept outside this
/// enum: an error is indeterminate and can only be retried, never interpreted
/// as proof that a live substrate may be replaced or disposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxObservation {
    /// The exact physical realization is still converging.
    Provisioning,
    /// The exact physical realization is ready to be adopted.
    Ready,
    /// The provider proved that the exact durable realization is gone. A
    /// daemon-backed provider includes the immutable incarnation it searched
    /// for; a deterministic filesystem provider may use its marker as the
    /// complete absence proof and therefore carry no separate value.
    DefinitivelyUnavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        physical_incarnation: Option<String>,
    },
    /// The exact immutable physical realization still exists in a terminal
    /// state and must be disposed under the same aggregate effect fence.
    /// Keeping this distinct from absence prevents a terminal Pod/container
    /// from being acknowledged without deletion.
    Terminal { physical_incarnation: String },
    /// The exact physical realization has crossed its provider-owned durable
    /// cleanup gate. Live output, credential, checkpoint, and Memory reads are
    /// no longer permitted; only replay of already-durable receipts and exact
    /// physical disposal may continue under the same aggregate fence.
    /// This is an observation of backend evidence, not a second cleanup state
    /// machine or mutation authority.
    Disposing { physical_incarnation: String },
    /// The provider reached a stable physical fact, but it cannot represent
    /// the durable handle being observed (for example a foreign incarnation,
    /// missing immutable fingerprint, or duplicate realization). This is a
    /// non-destructive permanent rejection, not transient unavailability and
    /// never replacement authority.
    Incompatible { reason: String },
}

/// Provider-neutral authorization fence for one physical Sandbox effect.
///
/// The operation identity and lease fields are projected from an existing
/// aggregate-owned effect and realization lease. Providers may persist this
/// value as backend evidence, but must never mint or reinterpret it as a second
/// ownership authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxEffectFence {
    pub operation_id: String,
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

impl SandboxEffectFence {
    pub fn new(
        operation_id: impl Into<String>,
        owner: impl Into<String>,
        runtime_incarnation: impl Into<String>,
        epoch: u64,
        expires_at_unix_ms: u64,
    ) -> Result<Self, SandboxError> {
        let fence = Self {
            operation_id: operation_id.into(),
            owner: owner.into(),
            runtime_incarnation: runtime_incarnation.into(),
            epoch,
            expires_at_unix_ms,
        };
        fence.validate_identity()?;
        Ok(fence)
    }

    /// Validate the complete aggregate effect identity carried across provider
    /// boundaries. Deserialized and structurally constructed values use the
    /// same rule as [`Self::new`].
    pub fn validate_identity(&self) -> Result<(), SandboxError> {
        for (field, value) in [
            ("operation id", self.operation_id.as_str()),
            ("owner", self.owner.as_str()),
            ("runtime incarnation", self.runtime_incarnation.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(SandboxError::new(format!(
                    "Sandbox effect fence requires a non-empty {field}"
                )));
            }
        }
        Ok(())
    }

    /// Validate identity and expiry against a caller-observed clock instant.
    /// Clock acquisition remains adapter-owned; the decision rule does not.
    pub fn validate_live_at(&self, now_unix_ms: u64) -> Result<(), SandboxError> {
        self.validate_identity()?;
        if self.expired_at(now_unix_ms) {
            return Err(SandboxError::new(
                "Sandbox effect fence expired before the provider boundary",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn expired_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }

    /// Compare the immutable aggregate effect identity while deliberately
    /// ignoring its renewable expiry boundary.
    #[must_use]
    pub fn same_effect_identity(&self, other: &Self) -> bool {
        self.operation_id == other.operation_id
            && self.owner == other.owner
            && self.runtime_incarnation == other.runtime_incarnation
            && self.epoch == other.epoch
    }

    /// Compare the realization lease independently of the aggregate operation.
    /// An aggregate-authorized successor effect may reuse process-local state
    /// only while all three lease coordinates remain exact.
    #[must_use]
    pub fn same_realization_lease(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.runtime_incarnation == other.runtime_incarnation
            && self.epoch == other.epoch
    }

    /// Whether an aggregate-authorized successor may take over a provider
    /// participant created under this fence. A renewal preserves the exact
    /// owner/incarnation/epoch and may only extend expiry; a reassignment must
    /// advance the aggregate's monotonic epoch. Operation identity is checked
    /// separately by the typed effect that owns the transition.
    #[must_use]
    pub fn authorizes_successor(&self, successor: &Self) -> bool {
        successor.epoch > self.epoch
            || (self.same_realization_lease(successor)
                && successor.expires_at_unix_ms >= self.expires_at_unix_ms)
    }

    /// Whether this fence authorizes a successor for the same aggregate
    /// operation. This is the canonical provider-preparation comparison:
    /// callers must not combine lease monotonicity with a separately guessed
    /// operation identity.
    #[must_use]
    pub fn authorizes_effect_successor(&self, successor: &Self) -> bool {
        self.operation_id == successor.operation_id && self.authorizes_successor(successor)
    }
}

/// Isolation strength, ordered `Workdir < Namespace < Container`. A provider
/// admits a spec only when its class is `>=` the requested one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationClass {
    /// Working-directory selection only; no OS isolation (dev/CI/trusted).
    #[default]
    Workdir,
    /// OS-namespace isolation (bubblewrap / sandbox-exec).
    Namespace,
    /// Full container/VM isolation.
    Container,
}

/// Minimum enforceable Sandbox properties required before a workload may be
/// placed on a Worker. This is the one requirement vocabulary shared by
/// provider admission and distributed Worker placement; it deliberately omits
/// live handles, paths, mounts, credentials, and provider implementation names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRequirements {
    #[serde(default)]
    pub isolation: IsolationClass,
    #[serde(default)]
    pub tool_transparent: bool,
    #[serde(default)]
    pub path_fidelity: bool,
    #[serde(default)]
    pub enforced_readonly: bool,
    #[serde(default)]
    pub network_isolation: bool,
    #[serde(default)]
    pub enforced_network_allowlist: bool,
    #[serde(default)]
    pub resource_limits: bool,
    #[serde(default)]
    pub custom_rootfs: bool,
    #[serde(default)]
    pub package_provisioning: bool,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

impl SandboxRequirements {
    /// Derive placement requirements from the exact neutral realization spec.
    /// `opaque_process` is true for ACP and for Native Hand execution because
    /// both must remain correct without cooperative lexical path rewriting.
    #[must_use]
    pub fn from_spec(spec: &SandboxSpec, opaque_process: bool) -> Self {
        use crate::vocab::NetworkPolicy;

        let custom_rootfs = spec.environment.is_some();
        Self {
            isolation: if opaque_process {
                spec.isolation.max(IsolationClass::Namespace)
            } else {
                spec.isolation
            },
            tool_transparent: opaque_process,
            path_fidelity: opaque_process,
            enforced_readonly: spec
                .mounts
                .iter()
                .any(|mount| mount.access == MountAccess::ReadOnly),
            network_isolation: spec.network.is_restricted(),
            enforced_network_allowlist: matches!(spec.network, NetworkPolicy::Allowlist { .. }),
            resource_limits: spec.limits.is_set(),
            custom_rootfs,
            package_provisioning: !spec.packages.is_empty(),
            control_services: spec.control_services.clone(),
        }
    }
}

/// What a backend can actually enforce — the host probes this to pick a provider
/// and to fail closed when a spec asks for more than a backend can give.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub isolation: IsolationClass,
    /// **The load-bearing flag.** True when isolation is OS-enforced on an
    /// arbitrary launched process (so it holds for Claude Code / any CLI); false
    /// for a cooperating-tool-only jail (lexical), which must never host an opaque
    /// agent process.
    pub tool_transparent: bool,
    /// Sandbox-absolute paths are real to launched processes (vs. lexical rewrite).
    pub path_fidelity: bool,
    /// Read-only mounts are OS-enforced.
    pub enforced_readonly: bool,
    /// Egress can be isolated/controlled.
    pub network_isolation: bool,
    /// Host allowlists are enforced for arbitrary workload traffic at a
    /// no-bypass network boundary. A process proxy environment variable is not
    /// sufficient evidence because the workload can remove or ignore it.
    #[serde(default)]
    pub enforced_network_allowlist: bool,
    /// `EnvVisibility::EgressOnly` secrets can be honored.
    pub secret_egress_substitution: bool,
    /// Resource limits are enforced.
    pub resource_limits: bool,
    /// Provides its own userland/rootfs (vs. borrowing the host's binaries).
    pub custom_rootfs: bool,
    /// Can materialize exact package requirements before workload launch and
    /// preserve them across adoption of the same sandbox handle.
    #[serde(default)]
    pub package_provisioning: bool,
    /// Closed control services this concrete provider can actually publish.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub control_services:
        std::collections::BTreeSet<awaken_sandbox_control::SandboxControlServiceKind>,
}

impl SandboxCapabilities {
    /// One monotonic compatibility predicate used by local provider selection
    /// and remote Worker admission. Ranking policy runs only after this succeeds.
    #[must_use]
    pub fn satisfies_requirements(&self, required: &SandboxRequirements) -> bool {
        capability_requirements_satisfied(
            self.isolation,
            required.isolation,
            self.network_isolation,
            required.network_isolation,
            self.resource_limits,
            required.resource_limits,
        ) && (!required.tool_transparent || self.tool_transparent)
            && (!required.path_fidelity || self.path_fidelity)
            && (!required.enforced_readonly || self.enforced_readonly)
            && (!required.enforced_network_allowlist || self.enforced_network_allowlist)
            && (!required.custom_rootfs || self.custom_rootfs)
            && (!required.package_provisioning || self.package_provisioning)
            && required.control_services.is_subset(&self.control_services)
    }

    /// Whether this provider can keep a real secret outside an arbitrary
    /// workload while forcing traffic through the substitution boundary.
    /// Neither substitution nor an allowlist alone is custody evidence.
    #[must_use]
    pub const fn supports_secret_egress_without_bypass(&self) -> bool {
        self.secret_egress_substitution && self.enforced_network_allowlist
    }

    /// Fail-closed backend selection (ADR-0021 §8): does this backend meet
    /// **everything** `spec` requires? A router filters candidate providers by this
    /// before applying any load/region/affinity policy, so a spec is never placed on
    /// a backend that cannot honor it.
    ///
    /// Matches the two load-bearing axes the vocabulary makes selectable: isolation
    /// class (the provider must *meet or exceed* the requested minimum) and network
    /// isolation (required for anything stricter than
    /// [`NetworkPolicy::Unrestricted`](crate::vocab::NetworkPolicy::Unrestricted)).
    #[must_use]
    pub fn satisfies(&self, spec: &crate::spec::SandboxSpec) -> bool {
        self.satisfies_requirements(&SandboxRequirements::from_spec(spec, false))
    }
}

/// Representation-free admission kernel shared by production provider selection
/// and the bounded proof harnesses. Every load-bearing requirement is conjunctive:
/// adding a requirement can only remove candidates, never make a weaker backend
/// admissible.
#[must_use]
pub const fn capability_requirements_satisfied(
    actual_isolation: IsolationClass,
    required_isolation: IsolationClass,
    has_network_isolation: bool,
    requires_network_isolation: bool,
    has_resource_limits: bool,
    requires_resource_limits: bool,
) -> bool {
    isolation_rank(actual_isolation) >= isolation_rank(required_isolation)
        && (!requires_network_isolation || has_network_isolation)
        && (!requires_resource_limits || has_resource_limits)
}

const fn isolation_rank(class: IsolationClass) -> u8 {
    match class {
        IsolationClass::Workdir => 0,
        IsolationClass::Namespace => 1,
        IsolationClass::Container => 2,
    }
}

/// Whether placing below the requested floor is explicitly authorized. This is
/// kept separate from readiness/ranking so a fail-closed policy can never silently
/// turn into a downgrade while candidate ordering changes.
#[must_use]
pub const fn degradation_is_authorized(
    actual: IsolationClass,
    required: IsolationClass,
    on_unmet: OnUnmet,
) -> bool {
    isolation_rank(actual) >= isolation_rank(required)
        || matches!(on_unmet, OnUnmet::DegradeWithConsent)
}

/// Why no backend could be selected for a spec.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectionError {
    /// No configured backend both satisfies the spec and passed its readiness probe.
    #[error("no configured backend can satisfy the requested isolation/network/limits")]
    NoCapableBackend,
}

/// Fail-closed provider selection: the first candidate whose capabilities
/// [`satisfies`](SandboxCapabilities::satisfies) the spec **and** whose
/// [`probe_ready`](SandboxProvider::probe_ready) check passes. It never downgrades to
/// a weaker tier — if nothing qualifies it returns [`SelectionError::NoCapableBackend`]
/// (the host maps this to a `Gated` outcome), so a spec is never silently placed on an
/// under-isolating or unavailable backend. This is the deliberate divergence from a
/// "degrade to a portable scope" policy: in a managed/multi-tenant plane a silent
/// isolation downgrade is a security regression, not a convenience.
pub async fn select_provider<'a>(
    candidates: &'a [Box<dyn SandboxProvider>],
    spec: &SandboxSpec,
) -> Result<&'a dyn SandboxProvider, SelectionError> {
    for provider in candidates {
        if provider.capabilities().satisfies(spec) && provider.probe_ready().await.is_ok() {
            return Ok(provider.as_ref());
        }
    }
    Err(SelectionError::NoCapableBackend)
}

/// What to do when no configured backend meets the isolation floor (ADR-0056 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUnmet {
    /// Never place below the floor — a silent isolation downgrade is a security
    /// regression, so an unmet floor fails closed (the never-downgrade default).
    FailClosed,
    /// Place on the strongest available weaker tier, but ONLY as a *recorded*
    /// degradation: the caller must emit the audit event + metric + run marker
    /// ([`PolicySelection::degraded_to`]). Degradation becomes representable and
    /// logged, never invisible.
    DegradeWithConsent,
}

/// The isolation floor as a policy input, so one selection mechanism serves two trust
/// models (ADR-0056 §5): local single-user (`require = Workdir`, soft) and multi-tenant
/// hosting (`require = Namespace|Container`, `on_unmet = FailClosed`). The floor is a
/// parameter, not a hardcoded default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationPolicy {
    /// The minimum isolation to place on; a weaker backend is used only under
    /// [`OnUnmet::DegradeWithConsent`].
    pub require: IsolationClass,
    /// The preferred isolation when several qualify — the exact-`prefer` tier wins a
    /// tie, else the strongest floor-meeting tier is chosen.
    pub prefer: IsolationClass,
    /// How to handle a spec no backend can place at or above `require`.
    pub on_unmet: OnUnmet,
}

/// The outcome of a policy-driven selection: the chosen provider, and — when the floor
/// could not be met and [`OnUnmet::DegradeWithConsent`] allowed it — the weaker
/// isolation class actually placed on. `degraded_to = Some(..)` obliges the caller to
/// emit the degradation audit event + metric + run marker (never silent).
pub struct PolicySelection<'a> {
    pub provider: &'a dyn SandboxProvider,
    pub degraded_to: Option<IsolationClass>,
}

/// Metadata handed to the one injected checkpoint object adapter. It contains
/// no storage URL, credential, or encryption material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointObjectMetadata {
    /// Workspace ownership scope used by hosted adapters to resolve the tenant
    /// through their existing placement authority. It is not a storage key.
    pub workspace_id: String,
    pub session_id: String,
    pub generation_id: String,
    pub suspend_effect_id: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
}

/// Result of an atomic object write. The adapter must expose the digest of the
/// exact durable plaintext so providers can verify reads after process loss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCheckpointObject {
    pub id: String,
    pub digest: String,
    pub size_bytes: u64,
}

/// Region/deployment-owned checkpoint byte custody. A filesystem adapter is
/// suitable for standalone deployments; hosted composition injects encrypted
/// object storage. This is a byte port, not a lifecycle state store.
///
/// `put` is one exact-operation, atomic idempotency boundary. Its logical key is
/// `(workspace_id, session_id, generation_id, suspend_effect_id)`. The first
/// successful call durably binds that key to the complete metadata and bytes.
/// An exact retry, including one after response loss or process restart, must
/// return the same [`StoredCheckpointObject`] without writing a second object.
/// A concurrent or later call with the same key but any different metadata or
/// bytes must fail without overwriting either version. Implementations must
/// preserve this behavior across processes; an in-memory deduplication cache is
/// not sufficient. `delete` is idempotent for an already-absent exact object.
#[async_trait]
pub trait SandboxCheckpointStore: Send + Sync {
    async fn put(
        &self,
        metadata: &CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<StoredCheckpointObject, SandboxError>;

    async fn get(&self, id: &str) -> Result<Vec<u8>, SandboxError>;

    async fn delete(&self, id: &str) -> Result<(), SandboxError>;
}

/// Exact, provider-neutral request for one idempotent filesystem checkpoint.
///
/// Session lifecycle types deliberately do not cross this port. The Runtime
/// adapter projects its operation/generation into these immutable facts and
/// later wraps the returned artifact in a Session-owned receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxCheckpointRequest {
    pub workspace_id: String,
    pub session_id: String,
    pub generation_id: String,
    pub environment_fingerprint: String,
    pub base_image_fingerprint: String,
    pub effect_id: String,
    pub format: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub max_bytes: u64,
}

/// Opaque, secret-free evidence for one verified durable checkpoint object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCheckpointRef {
    pub id: String,
    pub format: String,
    pub digest: String,
    pub size_bytes: u64,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub environment_fingerprint: String,
    pub base_image_fingerprint: String,
    #[serde(default)]
    pub excluded_mounts: Vec<String>,
    pub suspend_effect_id: String,
}

impl SandboxCheckpointRef {
    #[must_use]
    pub const fn expired_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }
}

// A backend honors the spec's non-isolation requirements (network) — the isolation
// floor is decided by the policy, so it is checked separately here.
fn non_isolation_ok(caps: &SandboxCapabilities, spec: &SandboxSpec) -> bool {
    let network_ok =
        matches!(spec.network, crate::vocab::NetworkPolicy::Unrestricted) || caps.network_isolation;
    // Resource caps are load-bearing like isolation: a spec asking for cgroup limits
    // must not be placed on a tier that cannot enforce them, even under a degrade.
    let limits_ok = !spec.limits.is_set() || caps.resource_limits;
    network_ok && limits_ok
}

/// Fail-closed provider selection with an explicit **policy floor** (ADR-0056 §5). It
/// first places on the strongest ready backend that meets `policy.require` (the exact
/// `prefer` class winning a tie). If none meets the floor, `on_unmet` decides: `FailClosed`
/// returns [`SelectionError::NoCapableBackend`] (the never-downgrade guarantee);
/// `DegradeWithConsent` places on the strongest ready backend *below* the floor and
/// reports `degraded_to` so the caller records the degradation. A spec is never silently
/// placed below its floor.
pub async fn select_provider_with_policy<'a>(
    candidates: &'a [Box<dyn SandboxProvider>],
    spec: &SandboxSpec,
    policy: &IsolationPolicy,
) -> Result<PolicySelection<'a>, SelectionError> {
    // Ready backends meeting the floor (isolation >= require) and the spec's network.
    let mut at_or_above: Vec<&dyn SandboxProvider> = Vec::new();
    // Ready backends below the floor but network-sound — the degrade candidates.
    let mut below: Vec<&dyn SandboxProvider> = Vec::new();
    for provider in candidates {
        let caps = provider.capabilities();
        if !non_isolation_ok(&caps, spec) || provider.probe_ready().await.is_err() {
            continue;
        }
        if caps.isolation >= policy.require {
            at_or_above.push(provider.as_ref());
        } else if degradation_is_authorized(caps.isolation, policy.require, policy.on_unmet) {
            below.push(provider.as_ref());
        }
    }

    if !at_or_above.is_empty() {
        // Prefer the exact `prefer` tier, else the strongest available.
        let chosen = at_or_above
            .iter()
            .find(|p| p.capabilities().isolation == policy.prefer)
            .copied()
            .unwrap_or_else(|| {
                *at_or_above
                    .iter()
                    .max_by_key(|p| p.capabilities().isolation)
                    .expect("non-empty")
            });
        return Ok(PolicySelection {
            provider: chosen,
            degraded_to: None,
        });
    }

    match policy.on_unmet {
        OnUnmet::FailClosed => Err(SelectionError::NoCapableBackend),
        OnUnmet::DegradeWithConsent => below
            .iter()
            .max_by_key(|p| p.capabilities().isolation)
            .map(|p| PolicySelection {
                provider: *p,
                degraded_to: Some(p.capabilities().isolation),
            })
            .ok_or(SelectionError::NoCapableBackend),
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn isolation(tag: u8) -> IsolationClass {
        match tag % 3 {
            0 => IsolationClass::Workdir,
            1 => IsolationClass::Namespace,
            _ => IsolationClass::Container,
        }
    }

    #[kani::proof]
    fn sandbox_admission_never_weakens_the_isolation_floor() {
        let actual = isolation(kani::any());
        let required = isolation(kani::any());
        let admitted = capability_requirements_satisfied(
            actual,
            required,
            kani::any(),
            kani::any(),
            kani::any(),
            kani::any(),
        );
        if admitted {
            assert!(isolation_rank(actual) >= isolation_rank(required));
        }
    }

    #[kani::proof]
    fn sandbox_admission_requires_every_requested_capability() {
        let has_network = kani::any();
        let needs_network = kani::any();
        let has_limits = kani::any();
        let needs_limits = kani::any();
        let admitted = capability_requirements_satisfied(
            isolation(kani::any()),
            IsolationClass::Workdir,
            has_network,
            needs_network,
            has_limits,
            needs_limits,
        );
        if admitted {
            assert!(!needs_network || has_network);
            assert!(!needs_limits || has_limits);
        }
    }

    #[kani::proof]
    fn fail_closed_sandbox_policy_never_authorizes_a_downgrade() {
        let actual = isolation(kani::any());
        let required = isolation(kani::any());
        if degradation_is_authorized(actual, required, OnUnmet::FailClosed) {
            assert!(isolation_rank(actual) >= isolation_rank(required));
        }
    }
}

/// Realizes environments. The local impl lives in `awaken-sandbox-local`; a
/// remote/container impl lives in a distributed repo and plugs in here.
#[async_trait]
pub trait SandboxProvider: Send + Sync {
    /// What this backend can enforce (probed at startup for selection).
    fn capabilities(&self) -> SandboxCapabilities;

    /// Portable filesystem checkpoint formats this concrete provider fully
    /// implements. Worker composition projects these into the existing
    /// `WorkerManifest.checkpoint_formats` authority; no second capability field
    /// is maintained on `SandboxCapabilities`.
    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    /// A cheap liveness probe run at selection time: `Ok` iff this backend is
    /// actually usable *right now* — bwrap/unprivileged-userns available, a container
    /// daemon reachable, etc. The default assumes readiness; the namespace/container
    /// providers override it with a real check so `select_provider` fails closed
    /// (never a silent unisolated run) rather than deferring the failure to `create`.
    async fn probe_ready(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Realize a validated spec into a live sandbox (bind mounts, apply ro/env/
    /// network/limits). Callers should validate first via `prepare_environment`.
    async fn create(&self, spec: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError>;

    /// Realize one Sandbox only after validating the aggregate-owned physical
    /// effect fence. Durable Session creation/rebuild must use this edge; the
    /// legacy method remains available only to explicitly unfenced callers.
    async fn create_for_effect(
        &self,
        _spec: &SandboxSpec,
        _effect_fence: &SandboxEffectFence,
    ) -> Result<Box<dyn Sandbox>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement fenced creation",
        ))
    }

    /// Observe one exact durable handle without renewing, mounting, deleting,
    /// or otherwise adopting it. Out-of-tree providers fail closed until they
    /// can preserve backend `NotFound`/terminal evidence separately from
    /// transport, authorization, and timeout errors.
    async fn observe(&self, _handle: &SandboxHandle) -> Result<SandboxObservation, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement effect-free adoption observation",
        ))
    }

    /// Re-observe one exact handle at an aggregate-authorized physical-effect
    /// boundary. Providers must validate the complete fence together with
    /// their immutable realization evidence; the default cannot safely infer
    /// that support from the legacy observation port.
    async fn observe_for_effect(
        &self,
        _spec: &SandboxSpec,
        _handle: &SandboxHandle,
        _effect_fence: &SandboxEffectFence,
    ) -> Result<SandboxObservation, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement fenced observation",
        ))
    }

    /// Reconnect to an already-realized sandbox from a persisted [`SandboxHandle`]
    /// — the recovery path after a host restart, and the takeover path across
    /// hosts. For a local backend this re-opens the directory; for a remote one it
    /// rebuilds a client against the still-running pod/container.
    async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError>;

    /// Adopt only after revalidating the exact aggregate-owned effect fence.
    /// The legacy port remains available for explicitly unfenced callers; a
    /// durable Session must use this fail-closed edge.
    async fn adopt_for_effect(
        &self,
        _spec: &SandboxSpec,
        _handle: &SandboxHandle,
        _effect_fence: &SandboxEffectFence,
    ) -> Result<Box<dyn Sandbox>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement fenced adoption",
        ))
    }

    /// Reconstruct the same Sandbox lifecycle owner solely to finish an exact
    /// terminal physical realization. Unlike ordinary adoption this must not
    /// renew a lease, launch a resident process, or reinterpret absence as a
    /// live Sandbox. A persisted `handle` proves an already-published physical
    /// incarnation. An in-flight restore has no aggregate binding yet, so its
    /// exact prior effect fence is instead supplied as immutable evidence while
    /// `terminal_effect_fence` remains the sole mutation authority. Returning
    /// `None` is permitted only for provider-proved exact absence or a durable
    /// Removed tombstone; the default is fail closed.
    async fn prepare_terminal_for_effect(
        &self,
        _spec: &SandboxSpec,
        _handle: Option<&SandboxHandle>,
        _expected_effect_fence: Option<&SandboxEffectFence>,
        _terminal_effect_fence: &SandboxEffectFence,
    ) -> Result<Option<Box<dyn Sandbox>>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement fenced terminal authorization",
        ))
    }

    /// Acquire the one physical target for an aggregate-projected exact restore
    /// without reading or interpreting checkpoint bytes.
    async fn acquire_restore(
        &self,
        _spec: &SandboxSpec,
        _request: &SandboxRestoreRequest,
    ) -> Result<SandboxRestoreTarget<Box<dyn Sandbox>>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement exact restore target acquisition",
        ))
    }

    /// Complete one exact restore and return canonical evidence only after the
    /// checkpoint bytes have been materialized and verified.
    async fn restore(
        &self,
        _spec: &SandboxSpec,
        _request: &SandboxRestoreRequest,
        _store: &dyn SandboxCheckpointStore,
    ) -> Result<SandboxRestoreResult<Box<dyn Sandbox>>, SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement checkpoint restore",
        ))
    }

    /// Remove only the unpublished physical target named by one durable
    /// restoring tuple. Provider-proved absence is an idempotent success.
    async fn dispose_restored(
        &self,
        _spec: &SandboxSpec,
        _request: &SandboxRestoreRequest,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "sandbox provider does not implement exact restored-target disposal",
        ))
    }
}

/// A live sandbox environment. **Execute** (`spawn`), **mount/inject** (`attach`),
/// **retrieve** (`artifacts`/`read_artifact`), and — for sandboxes that outlive the
/// owning host — **reconnect** (`handle`/`process`), **observe** (`status`), and
/// **keep alive** (`renew_lease`). `spawn` is primary and tool-transparent: the
/// runtime's `RawTool` model is a separate crate's adapter over `spawn`, not a
/// method here.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The environment id (= the spec scope).
    fn id(&self) -> &str;

    /// A durable, serializable reference for reconnecting later (persist this).
    fn handle(&self) -> SandboxHandle;

    /// Persist the complete mutable filesystem before disposal. Implementations
    /// must omit independently governed mounts and credential material, enforce
    /// `max_bytes`, and return only after the object adapter reports durability.
    async fn checkpoint(
        &self,
        _request: &SandboxCheckpointRequest,
        _store: &dyn SandboxCheckpointStore,
    ) -> Result<SandboxCheckpointRef, SandboxError> {
        Err(SandboxError::new(
            "sandbox does not implement filesystem checkpointing",
        ))
    }

    /// Persist a checkpoint only while the aggregate-owned realization fence
    /// still authorizes this exact physical Sandbox. Durable Session
    /// continuation must use this edge: the legacy method cannot prevent an
    /// expired or replaced owner from publishing bytes after root takeover.
    async fn checkpoint_for_effect(
        &self,
        _request: &SandboxCheckpointRequest,
        _store: &dyn SandboxCheckpointStore,
        _effect_fence: &SandboxEffectFence,
    ) -> Result<SandboxCheckpointRef, SandboxError> {
        Err(SandboxError::new(
            "sandbox does not implement fenced filesystem checkpointing",
        ))
    }

    /// Finish and delete one checkpoint upload whose root receipt was lost to
    /// a terminal takeover. `expected_effect_fence` identifies the immutable
    /// suspend participant and may already be expired; it never authorizes a
    /// mutation. Every write-ahead update, exact `put` replay, and idempotent
    /// object deletion is authorized only by the live terminal fence. This is
    /// the checkpoint port's terminal edge, not a second cleanup state machine.
    async fn cleanup_checkpoint_for_terminal(
        &self,
        _request: &SandboxCheckpointRequest,
        _store: &dyn SandboxCheckpointStore,
        _expected_effect_fence: &SandboxEffectFence,
        _terminal_effect_fence: &SandboxEffectFence,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "sandbox does not implement terminal checkpoint cleanup",
        ))
    }

    /// **EXECUTE** — launch any process under OS-enforced isolation. Isolation is
    /// transparent to what the process does inside.
    async fn spawn(&self, command: Command) -> Result<Box<dyn ProcessHandle>, SandboxError>;

    /// **INJECT** — attach a mount after creation (mirrors adding a session
    /// resource). Fails closed when the backend cannot honor the access mode.
    async fn attach(&self, req: MountRequirement) -> Result<RealizedMount, SandboxError>;

    /// **RETRIEVE (list)** — artifacts the agent wrote under the outputs path.
    /// The backend decides how (directory scan / copy-out / volume read).
    async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError>;

    /// **RETRIEVE (read)** — the bytes of one artifact by id.
    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, SandboxError>;

    /// The mounts realized so far — logical refs + content hashes (G3), for audit
    /// and replay.
    fn realized(&self) -> &[RealizedMount];

    /// Reconnect to a process launched earlier in this sandbox, by its id — the
    /// recovery path after a dropped connection or host restart (pair with
    /// [`ProcessHandle::poll`] to learn its outcome idempotently).
    async fn process(&self, process_id: &str) -> Result<Box<dyn ProcessHandle>, SandboxError>;

    /// The sandbox's current lifecycle state — an idempotent query, safe to call
    /// from any host after a reconnect.
    async fn status(&self) -> Result<SandboxStatus, SandboxError>;

    /// Renew the lease (the dead-man's switch). The owner calls this within the
    /// spec's `lease_ttl_secs`; if the owner vanishes and the lease expires, the
    /// backend reaps the sandbox. A local backend implements this as a no-op.
    async fn renew_lease(&self) -> Result<(), SandboxError>;

    /// Tear down the environment. Idempotent; `Durable` mounts persist.
    async fn dispose(&self) -> Result<(), SandboxError>;

    /// Confirm that the Host completed every copy-backed Memory obligation for
    /// this aggregate effect and supplied the provider's exact complete ordered
    /// materialization evidence. Implementations retire Copy guards without
    /// teardown; FUSE guards remain owned by ordinary disposal. This is a
    /// process-local lifecycle acknowledgement, never a durable Memory receipt.
    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &SandboxEffectFence,
        complete_materializations: &[MemoryMaterializationEvidence],
    ) -> Result<(), SandboxError> {
        validate_live_memory_reconciliation_fence(effect_fence)?;
        default_memory_reconciliation_ack(complete_materializations)
    }

    /// Complete every source-dependent disposal participant under one aggregate
    /// effect without deleting the physical realization. Implementations must
    /// return only after credential write-back and provider-local reconciliation
    /// gates are durable/replayable. The returned fence is the exact durable
    /// provider predecessor, which may predate the caller's fence after a
    /// same-generation response-loss replay. The aggregate persists that
    /// preparation fact before it may invoke [`Self::dispose_for_effect`].
    async fn prepare_disposal_for_effect(
        &self,
        _effect_fence: &SandboxEffectFence,
    ) -> Result<SandboxEffectFence, SandboxError> {
        Err(SandboxError::new(
            "sandbox does not implement fenced disposal preparation",
        ))
    }

    /// Physically tear down the exact realization authorized by one aggregate
    /// effect fence after the aggregate durably accepted disposal preparation.
    /// This edge must not repeat source-dependent Artifact, Memory, credential,
    /// or checkpoint I/O. Implementations revalidate immutable incarnation
    /// evidence at the destructive edge and retain retryable physical lifecycle
    /// guards when removal or dependency release fails.
    async fn dispose_for_effect(
        &self,
        _authorization: &SandboxDisposalAuthorization,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::new(
            "sandbox does not implement fenced disposal",
        ))
    }
}

/// A handle to a process launched by [`Sandbox::spawn`]. Lifecycle only — piped
/// stdio (for a protocol bridge such as ACP) is exposed by the provider's own
/// handle type, so the neutral contract needn't bind an async-IO abstraction.
#[async_trait]
pub trait ProcessHandle: Send + Sync {
    /// Provider-assigned process id.
    fn id(&self) -> &str;

    /// Await exit. Over a lossy transport this connection may drop mid-run; treat a
    /// transport error as "unknown" and re-establish via [`Sandbox::process`] +
    /// [`ProcessHandle::poll`] rather than assuming failure.
    async fn wait(&self) -> Result<ExitStatus, SandboxError>;

    /// Non-blocking, idempotent status: `None` while still running, `Some(status)`
    /// once exited. Safe to call repeatedly from any host after a reconnect — this
    /// is how you resolve an indeterminate outcome without re-running the process.
    async fn poll(&self) -> Result<Option<ExitStatus>, SandboxError>;

    /// Deliver a signal (terminate/kill/interrupt). This operation is idempotent:
    /// if the owned process exits before or during delivery, implementations return
    /// success after confirming that exit. Tearing down the sandbox reaps the whole
    /// process group regardless.
    async fn signal(&self, signal: Signal) -> Result<(), SandboxError>;
}

/// How a launched process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    /// Exit code, when it exited normally.
    pub code: Option<i32>,
    /// True when terminated by a signal.
    pub signaled: bool,
}

/// A signal to deliver to a launched process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Graceful terminate (SIGTERM).
    Term,
    /// Force kill (SIGKILL).
    Kill,
    /// Interrupt (SIGINT).
    Int,
}

#[cfg(test)]
mod terminal_memory_reconciliation_contract_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn disposal_authorization_requires_a_distinct_authorized_successor() {
        // Cause/effect graph: C1 a provider participant was prepared under an
        // exact fence; C2 the physical caller presents the same operation, a
        // same-lease successor, a higher-epoch successor, an older epoch, or a
        // foreign realization; C3 the aggregate preparation fingerprint is
        // present/empty. Effects: E1 the raw preparation cannot authorize
        // deletion; E2 same-lease or monotonic-epoch successors produce one
        // typed predecessor→successor projection; E3 stale/foreign/empty facts
        // fail before provider mutation.
        //
        // | Rule | successor | preparation fingerprint | Effect |
        // | D1 | same operation | exact | reject / E1 |
        // | D2 | distinct, same lease | exact | admit / E2 |
        // | D3 | distinct, higher epoch | exact | admit / E2 |
        // | D4 | older or foreign | exact | reject / E3 |
        // | D5 | authorized | empty | reject / E3 |
        let prepared =
            SandboxEffectFence::new("prepare", "owner-a", "runtime-a", 4, 40_000).unwrap();
        let same = prepared.clone();
        let preparation = SandboxDisposalPreparation::new(prepared.clone(), "prepared").unwrap();
        let disposal_id = preparation.operation_id().unwrap();
        let same_lease =
            SandboxEffectFence::new(disposal_id.clone(), "owner-a", "runtime-a", 4, 50_000)
                .unwrap();
        let higher =
            SandboxEffectFence::new(disposal_id.clone(), "owner-b", "runtime-b", 5, 60_000)
                .unwrap();
        let older = SandboxEffectFence::new(disposal_id.clone(), "owner-a", "runtime-a", 3, 60_000)
            .unwrap();
        let foreign =
            SandboxEffectFence::new(disposal_id, "owner-b", "runtime-b", 4, 60_000).unwrap();

        assert!(preparation.authorize(same).is_err(), "D1/E1");
        for (rule, successor) in [("D2", same_lease.clone()), ("D3", higher.clone())] {
            let authorization = preparation.authorize(successor.clone()).expect(rule);
            assert_eq!(
                authorization.prepared_effect_fence(),
                &prepared,
                "{rule}/E2"
            );
            assert_eq!(authorization.effect_fence(), &successor, "{rule}/E2");
        }
        for (rule, successor) in [("D4 older", older), ("D4 foreign", foreign)] {
            assert!(preparation.authorize(successor).is_err(), "{rule}/E3");
        }
        assert!(
            SandboxDisposalPreparation::new(prepared, " ").is_err(),
            "D5/E3"
        );
    }

    #[test]
    fn disposal_authorization_wire_is_canonically_validated() {
        // Wire cause/effect table D6. Causes: C1 the serialized authorization
        // carries exact A/fingerprint/B, a B operation not derived from A+fp,
        // a same-epoch foreign B, an empty fingerprint, or an unknown field.
        // Effects: E1 only the exact value reconstructs through the canonical
        // value-object owner; E2 every malformed combination fails during
        // deserialization, before any provider can observe or mutate a Sandbox.
        //
        // | Rule | A/fingerprint/B relation | shape | Effect |
        // |---|---|---|---|
        // | D6a | exact | closed | E1 round-trip |
        // | D6b | B operation unbound | closed | E2 reject |
        // | D6c | B foreign at same epoch | closed | E2 reject |
        // | D6d | empty fingerprint | closed | E2 reject |
        // | D6e | exact | unknown field | E2 reject |
        let prepared =
            SandboxEffectFence::new("prepare", "owner-a", "runtime-a", 4, 40_000).unwrap();
        let fingerprint = "prepared";
        let preparation = SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
        let disposal_id = preparation.operation_id().unwrap();
        let successor =
            SandboxEffectFence::new(disposal_id, "owner-a", "runtime-a", 4, 50_000).unwrap();
        let authorization = preparation.authorize(successor).unwrap();
        let exact = serde_json::to_value(&authorization).unwrap();
        assert_eq!(
            serde_json::from_value::<SandboxDisposalAuthorization>(exact.clone()).unwrap(),
            authorization,
            "D6a/E1"
        );

        let mut unbound = exact.clone();
        unbound["effect_fence"]["operation_id"] = serde_json::json!("foreign-operation");
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(unbound).is_err(),
            "D6b/E2"
        );

        let mut foreign = exact.clone();
        foreign["effect_fence"]["owner"] = serde_json::json!("owner-b");
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(foreign).is_err(),
            "D6c/E2"
        );

        let mut empty_fingerprint = exact.clone();
        empty_fingerprint["preparation_fingerprint"] = serde_json::json!(" ");
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(empty_fingerprint).is_err(),
            "D6d/E2"
        );

        let mut unknown = exact;
        unknown["parallel_authority"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<SandboxDisposalAuthorization>(unknown).is_err(),
            "D6e/E2"
        );
    }

    #[test]
    fn default_acknowledgement_fails_closed_when_copy_evidence_exists() {
        // Provider-default cause/effect table: C1 the caller supplies no copy
        // materialization evidence / one or more exact copy materializations.
        // R1 no evidence is a provider-neutral no-op because FUSE writes through;
        // R2 any non-empty evidence fails closed so an out-of-tree provider cannot
        // silently claim that Host-owned terminal CAS retired its live copy guard.
        assert!(default_memory_reconciliation_ack(&[]).is_ok(), "R1");
        let evidence = MemoryMaterializationEvidence::new(
            "store",
            "/memory",
            vec![MemoryMaterializationHead {
                path: "value".into(),
                id: "head".into(),
                content_sha256: "digest".into(),
            }],
        )
        .unwrap();
        assert!(
            default_memory_reconciliation_ack(&[evidence]).is_err(),
            "R2"
        );
    }

    #[test]
    fn shared_ack_kernel_is_exact_effect_scoped_and_response_loss_safe() {
        // Shared-kernel decision table: C1 expected/supplied evidence is exact
        // including canonical order; C2 fence is live; C3 state is empty / same
        // effect / same realization lease with another operation / foreign lease.
        // K1 !C1 or !C2 rejects before the provider closure; K2 exact+live+empty
        // runs it once and records the complete effect identity; K3 a same-effect
        // replay succeeds without rerunning it; K4 a same-lease successor effect
        // reruns provider authorization/drain and rebinds the acknowledgement;
        // K5 a different or higher lease rejects. The disposal gate admits only
        // the latest exact K2/K4 effect. Empty evidence validates but records no
        // claim because it says nothing about FUSE teardown.
        let first = MemoryMaterializationEvidence::new(
            "store-a",
            "/a",
            vec![MemoryMaterializationHead {
                path: "value".into(),
                id: "head-a".into(),
                content_sha256: "digest-a".into(),
            }],
        )
        .unwrap();
        let second = MemoryMaterializationEvidence::new(
            "store-b",
            "/b",
            vec![MemoryMaterializationHead {
                path: "value".into(),
                id: "head-b".into(),
                content_sha256: "digest-b".into(),
            }],
        )
        .unwrap();
        let expected = vec![first.clone(), second.clone()];
        let reversed = vec![second, first];
        let live = SandboxEffectFence::new("effect", "owner", "runtime", 2, u64::MAX).unwrap();
        let expired = SandboxEffectFence::new("effect", "owner", "runtime", 2, 0).unwrap();
        let takeover =
            SandboxEffectFence::new("terminal", "owner", "runtime", 2, u64::MAX).unwrap();
        let foreign =
            SandboxEffectFence::new("foreign", "other-owner", "runtime", 2, u64::MAX).unwrap();
        let higher = SandboxEffectFence::new("higher", "owner", "runtime", 3, u64::MAX).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let ack = MemoryReconciliationAck::default();

        for (rule, fence, supplied) in [
            ("K1 order", &live, reversed.as_slice()),
            ("K1 expired", &expired, expected.as_slice()),
        ] {
            let calls = calls.clone();
            assert!(
                ack.acknowledge(fence, &expected, supplied, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .is_err(),
                "{rule}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "K1");

        ack.acknowledge(&live, &expected, &expected, {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .expect("K2");
        ack.acknowledge(&live, &expected, &expected, {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .expect("K3");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "K2/K3");

        ack.acknowledge(&takeover, &expected, &expected, {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .expect("K4");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "K4 reauthorizes");
        assert!(
            ack.require_for_disposal(&expected, &live).is_err(),
            "K4 supersedes the old operation"
        );
        ack.require_for_disposal(&expected, &takeover)
            .expect("K4 latest gate");

        assert!(
            ack.acknowledge(&foreign, &expected, &expected, || Ok(()))
                .is_err(),
            "K5 foreign owner"
        );
        assert!(
            ack.acknowledge(&higher, &expected, &expected, || Ok(()))
                .is_err(),
            "K5 higher epoch"
        );
        assert!(
            ack.require_for_disposal(&expected, &foreign).is_err(),
            "K5 gate"
        );

        let empty = MemoryReconciliationAck::default();
        empty
            .acknowledge(&live, &[], &[], || {
                panic!("empty evidence has no Copy drain")
            })
            .expect("empty evidence");
        empty
            .require_for_disposal(&[], &foreign)
            .expect("empty evidence leaves FUSE to ordinary disposal");
    }
}
