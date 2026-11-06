// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The single namespace SandboxProvider delegates physical work
// @formal reason: to its existing marker/authority owner; tests preserve fencing.
//! Provider port for the OS-native namespace substrate.

use super::{NamespaceProvider, run_os_native_probe};
use crate::FilesystemHandleCodec as _;
use async_trait::async_trait;
use awaken_provisioning_contract as pc;

#[async_trait]
impl pc::SandboxReadiness for NamespaceProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        Self::capabilities()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    /// A real readiness probe of the OS-native isolator: bwrap must run a
    /// full unprivileged Namespace profile here (Linux), or `sandbox-exec` must run a trivial
    /// Seatbelt profile (macOS). On a host that can't, this returns an error so
    /// `select_provider` fails closed at selection instead of deferring the failure
    /// to `create` (or, worse, launching an unisolated process).
    ///
    /// Each observation is fresh and bounded. A previous success cannot mask
    /// host policy revocation; a previous failure cannot mask installation.
    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        let ok = run_os_native_probe().await;
        if ok {
            Ok(())
        } else {
            Err(pc::SandboxError::unsupported(
                if cfg!(any(target_os = "linux", target_os = "macos")) {
                    "OS-native sandbox (bwrap userns / macOS Seatbelt) unavailable on this host"
                } else {
                    "OS-native Namespace sandbox is unsupported on this platform; use sandbox_tier=local or a container-backed Worker"
                },
            ))
        }
    }
}

#[async_trait]
impl pc::SandboxProvider for NamespaceProvider {
    type Environment = dyn pc::Sandbox;

    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    fn adoption_layout(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxAdoptionLayout, pc::SandboxError> {
        let payload = handle.namespace_payload(Self::provider_kind())?;
        Ok(pc::SandboxAdoptionLayout {
            outputs_path: payload.outputs_path,
            base_env: payload.base_env,
            network: Some(payload.network),
        })
    }
    fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        handle.namespace_payload(Self::provider_kind())?;
        crate::realization_marker::validate_closed_observation(handle, observation)
    }
    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a pc::SandboxSpec,
        effect: pc::SandboxEffect<'a>,
        intent: pc::SandboxRealizationIntent,
    ) -> pc::ClaimedEffect<'a, std::sync::Arc<dyn pc::Sandbox>> {
        let claim = pc::EffectClaim::durable(
            crate::abandoned_realization::REALIZATION_RECONCILER,
            self.abandoned
                .retain(crate::sandbox_dir(&self.base, &spec.scope)),
        );
        pc::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            let sandbox = match effect {
                pc::SandboxEffect::Unattached => self.create_sandbox(spec).await?,
                pc::SandboxEffect::Fenced { fence, authority } => {
                    self.create_sandbox_for_effect(spec, fence, intent.source(), authority)
                        .await?
                }
            };
            Ok(std::sync::Arc::new(sandbox) as std::sync::Arc<dyn pc::Sandbox>)
        })
    }

    async fn observe(
        &self,
        adoption: pc::SandboxAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        let handle = adoption.handle;
        handle.namespace_payload(Self::provider_kind())?;
        let expected_effect_fence = adoption.observation_fence(basis)?;
        if expected_effect_fence.is_some()
            && handle.realization_fingerprint()
                != Some(&pc::SandboxRealizationFingerprint::from_spec(adoption.spec))
        {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: "Namespace sandbox handle does not match the effective observation spec"
                    .into(),
            });
        }
        crate::realization_marker::observe_adoption(
            &crate::sandbox_dir(&self.base, &handle.sandbox_id),
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?.as_deref(),
            expected_effect_fence,
        )
    }
    async fn adopt_for_effect(
        &self,
        adoption: pc::SandboxAdoption<'_>,
        effect: pc::SandboxEffect<'_>,
    ) -> Result<std::sync::Arc<dyn pc::Sandbox>, pc::SandboxError> {
        let sandbox = match effect {
            pc::SandboxEffect::Unattached => self.adopt_sandbox(adoption.handle).await?,
            pc::SandboxEffect::Fenced { fence, authority } => {
                self.adopt_sandbox_for_effect(adoption.spec, adoption.handle, fence, authority)
                    .await?
            }
        };
        Ok(std::sync::Arc::new(sandbox))
    }

    async fn prepare_terminal_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<std::sync::Arc<dyn pc::Sandbox>>, pc::SandboxError> {
        Ok(self
            .prepare_terminal_sandbox_for_effect(
                spec,
                handle,
                expected_effect_fence,
                terminal_effect_fence,
                authority,
            )
            .await?
            .map(|sandbox| std::sync::Arc::new(sandbox) as std::sync::Arc<dyn pc::Sandbox>))
    }
    fn restoration(&self) -> pc::SandboxRestoration<'_, std::sync::Arc<dyn pc::Sandbox>> {
        pc::SandboxRestoration::Unsupported
    }
}

impl awaken_provisioning_contract::SandboxInstallation for NamespaceProvider {
    fn install_memory_mounter(&self, mounter: std::sync::Arc<dyn pc::MemoryMounter>) {
        self.install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: std::sync::Arc<dyn pc::SecretBroker>) {
        self.install_secret_broker(broker);
    }
}
