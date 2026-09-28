//! Typed provisioning failure shared by every Sandbox provider and consumer.

use awaken_agent_contract::resilience::{Classify, Disposition};

/// Provisioning failure, classified by what a caller may do next.
///
/// Callers branch on the variant, never on the detail text. Every variant keeps
/// the historical `sandbox error: …` rendering that operators, logs, and
/// transcripts already show; the detail is operator evidence only.
///
/// The classes are the distinctions provider and host callers act on:
/// an absent target ([`NotFound`](Self::NotFound)) versus one that existed and
/// is definitively over ([`Gone`](Self::Gone)); a transitional state that will
/// settle ([`NotReady`](Self::NotReady)) versus evidence that contradicts the
/// caller's exact expectation ([`Conflict`](Self::Conflict)); a lost or silent
/// channel ([`Transport`](Self::Transport), [`Timeout`](Self::Timeout)); an
/// authority refusal ([`Unauthorized`](Self::Unauthorized)) versus a stale or
/// missing effect fence ([`FenceRejected`](Self::FenceRejected)); a malformed
/// request ([`Invalid`](Self::Invalid)); a capability this provider or tier
/// does not have ([`Unsupported`](Self::Unsupported)); and a genuine failure of
/// the backend mechanism itself ([`Backend`](Self::Backend)).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// The addressed Sandbox, lease, reference, participant, or evidence does
    /// not exist.
    #[error("sandbox error: {0}")]
    NotFound(String),
    /// The addressed target existed but is definitively over: disposed,
    /// consumed, expired capability, or shut down.
    #[error("sandbox error: {0}")]
    Gone(String),
    /// The target is in a transitional state (provisioning, busy, unsettled
    /// cleanup); the same request may succeed once it settles.
    #[error("sandbox error: {0}")]
    NotReady(String),
    /// The channel to the backend or workload failed or lost its response.
    #[error("sandbox error: {0}")]
    Transport(String),
    /// A bounded wait elapsed before the backend answered.
    #[error("sandbox error: {0}")]
    Timeout(String),
    /// The current authority refused the effect.
    #[error("sandbox error: {0}")]
    Unauthorized(String),
    /// The effect fence or realization lease is stale, expired, missing, or not
    /// a successor of the admitted effect; only fresh authority can proceed.
    #[error("sandbox error: {0}")]
    FenceRejected(String),
    /// The request, spec, path, or persisted evidence is malformed.
    #[error("sandbox error: {0}")]
    Invalid(String),
    /// This provider, tier, or legacy handle does not have the capability.
    #[error("sandbox error: {0}")]
    Unsupported(String),
    /// Observed state contradicts the caller's exact expectation.
    #[error("sandbox error: {0}")]
    Conflict(String),
    /// The backend mechanism failed while performing `operation`.
    #[error("sandbox error: {operation}: {detail}")]
    Backend {
        operation: &'static str,
        detail: String,
    },
}

impl SandboxError {
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::NotFound(detail.into())
    }

    pub fn gone(detail: impl Into<String>) -> Self {
        Self::Gone(detail.into())
    }

    pub fn not_ready(detail: impl Into<String>) -> Self {
        Self::NotReady(detail.into())
    }

    pub fn transport(detail: impl Into<String>) -> Self {
        Self::Transport(detail.into())
    }

    pub fn timeout(detail: impl Into<String>) -> Self {
        Self::Timeout(detail.into())
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::Unauthorized(detail.into())
    }

    pub fn fence_rejected(detail: impl Into<String>) -> Self {
        Self::FenceRejected(detail.into())
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid(detail.into())
    }

    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::Unsupported(detail.into())
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict(detail.into())
    }

    /// A genuine backend failure; `cause` is rendered once into the detail.
    pub fn backend(operation: &'static str, cause: impl std::fmt::Display) -> Self {
        Self::Backend {
            operation,
            detail: cause.to_string(),
        }
    }

    /// Operator detail without the class or operation.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::NotFound(detail)
            | Self::Gone(detail)
            | Self::NotReady(detail)
            | Self::Transport(detail)
            | Self::Timeout(detail)
            | Self::Unauthorized(detail)
            | Self::FenceRejected(detail)
            | Self::Invalid(detail)
            | Self::Unsupported(detail)
            | Self::Conflict(detail)
            | Self::Backend { detail, .. } => detail,
        }
    }

    /// Extend the detail (for example with a secondary cleanup failure) while
    /// keeping the primary cause's class, so compensation never reclassifies
    /// the failure a caller must act on.
    #[must_use]
    pub fn map_detail(self, extend: impl FnOnce(String) -> String) -> Self {
        match self {
            Self::NotFound(detail) => Self::NotFound(extend(detail)),
            Self::Gone(detail) => Self::Gone(extend(detail)),
            Self::NotReady(detail) => Self::NotReady(extend(detail)),
            Self::Transport(detail) => Self::Transport(extend(detail)),
            Self::Timeout(detail) => Self::Timeout(extend(detail)),
            Self::Unauthorized(detail) => Self::Unauthorized(extend(detail)),
            Self::FenceRejected(detail) => Self::FenceRejected(extend(detail)),
            Self::Invalid(detail) => Self::Invalid(extend(detail)),
            Self::Unsupported(detail) => Self::Unsupported(extend(detail)),
            Self::Conflict(detail) => Self::Conflict(extend(detail)),
            Self::Backend { operation, detail } => Self::Backend {
                operation,
                detail: extend(detail),
            },
        }
    }
}

impl Classify for SandboxError {
    /// Only a failure of the mechanism or a transitional state can clear on
    /// an identical retry. Every other class needs different input, fresh
    /// authority, or another provider. No Sandbox failure is a quota signal.
    fn disposition(&self) -> Disposition {
        match self {
            Self::NotReady(_) | Self::Transport(_) | Self::Timeout(_) | Self::Backend { .. } => {
                Disposition::Transient
            }
            Self::NotFound(_)
            | Self::Gone(_)
            | Self::Unauthorized(_)
            | Self::FenceRejected(_)
            | Self::Invalid(_)
            | Self::Unsupported(_)
            | Self::Conflict(_) => Disposition::Permanent,
        }
    }
}

impl From<crate::prepare::PrepareError> for SandboxError {
    /// Admission failures are either a missing backend capability or a
    /// malformed spec; neither can succeed on retry against the same backend.
    fn from(error: crate::prepare::PrepareError) -> Self {
        use crate::prepare::PrepareError as P;
        let detail = error.to_string();
        match error {
            P::InsufficientIsolation
            | P::ProcessTreeSupervisionUnsupported
            | P::ReadOnlyUnsupported(_)
            | P::EgressSecretUnsupported(_)
            | P::NetworkIsolationUnsupported
            | P::NetworkAllowlistUnsupported
            | P::PackageProvisioningUnsupported
            | P::ResourceLimitsUnsupported
            | P::SandboxControlServiceUnsupported => Self::Unsupported(detail),
            P::SandboxControlServiceRealizationMismatch => Self::Conflict(detail),
            P::TrustedHostRequiresWorkdir
            | P::ResourceRequestExceedsLimit(_)
            | P::SandboxControlPathConflict(_)
            | P::ReservedEnvKey(_)
            | P::OutputsPathNotAbsolute => Self::Invalid(detail),
        }
    }
}

impl From<awaken_resource_contract::MemErr> for SandboxError {
    /// Memory-store failures keep their store-defined class at the Sandbox port.
    fn from(error: awaken_resource_contract::MemErr) -> Self {
        use awaken_resource_contract::MemErr as M;
        let detail = error.to_string();
        match error {
            M::NotFound(_) => Self::NotFound(detail),
            M::Conflict { .. } | M::PathConflict(_) => Self::Conflict(detail),
            M::InvalidPath(_) | M::TooLarge | M::AtCapacity => Self::Invalid(detail),
            M::Storage(cause) => Self::backend("memory store", cause),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepositoryPublicationExpectation, SandboxEffectFence, SecretWritebackEffect};

    fn class(error: &SandboxError) -> &'static str {
        match error {
            SandboxError::NotFound(_) => "NotFound",
            SandboxError::Gone(_) => "Gone",
            SandboxError::NotReady(_) => "NotReady",
            SandboxError::Transport(_) => "Transport",
            SandboxError::Timeout(_) => "Timeout",
            SandboxError::Unauthorized(_) => "Unauthorized",
            SandboxError::FenceRejected(_) => "FenceRejected",
            SandboxError::Invalid(_) => "Invalid",
            SandboxError::Unsupported(_) => "Unsupported",
            SandboxError::Conflict(_) => "Conflict",
            SandboxError::Backend { .. } => "Backend",
        }
    }

    #[test]
    fn construction_sites_choose_the_class_callers_act_on() {
        // Decision table over real construction sites. Columns: C1 site/cause,
        // E1 variant, E2 Classify disposition. Rules: only a mechanism failure
        // or a transitional state (Backend/Transport/Timeout/NotReady) may be
        // retried identically; stale authority, malformed input, missing
        // capability, and contradicted expectations are permanent.
        let live = SandboxEffectFence::new("operation", "owner", "incarnation", 1, 100).unwrap();
        let rows: Vec<(&str, SandboxError, &str, Disposition)> = vec![
            (
                "S1 expired effect fence at the provider boundary",
                live.validate_live_at(100).unwrap_err(),
                "FenceRejected",
                Disposition::Permanent,
            ),
            (
                "S2 fence with an empty identity field",
                SandboxEffectFence::new("operation", " ", "incarnation", 1, 100).unwrap_err(),
                "Invalid",
                Disposition::Permanent,
            ),
            (
                "S3 write-back without a credential reference",
                SecretWritebackEffect::new("", "pod", live.clone()).unwrap_err(),
                "Invalid",
                Disposition::Permanent,
            ),
            (
                "S4 publication commit that is not canonical hex",
                RepositoryPublicationExpectation {
                    branch: "main".into(),
                    commit: "HEAD".into(),
                    expected_prior_commit: None,
                }
                .validate()
                .unwrap_err(),
                "Invalid",
                Disposition::Permanent,
            ),
            (
                "S5 backend lacks no-bypass allowlist enforcement",
                crate::prepare::PrepareError::NetworkAllowlistUnsupported.into(),
                "Unsupported",
                Disposition::Permanent,
            ),
            (
                "S6 adopted control services differ from the realized set",
                crate::prepare::PrepareError::SandboxControlServiceRealizationMismatch.into(),
                "Conflict",
                Disposition::Permanent,
            ),
            (
                "S7 Memory store storage outage",
                awaken_resource_contract::MemErr::Storage("disk".into()).into(),
                "Backend",
                Disposition::Transient,
            ),
            (
                "S8 Memory store path absent",
                awaken_resource_contract::MemErr::NotFound("/a".into()).into(),
                "NotFound",
                Disposition::Permanent,
            ),
            (
                "S9 container realization still provisioning",
                SandboxError::not_ready("container realization is still provisioning"),
                "NotReady",
                Disposition::Transient,
            ),
            (
                "S10 bubblewrap report wait elapsed",
                SandboxError::timeout("bubblewrap process report timed out"),
                "Timeout",
                Disposition::Transient,
            ),
            (
                "S11 lost response after a possible effect",
                SandboxError::transport("mount response was lost after a possible effect"),
                "Transport",
                Disposition::Transient,
            ),
            (
                "S12 consumed process-secret capability",
                SandboxError::gone("process-secret reference expired"),
                "Gone",
                Disposition::Permanent,
            ),
            (
                "S13 read-only repository publication",
                SandboxError::unauthorized("read-only repository cannot be published"),
                "Unauthorized",
                Disposition::Permanent,
            ),
        ];
        for (site, error, expected, disposition) in rows {
            assert_eq!(class(&error), expected, "{site}: {error}");
            assert_eq!(error.disposition(), disposition, "{site}");
            assert!(error.to_string().starts_with("sandbox error: "), "{site}");
        }
    }

    #[test]
    fn detail_extension_keeps_the_primary_class_and_rendering() {
        // M1 a compensation failure appended to a conflict stays a conflict;
        // M2 Backend keeps its operation; M3 detail omits the class prefix.
        let conflict = SandboxError::conflict("cannot push a repository with detached HEAD")
            .map_detail(|detail| format!("{detail}; stage cleanup failed"));
        assert_eq!(class(&conflict), "Conflict", "M1");
        assert_eq!(
            conflict.detail(),
            "cannot push a repository with detached HEAD; stage cleanup failed",
            "M3"
        );
        let backend = SandboxError::backend("spawn", "EPERM").map_detail(|detail| detail + "!");
        assert_eq!(backend.to_string(), "sandbox error: spawn: EPERM!", "M2");
        assert_eq!(backend.disposition(), Disposition::Transient, "M2");
    }
}
