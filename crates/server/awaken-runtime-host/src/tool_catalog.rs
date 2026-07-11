//! Scope-keyed tool visibility (ADR-0052 D3).
//!
//! Which tools an agent config may *name* is a projection of the global tool catalog
//! over the request's scope — not a field on the tool, and not a duplicated
//! `ConfigService`. The [`ToolCatalogSource`] port answers "which descriptors exist
//! for this scope"; [`ConfigService`](crate::config_plane::ConfigService) feeds the
//! answer to `compile`, so a config that names a tool absent from its scope's catalog
//! hits `UnknownTool` at compile time (fail-closed) — and the tool's very existence
//! is never disclosed to other tenants.
//!
//! This is the same grain as ADR-0051's `ScopedConfig` decorator and ADR-0034's
//! protocol-as-projection: "the tools for this scope" is a function of the opaque
//! [`ScopeId`], resolved at the edge, leaving the neutral core untouched.

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;

/// The reserved scope the management assistant lives in (ADR-0052 D2). It is a
/// distinct owner from [`awaken_config_store::DEFAULT_SCOPE`] so, in a multi-tenant
/// deployment, the admin tools are invisible to tenant scopes; a self-hosted single
/// org may still seed the assistant here without any tenant colliding with it.
pub const RESERVED_ADMIN_SCOPE: &str = "__admin";

/// The compile-feed's view of the tool catalog: the descriptors an agent config in
/// `scope` may name. Named for its consumer — `ConfigService`'s compile step.
pub trait ToolCatalogSource: Send + Sync {
    /// The tool descriptors nameable by a config owned by `scope`.
    fn catalog_for(&self, scope: &ScopeId) -> Vec<ToolDescriptor>;
}

/// The default projection (ADR-0052 D3): every scope sees the `global` catalog; the
/// one `reserved_scope` additionally sees the `admin` descriptors. Membership — not a
/// flag on any descriptor — is the fence, computed per scope.
pub struct ScopedToolCatalog {
    global: Vec<ToolDescriptor>,
    reserved_scope: ScopeId,
    admin: Vec<ToolDescriptor>,
}

impl ScopedToolCatalog {
    /// `global` is what every scope sees (the advertised hand/client/dynamic tools);
    /// `admin` is the management descriptors, visible only in `reserved_scope`.
    pub fn new(
        global: Vec<ToolDescriptor>,
        reserved_scope: impl Into<ScopeId>,
        admin: Vec<ToolDescriptor>,
    ) -> Self {
        Self {
            global,
            reserved_scope: reserved_scope.into(),
            admin,
        }
    }
}

impl ToolCatalogSource for ScopedToolCatalog {
    fn catalog_for(&self, scope: &ScopeId) -> Vec<ToolDescriptor> {
        if scope == &self.reserved_scope {
            let mut all = self.global.clone();
            all.extend(self.admin.iter().cloned());
            all
        } else {
            self.global.clone()
        }
    }
}

/// A scope-blind catalog: every scope sees the same fixed descriptors. This is the
/// pre-ADR-0052 behavior, kept for the callers (and tests) that never fence by scope.
pub struct StaticToolCatalog(pub Vec<ToolDescriptor>);

impl ToolCatalogSource for StaticToolCatalog {
    fn catalog_for(&self, _scope: &ScopeId) -> Vec<ToolDescriptor> {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("t", id, "d", serde_json::json!({"type": "object"}))
    }

    #[test]
    fn admin_tools_are_visible_only_in_the_reserved_scope() {
        let catalog = ScopedToolCatalog::new(
            vec![tool("read")],
            RESERVED_ADMIN_SCOPE,
            vec![tool("admin_get_platform_capabilities")],
        );

        // The reserved scope sees global + admin.
        let reserved = catalog.catalog_for(&ScopeId::from(RESERVED_ADMIN_SCOPE));
        let reserved_ids: Vec<&str> = reserved.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            reserved_ids,
            vec!["read", "admin_get_platform_capabilities"]
        );

        // Any other scope sees only the global catalog — the admin tool's existence
        // is not even disclosed.
        let tenant = catalog.catalog_for(&ScopeId::from("wrkspc_acme"));
        let tenant_ids: Vec<&str> = tenant.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(tenant_ids, vec!["read"]);
        assert!(!tenant_ids.contains(&"admin_get_platform_capabilities"));
    }

    #[test]
    fn static_catalog_is_scope_blind() {
        let catalog = StaticToolCatalog(vec![tool("read")]);
        assert_eq!(catalog.catalog_for(&ScopeId::from("a")).len(), 1);
        assert_eq!(catalog.catalog_for(&ScopeId::from("b")).len(), 1);
    }

    #[test]
    fn a_tenant_authority_does_not_cover_the_reserved_admin_scope() {
        use awaken_tenancy::Authority;
        // Access to the reserved scope is gated by the existing ingress reconciliation
        // (ADR-0052 D6, no bespoke guard): a narrow tenant token's authority does not
        // cover the reserved scope, so it cannot select it via path or domain.
        let tenant = Authority::bound(ScopeId::from("wrkspc_acme"));
        assert!(!tenant.covers(&ScopeId::from(RESERVED_ADMIN_SCOPE)));
        // The admin console principal, bound to the reserved scope, does cover it.
        let admin = Authority::bound(ScopeId::from(RESERVED_ADMIN_SCOPE));
        assert!(admin.covers(&ScopeId::from(RESERVED_ADMIN_SCOPE)));
    }
}
