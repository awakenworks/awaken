// @formal entrypoint environment_image_build.single_claim_exact_reclaim_and_stale_fence
//! Coordinator-owned image realization of a registered executable Environment.
//!
//! It describes durable build demand, lease transitions, and the builder and
//! readiness ports without naming SQL, Kubernetes, or a container engine.

use async_trait::async_trait;
#[cfg(test)]
use awaken_environment_contract::{EnvironmentConfig, EnvironmentRevision};
use {
    crate::{EnvironmentImageBuildDemand, ExecutableEnvironmentRegistration},
    awaken_lease_contract::fence::{Epoch, EpochIntegrityViolation},
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EnvironmentImageBuildState {
    Pending {
        attempt: u64,
    },
    Building {
        owner: String,
        lease_epoch: Epoch,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvironmentImageBuildClaimDecision {
    Blocked,
    Claim {
        epoch: Epoch,
        lease_expires_at_ms: u64,
    },
    Integrity(EpochIntegrityViolation),
    DeadlineExhausted,
}

/// The scalar claim kernel used by every Environment image-build store. State
/// shape decides eligibility; this function alone owns epoch and deadline
/// advancement so adapters cannot disagree or wrap authority. The attempt
/// counter is the last minted lease epoch. `granted` says a claim wrote it
/// (`Building` and `Failed` only follow a claim); otherwise it is the
/// `Pending` default, where zero means never claimed. A granted zero, or an
/// attempt with no successor, is a lease-generation integrity failure.
fn environment_image_build_claim_decision(
    eligible: bool,
    granted: bool,
    attempt: u64,
    now_ms: u64,
    lease_ms: u64,
) -> EnvironmentImageBuildClaimDecision {
    if !eligible {
        return EnvironmentImageBuildClaimDecision::Blocked;
    }
    let successor = if granted {
        Epoch::successor_of_granted(Some(attempt))
    } else {
        match Epoch::successor(Epoch::from_persisted(attempt)) {
            Ok(epoch) => Ok(epoch),
            Err(exhausted) => Err(EpochIntegrityViolation::from(exhausted)),
        }
    };
    let epoch = match successor {
        Ok(epoch) => epoch,
        Err(violation) => return EnvironmentImageBuildClaimDecision::Integrity(violation),
    };
    let Some(lease_expires_at_ms) = now_ms.checked_add(lease_ms) else {
        return EnvironmentImageBuildClaimDecision::DeadlineExhausted;
    };
    EnvironmentImageBuildClaimDecision::Claim {
        epoch,
        lease_expires_at_ms,
    }
}

/// One exact authority predicate gates every mutation made by a claimed image
/// build. Completion additionally requires an admissible image; failure passes
/// `command_admitted=true` because an empty diagnostic is still a valid retry.
const fn environment_image_build_claim_mutation_admitted(
    owner_matches: bool,
    epoch_matches: bool,
    command_admitted: bool,
) -> bool {
    owner_matches && epoch_matches && command_admitted
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

    pub fn claim(
        &self,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<Self>, EnvironmentImageBuildError> {
        let (eligible, granted, attempt) = match self {
            Self::Pending { attempt } => (true, false, *attempt),
            Self::Failed {
                retry_at_ms,
                attempt,
                ..
            } => (*retry_at_ms <= now_ms, true, *attempt),
            Self::Building {
                lease_expires_at_ms,
                attempt,
                ..
            } => (*lease_expires_at_ms <= now_ms, true, *attempt),
            Self::Ready { attempt, .. } => (false, true, *attempt),
        };
        let (epoch, lease_expires_at_ms) = match environment_image_build_claim_decision(
            eligible, granted, attempt, now_ms, lease_ms,
        ) {
            EnvironmentImageBuildClaimDecision::Blocked => return Ok(None),
            EnvironmentImageBuildClaimDecision::Claim {
                epoch,
                lease_expires_at_ms,
            } => (epoch, lease_expires_at_ms),
            EnvironmentImageBuildClaimDecision::Integrity(violation) => {
                return Err(EnvironmentImageBuildError::LeaseGenerationIntegrity(
                    violation,
                ));
            }
            EnvironmentImageBuildClaimDecision::DeadlineExhausted => {
                return Err(EnvironmentImageBuildError::AuthorityExhausted(
                    "image build lease deadline".to_string(),
                ));
            }
        };
        Ok(Some(Self::Building {
            owner: owner.to_owned(),
            lease_epoch: epoch,
            lease_expires_at_ms,
            attempt: epoch.get(),
        }))
    }

    #[must_use]
    pub fn complete(
        &self,
        owner: &str,
        lease_epoch: Epoch,
        image: &str,
        now_ms: u64,
    ) -> Option<Self> {
        match self {
            Self::Building {
                owner: current_owner,
                lease_epoch: current_epoch,
                attempt,
                ..
            } if environment_image_build_claim_mutation_admitted(
                current_owner == owner,
                *current_epoch == lease_epoch,
                !image.trim().is_empty(),
            ) =>
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
        lease_epoch: Epoch,
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
            } if environment_image_build_claim_mutation_admitted(
                current_owner == owner,
                *current_epoch == lease_epoch,
                true,
            ) =>
            {
                Some(Self::Failed {
                    message: message.to_owned(),
                    retry_at_ms: now_ms.saturating_add(retry_ms),
                    attempt: *attempt,
                })
            }
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
    pub lease_epoch: Epoch,
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
    #[error("Environment image build authority exhausted: {0}")]
    AuthorityExhausted(String),
    /// This build's persisted attempt/lease generation is untrustworthy. A
    /// data-integrity failure of this build only: permanent, never retried or
    /// reset; recovery is a new build demand.
    #[error("Environment image build lease: {0}")]
    LeaseGenerationIntegrity(EpochIntegrityViolation),
}

#[async_trait]
pub trait EnvironmentImageBuildStore: Send + Sync {
    /// Ensure one recipe, optionally selecting its first immutable registration
    /// binding atomically with job insertion. Return the selected recipe; a
    /// racing candidate may adopt an earlier winner. None addresses a bare
    /// content recipe without claiming a registration pin.
    async fn ensure(
        &self,
        demand: EnvironmentImageBuildDemand,
        now_ms: u64,
        binding_key: Option<&str>,
    ) -> Result<EnvironmentImageBuildDemand, EnvironmentImageBuildError>;

    /// Resolve an immutable registration binding through its existing job.
    /// Missing/corrupt jobs fail; a binding never falls back to a mutable tag.
    async fn pinned_demand(
        &self,
        binding_key: &str,
    ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError>;

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

    async fn available(
        &self,
        demand: &EnvironmentImageBuildDemand,
        image: &str,
    ) -> Result<bool, EnvironmentImageBuildError>;
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
mod identity_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_environment_contract::{EnvItem, EnvironmentPackages, EnvironmentRevision};

    fn epoch(value: u64) -> Epoch {
        Epoch::from_persisted(value).expect("test epochs are nonzero")
    }

    #[test]
    fn build_state_lease_decision_table() {
        // Cause/effect decision table: R1 Pending claims; R2 a live lease cannot
        // be stolen; R3 an expired lease is reclaimed with a higher epoch; R4 a
        // stale owner/epoch cannot complete; R5 the active claim can fail into
        // backoff; R6 backoff blocks early claim and permits claim at its edge;
        // R7 only the active claim and a non-empty image reach Ready.
        let pending = EnvironmentImageBuildState::default();
        let first = pending
            .claim("worker-a", 100, 50)
            .expect("R1 valid authority")
            .expect("R1");
        assert!(
            first
                .claim("worker-b", 149, 50)
                .expect("R2 valid authority")
                .is_none(),
            "R2"
        );
        let reclaimed = first
            .claim("worker-b", 150, 50)
            .expect("R3 valid authority")
            .expect("R3");
        assert!(
            reclaimed
                .complete("worker-a", epoch(1), "image@sha256:a", 151)
                .is_none(),
            "R4"
        );
        let failed = reclaimed
            .fail("worker-b", epoch(2), "offline", 151, 20)
            .expect("R5");
        assert!(
            failed
                .claim("worker-c", 170, 50)
                .expect("R6 valid authority")
                .is_none(),
            "R6 early"
        );
        let retried = failed
            .claim("worker-c", 171, 50)
            .expect("R6 valid authority")
            .expect("R6 edge");
        assert!(
            retried.complete("worker-c", epoch(3), "", 172).is_none(),
            "R7 empty"
        );
        assert!(
            matches!(
                retried.complete("worker-c", epoch(3), "image@sha256:a", 172),
                Some(EnvironmentImageBuildState::Ready { .. })
            ),
            "R7"
        );
    }

    #[test]
    fn build_claim_rejects_attempt_and_deadline_exhaustion() {
        // The kernel classifies the persisted attempt generation once.
        // | Rule | state                         | effect                                  |
        // |------|-------------------------------|-----------------------------------------|
        // | A1   | Pending { 0 } (field default) | never claimed: Building epoch 1         |
        // | A2   | any claimable at u64::MAX     | LeaseGenerationIntegrity(Exhausted)     |
        // | A3   | expired Building / due Failed | LeaseGenerationIntegrity(ZeroGranted)   |
        // |      | at attempt 0 (a claim wrote)  |                                         |
        // | A4   | A2/A3 repeated                | the same refusal; nothing advances      |
        // | A5   | now + lease overflows         | AuthorityExhausted (deadline)           |
        let claimed = EnvironmentImageBuildState::default()
            .claim("worker", 1, 1)
            .expect("A1")
            .expect("A1 claimable");
        assert!(
            matches!(
                claimed,
                EnvironmentImageBuildState::Building { attempt: 1, .. }
            ),
            "A1"
        );
        let building = |attempt| EnvironmentImageBuildState::Building {
            owner: "old".into(),
            lease_epoch: Epoch::FIRST,
            lease_expires_at_ms: 0,
            attempt,
        };
        let failed = |attempt| EnvironmentImageBuildState::Failed {
            message: "m".into(),
            retry_at_ms: 0,
            attempt,
        };
        for attempt in 0..3 {
            for (rule, state, violation) in [
                (
                    "A2 pending",
                    EnvironmentImageBuildState::Pending { attempt: u64::MAX },
                    EpochIntegrityViolation::Exhausted,
                ),
                (
                    "A2 building",
                    building(u64::MAX),
                    EpochIntegrityViolation::Exhausted,
                ),
                (
                    "A2 failed",
                    failed(u64::MAX),
                    EpochIntegrityViolation::Exhausted,
                ),
                (
                    "A3 building",
                    building(0),
                    EpochIntegrityViolation::ZeroGranted,
                ),
                ("A3 failed", failed(0), EpochIntegrityViolation::ZeroGranted),
            ] {
                assert_eq!(
                    state.claim("worker", 1, 1),
                    Err(EnvironmentImageBuildError::LeaseGenerationIntegrity(
                        violation
                    )),
                    "{rule} attempt {attempt}"
                );
            }
        }
        let pending = EnvironmentImageBuildState::default();
        assert!(matches!(
            pending.claim("worker", u64::MAX, 1),
            Err(EnvironmentImageBuildError::AuthorityExhausted(_))
        ));
    }

    #[test]
    fn image_build_claim_mutation_authority_decision_table() {
        // Cause/effect graph: C1 owner matches; C2 epoch matches; C3 command is
        // admissible. Effect E1 permits the claimed mutation; E2 is an atomic
        // stutter. The complete 2^3 table has exactly one E1 rule, C1+C2+C3;
        // every stale, torn, or invalid combination is E2.
        for owner_matches in [false, true] {
            for epoch_matches in [false, true] {
                for command_admitted in [false, true] {
                    assert_eq!(
                        environment_image_build_claim_mutation_admitted(
                            owner_matches,
                            epoch_matches,
                            command_admitted,
                        ),
                        owner_matches && epoch_matches && command_admitted,
                    );
                }
            }
        }
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
        // Constraints/invariants: only executable base+config define recipe
        // identity; authoring metadata and Environment coordinates never do.
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
                    description: Some(description.into()),
                    metadata: [("revision".into(), revision.to_string())]
                        .into_iter()
                        .collect(),
                    scope: None,
                    config,
                    sandbox_policy: None,
                    archived_at: None,
                    created_at: Some("2026-02-03T04:05:06.007Z".into()),
                    updated_at: Some("2026-02-03T04:05:06.007Z".into()),
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

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation environment_image_build.single_claim_exact_reclaim_and_stale_fence
    #[kani::proof]
    fn environment_image_build_claim_is_exact_non_wrapping_and_blocked_is_inert() {
        let eligible = kani::any::<bool>();
        let attempt = kani::any::<u64>();
        let now_ms = kani::any::<u64>();
        let lease_ms = kani::any::<u64>();
        let granted = kani::any::<bool>();
        let decision =
            environment_image_build_claim_decision(eligible, granted, attempt, now_ms, lease_ms);

        if !eligible {
            assert_eq!(decision, EnvironmentImageBuildClaimDecision::Blocked);
            return;
        }
        if granted && attempt == 0 {
            assert_eq!(
                decision,
                EnvironmentImageBuildClaimDecision::Integrity(EpochIntegrityViolation::ZeroGranted)
            );
            return;
        }
        match (attempt.checked_add(1), now_ms.checked_add(lease_ms)) {
            (None, _) => assert_eq!(
                decision,
                EnvironmentImageBuildClaimDecision::Integrity(EpochIntegrityViolation::Exhausted)
            ),
            (Some(_), None) => assert_eq!(
                decision,
                EnvironmentImageBuildClaimDecision::DeadlineExhausted
            ),
            (Some(expected_attempt), Some(expected_deadline)) => assert_eq!(
                decision,
                EnvironmentImageBuildClaimDecision::Claim {
                    epoch: Epoch::from_persisted(expected_attempt).expect("a successor is nonzero"),
                    lease_expires_at_ms: expected_deadline,
                }
            ),
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// @exhaustive bool(2) x bool(2) x bool(2) = 8
    /// @obligation environment_image_build.single_claim_exact_reclaim_and_stale_fence
    #[test]
    fn environment_image_build_mutation_requires_exact_claim_and_admissible_command() {
        for owner_matches in [false, true] {
            for epoch_matches in [false, true] {
                for command_admitted in [false, true] {
                    assert_eq!(
                        environment_image_build_claim_mutation_admitted(
                            owner_matches,
                            epoch_matches,
                            command_admitted,
                        ),
                        owner_matches && epoch_matches && command_admitted,
                    );
                }
            }
        }
    }
}
