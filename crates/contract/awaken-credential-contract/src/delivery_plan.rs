//! Orthogonal, secret-free credential delivery decisions.
//!
//! A carrier answers how bytes reach their consumer. A route answers where the
//! external effect is performed. A payload answers whether the delivered value
//! is upstream plaintext or a delegated capability. Keeping these axes separate
//! prevents deployment topology from inventing a second injection mechanism.

use super::{PlaintextBoundary, PlaintextHolder};

/// Last-mile mechanism used by a trusted consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialDeliveryCarrier {
    ProcessEnvironment,
    PrivateFile,
    ProcessProtocol,
}

/// Network route used by the credential-consuming effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialInvocationRoute {
    Direct,
    EgressGateway,
}

/// Authority represented by the value delivered through the carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialDeliveredAuthority {
    PlaintextMaterial,
    DelegatedCapability,
}

/// One admitted delivery decision. Locations remain adapter-owned: this value
/// says which mechanism is legal without leaking an ENV name, host path, socket
/// path, or deployment-specific endpoint into the credential domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialDeliveryPlan {
    pub holder: PlaintextHolder,
    pub carrier: CredentialDeliveryCarrier,
    pub route: CredentialInvocationRoute,
    pub authority: CredentialDeliveredAuthority,
}

const fn repository_git_delivery_shape(
    boundary: PlaintextBoundary,
) -> Option<(CredentialInvocationRoute, CredentialDeliveredAuthority)> {
    match boundary {
        PlaintextBoundary::Workload => Some((
            CredentialInvocationRoute::Direct,
            CredentialDeliveredAuthority::PlaintextMaterial,
        )),
        PlaintextBoundary::Platform => Some((
            CredentialInvocationRoute::EgressGateway,
            CredentialDeliveredAuthority::DelegatedCapability,
        )),
        PlaintextBoundary::Worker => None,
    }
}

/// Repository Git has one Sandbox-visible protocol carrier in every topology.
/// Standalone deployments deliver upstream plaintext to a Workload and connect
/// directly; hosted deployments deliver a short-lived capability to a
/// Platform-mediated route. A Worker/Runtime Host is never a valid effect path.
#[must_use]
pub fn repository_git_credential_delivery(
    holder: &PlaintextHolder,
) -> Option<CredentialDeliveryPlan> {
    let (route, authority) = repository_git_delivery_shape(holder.boundary)?;
    Some(CredentialDeliveryPlan {
        holder: holder.clone(),
        carrier: CredentialDeliveryCarrier::ProcessProtocol,
        route,
        authority,
    })
}

#[cfg(kani)]
#[kani::proof]
fn credential_effect_repository_git_delivery_never_admits_worker_or_mixes_route_authority() {
    let boundary = match kani::any::<u8>() % 3 {
        0 => PlaintextBoundary::Workload,
        1 => PlaintextBoundary::Worker,
        _ => PlaintextBoundary::Platform,
    };
    let shape = repository_git_delivery_shape(boundary);
    match boundary {
        PlaintextBoundary::Workload => assert!(matches!(
            shape,
            Some((
                CredentialInvocationRoute::Direct,
                CredentialDeliveredAuthority::PlaintextMaterial
            ))
        )),
        PlaintextBoundary::Worker => assert!(shape.is_none()),
        PlaintextBoundary::Platform => assert!(matches!(
            shape,
            Some((
                CredentialInvocationRoute::EgressGateway,
                CredentialDeliveredAuthority::DelegatedCapability
            ))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_git_delivery_is_closed_over_sandbox_and_gateway_routes() {
        // Decision table: Workload -> direct/plaintext/protocol; Platform ->
        // gateway/capability/protocol; Worker -> reject. These three exhaustive
        // boundary cases mechanically exclude Runtime Host Git custody.
        let workload = PlaintextHolder::new(PlaintextBoundary::Workload, "sandbox");
        let direct = repository_git_credential_delivery(&workload).expect("sandbox route");
        assert_eq!(direct.carrier, CredentialDeliveryCarrier::ProcessProtocol);
        assert_eq!(direct.route, CredentialInvocationRoute::Direct);
        assert_eq!(
            direct.authority,
            CredentialDeliveredAuthority::PlaintextMaterial
        );

        let platform = PlaintextHolder::new(PlaintextBoundary::Platform, "gateway");
        let mediated = repository_git_credential_delivery(&platform).expect("gateway route");
        assert_eq!(mediated.carrier, CredentialDeliveryCarrier::ProcessProtocol);
        assert_eq!(mediated.route, CredentialInvocationRoute::EgressGateway);
        assert_eq!(
            mediated.authority,
            CredentialDeliveredAuthority::DelegatedCapability
        );

        let worker = PlaintextHolder::new(PlaintextBoundary::Worker, "runtime-host");
        assert!(repository_git_credential_delivery(&worker).is_none());
    }

    #[test]
    fn standalone_default_is_direct_plaintext_while_hosted_is_gateway_capability() {
        let standalone = crate::CredentialRealizationProfile::self_hosted_native();
        let direct = repository_git_credential_delivery(&standalone.resource_holder)
            .expect("standalone Sandbox delivery");
        assert_eq!(direct.carrier, CredentialDeliveryCarrier::ProcessProtocol);
        assert_eq!(direct.route, CredentialInvocationRoute::Direct);
        assert_eq!(
            direct.authority,
            CredentialDeliveredAuthority::PlaintextMaterial
        );

        let hosted_holder =
            PlaintextHolder::new(PlaintextBoundary::Platform, "hosted.example.egress-gateway");
        let hosted =
            repository_git_credential_delivery(&hosted_holder).expect("hosted Gateway delivery");
        assert_eq!(hosted.carrier, CredentialDeliveryCarrier::ProcessProtocol);
        assert_eq!(hosted.route, CredentialInvocationRoute::EgressGateway);
        assert_eq!(
            hosted.authority,
            CredentialDeliveredAuthority::DelegatedCapability
        );
    }
}
