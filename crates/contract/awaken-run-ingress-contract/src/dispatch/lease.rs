//! The Dispatch claim's value and its unchanged Control/Worker wire projection.
//!
//! Stores grant, renew and fence claims under their existing row authority.
//! This value carries that observation; it never admits a store mutation.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_lease_contract::fence::{Epoch, Lease, Tenure, TenureScope};
use awaken_time::Expiry;
use serde::{Deserialize, Serialize};

use super::DispatchError;

/// The family of grants over Dispatch's RunId, held by its existing String
/// owner. This marker carries no Run data; Worker assignment stays separate.
struct DispatchRun;

impl TenureScope for DispatchRun {
    type Subject = RunId;
    type Holder = String;
}

/// One store-observed Dispatch grant and its deadline.
///
/// The only stored value is the shared lease kernel. The flat wire projection
/// belongs here because Dispatch's row/CAS authority and renewal rules differ
/// from the kernel's successor policy: an expired, unreclaimed exact claim may
/// renew, and that renewal may shorten its deadline. Neither a local liveness
/// observation nor a greater epoch authorizes a Dispatch mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "DispatchLeaseWire", into = "DispatchLeaseWire")]
pub struct DispatchLease(Lease<DispatchRun>);

/// The original flat transport shape; never retained beside the typed value.
/// Unknown fields stay tolerated, just as they were by the original decoder.
#[derive(Serialize, Deserialize)]
struct DispatchLeaseWire {
    run_id: RunId,
    owner: String,
    expires_ms: u64,
    epoch: Epoch,
}

impl DispatchLease {
    /// Project coordinates minted or read by the granting store. A never-claimed
    /// row's epoch-zero sentinel is not a grant and cannot become this value.
    pub fn from_persisted(
        run_id: RunId,
        owner: String,
        expires_ms: u64,
        epoch: u64,
    ) -> Result<Self, DispatchError> {
        Ok(Self::from_coordinates(
            run_id,
            owner,
            expires_ms,
            Epoch::try_from(epoch).map_err(|error| DispatchError::Rejected(error.to_string()))?,
        ))
    }

    fn from_coordinates(run_id: RunId, owner: String, expires_ms: u64, epoch: Epoch) -> Self {
        Self(Lease::new(
            Tenure::granted(run_id, owner, epoch),
            Expiry::from_epoch_ms(expires_ms),
        ))
    }

    #[must_use]
    pub fn run_id(&self) -> &RunId {
        self.0.tenure().subject()
    }

    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.tenure().holder().as_str()
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.0.tenure().epoch().get()
    }

    #[must_use]
    pub fn expires_ms(&self) -> u64 {
        self.0.expires().to_epoch_ms()
    }

    /// The same observed grant at the deadline selected by the store's existing
    /// renewal transaction. This is a value projection, not renewal admission;
    /// it deliberately permits a shorter expiry or an already-expired value.
    #[must_use]
    pub fn with_deadline(&self, expires_ms: u64) -> Self {
        Self(Lease::new(
            self.0.tenure().clone(),
            Expiry::from_epoch_ms(expires_ms),
        ))
    }
}

impl From<DispatchLeaseWire> for DispatchLease {
    fn from(wire: DispatchLeaseWire) -> Self {
        Self::from_coordinates(wire.run_id, wire.owner, wire.expires_ms, wire.epoch)
    }
}

impl From<DispatchLease> for DispatchLeaseWire {
    fn from(lease: DispatchLease) -> Self {
        Self {
            run_id: lease.run_id().clone(),
            owner: lease.owner().to_owned(),
            expires_ms: lease.expires_ms(),
            epoch: lease.0.tenure().epoch(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_lease_keeps_exact_flat_wire_and_full_value_domain() {
        // Causes: C1 positive epoch at FIRST/ordinary/MAX; C2 expiry at
        // zero/ordinary/MAX; C3 independent Run and owner strings: empty,
        // ordinary, Unicode, quote, backslash and control characters; C4 an unknown
        // field is present. Effects: E1 the original flat field order/JSON
        // bytes are unchanged; E2 every valid coordinate survives; E3 unknown
        // fields are ignored. Rows are the full C1 x C2 x C3 cross-product;
        // C4 is exercised for each row, without inventing new string admission.
        for epoch in [1, 7, u64::MAX] {
            for expires_ms in [0, 100, u64::MAX] {
                for run in ["", "run-wire", "运行\"\\\n\r\t\u{0}\u{8}\u{c}\u{1f}"] {
                    for owner in [
                        "",
                        "worker:incarnation",
                        "持有人😀\"\\\n\r\t\u{0}\u{8}\u{c}\u{1f}",
                    ] {
                        let lease = DispatchLease::from_persisted(
                            RunId(run.into()),
                            owner.into(),
                            expires_ms,
                            epoch,
                        )
                        .unwrap();
                        // The original serializer delegated each String to serde,
                        // not Rust Debug; keep that byte oracle and the literal
                        // historical field order without introducing an old DTO.
                        let run_json = serde_json::to_string(run).unwrap();
                        let owner_json = serde_json::to_string(owner).unwrap();
                        let original = format!(
                            "{{\"run_id\":{run_json},\"owner\":{owner_json},\"expires_ms\":{expires_ms},\"epoch\":{epoch}}}"
                        );
                        assert_eq!(serde_json::to_string(&lease).unwrap(), original, "E1");
                        let back: DispatchLease = serde_json::from_str(&original).unwrap();
                        assert_eq!(back, lease, "E2");
                        let mut extended = serde_json::to_value(&lease).unwrap();
                        extended["future_field"] = serde_json::json!({"ignored": true});
                        assert_eq!(
                            serde_json::from_value::<DispatchLease>(extended).unwrap(),
                            lease,
                            "E3"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn deadline_projection_keeps_the_exact_grant_without_renewal_policy() {
        // Causes: C1 replacement expiry shorter/equal/longer than 100, including
        // zero and MAX; C2 original Run/owner/epoch; C3 an unclaimed row's epoch
        // zero. E1 all C1 values are retained
        // exactly while C2 stays identical. This distinguishes representation
        // from the shared Lease::renewed_to policy; only the store can admit the
        // renewal. C3 => refused by the canonical Epoch, never a typed lease.
        // The cross-backend table owns expired/current/reclaimed CAS.
        let lease =
            DispatchLease::from_persisted(RunId("run".into()), "owner".into(), 100, 7).unwrap();
        for deadline in [0, 99, 100, 101, u64::MAX] {
            let observed = lease.with_deadline(deadline);
            assert_eq!(observed.expires_ms(), deadline, "E1 deadline");
            assert_eq!(observed.run_id(), lease.run_id(), "E1 Run");
            assert_eq!(observed.owner(), lease.owner(), "E1 owner");
            assert_eq!(observed.epoch(), lease.epoch(), "E1 epoch");
            assert_eq!(
                observed == lease,
                deadline == 100,
                "expiry is part of value identity"
            );
        }
        assert!(
            DispatchLease::from_persisted(RunId("run".into()), "owner".into(), 100, 0).is_err(),
            "an unclaimed row is not a lease"
        );
    }
}
