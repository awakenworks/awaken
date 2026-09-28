// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Static owner: the container provider's own durable handle payload codec (container
// @formal reason: locator, Kubernetes Pod/claim continuation, HostBind restore locator, control
// @formal reason: incarnation) moved out of the neutral provisioning contract unchanged. Dynamic path:
// @formal reason: accessors decode only the container schema, fail closed on another provider or malformed
// @formal reason: payload, and keep the historical physical-incarnation and adoption-fingerprint
// @formal reason: decisions; historical-wire decision tables qualify the bytes. It adds no adoption,
// @formal reason: disposal or authorization decision and is not a direct formal-proof claim.
//! Durable handle payloads owned by the container provider (Docker, Podman and
//! Kubernetes runtimes).
//!
//! The neutral [`pc::SandboxHandle`] carries these payloads opaquely; this
//! module is their only codec. The wire shapes are the historical ones: the
//! schema tag is `container_v1`/`container_v2`, and V2 records the neutral
//! realization evidence (`realization_fingerprint`, `owned_paths`) at the top
//! level of the payload beside the container locator and adoption fingerprint.

use std::collections::BTreeSet;

use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServiceKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSandboxHandleV1 {
    pub container_id: String,
    pub outputs_path: String,
    pub base_env: Vec<pc::EnvVar>,
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
    pub sandbox_control_incarnation: Option<SandboxControlIncarnation>,
    /// Exact provider control topology realized alongside the incarnation.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub control_services: BTreeSet<SandboxControlServiceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSandboxHandleV2 {
    pub previous: ContainerSandboxHandleV1,
    /// Pure provider-effective configuration identity recomputed before any
    /// backend observation or adoption I/O. The physical realization fingerprint
    /// below extends this value with facts resolved during creation (for example
    /// an immutable package-image digest).
    pub adoption_fingerprint: pc::SandboxRealizationFingerprint,
    pub realization_fingerprint: pc::SandboxRealizationFingerprint,
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

/// Provider-owned host locator accepted by Phase-A readers but not constructible
/// through the typed API until the restoring composition enables its sole writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HostBindRestorationHandle {
    staging_root: String,
}

impl HostBindRestorationHandle {
    /// Construct the exact host-owned staging locator for a Phase-B physical
    /// restore target. The provider must still bind restore evidence separately.
    pub fn for_restore(staging_root: impl Into<String>) -> Result<Self, pc::SandboxError> {
        let staging_root = staging_root.into();
        let path = std::path::Path::new(&staging_root);
        if staging_root.is_empty() || !path.is_absolute() {
            return Err(pc::SandboxError::invalid(
                "host-bind restoration root must be an absolute path",
            ));
        }
        Ok(Self { staging_root })
    }

    #[must_use]
    pub fn staging_root(&self) -> &str {
        &self.staging_root
    }
}

/// Validated Kubernetes Pod UID used as durable incarnation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct KubernetesPodUid(String);

impl KubernetesPodUid {
    pub fn new(value: impl Into<String>) -> Result<Self, pc::SandboxError> {
        let value = value.into();
        if value.trim().is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            return Err(pc::SandboxError::invalid("Kubernetes Pod UID is invalid"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for KubernetesPodUid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Durable evidence for the exact runtime object allowed to host a Sandbox
/// control endpoint. The closed enum prevents adapters from inventing
/// unverified opaque identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlIncarnation {
    KubernetesPod { uid: KubernetesPodUid },
}

impl SandboxControlIncarnation {
    pub fn kubernetes_pod(uid: impl Into<String>) -> Result<Self, pc::SandboxError> {
        Ok(Self::KubernetesPod {
            uid: KubernetesPodUid::new(uid)?,
        })
    }

    #[must_use]
    pub fn kubernetes_pod_uid(&self) -> Option<&str> {
        match self {
            Self::KubernetesPod { uid } => Some(uid.as_str()),
        }
    }
}

const CONTAINER: &str = "container";

mod sealed {
    pub trait Sealed {}
    impl Sealed for awaken_provisioning_contract::SandboxHandle {}
}

/// Typed codec for the container provider's durable handle payloads.
///
/// Implemented only for [`pc::SandboxHandle`]; import it to construct or read
/// a container handle. Every accessor rejects another provider's schema and
/// any payload that does not decode into this provider's exact type.
pub trait ContainerHandleCodec: sealed::Sealed + Sized {
    fn container(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV1) -> Self;

    fn container_v2(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV2) -> Self;

    /// The container locator of a `container_v1`/`container_v2` handle.
    fn container_payload(&self) -> Result<ContainerSandboxHandleV1, pc::SandboxError>;

    /// Exact immutable physical incarnation used by an authorized container
    /// rebuild. Docker and Podman use their immutable container id. Kubernetes
    /// uses the Pod UID persisted by the current continuation handle; the
    /// legacy claim-only shape deliberately fails closed.
    fn container_physical_incarnation(&self) -> Result<String, pc::SandboxError>;

    /// Provider-effective configuration identity required to adopt a current
    /// container handle. V1 remains continuity-only and returns `None`; callers
    /// must not synthesize current evidence from its partial payload.
    fn container_adoption_fingerprint(
        &self,
    ) -> Result<Option<pc::SandboxRealizationFingerprint>, pc::SandboxError>;
}

fn encode(
    sandbox_id: impl Into<String>,
    version: u32,
    payload: &impl Serialize,
) -> pc::SandboxHandle {
    let schema = pc::SandboxPayloadSchema::new(CONTAINER, version)
        .expect("container payload schema is a lowercase identifier with a positive version");
    pc::SandboxHandle::from_provider_payload(sandbox_id, schema, payload)
        .expect("typed container payloads always encode one object with complete evidence")
}

fn container_version(handle: &pc::SandboxHandle) -> Option<u32> {
    handle
        .payload_schema()
        .filter(|schema| schema.provider() == CONTAINER)
        .map(pc::SandboxPayloadSchema::version)
}

impl ContainerHandleCodec for pc::SandboxHandle {
    fn container(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV1) -> Self {
        encode(sandbox_id, 1, &payload)
    }

    fn container_v2(sandbox_id: impl Into<String>, payload: ContainerSandboxHandleV2) -> Self {
        encode(sandbox_id, 2, &payload)
    }

    fn container_payload(&self) -> Result<ContainerSandboxHandleV1, pc::SandboxError> {
        match container_version(self) {
            Some(1) => self.decode_provider_payload(),
            Some(2) => self
                .decode_provider_payload::<ContainerSandboxHandleV2>()
                .map(|payload| payload.previous),
            _ => Err(self.provider_mismatch(CONTAINER)),
        }
    }

    fn container_physical_incarnation(&self) -> Result<String, pc::SandboxError> {
        let payload = match container_version(self) {
            Some(2) => {
                self.decode_provider_payload::<ContainerSandboxHandleV2>()?
                    .previous
            }
            Some(1) => {
                return Err(pc::SandboxError::unsupported(
                    "legacy container handle has no current realization evidence",
                ));
            }
            _ => return Err(self.provider_mismatch(CONTAINER)),
        };
        match payload.runtime_handle {
            Some(ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, .. }) => {
                if pod_uid.trim().is_empty() {
                    Err(pc::SandboxError::invalid(
                        "Kubernetes continuation handle has an empty Pod incarnation",
                    ))
                } else {
                    Ok(pod_uid)
                }
            }
            Some(ContainerContinuationHandle::KubernetesContinuation { .. }) => {
                Err(pc::SandboxError::unsupported(
                    "legacy Kubernetes handle has no Pod incarnation evidence",
                ))
            }
            Some(ContainerContinuationHandle::HostBindRestoration(_)) => {
                Err(pc::SandboxError::invalid(
                    "host-bind restore handle cannot authorize ordinary container incarnation effects",
                ))
            }
            None if payload.container_id.trim().is_empty() => Err(pc::SandboxError::invalid(
                "container handle has an empty physical incarnation",
            )),
            None => Ok(payload.container_id),
        }
    }

    fn container_adoption_fingerprint(
        &self,
    ) -> Result<Option<pc::SandboxRealizationFingerprint>, pc::SandboxError> {
        match container_version(self) {
            Some(1) => self
                .decode_provider_payload::<ContainerSandboxHandleV1>()
                .map(|_| None),
            Some(2) => self
                .decode_provider_payload::<ContainerSandboxHandleV2>()
                .map(|payload| Some(payload.adoption_fingerprint)),
            _ => Err(self.provider_mismatch(CONTAINER)),
        }
    }
}

#[cfg(test)]
mod tests;
