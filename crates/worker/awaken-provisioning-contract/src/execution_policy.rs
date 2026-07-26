//! Versioned sandbox execution policy and exact Environment binding.

use async_trait::async_trait;
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxExecutionPolicy {
    pub id: SandboxExecutionPolicyId,
    pub version: SandboxExecutionPolicyVersion,
    pub config: SandboxOverride,
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
    #[error("sandbox_execution_policy_binding_unavailable")]
    BindingUnavailable,
    #[error("sandbox_execution_policy_invalid: {0}")]
    Invalid(String),
    #[error("sandbox_execution_policy_store_failed: {0}")]
    StoreFailed(String),
}

/// Authoritative store for immutable policy versions and the Environment's exact
/// binding. Implementations must never substitute the current policy version.
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

    async fn bind_environment(
        &self,
        environment_id: &str,
        reference: SandboxExecutionPolicyRef,
    ) -> Result<(), SandboxExecutionPolicyError>;

    async fn environment_binding(
        &self,
        environment_id: &str,
    ) -> Result<Option<SandboxExecutionPolicyRef>, SandboxExecutionPolicyError>;
}
