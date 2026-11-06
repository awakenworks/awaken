// @formal surface exclude=projection_dto
// @formal owner crates/contract/awaken-tenancy/src/lib.rs
// @formal reason: Request-local selector projections do not authorize access;
// @formal reason: the access edge retains scope reconciliation and enforcement.
//! Request-local identities supplied by a trusted ingress.

/// A request-local Workspace selected by the host's trusted ingress.
///
/// Protocol projections share this value without depending on the host's access
/// policy implementation. Constructing it grants no access or execution authority.
#[derive(Debug, Clone)]
pub struct WorkspaceScope(pub String);

impl WorkspaceScope {
    /// Return the edge-selected Workspace only when it is a usable ownership key.
    ///
    /// Protocol adapters remain responsible for mapping absence to their wire
    /// error shape; this method is the single validation rule for a stamped scope.
    #[must_use]
    pub fn non_empty(&self) -> Option<&str> {
        (!self.0.trim().is_empty()).then_some(self.0.as_str())
    }
}

/// An opaque resource id resolved by a trusted ingress policy layer.
///
/// A protocol adapter may prefer this over an untrusted public id. It carries
/// no customer user, role, project, or authorization vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResourceId(pub String);

/// The caller-visible thread id already authorized by a trusted ingress.
/// This request-local projection is for protocol output only: execution and
/// persistence must use the separately resolved resource id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedThreadId(pub String);

/// An Agent id selected and allow-listed by a trusted ingress policy layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentId(pub String);
