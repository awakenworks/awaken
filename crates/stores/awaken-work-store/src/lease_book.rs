//! Process-local lease and poll-liveness bookkeeping shared by WorkQueue backends.
//!
//! Durable backends keep lease ownership and expiry in database rows; they use this
//! book only for the explicitly ephemeral `workers_polling` observation. The
//! test-support in-memory backend additionally uses its lease/owner maps as its
//! executable reference model.

use std::collections::BTreeMap;

#[cfg(all(test, feature = "loom"))]
use loom::sync::Mutex;
#[cfg(not(all(test, feature = "loom")))]
use std::sync::Mutex;

/// The lease window used by the reference backend.
pub const LEASE_TTL_MS: u64 = 60_000;
/// A worker counts as polling while its last poll is within this window.
pub const POLLER_WINDOW_MS: u64 = 30_000;

/// Shared process-local bookkeeping. This is not durable lease authority.
#[derive(Default)]
pub struct LeaseBook {
    authority: Mutex<LeaseAuthority>,
    polls: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
}

#[derive(Default)]
struct LeaseAuthority {
    leases: BTreeMap<String, LeaseWindow>,
    owners: BTreeMap<String, String>,
    epochs: BTreeMap<String, u64>,
}

#[derive(Clone, Copy)]
struct LeaseWindow {
    refreshed_at_ms: u64,
    expires_at_ms: u64,
}

impl LeaseBook {
    pub fn record_poll(&self, env_id: &str, worker_id: &str, now_ms: u64) {
        self.polls
            .lock()
            .unwrap()
            .entry(env_id.to_string())
            .or_default()
            .insert(worker_id.to_string(), now_ms);
    }

    pub fn is_leased(&self, work_id: &str, now_ms: u64) -> bool {
        self.authority
            .lock()
            .unwrap()
            .leases
            .get(work_id)
            .is_some_and(|lease| lease.expires_at_ms > now_ms)
    }

    pub fn is_leased_with_reclaim_age(&self, work_id: &str, now_ms: u64, age_ms: u64) -> bool {
        self.authority
            .lock()
            .unwrap()
            .leases
            .get(work_id)
            .is_some_and(|lease| now_ms < lease.refreshed_at_ms.saturating_add(age_ms))
    }

    pub fn lease(&self, work_id: &str, now_ms: u64) {
        self.lease_for(work_id, now_ms, LEASE_TTL_MS);
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
    ) -> Result<u64, &'static str> {
        let mut authority = self.authority.lock().unwrap();
        let epoch = authority.epochs.entry(work_id.to_string()).or_default();
        *epoch = epoch.checked_add(1).ok_or("work lease epoch exhausted")?;
        let epoch = *epoch;
        authority
            .owners
            .insert(work_id.to_string(), worker_id.to_string());
        authority.leases.insert(
            work_id.to_string(),
            LeaseWindow {
                refreshed_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
        );
        Ok(epoch)
    }

    pub fn is_owned_by(&self, work_id: &str, worker_id: &str) -> bool {
        self.authority
            .lock()
            .unwrap()
            .owners
            .get(work_id)
            .is_some_and(|owner| owner == worker_id)
    }

    pub fn authority(&self, work_id: &str, now_ms: u64) -> Option<(String, u64, u64)> {
        let authority = self.authority.lock().unwrap();
        let lease = authority.leases.get(work_id).copied()?;
        if lease.expires_at_ms <= now_ms {
            return None;
        }
        let owner = authority.owners.get(work_id).cloned()?;
        let epoch = authority.epochs.get(work_id).copied()?;
        Some((owner, epoch, lease.expires_at_ms))
    }

    pub fn lease_for(&self, work_id: &str, now_ms: u64, ttl_ms: u64) {
        self.authority.lock().unwrap().leases.insert(
            work_id.to_string(),
            LeaseWindow {
                refreshed_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
        );
    }

    pub fn release(&self, work_id: &str) {
        let mut authority = self.authority.lock().unwrap();
        authority.leases.remove(work_id);
        authority.owners.remove(work_id);
    }

    /// Remove authority only when both owner and fencing epoch still identify
    /// the observed claim. This is the in-memory compare-and-set counterpart of
    /// the durable stores' conditional compensation update.
    pub fn release_exact(&self, work_id: &str, owner: &str, epoch: u64) -> bool {
        let mut authority = self.authority.lock().unwrap();
        let matches = authority
            .owners
            .get(work_id)
            .is_some_and(|current| current == owner)
            && authority.epochs.get(work_id).copied() == Some(epoch);
        if matches {
            authority.leases.remove(work_id);
            authority.owners.remove(work_id);
        }
        matches
    }

    pub fn workers_polling(&self, env_id: &str, now_ms: u64) -> i64 {
        self.polls
            .lock()
            .unwrap()
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
        let mut authority = self.authority.lock().unwrap();
        for work_id in work_ids {
            authority.leases.remove(work_id);
            authority.owners.remove(work_id);
            authority.epochs.remove(work_id);
        }
        drop(authority);
        self.polls.lock().unwrap().remove(env_id);
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn exhausted_epoch_cannot_install_an_owner() {
        // Cause C1: the canonical epoch reached u64::MAX; effect E1: a new
        // claim fails and no owner is installed. This is the in-memory half of
        // SQL rule X2 and prevents saturating reuse of a fencing token.
        let book = LeaseBook::default();
        book.authority
            .lock()
            .unwrap()
            .epochs
            .insert("work".into(), u64::MAX);
        assert_eq!(
            book.claim_for("work", "owner", 0, LEASE_TTL_MS),
            Err("work lease epoch exhausted")
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
