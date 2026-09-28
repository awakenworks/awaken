// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: The existing Platform MCP egress port accepts an exact generation and credential
// @formal reason: reference and returns only a mediated route and expiry. Runtime Host consumes this port
// @formal reason: and refuses a missing mediator instead of resolving Platform-held plaintext locally.
// @formal reason: This is a contract and product-requirement classification, not a proof that an external
// @formal reason: mediator enforces the route or credential revocation.
//! Platform-mediated MCP egress port.
//!
//! A hosted deployment performs the authenticated MCP call from its own egress
//! mediator. The Runtime Host is then a *caller*, not a holder: it learns a
//! route and an expiry, and never the upstream credential material. This port
//! is the seam where that mediated route is minted.
//!
//! It deliberately has no plaintext-returning method. A provisioner that could
//! also hand back material would let one installed adapter satisfy both the
//! mediated and the Worker-held contract, and the distinction between them
//! would stop being checkable.

use awaken_credential_contract::{CredentialMaterialError, CredentialRef, PlaintextHolder};

/// One generation-scoped request for a mediated MCP route.
///
/// Generation identity travels as scalars so credential mediation does not
/// depend on the Session aggregate that happens to name it.
#[derive(Debug, Clone, Copy)]
pub struct PlatformMcpEgressRequest<'a> {
    /// The Platform holder selected by the frozen Environment profile.
    pub holder: &'a PlaintextHolder,
    /// Exact credential the mediator must bind, by id and revision.
    pub credential: &'a CredentialRef,
    pub workspace_id: &'a str,
    pub session_id: &'a str,
    pub attachment_id: &'a str,
    pub generation: u64,
    /// Runtime incarnation and lease epoch of the generation that asked. A
    /// mediator binds its route to these so a stale incarnation cannot keep
    /// spending a route minted for an earlier one.
    pub runtime_incarnation: &'a str,
    pub lease_epoch: u64,
    /// Upstream the mediator forwards to. The Runtime Host never calls it.
    pub upstream_url: &'a str,
}

/// A mediated route, carrying no upstream credential material.
///
/// There is deliberately no bearer field: nothing in this value can be
/// replayed by the Runtime Host into an upstream call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediatedMcpRoute {
    /// Opaque mediator URL, already scoped to the requested generation.
    pub url: String,
    /// When the mediator stops honouring this route.
    pub expires_at_unix_ms: u64,
}

/// Mints one mediated MCP route per generation.
///
/// A deployment without an installed provisioner refuses Platform-held MCP
/// credentials; it never falls back to resolving material into the Host.
#[async_trait::async_trait]
pub trait PlatformMcpEgressProvisioner: Send + Sync {
    async fn provision(
        &self,
        request: PlatformMcpEgressRequest<'_>,
    ) -> Result<MediatedMcpRoute, CredentialMaterialError>;
}
