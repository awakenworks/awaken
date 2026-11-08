// @formal entrypoint work.lease_authority_snapshot_and_owner_mutations_are_atomic
//! Process-local lease and poll-liveness bookkeeping shared by WorkQueue backends.
//!
//! Durable backends keep lease ownership and expiry in database rows; they use this
//! book only for the explicitly ephemeral `workers_polling` observation. The
//! test-support in-memory backend additionally uses its lease/owner maps as its
//! executable reference model.

use std::collections::BTreeMap;

#[cfg(not(all(test, feature = "loom")))]
use awaken_sync::LeafLock;
#[cfg(all(test, feature = "loom"))]
use loom_lock::LeafLock;

use awaken_lease_contract::fence::{Epoch, fence_is_live_at};

use crate::mutation::{
    LeaseAuthorityCommand, LeaseAuthorityDecision, LeaseAuthorityTuple, LeaseFence,
    lease_authority_transition,
};

/// The lease window used by the reference backend.
pub const LEASE_TTL_MS: u64 = 60_000;
/// A worker counts as polling while its last poll is within this window.
pub const POLLER_WINDOW_MS: u64 = 30_000;

/// Shared process-local bookkeeping. This is not durable lease authority.
#[derive(Default)]
pub struct LeaseBook {
    authority: LeafLock<LeaseAuthority>,
    polls: LeafLock<BTreeMap<String, BTreeMap<String, u64>>>,
}

#[derive(Default)]
struct LeaseAuthority {
    leases: BTreeMap<String, LeaseWindow>,
    owners: BTreeMap<String, String>,
    /// The last granted generation per item; absent until the first claim.
    epochs: BTreeMap<String, Epoch>,
}

#[derive(Clone, Copy)]
struct LeaseWindow {
    refreshed_at_ms: u64,
    expires_at_ms: u64,
}

/// One coherent read for the test-support in-memory authority, not a durable
/// backend's lease source. Compile it with its sole in-memory consumer.
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct LeaseAuthoritySnapshot {
    pub(crate) owner: Option<String>,
    pub(crate) epoch: Option<u64>,
    pub(crate) lease_present: bool,
}

impl LeaseBook {
    pub fn record_poll(&self, env_id: &str, worker_id: &str, now_ms: u64) {
        self.polls
            .lock()
            .entry(env_id.to_string())
            .or_default()
            .insert(worker_id.to_string(), now_ms);
    }

    pub fn is_leased(&self, work_id: &str, now_ms: u64) -> bool {
        self.authority
            .lock()
            .leases
            .get(work_id)
            .is_some_and(|lease| fence_is_live_at(lease.expires_at_ms, now_ms))
    }

    pub fn is_leased_with_reclaim_age(&self, work_id: &str, now_ms: u64, age_ms: u64) -> bool {
        self.authority
            .lock()
            .leases
            .get(work_id)
            .is_some_and(|lease| {
                fence_is_live_at(lease.refreshed_at_ms.saturating_add(age_ms), now_ms)
            })
    }

    pub fn lease(&self, work_id: &str, now_ms: u64) {
        self.lease_for(work_id, now_ms, LEASE_TTL_MS);
    }

    /// Overwrite one item's persisted generation, as the storage corruption,
    /// bad migration or tampering the integrity refusal exists for would.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn corrupt_epoch_for_test(
        &self,
        work_id: &str,
        epoch: awaken_lease_contract::fence::Epoch,
    ) {
        self.authority
            .lock()
            .epochs
            .insert(work_id.to_string(), epoch);
    }

    /// Atomically install one owner, fencing epoch and lease window.  Keeping
    /// these fields under one mutex prevents an observer from combining a new
    /// epoch with the preceding owner or a lease window without either.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn claim_for(
        &self,
        work_id: &str,
        worker_id: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<u64, awaken_executable_environment_contract::WorkQueueError> {
        let mut authority = self.authority.lock();
        let current = LeaseAuthorityTuple {
            owner: authority.owners.get(work_id).cloned(),
            epoch: authority.epochs.get(work_id).copied(),
            refreshed_at: authority
                .leases
                .get(work_id)
                .map(|lease| lease.refreshed_at_ms),
            expires_at: authority
                .leases
                .get(work_id)
                .map(|lease| lease.expires_at_ms),
        };
        let LeaseAuthorityDecision::Apply(next) = lease_authority_transition(
            current,
            LeaseAuthorityCommand::Claim {
                owner: worker_id.to_string(),
                refreshed_at: now_ms,
                expires_at: now_ms.saturating_add(ttl_ms),
            },
        ) else {
            return Err(crate::mutation::lease_generation_integrity(
                awaken_lease_contract::fence::EpochExhausted,
            ));
        };
        let epoch = next.epoch.ok_or_else(|| {
            awaken_executable_environment_contract::WorkQueueError::Storage(
                "work lease claim installed no epoch".into(),
            )
        })?;
        authority.epochs.insert(work_id.to_string(), epoch);
        authority
            .owners
            .insert(work_id.to_string(), next.owner.expect("claim owner"));
        authority.leases.insert(
            work_id.to_string(),
            LeaseWindow {
                refreshed_at_ms: next.refreshed_at.expect("claim refresh"),
                expires_at_ms: next.expires_at.expect("claim expiry"),
            },
        );
        Ok(epoch.get())
    }

    pub fn is_owned_by(&self, work_id: &str, worker_id: &str) -> bool {
        self.authority
            .lock()
            .owners
            .get(work_id)
            .is_some_and(|owner| owner == worker_id)
    }

    pub fn is_owned_at_epoch(&self, work_id: &str, worker_id: &str, expected_epoch: u64) -> bool {
        let authority = self.authority.lock();
        authority
            .owners
            .get(work_id)
            .is_some_and(|owner| owner == worker_id)
            && authority.epochs.get(work_id).map(|epoch| epoch.get()) == Some(expected_epoch)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn snapshot(&self, work_id: &str) -> LeaseAuthoritySnapshot {
        let authority = self.authority.lock();
        LeaseAuthoritySnapshot {
            owner: authority.owners.get(work_id).cloned(),
            epoch: authority.epochs.get(work_id).map(|epoch| epoch.get()),
            lease_present: authority.leases.contains_key(work_id),
        }
    }

    pub fn authority(&self, work_id: &str, now_ms: u64) -> Option<(String, u64, u64)> {
        let authority = self.authority.lock();
        let lease = authority.leases.get(work_id).copied()?;
        if !fence_is_live_at(lease.expires_at_ms, now_ms) {
            return None;
        }
        let owner = authority.owners.get(work_id).cloned()?;
        let epoch = authority.epochs.get(work_id)?.get();
        Some((owner, epoch, lease.expires_at_ms))
    }

    pub fn lease_for(&self, work_id: &str, now_ms: u64, ttl_ms: u64) {
        let mut authority = self.authority.lock();
        let current = LeaseAuthorityTuple {
            owner: authority.owners.get(work_id).cloned(),
            epoch: authority.epochs.get(work_id).copied(),
            refreshed_at: authority
                .leases
                .get(work_id)
                .map(|lease| lease.refreshed_at_ms),
            expires_at: authority
                .leases
                .get(work_id)
                .map(|lease| lease.expires_at_ms),
        };
        let Some(owner) = current.owner.clone() else {
            // `lease()` is the legacy unowned liveness clock used by the
            // property surface. It is not an owner mutation; owned renewals
            // below must consume the complete authority transition.
            authority.leases.insert(
                work_id.to_string(),
                LeaseWindow {
                    refreshed_at_ms: now_ms,
                    expires_at_ms: now_ms.saturating_add(ttl_ms),
                },
            );
            return;
        };
        if let LeaseAuthorityDecision::Apply(next) = lease_authority_transition(
            current,
            LeaseAuthorityCommand::Renew {
                owner,
                // The legacy liveness clock renews whoever holds the item; it
                // is not a Worker request and names no generation.
                fence: LeaseFence::OwnerOnly,
                refreshed_at: now_ms,
                expires_at: now_ms.saturating_add(ttl_ms),
            },
        ) {
            authority.leases.insert(
                work_id.to_string(),
                LeaseWindow {
                    refreshed_at_ms: next.refreshed_at.expect("renew refresh"),
                    expires_at_ms: next.expires_at.expect("renew expiry"),
                },
            );
        }
    }

    pub fn release(&self, work_id: &str) {
        let mut authority = self.authority.lock();
        let current = LeaseAuthorityTuple {
            owner: authority.owners.get(work_id).cloned(),
            epoch: authority.epochs.get(work_id).copied(),
            refreshed_at: authority
                .leases
                .get(work_id)
                .map(|lease| lease.refreshed_at_ms),
            expires_at: authority
                .leases
                .get(work_id)
                .map(|lease| lease.expires_at_ms),
        };
        let command = current
            .owner
            .clone()
            .map(|owner| LeaseAuthorityCommand::Release {
                owner,
                fence: current
                    .epoch
                    .map_or(LeaseFence::OwnerOnly, LeaseFence::Exact),
            })
            .unwrap_or(LeaseAuthorityCommand::Reclaim);
        let LeaseAuthorityDecision::Apply(next) = lease_authority_transition(current, command)
        else {
            unreachable!("reclaim always clears the tuple")
        };
        debug_assert!(next.owner.is_none());
        debug_assert!(next.refreshed_at.is_none());
        debug_assert!(next.expires_at.is_none());
        authority.leases.remove(work_id);
        authority.owners.remove(work_id);
    }

    pub fn workers_polling(&self, env_id: &str, now_ms: u64) -> i64 {
        self.polls
            .lock()
            .get(env_id)
            .map(|polls| {
                polls
                    .values()
                    .filter(|&&polled_at| {
                        now_ms < polled_at || now_ms - polled_at < POLLER_WINDOW_MS
                    })
                    .count() as i64
            })
            .unwrap_or(0)
    }

    pub fn forget_env(&self, env_id: &str, work_ids: &[String]) {
        let mut authority = self.authority.lock();
        for work_id in work_ids {
            authority.leases.remove(work_id);
            authority.owners.remove(work_id);
            authority.epochs.remove(work_id);
        }
        drop(authority);
        self.polls.lock().remove(env_id);
    }
}

/// Loom's model checker needs its own mutex. This mirrors
/// `awaken_sync::LeafLock`'s recovering acquisition, so the models run the
/// production critical sections unchanged.
#[cfg(all(test, feature = "loom"))]
pub(crate) mod loom_lock {
    pub(crate) struct LeafLock<T>(loom::sync::Mutex<T>);

    impl<T: Default> Default for LeafLock<T> {
        fn default() -> Self {
            Self::new(T::default())
        }
    }

    impl<T> LeafLock<T> {
        pub(crate) fn new(value: T) -> Self {
            Self(loom::sync::Mutex::new(value))
        }

        pub(crate) fn lock(&self) -> loom::sync::MutexGuard<'_, T> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn lease_observation_and_reclaim_age_share_the_expiry_boundary() {
        // Causes: C1 observation before/at expiry; C2 TTL zero/positive;
        // C3 reclaim age shorter/equal to TTL; C4 addition saturates near MAX.
        // Effects: E1 a live lease returns the exact complete authority;
        // E2 at the deadline neither liveness nor authority remains;
        // E3 a shorter reclaim age expires independently without rewriting the
        // stored lease; E4 u64 wire deadlines never clamp to the SQL domain.
        // Constraint: this single-thread test uses the production Mutex, while
        // the adjacent Loom scenarios own interleaved snapshot coherence.
        for (start, ttl, age, now, live, reclaim_live) in [
            (0, 0, 0, 0, false, false),
            (10, 10, 10, 19, true, true),
            (10, 10, 10, 20, false, false),
            (10, 10, 5, 15, true, false),
            (u64::MAX - 2, 10, 10, u64::MAX - 1, true, true),
            (u64::MAX - 2, 10, 10, u64::MAX, false, false),
        ] {
            let book = LeaseBook::default();
            assert_eq!(book.claim_for("work", "owner", start, ttl), Ok(1));
            assert_eq!(book.is_leased("work", now), live, "E2 {start}/{now}");
            assert_eq!(
                book.is_leased_with_reclaim_age("work", now, age),
                reclaim_live,
                "E3 {start}/{now}"
            );
            assert_eq!(
                book.authority("work", now),
                live.then(|| ("owner".into(), 1, start.saturating_add(ttl))),
                "E1/E2/E4 {start}/{now}"
            );
        }
    }

    #[test]
    fn reference_snapshot_follows_the_reference_backend_gate() {
        // T1 default product -> snapshot type/method absent and default --lib
        // Clippy is clean. T2 test/test-support -> canonical release_session
        // consumes the SAME coherent snapshot; port conformance owns execution.
        // Reuse the volatile_queue_is_opt_in_and_its_export_is_feature_gated
        // boundary, not another feature, snapshot, or compatibility wrapper.
        let source = include_str!("lease_book.rs").replace("\r\n", "\n");
        let gate = "#[cfg(any(test, feature = \"test-support\"))]";
        assert!(
            source.contains(&format!("{gate}\npub(crate) struct LeaseAuthoritySnapshot")),
            "T1/T2 type gate"
        );
        assert!(
            source.contains(&format!("{gate}\n    pub(crate) fn snapshot(")),
            "T1/T2 method gate"
        );
    }

    #[test]
    fn exhausted_epoch_cannot_install_an_owner() {
        // Cause C1: the canonical epoch reached u64::MAX; effect E1: a new
        // claim fails and no owner is installed. This is the in-memory half of
        // SQL rule X2 and prevents saturating reuse of a fencing token.
        let book = LeaseBook::default();
        book.authority
            .lock()
            .epochs
            .insert("work".into(), Epoch::from_persisted(u64::MAX).unwrap());
        assert_eq!(
            book.claim_for("work", "owner", 0, LEASE_TTL_MS),
            Err(
                awaken_executable_environment_contract::WorkQueueError::LeaseGenerationIntegrity(
                    awaken_lease_contract::fence::EpochIntegrityViolation::Exhausted
                )
            )
        );
        assert!(!book.is_owned_by("work", "owner"), "C1/E1");
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use std::sync::Arc;

    use loom::thread;

    use super::LeaseBook;

    #[test]
    fn reclaim_never_exposes_a_torn_owner_epoch_or_expiry() {
        loom::model(|| {
            let book = Arc::new(LeaseBook::default());
            assert_eq!(book.claim_for("work", "old", 0, 10), Ok(1));

            let releasing = Arc::clone(&book);
            let release = thread::spawn(move || releasing.release("work"));
            let claiming = Arc::clone(&book);
            let claim = thread::spawn(move || claiming.claim_for("work", "new", 1, 20));
            release.join().unwrap();
            assert_eq!(claim.join().unwrap(), Ok(2));

            match book.authority("work", 1) {
                None => {}
                Some((owner, epoch, expires)) => {
                    assert_eq!(owner, "new");
                    assert_eq!(epoch, 2);
                    assert_eq!(expires, 21);
                }
            }
        });
    }

    #[test]
    fn authority_observation_is_one_complete_claim_snapshot() {
        loom::model(|| {
            let book = Arc::new(LeaseBook::default());
            assert_eq!(book.claim_for("work", "old", 0, 10), Ok(1));

            let observing = Arc::clone(&book);
            let observation = thread::spawn(move || observing.authority("work", 1));
            let claiming = Arc::clone(&book);
            let claim = thread::spawn(move || claiming.claim_for("work", "new", 1, 20));

            let observed = observation.join().unwrap().expect("live authority");
            assert!(
                observed == ("old".to_string(), 1, 10) || observed == ("new".to_string(), 2, 21)
            );
            assert_eq!(claim.join().unwrap(), Ok(2));
        });
    }
}
