//! Transient evidence returned by the existing Session lease authority.
//! PersistedSession continues to store only SessionRealizationLease.

use super::{SessionRealizationControlFailure, SessionRealizationLease};
use awaken_runtime_contract::authority_lease::{AuthorityLeaseTiming, AuthorityProofStarted};
use std::{num::NonZeroU64, time::Duration};

/// A Control-clock horizon, never a Worker wall-clock expiry calculation.
/// Missing/zero horizons do not decode; older responses cannot mint evidence.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationLeaseReceipt {
    pub lease: SessionRealizationLease,
    pub remaining_ms: NonZeroU64,
}

impl SessionRealizationLeaseReceipt {
    /// Sample after the authoritative read/CAS and all projection-building I/O.
    /// Expired-at-publication is ownership loss, not a one-millisecond grant.
    pub fn issued_at(
        lease: SessionRealizationLease,
        authority_now_unix_ms: u64,
    ) -> Result<Self, SessionRealizationControlFailure> {
        let remaining_ms = lease
            .expires_at_unix_ms
            .checked_sub(authority_now_unix_ms)
            .and_then(NonZeroU64::new)
            .ok_or(SessionRealizationControlFailure::StaleOwnership)?;
        Ok(Self {
            lease,
            remaining_ms,
        })
    }

    /// Only the caller may supply its local request-start observation.
    /// No timestamp received over the wire can populate this local value.
    #[must_use]
    pub fn observed(self, started: AuthorityProofStarted) -> ObservedSessionRealizationLease {
        ObservedSessionRealizationLease {
            lease: self.lease,
            started,
            timing: AuthorityLeaseTiming::from_ttl_ms(self.remaining_ms.get()),
        }
    }
}

/// One rebuildable proof accompanying the existing slot's lease, not a
/// separate lease owner or registry. Deliberately not serializable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedSessionRealizationLease {
    pub lease: SessionRealizationLease,
    started: AuthorityProofStarted,
    timing: AuthorityLeaseTiming,
}

impl ObservedSessionRealizationLease {
    #[must_use]
    pub fn remaining_at(&self, now: AuthorityProofStarted) -> Option<Duration> {
        self.timing.remaining_proof_at(self.started, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn lease(expiry: u64) -> SessionRealizationLease {
        SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "boot".into(),
            epoch: 1,
            expires_at_unix_ms: expiry,
        }
    }

    #[test]
    fn receipt_horizon_is_positive_authority_relative_and_required_on_the_wire() {
        // C1 authority time before/equal/after expiry; C2 response horizon
        // missing/zero/positive. E1 only before+positive yields evidence;
        // E2 all other rows deny, never clamp invalid authority into 1ms.
        // The durable lease wire stays unchanged and stores no observation.
        for (now, expected) in [(70, Some(30)), (100, None), (101, None)] {
            let receipt = SessionRealizationLeaseReceipt::issued_at(lease(100), now);
            assert_eq!(
                receipt.as_ref().ok().map(|r| r.remaining_ms.get()),
                expected
            );
            if let Ok(receipt) = receipt {
                let encoded = serde_json::to_value(&receipt).unwrap();
                assert_eq!(
                    serde_json::from_value::<SessionRealizationLeaseReceipt>(encoded.clone())
                        .unwrap(),
                    receipt
                );
                for horizon in [None, Some(serde_json::json!(0))] {
                    let mut invalid = encoded.clone();
                    invalid.as_object_mut().unwrap().remove("remaining_ms");
                    if let Some(value) = horizon {
                        invalid["remaining_ms"] = value;
                    }
                    assert!(
                        serde_json::from_value::<SessionRealizationLeaseReceipt>(invalid).is_err()
                    );
                }
                assert!(
                    serde_json::to_value(&receipt.lease)
                        .unwrap()
                        .get("remaining_ms")
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn observation_deducts_response_latency_without_any_worker_wall_clock() {
        // C1 positive 30ms horizon => shared 20ms conservative proof;
        // C2 elapsed is before/equal/after the bound or regressed;
        // C3 scheduler advances/stalls. Only before grants exact remaining.
        // Changing the absolute authority epoch never changes the result:
        // no Worker wall-clock coordinate participates in this calculation.
        let scheduled = Instant::now();
        let started = AuthorityProofStarted::from_clock_sample(scheduled, Duration::from_secs(1));
        for authority_epoch in [0, 1_000_000, u64::MAX - 30] {
            let proof = SessionRealizationLeaseReceipt::issued_at(
                lease(authority_epoch + 30),
                authority_epoch,
            )
            .unwrap()
            .observed(started);
            for scheduler_delta in [Duration::ZERO, Duration::from_secs(2)] {
                for (elapsed, expected) in [
                    (999_999, None),
                    (1_019_999, Some(1)),
                    (1_020_000, None),
                    (1_020_001, None),
                ] {
                    let now = AuthorityProofStarted::from_clock_sample(
                        scheduled + scheduler_delta,
                        Duration::from_micros(elapsed),
                    );
                    assert_eq!(proof.remaining_at(now), expected.map(Duration::from_micros));
                }
            }
        }
    }
}
