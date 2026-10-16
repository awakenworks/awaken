// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Pure original-result wire values belong to the same Container codec; both the codec
// @formal reason: and the physical adapter consume this sole definition. No runtime or state service is introduced.
//! Original projected Kubernetes-object wire facts owned by the Container codec.
use awaken_provisioning_contract as pc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KubernetesProjectedKind {
    ConfigMap,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubernetesProjectedObject {
    pub(crate) kind: KubernetesProjectedKind,
    pub(crate) name: String,
    pub(crate) uid: String,
    pub(crate) owner_references_fingerprint: String,
}

/// Provenance of this one physical fact set, not another lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum KubernetesProjectedOrigin {
    Created {
        attempt: String,
    },
    Restoration {
        evidence: pc::SandboxRestorationEvidence,
        plan_fingerprint: String,
    },
    JoinedEmpty {
        attempt: String,
        predecessor: Box<crate::ContainerEffectFence>,
        original_group: String,
    },
    FrozenPlanEmpty {
        adoption: pc::SandboxRealizationFingerprint,
        realization: pc::SandboxRealizationFingerprint,
    },
}

/// Complete original API-object set, including an explicitly complete empty
/// set. No default/optional list: historical omission and partial results cannot
/// decode as absence. This contains no projected bytes or credential values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KubernetesProjectedParticipants {
    pub(crate) namespace: String,
    pub(crate) pod_uid: String,
    pub(crate) objects: Vec<KubernetesProjectedObject>,
    pub(crate) origin: KubernetesProjectedOrigin,
}

impl KubernetesProjectedParticipants {
    fn valid_domain(namespace: &str, pod_uid: &str) -> bool {
        !namespace.is_empty()
            && namespace.trim() == namespace
            && !namespace
                .chars()
                .any(|c| matches!(c, '\r' | '\n' | '\0' | '/' | '?' | '#' | '%'))
            && valid_kubernetes_uid(pod_uid)
    }

    pub(crate) fn new(
        namespace: String,
        pod_uid: String,
        mut objects: Vec<KubernetesProjectedObject>,
        attempt: Option<String>,
        restoration: Option<(pc::SandboxRestorationEvidence, String)>,
    ) -> Result<Self, pc::SandboxError> {
        let exact = |value: &str| {
            !value.is_empty()
                && value.trim() == value
                && !value.chars().any(|c| matches!(c, '\r' | '\n' | '\0' | '/'))
        };
        objects.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
        if !Self::valid_domain(&namespace, &pod_uid)
            || attempt.as_ref().is_some_and(|attempt| !exact(attempt))
            || (attempt.is_some() == restoration.is_some())
            || objects.iter().any(|object| {
                !exact(&object.name)
                    || object.name.chars().any(|c| matches!(c, '?' | '#' | '%'))
                    || !valid_kubernetes_uid(&object.uid)
                    || !exact(&object.owner_references_fingerprint)
            })
            || objects
                .windows(2)
                .any(|pair| pair[0].kind == pair[1].kind && pair[0].name == pair[1].name)
        {
            return Err(pc::SandboxError::invalid(
                "Kubernetes original projected-object set is incomplete or invalid",
            ));
        }
        if restoration.as_ref().is_some_and(|(_, plan)| !exact(plan)) {
            return Err(pc::SandboxError::invalid(
                "Kubernetes original restoration plan is invalid",
            ));
        }
        let origin = if let Some((evidence, plan_fingerprint)) = restoration {
            KubernetesProjectedOrigin::Restoration {
                evidence,
                plan_fingerprint,
            }
        } else {
            KubernetesProjectedOrigin::Created {
                attempt: attempt.expect("origin validation accepted exactly one source"),
            }
        };
        Ok(Self {
            namespace,
            pod_uid,
            objects,
            origin,
        })
    }
    pub(crate) fn joined_empty(
        namespace: String,
        pod_uid: String,
        attempt: String,
        predecessor: crate::ContainerEffectFence,
        original_group: String,
    ) -> Result<Self, pc::SandboxError> {
        if original_group.trim().is_empty() {
            return Err(pc::SandboxError::invalid(
                "joined original group is missing",
            ));
        }
        let mut set = Self::new(namespace, pod_uid, Vec::new(), Some(attempt.clone()), None)?;
        set.origin = KubernetesProjectedOrigin::JoinedEmpty {
            attempt,
            predecessor: Box::new(predecessor),
            original_group,
        };
        Ok(set)
    }

    pub(crate) fn frozen_plan_empty(
        namespace: String,
        pod_uid: String,
        adoption: pc::SandboxRealizationFingerprint,
        realization: pc::SandboxRealizationFingerprint,
    ) -> Result<Self, pc::SandboxError> {
        if !Self::valid_domain(&namespace, &pod_uid) {
            return Err(pc::SandboxError::invalid(
                "Kubernetes frozen-plan empty projection has an invalid domain",
            ));
        }
        Ok(Self {
            namespace,
            pod_uid,
            objects: Vec::new(),
            origin: KubernetesProjectedOrigin::FrozenPlanEmpty {
                adoption,
                realization,
            },
        })
    }
    #[cfg(feature = "k8s")]
    pub(crate) fn attempt(&self) -> Option<&str> {
        match &self.origin {
            KubernetesProjectedOrigin::Created { attempt }
            | KubernetesProjectedOrigin::JoinedEmpty { attempt, .. } => Some(attempt),
            KubernetesProjectedOrigin::Restoration { .. }
            | KubernetesProjectedOrigin::FrozenPlanEmpty { .. } => None,
        }
    }
    pub(crate) fn restoration(&self) -> Option<(&pc::SandboxRestorationEvidence, &str)> {
        match &self.origin {
            KubernetesProjectedOrigin::Restoration {
                evidence,
                plan_fingerprint,
            } => Some((evidence, plan_fingerprint)),
            _ => None,
        }
    }
    pub(crate) fn terminal_only(&self) -> bool {
        matches!(
            self.origin,
            KubernetesProjectedOrigin::JoinedEmpty { .. }
                | KubernetesProjectedOrigin::FrozenPlanEmpty { .. }
        )
    }
}

impl<'de> Deserialize<'de> for KubernetesProjectedParticipants {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            namespace: String,
            pod_uid: String,
            objects: Vec<KubernetesProjectedObject>,
            origin: KubernetesProjectedOrigin,
        }
        let wire = Wire::deserialize(deserializer)?;
        match wire.origin {
            KubernetesProjectedOrigin::Created { attempt } => Self::new(
                wire.namespace,
                wire.pod_uid,
                wire.objects,
                Some(attempt),
                None,
            ),
            KubernetesProjectedOrigin::Restoration {
                evidence,
                plan_fingerprint,
            } => Self::new(
                wire.namespace,
                wire.pod_uid,
                wire.objects,
                None,
                Some((evidence, plan_fingerprint)),
            ),
            KubernetesProjectedOrigin::JoinedEmpty {
                attempt,
                predecessor,
                original_group,
            } => {
                if !wire.objects.is_empty() {
                    return Err(serde::de::Error::custom(
                        "joined-empty witness has projected objects",
                    ));
                }
                Self::joined_empty(
                    wire.namespace,
                    wire.pod_uid,
                    attempt,
                    *predecessor,
                    original_group,
                )
            }
            KubernetesProjectedOrigin::FrozenPlanEmpty {
                adoption,
                realization,
            } => {
                if !wire.objects.is_empty() {
                    return Err(serde::de::Error::custom(
                        "frozen-plan empty witness has projected objects",
                    ));
                }
                Self::frozen_plan_empty(wire.namespace, wire.pod_uid, adoption, realization)
            }
        }
        .map_err(serde::de::Error::custom)
    }
}

pub(crate) fn valid_kubernetes_uid(value: &str) -> bool {
    !value.trim().is_empty()
        && !value
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
}

#[cfg(test)]
mod tests;
