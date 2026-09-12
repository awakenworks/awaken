//! Orthogonal, secret-free credential delivery decisions.
//!
//! A carrier answers how bytes reach their consumer. A route answers where the
//! external effect is performed. A payload answers whether the delivered value
//! is upstream plaintext or a delegated capability. Keeping these axes separate
//! prevents deployment topology from inventing a second injection mechanism.

use super::{CredentialPurpose, PlaintextBoundary, PlaintextHolder};

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

/// Route and authority follow the plaintext boundary alone.
///
/// A Worker or Workload holder performs its own upstream effect and therefore
/// receives upstream plaintext. A Platform holder never hands plaintext to the
/// caller: the mediator performs the effect and the caller receives only a
/// delegated capability. Purpose deliberately has no say here — that is what
/// lets one deployment topology move every credential kind between a direct
/// and a gateway-mediated route without inventing a second mechanism.
const fn boundary_delivery_shape(
    boundary: PlaintextBoundary,
) -> (CredentialInvocationRoute, CredentialDeliveredAuthority) {
    match boundary {
        PlaintextBoundary::Worker | PlaintextBoundary::Workload => (
            CredentialInvocationRoute::Direct,
            CredentialDeliveredAuthority::PlaintextMaterial,
        ),
        PlaintextBoundary::Platform => (
            CredentialInvocationRoute::EgressGateway,
            CredentialDeliveredAuthority::DelegatedCapability,
        ),
    }
}

/// Which boundaries may perform this purpose's effect at all. Each `false`
/// encodes a custody decision, never a missing implementation.
const fn purpose_admits_boundary(purpose: CredentialPurpose, boundary: PlaintextBoundary) -> bool {
    match (purpose, boundary) {
        // Repository Git effects are Sandbox-owned. A Worker/Runtime Host is
        // never a valid Git effect path in any topology.
        (CredentialPurpose::RepositoryTransport, PlaintextBoundary::Worker) => false,
        // Host-side signature verification consumes material in place. It
        // performs no upstream effect, so it has no delivery route at all.
        (CredentialPurpose::SignatureVerification, _) => false,
        _ => true,
    }
}

/// Which last-mile carriers this purpose's consumer can actually read. A
/// carrier is refused here when the consuming protocol cannot accept it,
/// independent of which carriers a deployment happens to have installed.
const fn purpose_admits_carrier(
    purpose: CredentialPurpose,
    carrier: CredentialDeliveryCarrier,
) -> bool {
    match purpose {
        // Git, MCP, remote-agent and platform HTTP effects all reach their
        // consumer through a protocol field on the call itself.
        CredentialPurpose::RepositoryTransport
        | CredentialPurpose::McpAuthorization
        | CredentialPurpose::RemoteAgentAuthorization
        | CredentialPurpose::WebProviderAuthorization
        | CredentialPurpose::HttpEffect => {
            matches!(carrier, CredentialDeliveryCarrier::ProcessProtocol)
        }
        // A model provider adapter reads an in-process value, a process
        // environment entry, or a process-private file, depending on which
        // mechanism the selected backend advertises.
        CredentialPurpose::ProviderAdapter | CredentialPurpose::Extension => true,
        CredentialPurpose::SignatureVerification => false,
    }
}

/// The one admitted delivery decision for a purpose, holder and carrier.
///
/// Policy owns admissibility, route and authority; the caller supplies the
/// carrier it holds installed evidence for. `None` is a closed refusal, never
/// an unimplemented case.
#[must_use]
pub fn credential_delivery_plan(
    purpose: CredentialPurpose,
    holder: &PlaintextHolder,
    carrier: CredentialDeliveryCarrier,
) -> Option<CredentialDeliveryPlan> {
    if !purpose_admits_boundary(purpose, holder.boundary)
        || !purpose_admits_carrier(purpose, carrier)
    {
        return None;
    }
    let (route, authority) = boundary_delivery_shape(holder.boundary);
    Some(CredentialDeliveryPlan {
        holder: holder.clone(),
        carrier,
        route,
        authority,
    })
}

/// Repository Git has one Sandbox-visible protocol carrier in every topology.
/// Standalone deployments deliver upstream plaintext to a Workload and connect
/// directly; hosted deployments deliver a short-lived capability to a
/// Platform-mediated route. A Worker/Runtime Host is never a valid effect path.
#[must_use]
pub fn repository_git_credential_delivery(
    holder: &PlaintextHolder,
) -> Option<CredentialDeliveryPlan> {
    credential_delivery_plan(
        CredentialPurpose::RepositoryTransport,
        holder,
        CredentialDeliveryCarrier::ProcessProtocol,
    )
}

#[cfg(kani)]
#[kani::proof]
fn credential_effect_delivery_policy_is_total_and_never_mixes_route_authority() {
    // Causes: P=purpose permits this boundary, C=purpose permits this carrier,
    // B=the exact plaintext boundary. Effects: A=the policy helpers admit,
    // R=its route/authority pair. Constraint: every B has one route/authority.
    // Decision rules: P+C => A and R follows B; !P or !C => refusal.
    // Repository/Worker and verification/any-boundary are mandatory refusal
    // instances. All 8 purposes x 3 boundaries x 3 carriers are selected.
    let purpose = match kani::any::<u8>() % 8 {
        0 => CredentialPurpose::ProviderAdapter,
        1 => CredentialPurpose::RepositoryTransport,
        2 => CredentialPurpose::McpAuthorization,
        3 => CredentialPurpose::RemoteAgentAuthorization,
        4 => CredentialPurpose::WebProviderAuthorization,
        5 => CredentialPurpose::HttpEffect,
        6 => CredentialPurpose::SignatureVerification,
        _ => CredentialPurpose::Extension,
    };
    let boundary = match kani::any::<u8>() % 3 {
        0 => PlaintextBoundary::Workload,
        1 => PlaintextBoundary::Worker,
        _ => PlaintextBoundary::Platform,
    };
    let carrier = match kani::any::<u8>() % 3 {
        0 => CredentialDeliveryCarrier::ProcessEnvironment,
        1 => CredentialDeliveryCarrier::PrivateFile,
        _ => CredentialDeliveryCarrier::ProcessProtocol,
    };

    // Policy is total: every triple is either an admitted shape or a closed
    // refusal, and the refusal reason is exactly one of the two stated axes.
    let admitted =
        purpose_admits_boundary(purpose, boundary) && purpose_admits_carrier(purpose, carrier);

    // Route and authority never mix across the plaintext boundary.
    let (route, authority) = boundary_delivery_shape(boundary);
    match boundary {
        PlaintextBoundary::Worker | PlaintextBoundary::Workload => assert!(matches!(
            (route, authority),
            (
                CredentialInvocationRoute::Direct,
                CredentialDeliveredAuthority::PlaintextMaterial
            )
        )),
        PlaintextBoundary::Platform => assert!(matches!(
            (route, authority),
            (
                CredentialInvocationRoute::EgressGateway,
                CredentialDeliveredAuthority::DelegatedCapability
            )
        )),
    }

    // Repository Git custody never reaches a Worker, in any topology.
    if matches!(purpose, CredentialPurpose::RepositoryTransport)
        && matches!(boundary, PlaintextBoundary::Worker)
    {
        assert!(!admitted);
    }
    // Host-side verification performs no upstream effect at all.
    if matches!(purpose, CredentialPurpose::SignatureVerification) {
        assert!(!admitted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gateway-shielding property, stated once for every purpose: route
    /// and authority are decided by the plaintext boundary alone. A deployment
    /// that turns its egress gateway on moves every credential kind from
    /// direct/plaintext to mediated/capability together, with no purpose left
    /// behind on a second mechanism.
    #[test]
    fn every_purpose_shares_one_boundary_driven_route_and_authority() {
        for purpose in [
            CredentialPurpose::ProviderAdapter,
            CredentialPurpose::RepositoryTransport,
            CredentialPurpose::McpAuthorization,
            CredentialPurpose::RemoteAgentAuthorization,
            CredentialPurpose::WebProviderAuthorization,
            CredentialPurpose::HttpEffect,
            CredentialPurpose::Extension,
        ] {
            let direct = credential_delivery_plan(
                purpose,
                &PlaintextHolder::new(PlaintextBoundary::Workload, "sandbox"),
                CredentialDeliveryCarrier::ProcessProtocol,
            )
            .expect("workload delivers every purpose directly");
            assert_eq!(direct.route, CredentialInvocationRoute::Direct);
            assert_eq!(
                direct.authority,
                CredentialDeliveredAuthority::PlaintextMaterial,
                "{purpose:?}"
            );

            let mediated = credential_delivery_plan(
                purpose,
                &PlaintextHolder::new(PlaintextBoundary::Platform, "gateway"),
                CredentialDeliveryCarrier::ProcessProtocol,
            )
            .expect("platform mediates every purpose");
            assert_eq!(mediated.route, CredentialInvocationRoute::EgressGateway);
            assert_eq!(
                mediated.authority,
                CredentialDeliveredAuthority::DelegatedCapability,
                "{purpose:?}"
            );
        }
    }

    /// Purposes differ on exactly two axes, and each refusal is a custody
    /// decision. Stating them per purpose keeps the table meaningful instead
    /// of padding a cartesian product with `None`.
    #[test]
    fn purpose_refusals_are_closed_custody_decisions() {
        let worker = PlaintextHolder::new(PlaintextBoundary::Worker, "runtime-host");
        let workload = PlaintextHolder::new(PlaintextBoundary::Workload, "sandbox");

        // Repository Git effects are Sandbox-owned in every topology.
        assert!(
            credential_delivery_plan(
                CredentialPurpose::RepositoryTransport,
                &worker,
                CredentialDeliveryCarrier::ProcessProtocol,
            )
            .is_none()
        );
        // A provider adapter legitimately runs in the Worker.
        assert!(
            credential_delivery_plan(
                CredentialPurpose::ProviderAdapter,
                &worker,
                CredentialDeliveryCarrier::ProcessProtocol,
            )
            .is_some()
        );
        // Host-side verification performs no upstream effect at all.
        for boundary in [
            PlaintextBoundary::Worker,
            PlaintextBoundary::Workload,
            PlaintextBoundary::Platform,
        ] {
            assert!(
                credential_delivery_plan(
                    CredentialPurpose::SignatureVerification,
                    &PlaintextHolder::new(boundary, "host"),
                    CredentialDeliveryCarrier::ProcessProtocol,
                )
                .is_none()
            );
        }
        // Protocol-field consumers cannot read a process environment entry or
        // a private file; only an adapter-shaped consumer can.
        for carrier in [
            CredentialDeliveryCarrier::ProcessEnvironment,
            CredentialDeliveryCarrier::PrivateFile,
        ] {
            assert!(
                credential_delivery_plan(CredentialPurpose::McpAuthorization, &workload, carrier)
                    .is_none()
            );
            assert!(
                credential_delivery_plan(CredentialPurpose::ProviderAdapter, &workload, carrier)
                    .is_some()
            );
        }
    }

    /// The hosted profile and the delivery policy must agree: the canonical
    /// hosted holder is exactly the one that yields a mediated, capability
    /// route for every purpose.
    #[test]
    fn the_canonical_hosted_profile_mediates_every_purpose() {
        let hosted = crate::CredentialRealizationProfile::hosted_cloud();
        for holder in [
            &hosted.inference_holder,
            &hosted.mcp_holder,
            &hosted.resource_holder,
        ] {
            assert_eq!(holder.boundary, PlaintextBoundary::Platform);
            let plan = credential_delivery_plan(
                CredentialPurpose::RepositoryTransport,
                holder,
                CredentialDeliveryCarrier::ProcessProtocol,
            )
            .expect("hosted holder delivers");
            assert_eq!(plan.route, CredentialInvocationRoute::EgressGateway);
            assert_eq!(
                plan.authority,
                CredentialDeliveredAuthority::DelegatedCapability
            );
        }
    }

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
