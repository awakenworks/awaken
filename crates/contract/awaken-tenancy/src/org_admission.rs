// @formal surface obligation=managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
// @formal reason: Values and port of Org request admission. The bucket decision and its Kani proofs
// @formal reason: live in awaken-authz-enforce org_admission; this vocabulary owns no state or clock.
//! Org request admission vocabulary: the values an edge exchanges with the Org
//! request-limit authority, and the port that authority implements.
//!
//! Every HTTP protocol entry admits a metered request against its Org's two
//! buckets (Create and Read) before its handler runs. The bucket decision lives
//! in the access edge (`awaken-authz-enforce`); a hosted deployment may replace
//! it behind [`OrgRequestAdmission`]. Each protocol classifies its own routes
//! into an [`OrgOperation`] and encodes an [`OrgAdmissionDecision`] in its own
//! wire shape, so nothing here names a protocol.

use awaken_runtime_contract::runs::request::{
    OrgAdmissionDecision, OrgAdmissionUnavailable, OrgOperation,
};

/// Where a metered request entered: an HTTP protocol entry, or a Deployment
/// launching a Session internally against the same Create bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgRequestSource {
    Http,
    Deployment,
}

/// One metered request, coordinated by the edge-authenticated Workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgAdmissionRequest {
    pub workspace_id: String,
    pub operation: OrgOperation,
    /// The resource family the protocol classified the request into.
    pub resource: &'static str,
    /// The caller's request or idempotency key, when it sent one.
    pub operation_id: Option<String>,
    pub source: OrgRequestSource,
}

/// The Org request-limit authority every protocol entry admits through.
#[async_trait::async_trait]
pub trait OrgRequestAdmission: Send + Sync {
    async fn admit(
        &self,
        request: OrgAdmissionRequest,
    ) -> Result<OrgAdmissionDecision, OrgAdmissionUnavailable>;
}

/// The two Org bucket capacities, per minute. The defaults are Anthropic's
/// documented organization-level Managed Agents limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgRateLimits {
    pub create_per_minute: u32,
    pub read_per_minute: u32,
}

impl Default for OrgRateLimits {
    fn default() -> Self {
        Self {
            create_per_minute: 300,
            read_per_minute: 1_200,
        }
    }
}
