//! Versioned sandbox execution policy.

use async_trait::async_trait;
use awaken_session_contract::SandboxProvisioning;
use serde::{Deserialize, Serialize};

use crate::SandboxOverride;

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
    pub idle_retention: awaken_session_contract::EnvironmentIdleRetentionPolicy,
    #[serde(default)]
    pub disabled: bool,
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
}
