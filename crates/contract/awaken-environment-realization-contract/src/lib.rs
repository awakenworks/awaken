//! Coordinator-owned Environment image-realization contract.
//!
//! Control registers exact definitions. This contract starts at the Coordinator
//! boundary: it describes durable build demand, lease transitions, and the
//! builder/readiness ports without naming SQL, Kubernetes, or a container engine.

use async_trait::async_trait;
use awaken_environment_contract::{
    EnvironmentConfig, EnvironmentRevision, ExecutableEnvironmentRegistration,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentImageBuildDemand {
    pub build_key: String,
    pub workspace_id: String,
    pub environment_id: String,
    pub source_revision: EnvironmentRevision,
    pub definition_fingerprint: String,
    pub base_image: String,
    pub config: EnvironmentConfig,
}

impl EnvironmentImageBuildDemand {
    #[must_use]
    pub fn from_registration(
        registration: &ExecutableEnvironmentRegistration,
        base_image: &str,
    ) -> Option<Self> {
        if registration.config.is_self_hosted() || registration.config.packages().is_empty() {
            return None;
        }
        let build_key = format!(
            "environment-image:{}:{base_image}",
            registration.fingerprint
        );
        Some(Self {
            build_key,
            workspace_id: registration.workspace_id.clone(),
            environment_id: registration.environment_id.clone(),
            source_revision: registration.source_revision,
            definition_fingerprint: registration.fingerprint.clone(),
            base_image: base_image.to_owned(),
            config: registration.config.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EnvironmentImageBuildState {
    Pending {
        attempt: u64,
    },
    Building {
        owner: String,
        lease_epoch: u64,
        lease_expires_at_ms: u64,
        attempt: u64,
    },
    Ready {
        image: String,
        ready_at_ms: u64,
        attempt: u64,
    },
    Failed {
        message: String,
        retry_at_ms: u64,
        attempt: u64,
    },
}

impl Default for EnvironmentImageBuildState {
    fn default() -> Self {
        Self::Pending { attempt: 0 }
    }
}

impl EnvironmentImageBuildState {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Pending { .. } => "pending",
            Self::Building { .. } => "building",
            Self::Ready { .. } => "ready",
            Self::Failed { .. } => "failed",
        }
    }

    #[must_use]
    pub fn claim(&self, owner: &str, now_ms: u64, lease_ms: u64) -> Option<Self> {
        let attempt = match self {
            Self::Pending { attempt } => *attempt,
            Self::Failed {
                retry_at_ms,
                attempt,
                ..
            } if *retry_at_ms <= now_ms => *attempt,
            Self::Building {
                lease_expires_at_ms,
                attempt,
                ..
            } if *lease_expires_at_ms <= now_ms => *attempt,
            Self::Failed { .. } | Self::Building { .. } | Self::Ready { .. } => return None,
        };
        Some(Self::Building {
            owner: owner.to_owned(),
            lease_epoch: attempt.saturating_add(1),
            lease_expires_at_ms: now_ms.saturating_add(lease_ms),
            attempt: attempt.saturating_add(1),
        })
    }

    #[must_use]
    pub fn complete(
        &self,
        owner: &str,
        lease_epoch: u64,
        image: &str,
        now_ms: u64,
    ) -> Option<Self> {
        match self {
            Self::Building {
                owner: current_owner,
                lease_epoch: current_epoch,
                attempt,
                ..
            } if current_owner == owner
                && *current_epoch == lease_epoch
                && !image.trim().is_empty() =>
            {
                Some(Self::Ready {
                    image: image.to_owned(),
                    ready_at_ms: now_ms,
                    attempt: *attempt,
                })
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn fail(
        &self,
        owner: &str,
        lease_epoch: u64,
        message: &str,
        now_ms: u64,
        retry_ms: u64,
    ) -> Option<Self> {
        match self {
            Self::Building {
                owner: current_owner,
                lease_epoch: current_epoch,
                attempt,
                ..
            } if current_owner == owner && *current_epoch == lease_epoch => Some(Self::Failed {
                message: message.to_owned(),
                retry_at_ms: now_ms.saturating_add(retry_ms),
                attempt: *attempt,
            }),
            _ => None,
        }
    }

    #[must_use]
    pub fn invalidate_ready(&self) -> Option<Self> {
        match self {
            Self::Ready { attempt, .. } => Some(Self::Pending { attempt: *attempt }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentImageBuildRecord {
    pub demand: EnvironmentImageBuildDemand,
    pub state: EnvironmentImageBuildState,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentImageBuildClaim {
    pub demand: EnvironmentImageBuildDemand,
    pub owner: String,
    pub lease_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvironmentImageBuildError {
    #[error("conflicting Environment image-build demand: {0}")]
    Conflict(String),
    #[error("Environment image-build storage failed: {0}")]
    Storage(String),
    #[error("Environment image build is unavailable: {0}")]
    Unavailable(String),
    #[error("Environment image build timed out: {0}")]
    Timeout(String),
}

#[async_trait]
pub trait EnvironmentImageBuildStore: Send + Sync {
    async fn ensure(
        &self,
        demand: EnvironmentImageBuildDemand,
        now_ms: u64,
    ) -> Result<(), EnvironmentImageBuildError>;

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<EnvironmentImageBuildRecord>, EnvironmentImageBuildError>;

    async fn claim_next(
        &self,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<EnvironmentImageBuildClaim>, EnvironmentImageBuildError>;

    async fn complete(
        &self,
        claim: &EnvironmentImageBuildClaim,
        image: &str,
        now_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError>;

    async fn fail(
        &self,
        claim: &EnvironmentImageBuildClaim,
        message: &str,
        now_ms: u64,
        retry_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError>;

    async fn invalidate_ready(
        &self,
        build_key: &str,
        now_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError>;
}

#[async_trait]
pub trait EnvironmentImageBuilder: Send + Sync {
    async fn build(
        &self,
        demand: &EnvironmentImageBuildDemand,
    ) -> Result<String, EnvironmentImageBuildError>;

    async fn available(&self, image: &str) -> Result<bool, EnvironmentImageBuildError>;
}

#[async_trait]
pub trait EnvironmentImageReadiness: Send + Sync {
    async fn ready_image(
        &self,
        registration: &ExecutableEnvironmentRegistration,
    ) -> Result<Option<String>, EnvironmentImageBuildError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_state_lease_decision_table() {
        // Cause/effect decision table: R1 Pending claims; R2 a live lease cannot
        // be stolen; R3 an expired lease is reclaimed with a higher epoch; R4 a
        // stale owner/epoch cannot complete; R5 the active claim can fail into
        // backoff; R6 backoff blocks early claim and permits claim at its edge;
        // R7 only the active claim and a non-empty image reach Ready.
        let pending = EnvironmentImageBuildState::default();
        let first = pending.claim("worker-a", 100, 50).expect("R1");
        assert!(first.claim("worker-b", 149, 50).is_none(), "R2");
        let reclaimed = first.claim("worker-b", 150, 50).expect("R3");
        assert!(
            reclaimed
                .complete("worker-a", 1, "image@sha256:a", 151)
                .is_none(),
            "R4"
        );
        let failed = reclaimed
            .fail("worker-b", 2, "offline", 151, 20)
            .expect("R5");
        assert!(failed.claim("worker-c", 170, 50).is_none(), "R6 early");
        let retried = failed.claim("worker-c", 171, 50).expect("R6 edge");
        assert!(
            retried.complete("worker-c", 3, "", 172).is_none(),
            "R7 empty"
        );
        assert!(
            matches!(
                retried.complete("worker-c", 3, "image@sha256:a", 172),
                Some(EnvironmentImageBuildState::Ready { .. })
            ),
            "R7"
        );
    }
}
