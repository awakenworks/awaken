//! The cross-restart container reaper — the garbage collector for awaken-managed
//! containers a **crashed** worker left behind.
//!
//! Why this exists (and why it is not a duplicate mechanism): [`SandboxManager`] is the
//! in-process reuse orchestrator (ADR-0056) — it renews/reconciles the sandboxes THIS
//! worker created, from an in-memory registry that dies with the process. So it
//! structurally cannot reap what a process that already crashed left running. The k8s
//! tier does not need a custom reaper: pods carry `ownerReferences`, so native GC
//! deletes them when their owner is deleted. The **docker/podman** tiers have no native
//! lease/TTL, so a container whose owning worker crashed (or a warm-pool instance never
//! claimed) lingers forever. This reaper closes exactly that gap.
//!
//! The decision is a pure value test over the two signals the runtime discovers
//! ([`ManagedContainer`]): the agent (the container's main process) has **exited** — its
//! work is done, brain gone or finished — or the container has outlived a **max age**
//! cap (a hung agent, a leaked warm instance). A young, still-running container is a
//! live session and is never touched. The runtime supplies the clock (`age_secs`), so
//! [`should_reap`] stays a pure function, exhaustively testable without a daemon.
//!
//! [`SandboxManager`]: https://docs.rs/awaken-sandbox-manager

use std::sync::Arc;

use crate::{ContainerRuntime, ManagedContainer};

/// Why a managed container was reaped — surfaced so a caller can log the cause and a
/// test can assert the exact rung that fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapReason {
    /// The agent (main process) exited: the container is finished work.
    Exited,
    /// Still running but older than the max-age cap: an abandoned / hung / leaked
    /// instance whose owning worker never tore it down.
    AgedOut,
}

/// The pure reap decision for one managed container. Reap when the agent has exited
/// (finished work) OR it has outlived `max_age_secs` while still running (abandoned).
/// A still-running container within the age cap is a live session — never reaped.
/// Priority is `Exited` over `AgedOut`: an exited container is unambiguously garbage.
#[must_use]
pub fn should_reap(mc: &ManagedContainer, max_age_secs: u64) -> Option<ReapReason> {
    if !mc.running {
        Some(ReapReason::Exited)
    } else if mc.age_secs > max_age_secs {
        Some(ReapReason::AgedOut)
    } else {
        None
    }
}

/// The default max-age cap for a still-running container (`AWAKEN_SANDBOX_REAP_MAX_AGE`
/// overrides). Deliberately generous — a legitimate long session must not be reaped out
/// from under itself; the primary signal for a normal teardown is `Exited`, and the age
/// cap is only the backstop for a hung agent or a leaked warm instance.
pub const DEFAULT_MAX_AGE_SECS: u64 = 6 * 60 * 60; // 6 hours

/// The default sweep interval (`AWAKEN_SANDBOX_REAP_INTERVAL` overrides).
pub const DEFAULT_INTERVAL_SECS: u64 = 60;

/// The cross-restart reaper over a [`ContainerRuntime`]. Cheap to clone (an `Arc`), so
/// the background loop and a caller can share one.
pub struct SandboxReaper<R: ContainerRuntime> {
    runtime: Arc<R>,
    max_age_secs: u64,
}

impl<R: ContainerRuntime + 'static> SandboxReaper<R> {
    /// A reaper over `runtime` with the given still-running age cap.
    #[must_use]
    pub fn new(runtime: Arc<R>, max_age_secs: u64) -> Self {
        Self {
            runtime,
            max_age_secs,
        }
    }

    /// Read the max-age cap from `AWAKEN_SANDBOX_REAP_MAX_AGE` (seconds), else the
    /// generous default.
    #[must_use]
    pub fn from_env(runtime: Arc<R>) -> Self {
        let max_age_secs = std::env::var("AWAKEN_SANDBOX_REAP_MAX_AGE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_AGE_SECS);
        Self::new(runtime, max_age_secs)
    }

    /// One sweep: discover the managed containers, decide each with [`should_reap`], and
    /// remove the reapable ones. Returns each reaped `(id, reason)`. A `remove` failure
    /// is skipped (not reported) so the next sweep retries it — never a hard error, so a
    /// single bad container can't stall the loop. A runtime with native GC (k8s) returns
    /// no managed containers, making this a no-op there.
    pub async fn sweep(&self) -> Vec<(String, ReapReason)> {
        let managed = match self.runtime.list_managed().await {
            Ok(managed) => managed,
            Err(_) => return Vec::new(),
        };
        let mut reaped = Vec::new();
        for mc in &managed {
            if let Some(reason) = should_reap(mc, self.max_age_secs)
                && self.runtime.remove(&mc.id).await.is_ok()
            {
                reaped.push((mc.id.clone(), reason));
            }
        }
        reaped
    }

    /// Spawn the background reaper: an immediate startup sweep (reaping what a prior
    /// crashed process left behind), then one every `interval`. Returns the task handle;
    /// dropping it does not stop the loop (it is detached for the process lifetime) —
    /// abort the handle to stop it. Off unless a caller spawns it.
    pub fn spawn(self, interval: std::time::Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip missed ticks rather than burst-catch-up after a long sweep.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let reaped = self.sweep().await;
                for (id, reason) in reaped {
                    eprintln!("awaken sandbox reaper: reaped {id} ({reason:?})");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    //! Decision-table + actuation coverage. A recording fake runtime returns a canned
    //! managed set and records every `remove`, so a test asserts exactly which
    //! containers a sweep tears down — with the runtime supplying the clock, so no wall
    //! time is involved.
    use super::*;
    use crate::{AgentChannel, ContainerPlan, ContainerState, RuntimeError};
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn mc(id: &str, running: bool, age_secs: u64) -> ManagedContainer {
        ManagedContainer {
            id: id.into(),
            running,
            age_secs,
        }
    }

    // --- pure decision table (should_reap): running × age vs cap ---
    #[test]
    fn exited_is_reaped_regardless_of_age() {
        assert_eq!(
            should_reap(&mc("x", false, 0), 100),
            Some(ReapReason::Exited)
        );
        assert_eq!(
            should_reap(&mc("x", false, 9_999), 100),
            Some(ReapReason::Exited)
        );
    }

    #[test]
    fn running_within_cap_is_kept() {
        assert_eq!(should_reap(&mc("x", true, 50), 100), None);
        assert_eq!(
            should_reap(&mc("x", true, 100), 100),
            None,
            "exactly at the cap is not yet over it"
        );
    }

    #[test]
    fn running_past_cap_ages_out() {
        assert_eq!(
            should_reap(&mc("x", true, 101), 100),
            Some(ReapReason::AgedOut)
        );
    }

    /// A fake runtime: `list_managed` returns a canned set; `remove` records ids (and
    /// can be made to fail for a chosen id, to prove a failed remove is retried).
    struct FakeRuntime {
        managed: Vec<ManagedContainer>,
        removed: Mutex<Vec<String>>,
        fail_remove: Mutex<std::collections::HashSet<String>>,
    }
    impl FakeRuntime {
        fn new(managed: Vec<ManagedContainer>) -> Arc<Self> {
            Arc::new(Self {
                managed,
                removed: Mutex::new(Vec::new()),
                fail_remove: Mutex::new(std::collections::HashSet::new()),
            })
        }
    }

    #[async_trait]
    impl ContainerRuntime for FakeRuntime {
        async fn create(&self, _id: &str, _plan: &ContainerPlan) -> Result<String, RuntimeError> {
            unreachable!()
        }
        async fn open_channel(&self, _id: &str) -> Result<Box<dyn AgentChannel>, RuntimeError> {
            unreachable!()
        }
        async fn inspect(&self, _id: &str) -> Result<ContainerState, RuntimeError> {
            unreachable!()
        }
        async fn wait(&self, _id: &str) -> Result<pc::ExitStatus, RuntimeError> {
            unreachable!()
        }
        async fn poll(&self, _id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
            unreachable!()
        }
        async fn signal(&self, _id: &str, _s: pc::Signal) -> Result<(), RuntimeError> {
            unreachable!()
        }
        async fn artifacts(&self, _id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
            unreachable!()
        }
        async fn read_artifact(&self, _id: &str, _a: &str) -> Result<Vec<u8>, RuntimeError> {
            unreachable!()
        }
        async fn touch_lease(&self, _id: &str) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn remove(&self, id: &str) -> Result<(), RuntimeError> {
            if self.fail_remove.lock().unwrap().contains(id) {
                return Err(RuntimeError::NotFound(id.into()));
            }
            self.removed.lock().unwrap().push(id.into());
            Ok(())
        }
        async fn list_managed(&self) -> Result<Vec<ManagedContainer>, RuntimeError> {
            Ok(self.managed.clone())
        }
    }

    use awaken_provisioning_contract as pc;

    #[tokio::test]
    async fn sweep_reaps_exited_and_aged_keeps_live() {
        let rt = FakeRuntime::new(vec![
            mc("exited", false, 10),  // agent done → Exited
            mc("hung", true, 10_000), // running but way past cap → AgedOut
            mc("live", true, 30),     // young live session → kept
        ]);
        let reaper = SandboxReaper::new(rt.clone(), 100);
        let mut reaped = reaper.sweep().await;
        reaped.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            reaped,
            vec![
                ("exited".to_string(), ReapReason::Exited),
                ("hung".to_string(), ReapReason::AgedOut),
            ]
        );
        let mut removed = rt.removed.lock().unwrap().clone();
        removed.sort();
        assert_eq!(removed, vec!["exited".to_string(), "hung".to_string()]);
    }

    #[tokio::test]
    async fn a_failed_remove_is_skipped_and_retried_next_sweep() {
        let rt = FakeRuntime::new(vec![mc("stuck", false, 10)]);
        rt.fail_remove.lock().unwrap().insert("stuck".to_string());
        let reaper = SandboxReaper::new(rt.clone(), 100);
        assert!(
            reaper.sweep().await.is_empty(),
            "a failed remove is not reported reaped"
        );
        // The next sweep, with remove now succeeding, tears it down.
        rt.fail_remove.lock().unwrap().clear();
        let reaped = reaper.sweep().await;
        assert_eq!(reaped, vec![("stuck".to_string(), ReapReason::Exited)]);
    }

    #[tokio::test]
    async fn native_gc_runtime_sweep_is_a_noop() {
        // A runtime that reports no managed containers (k8s native GC) → nothing swept.
        let rt = FakeRuntime::new(vec![]);
        let reaper = SandboxReaper::new(rt.clone(), 100);
        assert!(reaper.sweep().await.is_empty());
        assert!(rt.removed.lock().unwrap().is_empty());
    }
}
