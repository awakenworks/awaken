//! Versioned sandbox execution policy.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{SandboxIdleRetentionPolicy, SandboxOverride, SandboxProvisioning};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxExecutionPolicyId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxExecutionPolicyVersion(pub u64);

impl SandboxExecutionPolicyVersion {
    pub const INITIAL: Self = Self(1);

    pub fn checked_next(self) -> Result<Self, SandboxExecutionPolicyError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| SandboxExecutionPolicyError::Invalid("version exhausted".to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxExecutionPolicy {
    pub id: SandboxExecutionPolicyId,
    pub version: SandboxExecutionPolicyVersion,
    pub config: SandboxOverride,
    #[serde(default)]
    pub provisioning: SandboxProvisioning,
    /// Frozen into the Environment snapshot together with this exact version.
    #[serde(default)]
    pub idle_retention: SandboxIdleRetentionPolicy,
    #[serde(default)]
    pub disabled: bool,
}

impl SandboxExecutionPolicy {
    /// Validate cross-field trust invariants before a version enters any store.
    /// Trusted-host access is an exact execution choice, so combining it with a
    /// Namespace/Container requirement would otherwise admit two contradictory
    /// provider selections.
    pub fn validate(&self) -> Result<(), SandboxExecutionPolicyError> {
        if self.config.host_access == crate::HostAccessPolicy::TrustedHost {
            if self.config.isolation != Some(crate::IsolationClass::Workdir) {
                return Err(SandboxExecutionPolicyError::Invalid(
                    "trusted_host access requires isolation=workdir".into(),
                ));
            }
            if self.config.environment.is_some()
                || self.config.network.is_some()
                || self.config.limits.is_set()
            {
                return Err(SandboxExecutionPolicyError::Invalid(
                    "trusted_host access cannot promise an image, network enforcement, or resource limits"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxExecutionPolicyRef {
    pub id: SandboxExecutionPolicyId,
    pub version: SandboxExecutionPolicyVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SandboxExecutionPolicyError {
    #[error("sandbox_execution_policy_not_found")]
    NotFound,
    #[error("sandbox_execution_policy_version_conflict")]
    VersionConflict,
    #[error("sandbox_execution_policy_disabled")]
    Disabled,
    #[error("sandbox_execution_policy_invalid: {0}")]
    Invalid(String),
    #[error("sandbox_execution_policy_store_failed: {0}")]
    StoreFailed(String),
}

/// Authoritative store for immutable policy versions. Environment owns its exact
/// policy reference inside the same authored revision; implementations must never
/// substitute the current policy version.
#[async_trait]
pub trait SandboxExecutionPolicyStore: Send + Sync {
    async fn create(
        &self,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError>;

    async fn publish(
        &self,
        expected_current: SandboxExecutionPolicyVersion,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError>;

    async fn get_exact(
        &self,
        reference: &SandboxExecutionPolicyRef,
    ) -> Result<SandboxExecutionPolicy, SandboxExecutionPolicyError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_version_exhaustion_fails_closed() {
        assert_eq!(
            SandboxExecutionPolicyVersion(1).checked_next().unwrap(),
            SandboxExecutionPolicyVersion(2)
        );
        assert!(matches!(
            SandboxExecutionPolicyVersion(u64::MAX).checked_next(),
            Err(SandboxExecutionPolicyError::Invalid(message)) if message.contains("exhausted")
        ));
    }

    #[test]
    fn provisioning_wire_defaults_to_eager_and_round_trips_lazy() {
        let historical: SandboxExecutionPolicy = serde_json::from_value(serde_json::json!({
            "id": "policy",
            "version": 1,
            "config": {}
        }))
        .unwrap();
        assert_eq!(historical.provisioning, SandboxProvisioning::Eager);

        let lazy = SandboxExecutionPolicy {
            provisioning: SandboxProvisioning::OnToolUse,
            ..historical
        };
        let wire = serde_json::to_value(&lazy).unwrap();
        assert_eq!(wire["provisioning"], "on_tool_use");
        assert_eq!(
            serde_json::from_value::<SandboxExecutionPolicy>(wire)
                .unwrap()
                .provisioning,
            SandboxProvisioning::OnToolUse
        );
    }

    #[test]
    fn trusted_host_policy_requires_the_exact_workdir_boundary() {
        // Cause/effect graph: C1 host access={isolated,trusted};
        // C2 isolation={absent,workdir,namespace,container}. Effects:
        // E1 isolated preserves every legacy/minimum-isolation policy;
        // E2 trusted+workdir is valid; E3 every other trusted combination is
        // rejected before persistence. The nested loops exhaust the closed
        // discriminants, so this is the complete decision table.
        for host_access in [
            crate::HostAccessPolicy::Isolated,
            crate::HostAccessPolicy::TrustedHost,
        ] {
            for isolation in [
                None,
                Some(crate::IsolationClass::Workdir),
                Some(crate::IsolationClass::Namespace),
                Some(crate::IsolationClass::Container),
            ] {
                let policy = SandboxExecutionPolicy {
                    id: SandboxExecutionPolicyId("policy".into()),
                    version: SandboxExecutionPolicyVersion::INITIAL,
                    config: crate::SandboxOverride {
                        isolation,
                        host_access,
                        ..Default::default()
                    },
                    provisioning: Default::default(),
                    idle_retention: Default::default(),
                    disabled: false,
                };
                assert_eq!(
                    policy.validate().is_ok(),
                    host_access == crate::HostAccessPolicy::Isolated
                        || isolation == Some(crate::IsolationClass::Workdir),
                    "host_access={host_access:?} isolation={isolation:?}"
                );
            }
        }

        // Remaining cause axes are independent once trusted+workdir is fixed:
        // C3 image, C4 network policy, C5 enforceable limit. Any one produces
        // E3 rejection; scheduler-only resource requests remain admitted.
        let base = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("trusted".into()),
            version: SandboxExecutionPolicyVersion::INITIAL,
            config: crate::SandboxOverride {
                isolation: Some(crate::IsolationClass::Workdir),
                host_access: crate::HostAccessPolicy::TrustedHost,
                requests: crate::ResourceRequests {
                    cpu_millis: Some(100),
                    ..Default::default()
                },
                ..Default::default()
            },
            provisioning: Default::default(),
            idle_retention: Default::default(),
            disabled: false,
        };
        assert!(
            base.validate().is_ok(),
            "scheduler request is not a guarantee"
        );
        for invalid in [
            crate::SandboxOverride {
                environment: Some(crate::EnvironmentKind::Image {
                    reference: "example.invalid/image@sha256:00".into(),
                }),
                ..base.config.clone()
            },
            crate::SandboxOverride {
                network: Some(crate::NetworkPolicy::None),
                ..base.config.clone()
            },
            crate::SandboxOverride {
                limits: crate::ResourceLimits {
                    cpu_millis: Some(100),
                    ..Default::default()
                },
                ..base.config.clone()
            },
        ] {
            assert!(
                SandboxExecutionPolicy {
                    config: invalid,
                    ..base.clone()
                }
                .validate()
                .is_err()
            );
        }
    }
}
