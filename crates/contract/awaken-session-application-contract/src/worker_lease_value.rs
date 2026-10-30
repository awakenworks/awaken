// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Session operation values compose the canonical kernel; neutral transfer DTOs stay dependency-free.
//! Typed Session operation values over the existing neutral Work transfer.

use crate::SessionWork;
use awaken_lease_contract::fence::{Epoch, Lease, Tenure};
use awaken_session_types::{SessionId, SessionWorkLease as TransferSessionWorkLease};
use awaken_time::Expiry;

/// A Work lease generation, distinct from a Dispatch or activity generation.
/// Numeric transport and nonzero/succession laws delegate to the lease kernel.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct WorkLeaseEpoch(Epoch);

impl WorkLeaseEpoch {
    pub const FIRST: Self = Self(Epoch::FIRST);

    #[must_use]
    pub const fn from_persisted(value: u64) -> Option<Self> {
        match Epoch::from_persisted(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.next() {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl std::fmt::Display for WorkLeaseEpoch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The complete Work lease observed by the controlled Session authority.
///
/// This owns one canonical kernel lease, not a second copy of its coordinates.
/// It is an observation, not permission to bypass a fresh authority check.
/// Storage/transport roles retain their neutral compatibility DTO; conversion
/// happens at the Session operation boundary without coupling its leaf types
/// to the kernel or exposing a raw Work generation to Dispatch consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionWorkGrant(Lease<SessionWork>);

impl SessionWorkGrant {
    #[must_use]
    pub fn from_transfer(transfer: TransferSessionWorkLease) -> Self {
        let tenure = Tenure::granted(
            (
                transfer.work_id().to_owned(),
                transfer.environment_id().to_owned(),
                transfer.session_id().clone(),
            ),
            transfer.owner().to_owned(),
            Epoch::from_persisted(transfer.epoch()).expect("transfer epoch is nonzero"),
        );
        Self(Lease::new(
            tenure,
            Expiry::from_epoch_ms(transfer.expires_at_unix_ms()),
        ))
    }

    /// Encode the unchanged neutral representation at the storage boundary.
    #[must_use]
    pub fn to_transfer(&self) -> TransferSessionWorkLease {
        TransferSessionWorkLease::from_persisted(
            self.work_id(),
            self.environment_id(),
            self.session_id().clone(),
            self.owner(),
            self.epoch().get(),
            self.expires_at_unix_ms(),
        )
        .expect("kernel lease epoch is nonzero")
    }

    #[must_use]
    pub fn work_id(&self) -> &str {
        &self.0.tenure().subject().0
    }

    #[must_use]
    pub fn environment_id(&self) -> &str {
        &self.0.tenure().subject().1
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.0.tenure().subject().2
    }

    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.tenure().holder()
    }

    #[must_use]
    pub const fn epoch(&self) -> WorkLeaseEpoch {
        WorkLeaseEpoch(self.0.tenure().epoch())
    }

    #[must_use]
    pub fn expires_at_unix_ms(&self) -> u64 {
        self.0.expires().to_epoch_ms()
    }

    pub(crate) fn tenure(&self) -> &Tenure<SessionWork> {
        self.0.tenure()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_generations_preserve_the_numeric_wire_and_never_grant_zero_or_wrap() {
        assert_eq!(WorkLeaseEpoch::FIRST.get(), 1);
        for number in [1, 2, u64::MAX] {
            let generation = WorkLeaseEpoch::from_persisted(number).unwrap();
            assert_eq!(generation.get(), number);
            assert_eq!(generation.to_string(), number.to_string());
            assert_eq!(
                serde_json::to_value(generation).unwrap(),
                serde_json::json!(number)
            );
            assert_eq!(
                serde_json::from_value::<WorkLeaseEpoch>(serde_json::json!(number)).unwrap(),
                generation
            );
            assert_eq!(
                generation.next().map(WorkLeaseEpoch::get),
                number.checked_add(1)
            );
        }
        assert!(WorkLeaseEpoch::from_persisted(0).is_none());
        for wire in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("1"),
            serde_json::Value::Null,
        ] {
            assert!(serde_json::from_value::<WorkLeaseEpoch>(wire).is_err());
        }
    }

    #[test]
    fn controlled_work_lease_roundtrips_the_complete_neutral_transfer() {
        // A transfer is not a live grant: expired observations remain valid
        // data, and the entire u64 epoch/expiry domain must survive this bridge.
        for (generation, expiry) in [(1, 0), (2, 10), (u64::MAX, u64::MAX)] {
            let transfer = TransferSessionWorkLease::from_persisted(
                "work",
                "environment",
                "session".into(),
                "owner",
                generation,
                expiry,
            )
            .unwrap();
            let lease = SessionWorkGrant::from_transfer(transfer.clone());
            assert_eq!(
                (
                    lease.work_id(),
                    lease.environment_id(),
                    lease.session_id(),
                    lease.owner(),
                    lease.epoch().get(),
                    lease.expires_at_unix_ms()
                ),
                (
                    transfer.work_id(),
                    transfer.environment_id(),
                    transfer.session_id(),
                    transfer.owner(),
                    generation,
                    expiry
                )
            );
            assert_eq!(lease.to_transfer(), transfer);
            assert_eq!(lease.clone(), lease);
        }
    }
}
