//! One validated composition budget for a provider response and its Runtime watchdog.
//!
//! This is invocation-local timing, never durable identity, a lease or terminal evidence.

use super::Error;
#[cfg(test)]
use super::{DEFAULT_MODEL_ATTEMPT_WATCHDOG, DEFAULT_MODEL_RESPONSE_TIMEOUT};
use crate::model_operation::{ModelExecutionEvidence, NoDispatchProof, PreDispatchStage};
use std::time::Duration;
use tokio::time::Instant;

/// Monotonic scheduler clock for inference policy only. UTC observation and
/// suspend-aware Worker authority clocks retain their existing distinct owners.
pub trait ModelInferenceClock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The one scheduler adapter shared by Runtime and native provider timing.
/// Tokio's controlled clock lets real HTTP tests name elapsed policy time.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioModelInferenceClock;

impl ModelInferenceClock for TokioModelInferenceClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "inference requires a positive response budget strictly before a representable watchdog deadline"
)]
pub struct ModelInferenceBudgetError;

/// The positive gap is the caller's explicit return/journal allowance. The
/// default composition reserves 30 seconds; overrides must configure both layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInferenceBudget {
    response: Duration,
    watchdog: Duration,
}

const fn budget_is_coherent(response_ns: u128, watchdog_ns: u128) -> bool {
    response_ns > 0 && response_ns < watchdog_ns
}

impl ModelInferenceBudget {
    pub fn new(
        response: Duration,
        watchdog: Duration,
    ) -> std::result::Result<Self, ModelInferenceBudgetError> {
        if !budget_is_coherent(response.as_nanos(), watchdog.as_nanos()) {
            return Err(ModelInferenceBudgetError);
        }
        Ok(Self { response, watchdog })
    }

    #[must_use]
    pub const fn response_timeout(self) -> Duration {
        self.response
    }
    #[must_use]
    pub const fn attempt_watchdog(self) -> Duration {
        self.watchdog
    }
    #[must_use]
    pub fn return_allowance(self) -> Duration {
        self.watchdog - self.response
    }
}

/// Runtime starts this once before committing entry. Provider adapters inherit
/// the same start through entry and credential/materialization awaits instead of restarting
/// their response clock after those waits.
#[derive(Debug, Clone, Copy)]
pub struct ModelAttemptTiming {
    started: Instant,
    watchdog: Duration,
    deadline: Instant,
}

impl ModelAttemptTiming {
    pub fn start_at(
        started: Instant,
        watchdog: Duration,
    ) -> std::result::Result<Self, ModelInferenceBudgetError> {
        let deadline = started
            .checked_add(watchdog)
            .filter(|_| !watchdog.is_zero())
            .ok_or(ModelInferenceBudgetError)?;
        Ok(Self {
            started,
            watchdog,
            deadline,
        })
    }

    #[must_use]
    pub const fn watchdog_deadline(self) -> Instant {
        self.deadline
    }

    pub fn response_deadline(
        self,
        response: Duration,
    ) -> std::result::Result<Instant, ModelInferenceBudgetError> {
        let budget = ModelInferenceBudget::new(response, self.watchdog)?;
        self.started
            .checked_add(budget.response_timeout())
            .ok_or(ModelInferenceBudgetError)
    }
}

impl ModelInferenceBudgetError {
    pub fn before_dispatch(self) -> Error {
        Error::Binding(self.to_string()).with_execution_evidence(
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::before_dispatch(
                PreDispatchStage::RequestProjection,
            )),
        )
    }
}

#[cfg(kani)]
#[kani::proof]
fn model_budget_requires_positive_response_before_watchdog() {
    // Actual constructor predicate over the full u128 domain (a superset of
    // Duration nanoseconds). Clock representability, async scheduling, transport
    // and journal return are explicit TCB, not consequences of this scalar law.
    let response: u128 = kani::any();
    let watchdog: u128 = kani::any();
    let admitted = budget_is_coherent(response, watchdog);
    assert_eq!(admitted, response != 0 && watchdog > response);
    if admitted {
        assert!(watchdog - response > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ephemeral_invocation_timing_is_scoped_and_identity_only_calls_mask_it() {
        use crate::model_operation::{
            current_model_attempt_timing, current_model_operation_context,
            with_model_invocation_context, with_model_operation_context,
        };
        // S1 a budget-only ephemeral call => its exact timing, no fabricated
        // durable operation identity. S2 an explicit nested identity-only None
        // scope masks both outer facts. S3 return restores/removes exactly the
        // original scope. The one task-local remains the sole invocation owner.
        let timing = ModelAttemptTiming::start_at(
            ModelInferenceClock::now(&TokioModelInferenceClock),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(current_model_attempt_timing().is_none(), "S3 outside");
        with_model_invocation_context(None, timing, async {
            assert!(
                current_model_operation_context().is_none(),
                "S1 no journal identity"
            );
            assert_eq!(
                current_model_attempt_timing().unwrap().watchdog_deadline(),
                timing.watchdog_deadline(),
                "S1 exact inherited timing"
            );
            with_model_operation_context(None, async {
                assert!(current_model_attempt_timing().is_none(), "S2");
                assert!(current_model_operation_context().is_none(), "S2");
            })
            .await;
            assert_eq!(
                current_model_attempt_timing().unwrap().watchdog_deadline(),
                timing.watchdog_deadline(),
                "S3 restored"
            );
        })
        .await;
        assert!(current_model_attempt_timing().is_none(), "S3 removed");
    }

    #[test]
    fn budget_decision_table_requires_positive_ordered_representable_deadlines() {
        // Causes: C1 response positive; C2 watchdog strictly later; C3 absolute
        // clock addition representable. T1 !C1 or !C2=>configuration refusal;
        // T2 C1+C2=>exact unchanged budgets and positive allowance;
        // T3 !C3=>typed refusal, never clock panic. Default T2 retains 30s.
        for (response, watchdog, accepted) in [
            (0, 0, false),
            (0, 1, false),
            (1, 0, false),
            (1, 1, false),
            (2, 1, false),
            (1, 2, true),
            (u64::MAX - 1, u64::MAX, true),
        ] {
            let budget = ModelInferenceBudget::new(
                Duration::from_nanos(response),
                Duration::from_nanos(watchdog),
            );
            assert_eq!(budget.is_ok(), accepted, "T1/T2 {response}/{watchdog}");
            if let Ok(budget) = budget {
                assert_eq!(budget.response_timeout(), Duration::from_nanos(response));
                assert_eq!(budget.attempt_watchdog(), Duration::from_nanos(watchdog));
                assert_eq!(
                    budget.return_allowance(),
                    Duration::from_nanos(watchdog - response)
                );
            }
        }
        let default = ModelInferenceBudget::new(
            DEFAULT_MODEL_RESPONSE_TIMEOUT,
            DEFAULT_MODEL_ATTEMPT_WATCHDOG,
        )
        .unwrap();
        // T4 a caller supplies a named clock sample => the value preserves that
        // exact origin, rather than reading another clock in its constructor.
        struct FixedClock(Instant);
        impl ModelInferenceClock for FixedClock {
            fn now(&self) -> Instant {
                self.0
            }
        }
        let clock = FixedClock(TokioModelInferenceClock.now());
        let timing = ModelAttemptTiming::start_at(clock.now(), Duration::from_secs(10)).unwrap();
        assert_eq!(
            timing.watchdog_deadline(),
            clock.now() + Duration::from_secs(10),
            "T4"
        );
        assert_eq!(
            timing.response_deadline(Duration::from_secs(9)).unwrap(),
            clock.now() + Duration::from_secs(9),
            "T4"
        );
        assert_eq!(default.return_allowance(), Duration::from_secs(30), "T2");
        assert!(
            ModelAttemptTiming::start_at(
                ModelInferenceClock::now(&TokioModelInferenceClock),
                Duration::ZERO
            )
            .is_err(),
            "T1"
        );
        assert!(
            ModelAttemptTiming::start_at(
                ModelInferenceClock::now(&TokioModelInferenceClock),
                Duration::MAX
            )
            .is_err(),
            "T3"
        );
    }
}
