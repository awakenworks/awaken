//! Optional managed-platform adapters supplied by a hosted composition.

use std::sync::Arc;

#[async_trait::async_trait]
pub trait ManagedBackgroundService: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, cancellation: tokio_util::sync::CancellationToken) -> Result<(), String>;
}

/// Narrow hosted seams used by the canonical Awaken components.
///
/// Open/self-hosted startup uses [`Default`]: local protocol throttling remains
/// available, budgeted Session creation fails closed without an explicit price
/// authority, and Tunnel routes are absent.
#[derive(Default)]
pub struct ManagedServiceAdapters {
    /// Product-supplied commercial entitlement policy for embedded local IAM.
    /// The provider is consumed exactly once while constructing the authority;
    /// protocol adapters never inspect license claims or plan vocabulary.
    pub entitlement_provider: Option<Box<dyn awaken_iam_core::EntitlementProvider>>,
    pub request_limiter: Option<Arc<dyn awaken_protocol_managed::ManagedRequestLimiter>>,
    pub list_price_provider: Option<Arc<dyn awaken_session_contract::ManagedListPriceProvider>>,
    pub tunnel_application: Option<Arc<dyn awaken_protocol_managed::ManagedTunnelApplication>>,
    pub inference_geo_policy: Option<Arc<dyn awaken_protocol_managed::ManagedInferenceGeoPolicy>>,
    pub background_services: Vec<Arc<dyn ManagedBackgroundService>>,
    pub credential_material_delivery:
        Option<awaken_credential_contract::CredentialMaterialDelivery>,
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
    pub fn with_entitlement_provider(
        mut self,
        provider: Box<dyn awaken_iam_core::EntitlementProvider>,
    ) -> Self {
        self.entitlement_provider = Some(provider);
        self
    }

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

    #[must_use]
    pub fn with_background_service(mut self, service: Arc<dyn ManagedBackgroundService>) -> Self {
        self.background_services.push(service);
        self
    }

    /// Install the one product-selected plaintext delivery mechanism at the
    /// existing Vault compilation boundary.
    #[must_use]
    pub fn with_credential_material_delivery(
        mut self,
        delivery: awaken_credential_contract::CredentialMaterialDelivery,
    ) -> Self {
        self.credential_material_delivery = Some(delivery);
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

pub(super) fn install_background_services(
    lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    services: &[Arc<dyn ManagedBackgroundService>],
) {
    for service in services {
        let service = Arc::clone(service);
        lifecycle.spawn(service.name(), move |cancellation| async move {
            service.run(cancellation).await
        });
    }
}

struct ConjunctiveCredentialRolloutTarget {
    local: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>,
    external: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>,
}

#[async_trait::async_trait]
impl awaken_credential_vault::repo::ManagedCredentialRolloutTarget
    for ConjunctiveCredentialRolloutTarget
{
    async fn rollout(
        &self,
        event: &awaken_credential_vault::repo::ManagedCredentialRollout,
    ) -> Result<
        awaken_credential_vault::repo::ManagedCredentialAdoptionProgress,
        awaken_credential_vault::repo::ManagedCredentialAdoptionError,
    > {
        let local = self.local.rollout(event).await?;
        let external = self.external.rollout(event).await?;
        Ok(
            awaken_credential_vault::repo::conjunctive_managed_credential_adoption_progress(
                local, external,
            ),
        )
    }
}

pub(super) fn conjunctive_credential_rollout_target(
    local: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>,
    external: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget>,
) -> Arc<dyn awaken_credential_vault::repo::ManagedCredentialRolloutTarget> {
    Arc::new(ConjunctiveCredentialRolloutTarget { local, external })
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
    pub repository_transport_authorizer:
        Option<Arc<dyn awaken_resource_worker_http::RepositoryTransportAuthorizer>>,
    /// Product-selected implementation of the existing runtime inference port.
    /// The default remains Awaken's credential materializer; a managed
    /// composition may replace it without replacing Coordinator or Worker
    /// lifecycle ownership.
    pub inference_materializer:
        Option<Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>>,
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

    #[must_use]
    pub fn with_repository_transport_authorizer(
        mut self,
        authorizer: Arc<dyn awaken_resource_worker_http::RepositoryTransportAuthorizer>,
    ) -> Self {
        self.repository_transport_authorizer = Some(authorizer);
        self
    }

    #[must_use]
    pub fn with_inference_materializer(
        mut self,
        materializer: Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>,
    ) -> Self {
        self.inference_materializer = Some(materializer);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::{
        ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress,
        ManagedCredentialOperation, ManagedCredentialRollout, ManagedCredentialRolloutTarget,
        conjunctive_managed_credential_adoption_progress,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct HostedRolloutTarget;

    struct ProgressTarget {
        progress: ManagedCredentialAdoptionProgress,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ManagedCredentialRolloutTarget for HostedRolloutTarget {
        async fn rollout(
            &self,
            _event: &ManagedCredentialRollout,
        ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        }
    }

    #[async_trait::async_trait]
    impl ManagedCredentialRolloutTarget for ProgressTarget {
        async fn rollout(
            &self,
            _event: &ManagedCredentialRollout,
        ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.progress)
        }
    }

    #[test]
    fn hosted_rollout_target_is_an_explicit_optional_seam() {
        let defaults = ManagedServiceAdapters::default();
        assert!(defaults.credential_rollout_target.is_none());

        let hosted = defaults.with_credential_rollout_target(Arc::new(HostedRolloutTarget));
        assert!(hosted.credential_rollout_target.is_some());
    }

    #[test]
    fn rollout_acknowledgement_requires_both_targets_to_converge() {
        use ManagedCredentialAdoptionProgress::{Converged, Pending};

        // Decision table over the complete two-state product. This is the
        // safety invariant behind composing local Session adoption with an
        // injected custody or hosted rollout target.
        assert_eq!(
            conjunctive_managed_credential_adoption_progress(Converged, Converged),
            Converged
        );
        assert_eq!(
            conjunctive_managed_credential_adoption_progress(Converged, Pending),
            Pending
        );
        assert_eq!(
            conjunctive_managed_credential_adoption_progress(Pending, Converged),
            Pending
        );
        assert_eq!(
            conjunctive_managed_credential_adoption_progress(Pending, Pending),
            Pending
        );
    }

    #[tokio::test]
    async fn rollout_conjunction_delivers_to_both_targets_before_acknowledging() {
        let local_calls = Arc::new(AtomicUsize::new(0));
        let external_calls = Arc::new(AtomicUsize::new(0));
        let target = conjunctive_credential_rollout_target(
            Arc::new(ProgressTarget {
                progress: ManagedCredentialAdoptionProgress::Converged,
                calls: Arc::clone(&local_calls),
            }),
            Arc::new(ProgressTarget {
                progress: ManagedCredentialAdoptionProgress::Pending,
                calls: Arc::clone(&external_calls),
            }),
        );
        let event = ManagedCredentialRollout {
            id: "rollout-1".into(),
            workspace_id: "workspace-1".into(),
            vault_id: "vault-1".into(),
            credential_id: "credential-1".into(),
            source_id: awaken_credential_contract::CredentialSourceId("source-1".into()),
            source_version: 2,
            credential_revision: 2,
            operation: ManagedCredentialOperation::Update,
        };

        // Cause/effect: local convergence cannot hide an external pending
        // state, and both idempotent consumers see the exact same durable fact.
        let progress = target.rollout(&event).await.expect("rollout must deliver");
        assert_eq!(progress, ManagedCredentialAdoptionProgress::Pending);
        assert_eq!(local_calls.load(Ordering::Relaxed), 1);
        assert_eq!(external_calls.load(Ordering::Relaxed), 1);
    }

    // Cause/effect table: C1=product supplies a verified entitlement provider;
    // C2=provider absent. C1 is consumed by embedded IAM during composition;
    // C2 retains the canonical unlicensed baseline. No protocol adapter sees a
    // LicenseClaim or commercial catalog.
    #[test]
    fn entitlement_provider_is_an_explicit_one_shot_seam() {
        let defaults = ManagedServiceAdapters::default();
        assert!(defaults.entitlement_provider.is_none());
        assert!(defaults.background_services.is_empty());

        let licensed = defaults
            .with_entitlement_provider(Box::new(awaken_iam_core::EntitlementEngine::unlicensed()));
        assert!(licensed.entitlement_provider.is_some());
    }

    // Cause/effect table: C1=no product runtime adapter; C2=adapter supplied.
    // C1 retains Awaken's canonical credential materializer; C2 replaces only
    // the pre-existing inference port and leaves Coordinator ownership intact.
    #[test]
    fn coordinator_inference_override_is_absent_by_default() {
        assert!(
            CoordinatorServiceAdapters::default()
                .inference_materializer
                .is_none()
        );
    }
}
