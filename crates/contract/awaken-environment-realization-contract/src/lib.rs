//! Coordinator-owned Environment image-realization contract.
//!
//! Control registers exact definitions. This contract starts at the Coordinator
//! boundary: it describes durable build demand, lease transitions, and the
//! builder/readiness ports without naming SQL, Kubernetes, or a container engine.

use async_trait::async_trait;
use awaken_environment_contract::{EnvironmentConfig, EnvironmentRevision};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistration;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentImageBuildDemand {
    pub build_key: String,
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
        if registration.definition.config.is_self_hosted()
            || registration.definition.config.packages().is_empty()
        {
            return None;
        }
        // Only build inputs own image identity. Environment name, description,
        // metadata and authored revision remain provenance and must not create a
        // parallel image for identical package/network/base facts.
        let recipe_fingerprint = awaken_environment_contract::environment_facts_fingerprint(&(
            base_image,
            &registration.definition.config,
        ));
        let build_key = format!("environment-image:{recipe_fingerprint}");
        Some(Self {
            build_key,
            environment_id: registration.definition.id.clone(),
            source_revision: registration.definition.revision,
            definition_fingerprint: registration.fingerprint.clone(),
            base_image: base_image.to_owned(),
            config: registration.definition.config.clone(),
        })
    }

    /// Whether two demands name the same authoritative build recipe. Provenance
    /// may advance while a content-identical ready image remains reusable.
    #[must_use]
    pub fn same_recipe(&self, other: &Self) -> bool {
        self.build_key == other.build_key
            && self.base_image == other.base_image
            && self.config == other.config
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
    /// Resolve a mutable operator reference to the immutable identity that must
    /// participate in durable demand identity and the derived image recipe.
    async fn base_image_identity(
        &self,
        reference: &str,
    ) -> Result<String, EnvironmentImageBuildError> {
        Ok(reference.to_owned())
    }

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
        base_image: Option<&str>,
    ) -> Result<Option<String>, EnvironmentImageBuildError>;

    /// Non-blocking readiness used by capacity reconciliation. `None` means the
    /// exact image is not ready yet (or no image is required); callers already
    /// know whether the Environment declares packages and can distinguish those
    /// cases without manufacturing another state store.
    async fn ready_image_now(
        &self,
        _registration: &ExecutableEnvironmentRegistration,
        _base_image: Option<&str>,
    ) -> Result<Option<String>, EnvironmentImageBuildError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_environment_contract::{EnvItem, EnvironmentPackages, EnvironmentRevision};

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

    #[test]
    fn image_recipe_identity_ignores_authoring_only_changes() {
        // FMECA: F1 metadata/revision/id-only change duplicates a recipe build
        // (S4 O7 D2, RPN56);
        // F2 package/base/network update aliases an old image (S9 O3 D3,
        // RPN81). The recipe key therefore contains only base+config while the
        // Environment coordinate keeps unrelated Environments isolated.
        // Cause graph: C1=metadata/revision differs; C2=Environment id differs;
        // C3=config differs; C4=base differs. E1=same recipe; E2=distinct recipe.
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | I1   | 1  | 0  | 0  | 0  | E1     |
        // | I2   | -  | 1  | 0  | 0  | E1     |
        // | I3   | -  | -  | 1  | 0  | E2     |
        // | I4   | -  | -  | 0  | 1  | E2     |
        let config = EnvironmentConfig::Cloud {
            networking: Default::default(),
            packages: EnvironmentPackages {
                npm: vec!["tsx@4.0.0".into()],
                ..Default::default()
            },
        };
        let registration = |id: &str, revision, description: &str, config: EnvironmentConfig| {
            ExecutableEnvironmentRegistration::new(
                EnvItem {
                    id: id.into(),
                    revision: EnvironmentRevision(revision),
                    name: format!("name-{revision}"),
                    description: description.into(),
                    metadata: [("revision".into(), revision.to_string())]
                        .into_iter()
                        .collect(),
                    scope: None,
                    config,
                    sandbox_policy: None,
                    archived_at: None,
                },
                None,
            )
        };
        let first = EnvironmentImageBuildDemand::from_registration(
            &registration("env-a", 1, "first", config.clone()),
            "base@sha256:a",
        )
        .unwrap();
        let metadata = EnvironmentImageBuildDemand::from_registration(
            &registration("env-a", 2, "changed", config.clone()),
            "base@sha256:a",
        )
        .unwrap();
        assert!(first.same_recipe(&metadata), "I1");
        let coordinate = EnvironmentImageBuildDemand::from_registration(
            &registration("env-b", 1, "first", config.clone()),
            "base@sha256:a",
        )
        .unwrap();
        assert!(first.same_recipe(&coordinate), "I2");
        let packages = EnvironmentImageBuildDemand::from_registration(
            &registration(
                "env-a",
                3,
                "changed",
                EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["tsx@5.0.0".into()],
                        ..Default::default()
                    },
                },
            ),
            "base@sha256:a",
        )
        .unwrap();
        assert!(!first.same_recipe(&packages), "I3");
        let base = EnvironmentImageBuildDemand::from_registration(
            &registration("env-a", 4, "changed", config),
            "base@sha256:b",
        )
        .unwrap();
        assert!(!first.same_recipe(&base), "I4");
    }
}
