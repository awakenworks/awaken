// @formal surface exclude=projection_dto
// @formal owner crates/platform/access/awaken-tenancy/src/org_admission.rs
// @formal reason: Immutable request-limit result projections carry the access
// @formal reason: owner's decision; policy, clock and admission stay with that owner.
//! Protocol projections of an ingress request-limit decision.
//! Admission policy, its clock and its implementation remain at the access edge.

/// The Org bucket a metered request consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgOperation {
    Create,
    Read,
}

/// The bucket state after one admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgAdmissionDecision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    /// Whole seconds until one token is available again, when refused.
    pub retry_after: Option<u64>,
    /// Whole seconds until the bucket is full again.
    pub reset_after: u64,
    /// The instant, in Unix milliseconds on the limiter's clock, at which the
    /// bucket is full again; an edge renders it without reading a clock.
    pub reset_at_unix_ms: u64,
}

/// The Org limiter could not decide; the edge refuses rather than admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgAdmissionUnavailable {
    pub message: String,
}

impl std::fmt::Display for OrgAdmissionUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Org request limiter is unavailable: {}",
            self.message
        )
    }
}

impl std::error::Error for OrgAdmissionUnavailable {}
