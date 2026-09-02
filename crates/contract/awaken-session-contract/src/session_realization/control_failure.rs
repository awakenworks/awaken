//! Closed failure classification at the Session realization Control boundary.
//!
//! Keeping the wire error and its one effect policy together prevents embedded,
//! HTTP, and remote Worker adapters from growing parallel retry decisions.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum SessionRealizationControlFailure {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is not ready for this realization phase")]
    NotReady,
    #[error("Session realization retired after its driving work settled")]
    Retired,
    #[error("Session realization is terminal")]
    Terminal,
    #[error("Session realization ownership is stale")]
    StaleOwnership,
    #[error("Session changed concurrently")]
    Conflict,
    #[error("Session realization command is invalid: {0}")]
    Invalid(String),
    #[error("Session realization service is unavailable: {0}")]
    Unavailable(String),
}

/// Stable effect class for one Control failure at a Worker/Run boundary.
///
/// The Session contract owns this classification so HTTP, embedded, and remote
/// Workers cannot independently reinterpret the same durable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRealizationControlDisposition {
    NotReady,
    Retryable,
    Terminal,
}

impl SessionRealizationControlFailure {
    #[must_use]
    pub const fn disposition(&self) -> SessionRealizationControlDisposition {
        match self {
            Self::NotReady => SessionRealizationControlDisposition::NotReady,
            Self::StaleOwnership | Self::Conflict | Self::Unavailable(_) => {
                SessionRealizationControlDisposition::Retryable
            }
            Self::NotFound | Self::Retired | Self::Terminal | Self::Invalid(_) => {
                SessionRealizationControlDisposition::Terminal
            }
        }
    }

    /// Whether this reply conclusively denies the caller's exact current
    /// realization owner/fence. A retryable Run may be relinquished and
    /// executed by a newer owner, while the stale physical projection itself
    /// must stop immediately. NotReady, Conflict, and Unavailable do not prove
    /// that fact; their prior unexpired lease remains authoritative.
    #[must_use]
    pub const fn proves_current_realization_cannot_continue(&self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::Retired
                | Self::Terminal
                | Self::StaleOwnership
                | Self::Invalid(_)
        )
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_realization_control_failure_disposition_is_total_exact_and_fail_closed() {
    let selector = kani::any::<u8>() % 8;
    let failure = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        _ => SessionRealizationControlFailure::Unavailable(String::new()),
    };
    // This oracle is deliberately derived from the symbolic variant selector,
    // not from `failure.disposition()`: changing the production table cannot
    // change the expected result at the same time.
    let expected = match selector {
        1 => SessionRealizationControlDisposition::NotReady,
        4 | 5 | 7 => SessionRealizationControlDisposition::Retryable,
        _ => SessionRealizationControlDisposition::Terminal,
    };

    assert_eq!(failure.disposition(), expected);
}

#[cfg(kani)]
#[kani::proof]
fn realization_ownership_loss_proof_is_total_and_exact() {
    let selector = kani::any::<u8>() % 8;
    let failure = match selector {
        0 => SessionRealizationControlFailure::NotFound,
        1 => SessionRealizationControlFailure::NotReady,
        2 => SessionRealizationControlFailure::Retired,
        3 => SessionRealizationControlFailure::Terminal,
        4 => SessionRealizationControlFailure::StaleOwnership,
        5 => SessionRealizationControlFailure::Conflict,
        6 => SessionRealizationControlFailure::Invalid(String::new()),
        _ => SessionRealizationControlFailure::Unavailable(String::new()),
    };
    assert_eq!(
        failure.proves_current_realization_cannot_continue(),
        matches!(selector, 0 | 2 | 3 | 4 | 6)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realization_control_failures_round_trip_without_semantic_loss() {
        // Cause/effect graph: C1 transient backpressure, C2 retryable authority
        // or dependency failure, and C3 an absent/terminal/invalid aggregate.
        // Effects: E1 defer without crash accounting, E2 relinquish for the
        // existing retry path, and E3 absorb the Run instead of hot-looping.
        // Every cause also round-trips over the existing typed transport.
        //
        // | Rule | cause | disposition | current owner disproved |
        // | T1 | not ready | NotReady | no |
        // | T2a | stale | Retryable | yes |
        // | T2b | conflict/unavailable | Retryable | no |
        // | T3 | not found/retired/terminal/invalid | Terminal | yes |
        for (rule, failure, disposition, owner_disproved) in [
            (
                "T3 not found",
                SessionRealizationControlFailure::NotFound,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T1 not ready",
                SessionRealizationControlFailure::NotReady,
                SessionRealizationControlDisposition::NotReady,
                false,
            ),
            (
                "T3 retired",
                SessionRealizationControlFailure::Retired,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T3 terminal",
                SessionRealizationControlFailure::Terminal,
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T2a stale",
                SessionRealizationControlFailure::StaleOwnership,
                SessionRealizationControlDisposition::Retryable,
                true,
            ),
            (
                "T2b conflict",
                SessionRealizationControlFailure::Conflict,
                SessionRealizationControlDisposition::Retryable,
                false,
            ),
            (
                "T3 invalid",
                SessionRealizationControlFailure::Invalid("bad target".into()),
                SessionRealizationControlDisposition::Terminal,
                true,
            ),
            (
                "T2b unavailable",
                SessionRealizationControlFailure::Unavailable("control offline".into()),
                SessionRealizationControlDisposition::Retryable,
                false,
            ),
        ] {
            let wire = serde_json::to_value(&failure).expect(rule);
            let decoded: SessionRealizationControlFailure =
                serde_json::from_value(wire).expect(rule);
            assert_eq!(decoded, failure, "{rule}");
            assert_eq!(decoded.disposition(), disposition, "{rule}");
            assert_eq!(
                decoded.proves_current_realization_cannot_continue(),
                owner_disproved,
                "{rule}"
            );
        }
    }
}
