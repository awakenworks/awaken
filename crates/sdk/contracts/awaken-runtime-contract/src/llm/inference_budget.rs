// @formal entrypoint llm.inference_budget_requires_positive_response_before_actual_watchdog
//! The caller-declared inference budget and the per-attempt timing derived from it.
//!
//! [`ModelInferenceBudget`] is the one value a caller (a Runtime host, a
//! credential probe, a test) declares; it replaces the separately owned
//! response timeout, idle window and Runtime watchdog constants. Runtime turns
//! it into one logical deadline for the whole invocation (every retry
//! included) and one [`ModelAttemptTiming`] per attempt; provider adapters only
//! read that timing. This is invocation-local timing, never durable identity,
//! a lease or terminal evidence.

use super::Error;
use crate::model_operation::{ModelExecutionEvidence, NoDispatchProof};
use awaken_time::mono::{Instant, MonoDeadline};
use std::time::Duration;

// Inference policy reads the one monotonic point, `awaken_time::mono`:
// tokio's controlled clock, so a paused test runtime names elapsed policy time.
// UTC observation and suspend-aware Worker authority keep their own owners.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "inference requires 0 < idle <= attempt response <= total, a positive return allowance and representable deadlines"
)]
pub struct ModelInferenceBudgetError;

/// One caller-declared inference budget.
///
/// - `total`: the logical deadline of one model request, across every retry
///   and backoff pause;
/// - `attempt_response`: how long one provider attempt may take to open and
///   consume its complete response;
/// - `idle`: how long a streaming response may go without useful progress
///   (and how long connection establishment may take);
/// - `return_allowance`: how long past its response deadline Runtime waits
///   for an attempt to return its own typed observation before its watchdog
///   abandons it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInferenceBudget {
    total: Duration,
    attempt_response: Duration,
    idle: Duration,
    return_allowance: Duration,
}

/// The constructor predicate, over nanoseconds so Kani can exhaust it.
const fn budget_is_coherent(
    total_ns: u128,
    attempt_response_ns: u128,
    idle_ns: u128,
    return_allowance_ns: u128,
) -> bool {
    idle_ns > 0
        && idle_ns <= attempt_response_ns
        && attempt_response_ns <= total_ns
        && return_allowance_ns > 0
}

impl ModelInferenceBudget {
    /// The default composition. One attempt may reason for twenty minutes
    /// before its complete response; a stream may stay silent for five
    /// (long-context prefill and reasoning emit nothing for minutes); the
    /// whole request, retries included, gets three attempts' worth of time;
    /// the Runtime watchdog waits thirty seconds for a late typed return.
    pub const DEFAULT: Self = Self {
        total: Duration::from_secs(3 * 1_200),
        attempt_response: Duration::from_secs(1_200),
        idle: Duration::from_secs(300),
        return_allowance: Duration::from_secs(30),
    };

    pub fn new(
        total: Duration,
        attempt_response: Duration,
        idle: Duration,
        return_allowance: Duration,
    ) -> std::result::Result<Self, ModelInferenceBudgetError> {
        if !budget_is_coherent(
            total.as_nanos(),
            attempt_response.as_nanos(),
            idle.as_nanos(),
            return_allowance.as_nanos(),
        ) || attempt_response.checked_add(return_allowance).is_none()
        {
            return Err(ModelInferenceBudgetError);
        }
        Ok(Self {
            total,
            attempt_response,
            idle,
            return_allowance,
        })
    }

    #[must_use]
    pub const fn total(self) -> Duration {
        self.total
    }
    #[must_use]
    pub const fn attempt_response(self) -> Duration {
        self.attempt_response
    }
    #[must_use]
    pub const fn idle(self) -> Duration {
        self.idle
    }
    #[must_use]
    pub const fn return_allowance(self) -> Duration {
        self.return_allowance
    }

    /// Connection establishment produces no response progress, so it is
    /// bounded by the same silence window as a stalled stream.
    #[must_use]
    pub const fn connect_timeout(self) -> Duration {
        self.idle
    }

    /// The logical deadline of one model request started at `started`.
    pub fn logical_deadline(
        self,
        started: Instant,
    ) -> std::result::Result<MonoDeadline, ModelInferenceBudgetError> {
        started
            .checked_add(self.total)
            .map(MonoDeadline::at)
            .ok_or(ModelInferenceBudgetError)
    }
}

/// The timing of one provider attempt. Runtime starts it once before
/// committing entry; provider adapters inherit it through entry and
/// credential/materialization awaits instead of restarting a clock, and only
/// read it.
#[derive(Debug, Clone, Copy)]
pub struct ModelAttemptTiming {
    response_deadline: MonoDeadline,
    watchdog_deadline: MonoDeadline,
    idle: Duration,
}

impl ModelAttemptTiming {
    /// The attempt must return its complete response by
    /// `min(started + attempt_response, logical_deadline)`; Runtime's
    /// watchdog abandons it `return_allowance` later.
    pub fn start_at(
        started: Instant,
        budget: ModelInferenceBudget,
        logical_deadline: MonoDeadline,
    ) -> std::result::Result<Self, ModelInferenceBudgetError> {
        let response_deadline = started
            .checked_add(budget.attempt_response)
            .ok_or(ModelInferenceBudgetError)?
            .min(logical_deadline.instant());
        let watchdog_deadline = response_deadline
            .checked_add(budget.return_allowance)
            .ok_or(ModelInferenceBudgetError)?;
        Ok(Self {
            response_deadline: MonoDeadline::at(response_deadline),
            watchdog_deadline: MonoDeadline::at(watchdog_deadline),
            idle: budget.idle,
        })
    }

    /// A single attempt with no Runtime around it (a direct adapter call):
    /// its logical deadline is the budget's own total.
    pub fn standalone(
        started: Instant,
        budget: ModelInferenceBudget,
    ) -> std::result::Result<Self, ModelInferenceBudgetError> {
        Self::start_at(started, budget, budget.logical_deadline(started)?)
    }

    #[must_use]
    pub const fn response_deadline(self) -> MonoDeadline {
        self.response_deadline
    }
    #[must_use]
    pub const fn watchdog_deadline(self) -> MonoDeadline {
        self.watchdog_deadline
    }
    #[must_use]
    pub const fn idle(self) -> Duration {
        self.idle
    }
}

impl ModelInferenceBudgetError {
    pub fn before_dispatch(self) -> Error {
        Error::Binding(self.to_string()).with_execution_evidence(
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::request_projection()),
        )
    }
}

#[cfg(kani)]
/// @obligation llm.inference_budget_requires_positive_response_before_actual_watchdog
#[kani::proof]
fn model_budget_requires_positive_response_before_watchdog() {
    // Actual constructor predicate over the full u128 domain (a superset of
    // Duration nanoseconds). Clock representability, async scheduling,
    // transport and journal return are explicit TCB, not consequences of this
    // scalar law.
    let total: u128 = kani::any();
    let attempt: u128 = kani::any();
    let idle: u128 = kani::any();
    let allowance: u128 = kani::any();
    let admitted = budget_is_coherent(total, attempt, idle, allowance);
    assert_eq!(
        admitted,
        idle != 0 && idle <= attempt && attempt <= total && allowance != 0
    );
    if admitted {
        // A positive response budget strictly precedes the watchdog, and no
        // stream silence window outlives its attempt or the whole request.
        assert!(attempt > 0);
        assert!(
            attempt
                .checked_add(allowance)
                .is_none_or(|watchdog| watchdog > attempt)
        );
        assert!(idle <= total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

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
        let timing =
            ModelAttemptTiming::standalone(awaken_time::mono::now(), ModelInferenceBudget::DEFAULT)
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
    fn budget_decision_table_requires_ordered_positive_windows() {
        // Causes: total T, attempt response A, idle I, return allowance R.
        // Effect: admitted or refused. Rule: admitted <=> 0 < I <= A <= T and
        // R > 0 and A + R representable; an admitted budget keeps its exact
        // windows.
        let max = Duration::MAX;
        for (row, total, attempt, idle, allowance, admitted) in [
            ("zero idle", 3, 2, 0, 1, false),
            ("idle beyond attempt", 3, 2, 3, 1, false),
            ("attempt beyond total", 1, 2, 1, 1, false),
            ("zero allowance", 3, 2, 1, 0, false),
            ("minimal", 1, 1, 1, 1, true),
            ("ordered", 9, 3, 2, 1, true),
        ] {
            let budget =
                ModelInferenceBudget::new(total * MS, attempt * MS, idle * MS, allowance * MS);
            assert_eq!(budget.is_ok(), admitted, "{row}");
            if let Ok(budget) = budget {
                assert_eq!(
                    (
                        budget.total(),
                        budget.attempt_response(),
                        budget.idle(),
                        budget.return_allowance(),
                    ),
                    (total * MS, attempt * MS, idle * MS, allowance * MS),
                    "{row}"
                );
            }
        }
        assert!(
            ModelInferenceBudget::new(max, max, MS, MS).is_err(),
            "unrepresentable watchdog span"
        );
    }

    #[test]
    fn default_budget_relations() {
        // The default keeps these relations, not particular numbers: the whole
        // request spans three attempts; a complete response outlasts four
        // silence windows (long reasoning prefill); the watchdog waits a
        // positive allowance past the response deadline; connection
        // establishment is bounded by the silence window.
        let default = ModelInferenceBudget::DEFAULT;
        assert_eq!(
            ModelInferenceBudget::new(
                default.total(),
                default.attempt_response(),
                default.idle(),
                default.return_allowance()
            ),
            Ok(default),
            "DEFAULT is itself admitted"
        );
        assert_eq!(default.total(), default.attempt_response() * 3);
        assert!(default.idle() * 4 <= default.attempt_response());
        assert!(!default.return_allowance().is_zero());
        assert_eq!(default.connect_timeout(), default.idle());
    }

    #[test]
    fn attempt_timing_decision_table() {
        // Causes: attempt start S, budget (A, R), logical deadline L. Effects:
        // response deadline = min(S + A, L); watchdog = response + R; idle is
        // the budget's; an unrepresentable instant is a typed refusal, never a
        // clock panic.
        let start = awaken_time::mono::now();
        let budget = ModelInferenceBudget::new(100 * MS, 10 * MS, 5 * MS, 2 * MS).unwrap();
        for (row, logical, response) in [
            ("attempt window first", 100, 10),
            ("logical deadline first", 4, 4),
            ("logical deadline already spent", 0, 0),
        ] {
            let timing =
                ModelAttemptTiming::start_at(start, budget, MonoDeadline::at(start + logical * MS))
                    .unwrap();
            assert_eq!(
                timing.response_deadline().instant(),
                start + response * MS,
                "{row}"
            );
            assert_eq!(
                timing.watchdog_deadline().instant(),
                start + (response + 2) * MS,
                "{row}"
            );
            assert_eq!(timing.idle(), 5 * MS, "{row}");
        }
        let standalone = ModelAttemptTiming::standalone(start, budget).unwrap();
        assert_eq!(
            standalone.response_deadline().instant(),
            start + 10 * MS,
            "standalone"
        );
        let huge = ModelInferenceBudget::new(Duration::MAX, Duration::MAX / 2, MS, MS).unwrap();
        assert!(
            ModelAttemptTiming::standalone(start, huge).is_err(),
            "unrepresentable"
        );
        assert!(huge.logical_deadline(start).is_err(), "unrepresentable");
    }
}
