//! Container provider payload codec still hosted by the contract.

use serde::{Deserialize, Serialize};

use super::super::SandboxError;
use super::super::restore_wire::HostBindRestorationHandle;
use super::{SandboxHandle, SandboxPayloadSchema};

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

const CONTAINER: &str = "container";

impl SandboxHandle {
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
