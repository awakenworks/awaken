//! Failure disposition: what a router should DO with a failed inference,
//! projected from the error classification.
//!
//! A neutral value object — it names the *decision* (retry the same call, fail
//! over to another candidate and cool this one, or give up), never the mechanism
//! that carries it out. The projection is authored here once so the routing and
//! cooldown code stops re-deriving "is this a 429?" from an [`Error`] variant.
//!
//! This module is additive: [`Error::is_retryable`](crate::llm::Error::is_retryable)
//! stays the authoritative retry gate for the same-call retry loop; [`Disposition`]
//! is consumed by candidate-failover / credential-cooldown, which choose a *different*
//! binding rather than retrying the identical request.

use std::time::Duration;

use crate::llm::Error;

/// The failover disposition of an inference failure.
///
/// Three states, collapsing the finer [`Error`] taxonomy onto the only distinction
/// a router acts on:
/// - [`Transient`](Self::Transient): retrying the *same* binding may succeed.
/// - [`Quota`](Self::Quota): a rate/quota signal *against this credential-identity*
///   — prefer another candidate and cool this one until `retry_after`.
/// - [`Permanent`](Self::Permanent): retrying the identical request cannot succeed;
///   only a different binding or an authored change can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// A transport blip, timeout, or provider overload — the same call may work
    /// on retry.
    Transient,
    /// A rate-limit / quota exhaustion attributable to the credential-identity.
    /// `retry_after` is the provider's hint for when it may clear (`None` when
    /// unknown), used as the cooldown deadline.
    Quota { retry_after: Option<Duration> },
    /// A permanent rejection of this request shape (bad binding, context overflow,
    /// auth failure, missing model, content filter).
    Permanent,
}

impl Disposition {
    /// Whether retrying the *same* binding is worthwhile. Only [`Transient`] is —
    /// a quota signal wants a different identity, a permanent error wants a
    /// different request.
    ///
    /// [`Transient`]: Self::Transient
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Disposition::Transient)
    }

    /// Whether a router should prefer failing over to another candidate rather
    /// than retrying in place. True for a quota signal (cool this identity) and a
    /// permanent error (this binding cannot serve the request).
    #[must_use]
    pub fn prefers_failover(self) -> bool {
        matches!(self, Disposition::Quota { .. } | Disposition::Permanent)
    }

    /// The cooldown deadline hint carried by a [`Quota`](Self::Quota) signal; `None`
    /// for every other disposition.
    #[must_use]
    pub fn retry_after(self) -> Option<Duration> {
        match self {
            Disposition::Quota { retry_after } => retry_after,
            _ => None,
        }
    }
}

/// Project a failure onto its routing [`Disposition`]. Implemented for the
/// runtime's inference [`Error`]; other fallible sources can opt in without the
/// router learning their error type.
pub trait Classify {
    /// The failover disposition of this failure.
    fn disposition(&self) -> Disposition;
}

impl Classify for Error {
    fn disposition(&self) -> Disposition {
        match self {
            // Transient: retrying the same binding may succeed.
            Error::Provider(_) | Error::Timeout(_) => Disposition::Transient,
            // Quota: a rate/quota signal against the identity. `Overloaded` is a
            // provider-capacity 429/529 — treated as a (short) quota signal so a
            // router spreads off the hot endpoint rather than hammering it.
            Error::RateLimited { retry_after, .. } | Error::Overloaded { retry_after, .. } => {
                Disposition::Quota {
                    retry_after: *retry_after,
                }
            }
            Error::UsageLimit { reset_after, .. } => Disposition::Quota {
                retry_after: *reset_after,
            },
            // Permanent: the identical request cannot succeed on any retry.
            Error::Binding(_)
            | Error::ContextOverflow(_)
            | Error::InvalidRequest(_)
            | Error::Unauthorized(_)
            | Error::LoginRequired(_)
            | Error::ModelNotFound(_)
            | Error::ContentFiltered(_) => Disposition::Permanent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors_retry_in_place() {
        assert_eq!(
            Error::Provider("5xx".into()).disposition(),
            Disposition::Transient
        );
        assert_eq!(
            Error::Timeout("slow".into()).disposition(),
            Disposition::Transient
        );
        assert!(Disposition::Transient.is_retryable());
        assert!(!Disposition::Transient.prefers_failover());
        assert_eq!(Disposition::Transient.retry_after(), None);
    }

    #[test]
    fn rate_limit_is_a_quota_signal_carrying_the_retry_hint() {
        let d = Error::RateLimited {
            message: "429".into(),
            retry_after: Some(Duration::from_secs(30)),
        }
        .disposition();
        assert_eq!(
            d,
            Disposition::Quota {
                retry_after: Some(Duration::from_secs(30))
            }
        );
        assert!(!d.is_retryable());
        assert!(d.prefers_failover());
        assert_eq!(d.retry_after(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn overload_is_quota_without_a_hint_when_none_sent() {
        let d = Error::Overloaded {
            message: "529".into(),
            retry_after: None,
        }
        .disposition();
        assert_eq!(d, Disposition::Quota { retry_after: None });
        assert!(d.prefers_failover());
    }

    #[test]
    fn hard_usage_limit_is_quota_with_reset_window() {
        let d = Error::UsageLimit {
            message: "weekly".into(),
            reset_after: Some(Duration::from_secs(3600)),
        }
        .disposition();
        assert_eq!(
            d,
            Disposition::Quota {
                retry_after: Some(Duration::from_secs(3600))
            }
        );
    }

    #[test]
    fn auth_context_and_model_errors_are_permanent() {
        for e in [
            Error::Unauthorized("401".into()),
            Error::LoginRequired("expired".into()),
            Error::ModelNotFound("404".into()),
            Error::ContextOverflow("too long".into()),
            Error::InvalidRequest("400".into()),
            Error::ContentFiltered("blocked".into()),
            Error::Binding("bad".into()),
        ] {
            assert_eq!(e.disposition(), Disposition::Permanent, "{e:?}");
            assert!(!e.disposition().is_retryable());
            assert!(e.disposition().prefers_failover());
        }
    }
}
