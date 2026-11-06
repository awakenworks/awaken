// @formal surface obligation=session_run.execution_requires_dispatch_work_and_realization_owner
// @formal reason: Immutable transfer coordinates only; lease arithmetic and state transitions remain in the canonical fence/store.
//! Transfer values shared by Session and Environment without an application dependency.
use crate::SessionId;
use std::num::NonZeroU64;

/// Neutral compatibility coordinates for a Session Worker assignment.
/// Controlled Session operations use the kernel-backed value in
/// `awaken_session_application_contract`; this DTO is not an action proof.
/// This value contains no
/// lease mutation or grant policy. A receiver must compare it with current
/// authority before admitting an effect; expiry is a renewable observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionWorkLease {
    work_id: String,
    environment_id: String,
    session_id: SessionId,
    owner: String,
    epoch: NonZeroU64,
    expires_at_unix_ms: u64,
}
impl SessionWorkLease {
    /// Decode existing persisted coordinates. Zero is never a granted epoch.
    #[must_use]
    pub fn from_persisted(
        work_id: impl Into<String>,
        environment_id: impl Into<String>,
        session_id: SessionId,
        owner: impl Into<String>,
        epoch: u64,
        expires_at_unix_ms: u64,
    ) -> Option<Self> {
        Some(Self {
            work_id: work_id.into(),
            environment_id: environment_id.into(),
            session_id,
            owner: owner.into(),
            epoch: NonZeroU64::new(epoch)?,
            expires_at_unix_ms,
        })
    }
    #[must_use]
    pub fn work_id(&self) -> &str {
        &self.work_id
    }
    #[must_use]
    pub fn environment_id(&self) -> &str {
        &self.environment_id
    }
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch.get()
    }
    #[must_use]
    pub const fn expires_at_unix_ms(&self) -> u64 {
        self.expires_at_unix_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_lease_transfer_validates_epoch_without_deciding_live_authority() {
        // T1 epoch zero cannot represent a grant. T2 an expired observation
        // retains its exact coordinate; transfer is not revocation. T3 the
        // complete u64 domain round trips without narrowing epoch or expiry.
        for (epoch, expiry, accepted) in [(0, 10, false), (1, 0, true), (u64::MAX, u64::MAX, true)]
        {
            let lease = SessionWorkLease::from_persisted(
                "work",
                "environment",
                "session".into(),
                "owner",
                epoch,
                expiry,
            );
            assert_eq!(lease.is_some(), accepted, "epoch={epoch}, expiry={expiry}");
            if let Some(lease) = lease {
                assert_eq!(
                    (
                        lease.work_id(),
                        lease.environment_id(),
                        lease.session_id().as_str(),
                        lease.owner(),
                        lease.epoch(),
                        lease.expires_at_unix_ms()
                    ),
                    ("work", "environment", "session", "owner", epoch, expiry)
                );
            }
        }
    }
}
