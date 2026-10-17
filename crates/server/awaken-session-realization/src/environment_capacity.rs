//! Warm Environment capacity owned by the existing [`EnvironmentProviders`]: CacheVolume and image
//! preparation, the default and per-Environment warm Session shapes, and their
//! disposal. Every operation enters through the installed Session provider.

use crate::environment_providers::EnvironmentProviders;
use awaken_session_configuration_contract::environment_projection;

impl EnvironmentProviders {
    /// The exact capacity shape of one Environment under this provider's
    /// network isolation.
    fn capacity_projection(
        &self,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    ) -> environment_projection::EnvironmentCapacityProjection {
        let isolation = self.session_provider.realizer().capabilities();
        environment_projection::environment_capacity_projection(
            environment,
            isolation.network_isolation,
        )
    }

    /// Eagerly prepare one caller-owned CacheVolume. The same `(key, location)`
    /// is a no-op when Session realization later requests it. `location` remains
    /// exact for HostPath, Docker/Podman bind mounts, and Kubernetes PVCs.
    pub async fn prewarm_cache_volume(
        &self,
        volume: crate::cache_volume::CacheVolumeWarmup,
    ) -> Result<(), crate::cache_volume::CacheVolumeError> {
        self.cache_volume_prewarmer.prewarm(volume).await
    }

    /// Reuse current Environment demand for node-image preparation, independently
    /// of physical warm capacity (which cannot transfer retained filesystems).
    pub async fn prewarm_environment_images(
        &self,
        environments: &[awaken_session_configuration_contract::EnvironmentSnapshot],
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let mut specs = vec![environment_projection::default_session_sandbox_spec(
            "image-warmup",
        )];
        specs.extend(
            environments
                .iter()
                .filter(|environment| !environment.self_hosted)
                .map(|environment| self.capacity_projection(environment).spec),
        );
        self.session_provider
            .capacity()
            .prewarm_images(&specs)
            .await
    }

    /// Warm the deployment's canonical empty Session shape to the target selected
    /// by the Worker's one global capacity plan.
    pub async fn prewarm_default_environment_capacity(
        &self,
        target: usize,
    ) -> Result<usize, awaken_provisioning_contract::SandboxError> {
        if !self.runs_local_dispatch_pool || self.warm_pool.limits().is_none() || target == 0 {
            return Ok(0);
        }
        self.session_provider
            .capacity()
            .prewarm(
                &environment_projection::default_session_sandbox_spec("environment-warmup"),
                target,
            )
            .await
    }

    #[must_use]
    pub fn default_environment_capacity_shape(
        &self,
    ) -> awaken_provisioning_contract::SandboxCapacityShapeId {
        awaken_provisioning_contract::SandboxCapacityShapeId::from_spec(
            &environment_projection::default_session_sandbox_spec("environment-warmup"),
        )
        .expect("default Environment capacity spec is mount-less")
    }

    pub async fn discard_default_environment_capacity(&self) {
        self.session_provider
            .capacity()
            .discard_capacity(&environment_projection::default_session_sandbox_spec(
                "environment-warmup",
            ))
            .await;
    }

    /// Reconcile one current executable Environment into the exact mount-less
    /// shape ordinary Session creation requests. Docker, Podman and Kubernetes
    /// all enter through the installed provider/capacity pair.
    pub async fn prewarm_environment_snapshot(
        &self,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
        target: usize,
    ) -> Result<usize, awaken_provisioning_contract::SandboxError> {
        if !self.runs_local_dispatch_pool
            || self.warm_pool.limits().is_none()
            || target == 0
            || environment.self_hosted
        {
            return Ok(0);
        }
        let projection = self.capacity_projection(environment);
        self.session_provider
            .capacity()
            .prewarm(&projection.spec, target)
            .await
    }

    /// Per-shape target and the global ready-capacity budget across both the
    /// default Session shape and current Environment shapes.
    #[must_use]
    pub fn environment_warmup_limits(&self) -> (usize, usize) {
        if !self.runs_local_dispatch_pool {
            return (0, 0);
        }
        self.warm_pool.limits().unwrap_or((0, 0))
    }

    /// Observe actual ready capacity instead of trusting the last reconciliation
    /// result; checkout and global eviction may change it between heartbeats.
    #[must_use]
    pub fn ready_environment_snapshot_capacity(
        &self,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    ) -> usize {
        let projection = self.capacity_projection(environment);
        self.session_provider
            .capacity()
            .ready_capacity(&projection.spec)
    }

    /// Exact identity published in Worker receipts for the same canonical spec
    /// consumed by the installed capacity provider.
    #[must_use]
    pub fn environment_snapshot_capacity_shape(
        &self,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    ) -> awaken_provisioning_contract::SandboxCapacityShapeId {
        self.capacity_projection(environment).shape_id
    }

    /// Drop unused capacity for an Environment shape removed from current
    /// desired state. Active Sessions are unaffected.
    pub async fn discard_environment_snapshot_capacity(
        &self,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    ) {
        let projection = self.capacity_projection(environment);
        self.session_provider
            .capacity()
            .discard_capacity(&projection.spec)
            .await;
    }

    /// Dispose never-used warm capacity after claim admission and in-flight work
    /// have drained. Active Session environments remain owned by their slots.
    pub async fn shutdown_environment_capacity(&self) {
        self.session_provider.capacity().shutdown_capacity().await;
    }
}
