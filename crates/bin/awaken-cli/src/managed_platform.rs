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
}

/// Infrastructure adapters for the one canonical Coordinator composition.
/// Hosted products may replace transport authentication, but cannot replace or
/// add a Coordinator router.
#[derive(Clone, Default)]
pub struct CoordinatorServiceAdapters {
    pub worker_authenticator:
        Option<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>>,
    pub worker_placement_policy: Option<Arc<dyn awaken_run_ingress::PlacementPolicy>>,
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
}
