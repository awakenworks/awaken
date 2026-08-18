//! Optional managed-platform adapters supplied by a hosted composition.

use std::sync::Arc;

/// Narrow hosted seams used by the canonical Awaken components.
///
/// Open/self-hosted startup uses [`Default`]: local protocol throttling remains
/// available, budgeted Session creation fails closed without an explicit price
/// authority, and Tunnel routes are absent.
#[derive(Clone, Default)]
pub struct ManagedServiceAdapters {
    pub request_limiter: Option<Arc<dyn awaken_protocol_managed::ManagedRequestLimiter>>,
    pub list_price_provider: Option<Arc<dyn awaken_session_contract::ManagedListPriceProvider>>,
    pub tunnel_application: Option<Arc<dyn awaken_protocol_managed::ManagedTunnelApplication>>,
    pub inference_geo_policy: Option<Arc<dyn awaken_protocol_managed::ManagedInferenceGeoPolicy>>,
    pub credential_envelope_issuer:
        Option<Arc<dyn awaken_credential_contract::CredentialEnvelopeIssuer>>,
    /// Hosted delivery of the durable Managed Credential outbox. The open
    /// Control remains the publication authority; a hosted composition may
    /// replace only the target-side rollout mechanism.
    pub credential_rollout_target:
        Option<Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>>,
    /// The browser-serving origin routes Awaken's exported Managed-runtime
    /// families to the canonical Coordinator. This changes presentation only;
    /// it never mounts runtime state or handlers in Control.
    pub same_origin_managed_runtime: bool,
}

impl ManagedServiceAdapters {
    #[must_use]
    pub fn with_request_limiter(
        mut self,
        limiter: Arc<dyn awaken_protocol_managed::ManagedRequestLimiter>,
    ) -> Self {
        self.request_limiter = Some(limiter);
        self
    }

    #[must_use]
    pub fn with_list_price_provider(
        mut self,
        provider: Arc<dyn awaken_session_contract::ManagedListPriceProvider>,
    ) -> Self {
        self.list_price_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_tunnel_application(
        mut self,
        application: Arc<dyn awaken_protocol_managed::ManagedTunnelApplication>,
    ) -> Self {
        self.tunnel_application = Some(application);
        self
    }

    #[must_use]
    pub fn with_inference_geo_policy(
        mut self,
        policy: Arc<dyn awaken_protocol_managed::ManagedInferenceGeoPolicy>,
    ) -> Self {
        self.inference_geo_policy = Some(policy);
        self
    }

    /// Install the hosted cryptographic transport at the existing Vault
    /// compilation boundary. The adapter cannot select a credential, holder,
    /// usage or target; it seals only the exact request supplied by Control.
    #[must_use]
    pub fn with_credential_envelope_issuer(
        mut self,
        issuer: Arc<dyn awaken_credential_contract::CredentialEnvelopeIssuer>,
    ) -> Self {
        self.credential_envelope_issuer = Some(issuer);
        self
    }

    /// Install one hosted target for the existing durable credential outbox.
    /// This does not add a second event store or acknowledgement protocol.
    #[must_use]
    pub fn with_credential_rollout_target(
        mut self,
        target: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>,
    ) -> Self {
        self.credential_rollout_target = Some(target);
        self
    }

    /// Declare that deployment routing makes the canonical Coordinator surface
    /// reachable at the browser-serving Control origin.
    #[must_use]
    pub fn with_same_origin_managed_runtime(mut self) -> Self {
        self.same_origin_managed_runtime = true;
        self
    }
}

/// Infrastructure adapters for the one canonical Coordinator composition.
/// Hosted products may replace transport authentication, but cannot replace or
/// add a Coordinator router.
#[derive(Clone, Default)]
pub struct CoordinatorServiceAdapters {
    pub worker_authenticator:
        Option<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>>,
    pub worker_placement_policy: Option<Arc<dyn awaken_run_ingress::PlacementPolicy>>,
    pub cloud_native_credential_realization:
        Option<awaken_runtime_contract::CredentialRealizationProfile>,
}

impl CoordinatorServiceAdapters {
    #[must_use]
    pub fn with_worker_authenticator(
        mut self,
        authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    ) -> Self {
        self.worker_authenticator = Some(authenticator);
        self
    }

    #[must_use]
    pub fn with_worker_placement_policy(
        mut self,
        policy: Arc<dyn awaken_run_ingress::PlacementPolicy>,
    ) -> Self {
        self.worker_placement_policy = Some(policy);
        self
    }

    /// Select the exact credential holders frozen into Cloud Native Session
    /// snapshots by the canonical Environment application.
    #[must_use]
    pub fn with_cloud_native_credential_realization(
        mut self,
        profile: awaken_runtime_contract::CredentialRealizationProfile,
    ) -> Self {
        self.cloud_native_credential_realization = Some(profile);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::{
        ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress,
        ManagedCredentialRollout, ManagedCredentialRolloutTarget,
    };

    struct HostedRolloutTarget;

    #[async_trait::async_trait]
    impl ManagedCredentialRolloutTarget for HostedRolloutTarget {
        async fn rollout(
            &self,
            _event: &ManagedCredentialRollout,
        ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        }
    }

    #[test]
    fn hosted_rollout_target_is_an_explicit_optional_seam() {
        let defaults = ManagedServiceAdapters::default();
        assert!(defaults.credential_rollout_target.is_none());

        let hosted = defaults.with_credential_rollout_target(Arc::new(HostedRolloutTarget));
        assert!(hosted.credential_rollout_target.is_some());
    }
}
