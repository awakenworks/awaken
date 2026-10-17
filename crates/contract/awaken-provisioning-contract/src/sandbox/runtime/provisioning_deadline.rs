//! The provider's deadline over one Sandbox it still reports provisioning.
//!
//! Waiting for a provider is not a failure until the provider's own deadline
//! passes. The provider that owns the asynchronous provisioning measures the
//! deadline from the backend's durable creation record and reads it together
//! with its clock at the observation, so a consumer decides "before or after
//! the deadline" from this value alone and never reads a clock itself.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One provider reading of a provisioning wait: when the provider stops
/// waiting for the Sandbox to become ready, and when it observed the Sandbox
/// still provisioning. Both instants are epoch milliseconds; the deadline
/// follows the one expiry law (`observed < deadline` is still waiting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisioningDeadline {
    deadline_unix_ms: u64,
    observed_at_unix_ms: u64,
}

impl ProvisioningDeadline {
    #[must_use]
    pub const fn new(deadline_unix_ms: u64, observed_at_unix_ms: u64) -> Self {
        Self {
            deadline_unix_ms,
            observed_at_unix_ms,
        }
    }

    #[must_use]
    pub const fn deadline_unix_ms(&self) -> u64 {
        self.deadline_unix_ms
    }

    #[must_use]
    pub const fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }

    /// Whether the observation is still before the deadline, under the one
    /// expiry law.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        awaken_time::Expiry::from_epoch_ms(self.deadline_unix_ms).is_live_at(
            awaken_time::UtcTime::from_epoch_ms(self.observed_at_unix_ms),
        )
    }

    /// The wait left at the observation, or `None` once the deadline passed.
    /// `Some` is never zero, so it is exactly "still within the deadline".
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        awaken_time::Expiry::from_epoch_ms(self.deadline_unix_ms).remaining_at(
            awaken_time::UtcTime::from_epoch_ms(self.observed_at_unix_ms),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_is_some_strictly_before_the_deadline() {
        // Decision table: C1 the observation is before / at / after the
        // deadline. Effect: the remaining wait, `None` from the deadline on
        // (the instant at the deadline already belongs to the failure).
        for (rule, deadline, observed, remaining) in [
            (
                "R1 before",
                10_000,
                4_000,
                Some(Duration::from_millis(6_000)),
            ),
            (
                "R1 one ms before",
                10_000,
                9_999,
                Some(Duration::from_millis(1)),
            ),
            ("R2 at", 10_000, 10_000, None),
            ("R3 after", 10_000, 25_000, None),
            ("R3 epoch deadline", 0, 0, None),
        ] {
            let reading = ProvisioningDeadline::new(deadline, observed);
            assert_eq!(reading.remaining(), remaining, "{rule}");
            assert_eq!(reading.is_pending(), remaining.is_some(), "{rule}");
        }
    }

    #[test]
    fn a_provisioning_observation_carries_its_deadline_on_the_wire() {
        // W1 no deadline omits the member; W2 a deadline round-trips exactly.
        use crate::SandboxObservation;
        for (rule, observation, wire) in [
            (
                "W1",
                SandboxObservation::Provisioning { deadline: None },
                r#"{"provisioning":{}}"#,
            ),
            (
                "W2",
                SandboxObservation::Provisioning {
                    deadline: Some(ProvisioningDeadline::new(9, 4)),
                },
                r#"{"provisioning":{"deadline":{"deadline_unix_ms":9,"observed_at_unix_ms":4}}}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&observation).unwrap(), wire, "{rule}");
            assert_eq!(
                serde_json::from_str::<SandboxObservation>(wire).unwrap(),
                observation,
                "{rule}"
            );
        }
    }
}
