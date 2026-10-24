// @formal surface exclude=test_support_crate
// @formal owner crates/server/awaken-dispatch/tests/dispatch_conformance.rs
// @formal reason: Test-only observation of an adapter's authority clock; no production transition.
//! Wait on the clock that issued a persisted lease, independently of host skew.
use std::future::Future;
use std::time::Duration;

use crate::ConformanceClock;

/// A moving store clock observed through the adapter's existing test fixture.
///
/// The reader must query the same authority that issued the lease. A failed
/// observation must fail the test; it must never fall back to the host clock.
pub struct ObservedStoreClock<F>(F);

impl<F> ObservedStoreClock<F> {
    pub fn new(read: F) -> Self {
        Self(read)
    }
}

async fn wait_past<F, R>(
    read: &F,
    deadline_ms: u64,
    budget: Duration,
) -> Result<(), tokio::time::error::Elapsed>
where
    F: Fn() -> R + Send + Sync,
    R: Future<Output = u64> + Send,
{
    tokio::time::timeout(budget, async {
        loop {
            let observed_ms = read().await;
            if observed_ms > deadline_ms {
                return;
            }
            let remaining = deadline_ms.saturating_sub(observed_ms).clamp(1, 250);
            tokio::time::sleep(Duration::from_millis(remaining)).await;
        }
    })
    .await
}

#[async_trait::async_trait]
impl<F, R> ConformanceClock for ObservedStoreClock<F>
where
    F: Fn() -> R + Send + Sync,
    R: Future<Output = u64> + Send,
{
    fn set(&self, _now_ms: u64) {}

    fn exact_boundary_is_controllable(&self) -> bool {
        false
    }

    async fn advance_past(&self, deadline_ms: u64) {
        wait_past(&self.0, deadline_ms, Duration::from_secs(90))
            .await
            .expect("store authority clock did not cross the persisted deadline");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future);
    }

    #[test]
    fn observed_clock_waits_for_strict_authority_progress_despite_host_skew() {
        // C1 all authority samples are far behind the real host epoch; C2
        // authority reaches equality then moves backwards before crossing.
        // R1 C1+C2 => observe all four samples; equality/backwards never ACK.
        let reads = AtomicUsize::new(0);
        let samples = [900, 1_000, 999, 1_001];
        let clock = ObservedStoreClock::new(|| {
            std::future::ready(samples[reads.fetch_add(1, Ordering::SeqCst)])
        });
        assert!(!clock.exact_boundary_is_controllable());
        clock.set(u64::MAX);
        run(clock.advance_past(1_000));
        assert_eq!(reads.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn observed_clock_already_past_needs_one_observation() {
        // R2 authority already crossed => one read, no host-clock condition.
        let reads = AtomicUsize::new(0);
        let clock = ObservedStoreClock::new(|| {
            reads.fetch_add(1, Ordering::SeqCst);
            std::future::ready(2)
        });
        run(clock.advance_past(1));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn observed_clock_stall_and_unresponsive_reader_fail_within_budget() {
        // R3 no progress (including an unrepresentable next millisecond), or
        // R4 an observation that never answers => bounded failure, never skip.
        run(async {
            assert!(
                wait_past(
                    &|| std::future::ready(0),
                    u64::MAX,
                    Duration::from_millis(5)
                )
                .await
                .is_err()
            );
            assert!(
                wait_past(
                    &|| std::future::pending::<u64>(),
                    0,
                    Duration::from_millis(5)
                )
                .await
                .is_err()
            );
        });
    }

    #[test]
    #[should_panic(expected = "configured authority unavailable")]
    fn observed_clock_read_failure_is_not_successful_expiration() {
        // R5 configured authority failure => preserve failure, no wall fallback.
        fn unavailable() -> std::future::Ready<u64> {
            panic!("configured authority unavailable")
        }
        let clock = ObservedStoreClock::new(unavailable);
        run(clock.advance_past(0));
    }
}
