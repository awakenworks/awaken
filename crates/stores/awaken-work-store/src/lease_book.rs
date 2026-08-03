//! Process-local lease and poll-liveness bookkeeping shared by WorkQueue backends.
//!
//! Durable backends keep lease ownership and expiry in database rows; they use this
//! book only for the explicitly ephemeral `workers_polling` observation. The
//! test-support in-memory backend additionally uses its lease/owner maps as its
//! executable reference model.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The lease window used by the reference backend.
pub const LEASE_TTL_MS: u64 = 60_000;
/// A worker counts as polling while its last poll is within this window.
pub const POLLER_WINDOW_MS: u64 = 30_000;

/// Shared process-local bookkeeping. This is not durable lease authority.
#[derive(Default)]
pub struct LeaseBook {
    leases: Mutex<BTreeMap<String, LeaseWindow>>,
    owners: Mutex<BTreeMap<String, String>>,
    polls: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
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
        self.leases
            .lock()
            .unwrap()
            .get(work_id)
            .is_some_and(|lease| lease.expires_at_ms > now_ms)
    }

    pub fn is_leased_with_reclaim_age(&self, work_id: &str, now_ms: u64, age_ms: u64) -> bool {
        self.leases
            .lock()
            .unwrap()
            .get(work_id)
            .is_some_and(|lease| now_ms < lease.refreshed_at_ms.saturating_add(age_ms))
    }

    pub fn lease(&self, work_id: &str, now_ms: u64) {
        self.lease_for(work_id, now_ms, LEASE_TTL_MS);
    }

    pub fn own(&self, work_id: &str, worker_id: &str) {
        self.owners
            .lock()
            .unwrap()
            .insert(work_id.to_string(), worker_id.to_string());
    }

    pub fn is_owned_by(&self, work_id: &str, worker_id: &str) -> bool {
        self.owners
            .lock()
            .unwrap()
            .get(work_id)
            .is_some_and(|owner| owner == worker_id)
    }

    pub fn lease_for(&self, work_id: &str, now_ms: u64, ttl_ms: u64) {
        self.leases.lock().unwrap().insert(
            work_id.to_string(),
            LeaseWindow {
                refreshed_at_ms: now_ms,
                expires_at_ms: now_ms.saturating_add(ttl_ms),
            },
        );
    }

    pub fn release(&self, work_id: &str) {
        self.leases.lock().unwrap().remove(work_id);
        self.owners.lock().unwrap().remove(work_id);
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
        let mut leases = self.leases.lock().unwrap();
        for work_id in work_ids {
            leases.remove(work_id);
        }
        let mut owners = self.owners.lock().unwrap();
        for work_id in work_ids {
            owners.remove(work_id);
        }
        self.polls.lock().unwrap().remove(env_id);
    }
}
