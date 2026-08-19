//! Product enforcement adapter for the entitlement provider injected into IAM.
//!
//! Awaken owns only the feature check. Commercial subscription resolution,
//! license parsing/verification, expiry, and rollback state are supplied by the
//! closed composition layer through `embedded_iam_for_tenant_with_entitlements`.

use awaken_iam_contract::{EntitlementDecision, EntitlementRequest, PrincipalRef};
use awaken_iam_host::{IamClient, IamGate};

use super::ManagementAuthz;

fn check(
    gate: &IamGate,
    principal: PrincipalRef,
    entitlement: impl Into<String>,
    resource: Option<String>,
) -> EntitlementDecision {
    IamClient::check_entitlement(
        gate,
        EntitlementRequest {
            principal,
            entitlement: entitlement.into(),
            resource,
        },
    )
}

impl ManagementAuthz {
    /// Evaluate a commercial feature through the same IAM PDP used for authz.
    pub fn check_entitlement(
        &self,
        principal: PrincipalRef,
        entitlement: impl Into<String>,
        resource: Option<String>,
    ) -> EntitlementDecision {
        check(&self.gate, principal, entitlement, resource)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use awaken_iam_core::EntitlementEngine;
    use awaken_iam_host::LocalIamState;
    use awaken_iam_server::AuthzApi;

    use super::*;

    fn gate(provider: EntitlementEngine) -> IamGate {
        IamGate::from_local_state(Arc::new(Mutex::new(LocalIamState {
            authz: AuthzApi::with_entitlements(provider),
            directory: Default::default(),
        })))
    }

    #[test]
    fn product_pep_obeys_the_injected_provider_without_license_knowledge() {
        // Cause/effect graph: C1=provider injected, C2=provider allows the
        // feature. Effect E1 mirrors C2. Decision table: unlicensed -> deny;
        // explicit test provider -> allow. Signature/binding/expiry/rollback
        // causes belong to the injected Cloud/IAM provider, not this product.
        let principal = PrincipalRef::Service {
            service_id: "entitlement-test".into(),
        };
        for (provider, expected) in [
            (EntitlementEngine::unlicensed(), EntitlementDecision::Deny),
            (
                EntitlementEngine::default_allow(),
                EntitlementDecision::Allow,
            ),
        ] {
            assert_eq!(
                check(
                    &gate(provider),
                    principal.clone(),
                    "product:awaken-runtime",
                    Some("workspace-test".into()),
                ),
                expected
            );
        }
    }
}
