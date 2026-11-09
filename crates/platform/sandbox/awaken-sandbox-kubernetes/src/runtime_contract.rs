// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
//! `ContainerRuntime` adapter for the Kubernetes backend.
//!
//! Kubernetes object construction, continuation, and lifecycle primitives remain
//! owned by the adjacent focused modules; this file owns only the neutral runtime
//! port projection and its attached-agent transport sequence.

use super::terminal_evidence::unmaterialized_absence_shape_is_closed;
use super::{
    K8sRuntime, PodProcessObserver, PodReadiness, api_not_found, backend, checkpoint, continuation,
    creation, egress_label, live_inputs, participants, pod_readiness, restore, sandbox_control,
};
use super::{guarded_claim, observation, process, terminal_evidence};
use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{AgentProcess, SandboxCreateAttempt, SandboxEffectFence};
use awaken_sandbox_container::engine::{ExistingRealizationDecision, legacy_unfenced_fingerprint};
use awaken_sandbox_container::{
    ContainerObservationExpectation, ContainerPlan, ContainerRealizationContext,
    ContainerRealizationIntent, ContainerRuntime, ContainerState, SandboxControlBindingRequest,
};
use awaken_sandbox_container::{MissingHandleTerminalEvidence, PreparedContainer};
use awaken_sandbox_control::SandboxControlServiceKind;
use std::sync::atomic::Ordering;

/// Reuse the canonical physical observer after a strict read-purpose preflight.
/// Prepared source data needs only the existing participant-disposal replay;
/// unprepared data still needs the independent retained claim checked below.
fn retirement_observation_expectation<'a>(
    mut expectation: ContainerObservationExpectation<'a>,
) -> Result<ContainerObservationExpectation<'a>, RuntimeError> {
    if let pc::SandboxObservationPurpose::WorkerRetirement { source_prepared } = expectation.purpose
    {
        if expectation.realization_fingerprint.is_none()
            || expectation
                .runtime_handle
                .and_then(awaken_sandbox_container::ContainerContinuationHandle::kubernetes_incarnation)
                .is_none()
            || (!source_prepared
                && expectation
                    .runtime_handle
                    .and_then(awaken_sandbox_container::ContainerContinuationHandle::kubernetes_incarnation)
                    .is_none_or(|(_, claim)| claim.is_none()))
        {
            return Err(backend(
                "Worker retirement requires original Kubernetes participant identities",
            ));
        }
        if source_prepared {
            // The root already made source effects durable. Reuse the one
            // exact original-participant observation for remaining disposal.
            expectation.purpose = pc::SandboxObservationPurpose::TerminalCleanup;
        }
    }
    Ok(expectation)
}

impl K8sRuntime {
    /// Compose the runtime with the process's wall clock (see the `clock`
    /// field). The execution composition always installs it.
    #[must_use]
    pub fn with_clock(mut self, clock: std::sync::Arc<dyn awaken_time::WallClock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Bound the wait for the scheduler (`k8s_unschedulable_wait_secs`).
    #[must_use]
    pub fn with_unschedulable_wait(mut self, wait: std::time::Duration) -> Self {
        self.unschedulable_wait = wait;
        self
    }

    /// Replace the CPU/memory bounds and disposable-Pod deadline
    /// (`k8s_pod_bounds`). An unbounded or inverted role is refused.
    pub fn with_pod_bounds(
        mut self,
        bounds: awaken_container_contract::K8sPodBounds,
    ) -> Result<Self, RuntimeError> {
        bounds.validate().map_err(RuntimeError::Backend)?;
        self.pod_bounds = bounds;
        Ok(self)
    }

    pub(super) async fn admit_observed_creation_attempt(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>, RuntimeError> {
        // Even a rich or unfenced direct plan cannot switch to the broker
        // after its first runtime admission. This is a mode freeze, not an
        // additional physical completion witness.
        self.create_mode_frozen.store(true, Ordering::Release);
        if self.create_transport.is_some() {
            // Brokered admission and close are owned by one durable provider
            // ledger. Do not maintain a parallel process-local attempt gate.
            return Ok(None);
        }
        let Some(fence) = context.effect_fence else {
            return Ok(None);
        };
        if !unmaterialized_absence_shape_is_closed(
            !plan.packages.is_empty(),
            !plan.binds.is_empty(),
            !plan.memory_mounts.is_empty(),
            continuation::claim_required(plan, self.continuation_volume.is_some()),
        ) {
            return Ok(None);
        }
        let gate = self.observed_creation_attempt(context.scope, fence);
        let guard = gate.barrier.clone().read_owned().await;
        if gate.closed.load(Ordering::Acquire) {
            return Err(RuntimeError::EffectFenceExpired);
        }
        Ok(Some(guard))
    }

    pub(super) async fn close_observed_creation_attempt(
        &self,
        scope: &str,
        fence: &SandboxEffectFence,
    ) -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
        let gate = self.existing_creation_attempt(scope, fence)?;
        let guard = gate.barrier.clone().write_owned().await;
        gate.closed.store(true, Ordering::Release);
        Some(guard)
    }
}

pub(super) fn deletion_incarnation(
    runtime_handle: Option<&awaken_sandbox_container::ContainerContinuationHandle>,
) -> Result<(Option<&str>, Option<&str>), RuntimeError> {
    match runtime_handle {
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid,
        })
        | Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            claim_uid,
            ..
        }) => Ok((Some(pod_uid.as_str()), claim_uid.as_deref())),
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuation {
            ..
        }) => Err(backend(
            "legacy Kubernetes handle has no Pod UID deletion fence",
        )),
        Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(_)) => {
            Err(backend(
                "guarded Kubernetes claim requires aggregate-authorized terminal disposal",
            ))
        }
        Some(awaken_sandbox_container::ContainerContinuationHandle::HostBindRestoration(_)) => Err(
            backend("Kubernetes removal received a host-bind continuation handle"),
        ),
        None => Ok((None, None)),
    }
}

#[async_trait]
impl ContainerRuntime for K8sRuntime {
    async fn owned_process_binding(
        &self,
        container_id: &str,
        handle: Option<&awaken_sandbox_container::ContainerContinuationHandle>,
    ) -> Result<Option<String>, RuntimeError> {
        let Some((pod_uid, _)) = handle.and_then(
            awaken_sandbox_container::ContainerContinuationHandle::kubernetes_incarnation,
        ) else {
            return Ok(None);
        };
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        if pod.metadata.uid.as_deref() != Some(pod_uid)
            || pod
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("awaken-egress"))
                .map(String::as_str)
                != Some(egress_label(&awaken_sandbox_container::NetworkMode::None))
        {
            return Ok(None);
        }
        self.network_policy_attestation
            .refresh(&self.clients.control, &self.namespace)
            .await?;
        let observer = PodProcessObserver::new(self.clients.control.clone(), &self.namespace)?;
        let binding = observer.bind_pod_process(container_id, pod_uid).await?;
        if !binding.evidence_was_retained() {
            return Ok(None);
        }
        serde_json::to_string(&binding).map(Some).map_err(backend)
    }
    async fn prewarm_images(&self, images: &[String]) -> Result<(), RuntimeError> {
        self.reconcile_image_prewarm(images).await
    }

    async fn checkpoint_upload(
        &self,
        expectation: ContainerObservationExpectation<'_>,
        access: awaken_provisioning_contract::SandboxCheckpointAccess<'_>,
        mutation: awaken_provisioning_contract::SandboxCheckpointMutation<'_>,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
        checkpoint::persist_checkpoint_upload(self, expectation, access, mutation).await
    }

    async fn historical_terminal_projection(
        &self,
        expectation: ContainerObservationExpectation<'_>,
    ) -> Result<Option<awaken_sandbox_container::ContainerTerminalHistory>, RuntimeError> {
        use awaken_sandbox_container::{
            ContainerContinuationHandle, ContainerTerminalHistory, RootfsPlan,
        };
        let handle = expectation.runtime_handle;
        // Preserve the existing explicit no-claim historical candidate. It
        // grants nothing and the provider still requires an exact hash match.
        if matches!(handle.and_then(ContainerContinuationHandle::kubernetes_incarnation), Some((pod_uid, None)) if !pod_uid.trim().is_empty())
            && !matches!(
                handle,
                Some(ContainerContinuationHandle::KubernetesContinuationV3 { .. })
            )
        {
            let mut configuration = self.realization_configuration()?;
            configuration.insert("continuation_volume".into(), "none".into());
            return Ok(Some(ContainerTerminalHistory::Candidate {
                configuration,
                rootfs: None,
                resolved_image: None,
            }));
        }
        let Some(ContainerContinuationHandle::KubernetesContinuationV3 {
            pod_uid,
            participants,
            ..
        }) = handle
        else {
            return Ok(None);
        };
        if expectation.purpose != pc::SandboxObservationPurpose::TerminalCleanup
            || expectation.effect_fence.is_none()
            || expectation.adoption_fingerprint.is_none()
            || expectation.realization_fingerprint.is_none()
            || participants.namespace() != self.namespace
            || participants.pod_uid() != pod_uid
        {
            return Ok(None);
        }
        // The ONE observer proves all original Pod/PVC/participant identities.
        // Backend errors and a live retained PVC never become total absence.
        let observed = observation::observe(self, expectation).await?;
        match observed {
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some(physical_incarnation),
            } if physical_incarnation == *pod_uid => {
                return Ok(Some(ContainerTerminalHistory::Absent {
                    physical_incarnation,
                }));
            }
            pc::SandboxObservation::Ready
            | pc::SandboxObservation::Terminal { .. }
            | pc::SandboxObservation::Disposing { .. }
            | pc::SandboxObservation::Provisioning { .. } => {}
            _ => return Ok(None),
        }
        let pod = self
            .pods()
            .get(expectation.container_id)
            .await
            .map_err(backend)?;
        if pod.metadata.uid.as_deref() != Some(pod_uid.as_str()) {
            return Err(backend(
                "historical image observation changed its Pod incarnation",
            ));
        }
        let image = pod
            .spec
            .as_ref()
            .and_then(|spec| {
                spec.containers
                    .iter()
                    .find(|container| container.name == "agent")
            })
            .and_then(|container| container.image.clone())
            .ok_or_else(|| backend("historical Pod has no exact agent image"))?;
        // The provider recomputes BOTH fingerprints. The final image is only
        // a base-rootfs candidate: a derived package image cannot be guessed
        // into the old base when those immutable projections disagree.
        Ok(Some(ContainerTerminalHistory::Candidate {
            configuration: self.realization_configuration()?,
            rootfs: Some(RootfsPlan::Image(image.clone())),
            resolved_image: Some(image),
        }))
    }

    fn realization_configuration(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, RuntimeError> {
        let owner = self
            .owner
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(backend)?;
        let image_pull_secrets =
            serde_json::to_string(&self.image_pull_secrets).map_err(backend)?;
        let continuation = self.continuation_volume.as_ref().map_or_else(
            || "none".to_owned(),
            |volume| {
                format!(
                    "storage_class={};size={}",
                    volume.storage_class_name.as_deref().unwrap_or(""),
                    volume.size
                )
            },
        );
        let agent_transport = self.pod_channel_port.map_or_else(
            || {
                self.rendezvous.map_or_else(
                    || format!("direct:{}", self.agent_addr),
                    |address| format!("rendezvous:{address}"),
                )
            },
            |port| format!("pod_channel:{port}"),
        );
        let mut configuration = std::collections::BTreeMap::from([
            ("backend".into(), "kubernetes".into()),
            (
                "execution_uid".into(),
                awaken_sandbox_container::engine::SANDBOX_EXECUTION_ID.to_string(),
            ),
            (
                "hardening_revision".into(),
                awaken_sandbox_container::engine::CONTAINER_HARDENING_REVISION.into(),
            ),
            ("namespace".into(), self.namespace.clone()),
            (
                "realization_namespace".into(),
                self.realization_namespace.as_str().to_owned(),
            ),
            ("agent_transport".into(), agent_transport),
            ("image_pull_secrets".into(), image_pull_secrets),
            ("continuation_volume".into(), continuation),
            ("owner_reference".into(), owner.unwrap_or_default()),
            // Live attestation proves this contract before admission. The
            // version binds the immutable Pod to policy semantics, not to one
            // mutable apiserver observation.
            ("network_policy_contract".into(), "v1".into()),
        ]);
        if self.create_transport.is_some() {
            // Existing direct-created Pods retain their exact old fingerprint;
            // brokered Pods name the stricter Create authority explicitly.
            configuration.insert("create_request_transport".into(), "brokered-v1".into());
        }
        Ok(configuration)
    }

    fn enforces_network_none(&self) -> bool {
        self.network_policy_attestation.current()
    }

    fn enforces_network_allowlist(&self) -> bool {
        self.network_policy_attestation.allowlist_current()
    }

    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        self.network_policy_attestation
            .refresh(&self.clients.control, &self.namespace)
            .await
    }

    fn has_native_memory_mounts(&self) -> bool {
        true
    }

    fn sandbox_control_services(&self) -> std::collections::BTreeSet<SandboxControlServiceKind> {
        sandbox_control::services(self)
    }

    async fn sandbox_control_binding(
        &self,
        container_id: &str,
        request: SandboxControlBindingRequest<'_>,
    ) -> Result<Option<awaken_sandbox_container::SandboxControlIncarnation>, RuntimeError> {
        sandbox_control::bind(self, container_id, request).await
    }

    async fn open_sandbox_control_channel(
        &self,
        container_id: &str,
        binding: &awaken_sandbox_container::SandboxControlIncarnation,
        kind: SandboxControlServiceKind,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        sandbox_control::open_channel(self, container_id, binding, kind).await
    }

    fn uses_persistent_volume_claims(&self) -> bool {
        true
    }

    fn uses_host_bind_materialization(&self) -> bool {
        false
    }

    fn supports_secret_writeback(&self) -> bool {
        true
    }

    fn supports_live_input_projection(&self) -> bool {
        true
    }

    async fn project_live_input(
        &self,
        container_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        live_inputs::project(self, container_id, path, bytes).await
    }

    async fn remove_live_input(&self, container_id: &str, path: &str) -> Result<(), RuntimeError> {
        live_inputs::remove(self, container_id, path).await
    }

    async fn preflight_create_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<(), RuntimeError> {
        let _attempt = self.admit_observed_creation_attempt(context, plan).await?;
        super::existing_realization::admit_plan(self, plan).await?;
        super::existing_realization::create_decision(self, context, plan, realization_fingerprint)
            .await
            .map(drop)
    }

    async fn missing_handle_terminal_evidence(
        &self,
        scope: &str,
        plan: &ContainerPlan,
        expected_effect_fence: Option<&SandboxEffectFence>,
    ) -> Result<MissingHandleTerminalEvidence, RuntimeError> {
        terminal_evidence::missing_handle_terminal_evidence(
            self,
            scope,
            plan,
            expected_effect_fence,
        )
        .await
    }

    fn terminal_continuation(
        &self,
        plan: &ContainerPlan,
        expected_adoption: &pc::SandboxRealizationFingerprint,
        handle: &awaken_sandbox_container::DecodedContainerHandle,
    ) -> Result<Option<awaken_sandbox_container::ContainerContinuationHandle>, RuntimeError> {
        participants::terminal_continuation(self, plan, expected_adoption, handle)
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let fingerprint = legacy_unfenced_fingerprint(id);
        let attempt = awaken_sandbox_container::engine::fresh_create_attempt();
        let intent = ContainerRealizationIntent::Create;
        let context = ContainerRealizationContext::new(id, &fingerprint, None, &intent, &attempt);
        self.create_for_effect(&context, plan, &fingerprint).await
    }

    async fn prepare_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<PreparedContainer, RuntimeError> {
        let _attempt = self.admit_observed_creation_attempt(context, plan).await?;
        super::existing_realization::admit_plan(self, plan).await?;
        if !plan.memory_mounts.is_empty() && context.effect_fence.is_none() {
            return Err(backend(
                "Kubernetes Memory projection requires a durable Environment effect fence",
            ));
        }
        let mut decision = super::existing_realization::create_decision(
            self,
            context,
            plan,
            Some(realization_fingerprint),
        )
        .await?;
        for _ in 0..4 {
            let before = decision.clone();
            let outcome = match decision {
                ExistingRealizationDecision::Create => {
                    creation::prepare(self, context, plan, realization_fingerprint)
                        .await
                        .map(|locator| {
                            PreparedContainer::fresh(locator, context.attempt.as_str().to_owned())
                        })
                }
                ExistingRealizationDecision::ConvergeCreating(observed) => {
                    let attempt_id = observed.attempt_id.ok_or_else(|| {
                        backend("Kubernetes prepared realization has no Memory attempt identity")
                    })?;
                    Ok(PreparedContainer::recovering(observed.locator, attempt_id))
                }
                ExistingRealizationDecision::ReuseReady(observed) => {
                    let attempt_id = observed.attempt_id.ok_or_else(|| {
                        backend("Kubernetes active realization has no Memory attempt identity")
                    })?;
                    Ok(PreparedContainer::already_active(
                        observed.locator,
                        attempt_id,
                    ))
                }
                ExistingRealizationDecision::ReplaceExact(observed) => {
                    super::pod_removal::replace_exact_realization(self, &observed)
                        .await
                        .map(|()| {
                            PreparedContainer::fresh(
                                String::new(),
                                context.attempt.as_str().to_owned(),
                            )
                        })
                }
                ExistingRealizationDecision::ValidateExisting(_) => {
                    return Err(backend(
                        "Kubernetes create reached a fingerprint-deferred decision",
                    ));
                }
            };
            match outcome {
                Ok(prepared) if !prepared.locator.is_empty() => return Ok(prepared),
                Ok(_) => {}
                Err(error) => {
                    let after = super::existing_realization::create_decision(
                        self,
                        context,
                        plan,
                        Some(realization_fingerprint),
                    )
                    .await
                    .map_err(RuntimeError::after_mutation)?;
                    if after == before {
                        return Err(error.after_mutation());
                    }
                    decision = after;
                    continue;
                }
            }
            decision = super::existing_realization::create_decision(
                self,
                context,
                plan,
                Some(realization_fingerprint),
            )
            .await
            .map_err(RuntimeError::after_mutation)?;
        }
        Err(backend("Kubernetes exact reservation did not converge").after_mutation())
    }

    async fn activate_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: &pc::SandboxRealizationFingerprint,
        prepared_locator: &str,
    ) -> Result<String, RuntimeError> {
        let _attempt = self.admit_observed_creation_attempt(context, plan).await?;
        super::existing_realization::admit_plan(self, plan).await?;
        let runtime_id = self.realization_runtime_id(context.scope)?;
        if context.effect_fence.is_none() {
            let pod = self.pods().get(prepared_locator).await.map_err(backend)?;
            let pod_uid = pod
                .metadata
                .uid
                .as_deref()
                .ok_or_else(|| backend("prepared Kubernetes Pod has no UID"))?;
            creation::converge(
                self,
                &runtime_id,
                prepared_locator,
                pod_uid,
                context,
                plan,
                realization_fingerprint,
            )
            .await?;
            return Ok(prepared_locator.to_owned());
        }
        let decision = super::existing_realization::create_decision(
            self,
            context,
            plan,
            Some(realization_fingerprint),
        )
        .await?;
        let observed = match decision {
            ExistingRealizationDecision::ConvergeCreating(observed)
            | ExistingRealizationDecision::ReuseReady(observed)
                if observed.locator == prepared_locator =>
            {
                observed
            }
            ExistingRealizationDecision::Create => {
                return Err(backend(
                    "Kubernetes prepared realization disappeared before activation",
                ));
            }
            _ => {
                return Err(backend(
                    "Kubernetes activation resolved a different or inadmissible realization",
                ));
            }
        };
        let recovery_attempt =
            SandboxCreateAttempt::new(observed.attempt_id.clone().ok_or_else(|| {
                backend("Kubernetes Sandbox realization has no projected-content attempt fence")
            })?);
        let recovery_context = context.with_attempt(&recovery_attempt);
        creation::converge(
            self,
            &runtime_id,
            &observed.locator,
            &observed.incarnation.identity,
            &recovery_context,
            plan,
            realization_fingerprint,
        )
        .await
        .map(|()| observed.locator)
    }

    async fn recover_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<Option<awaken_sandbox_container::RuntimeRestoreTarget>, RuntimeError> {
        super::existing_realization::admit_plan(self, plan).await?;
        restore::recover(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn restore_or_adopt(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<awaken_sandbox_container::RuntimeRestoreTarget, RuntimeError> {
        super::existing_realization::admit_plan(self, plan).await?;
        creation::restore_or_adopt(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn restoration_evidence(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        restore::restoration_evidence(self, container_id).await
    }

    async fn restoration_plan_fingerprint(
        &self,
        container_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        restore::plan_fingerprint(self, container_id).await
    }

    async fn dispose_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), RuntimeError> {
        restore::dispose(
            self,
            id,
            plan,
            plan_fingerprint,
            evidence,
            authorization,
            authority,
        )
        .await
    }

    async fn handle_extra(
        &self,
        container_id: &str,
    ) -> Result<Option<awaken_sandbox_container::ContainerContinuationHandle>, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        continuation::handle_extra(&pod)
    }

    async fn observe(
        &self,
        expectation: ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        let expectation = retirement_observation_expectation(expectation)?;
        observation::observe(self, expectation).await
    }

    async fn spawn(
        &self,
        admitted: awaken_sandbox_container::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        process::spawn_exec(self, admitted, command).await
    }

    async fn spawn_agent(
        &self,
        admitted: awaken_sandbox_container::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<AgentProcess, RuntimeError> {
        process::spawn_agent(self, admitted, command).await
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        process::open_agent_channel(self, container_id).await
    }

    async fn read_live_file(
        &self,
        container_id: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        process::read_live_file(self, container_id, path).await
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        let pod = match self.pods().get(container_id).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => {
                return Ok(ContainerState::Gone);
            }
            Err(error) => return Err(backend(error)),
        };
        Ok(match pod_readiness(&pod) {
            PodReadiness::Ready => ContainerState::Running,
            PodReadiness::Waiting(_) => ContainerState::Provisioning,
            PodReadiness::Failed(_) => ContainerState::Gone,
        })
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        // Compile path: read the terminated exit code. A running deployment watches
        // the Pod to completion instead of a single read.
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        let code = pod
            .status
            .and_then(|s| s.container_statuses)
            .and_then(|c| c.into_iter().next())
            .and_then(|cs| cs.state)
            .and_then(|st| st.terminated)
            .map(|t| t.exit_code);
        match code {
            Some(code) => Ok(pc::ExitStatus {
                code: Some(code),
                signaled: false,
            }),
            None => Err(backend("agent pod has not terminated")),
        }
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        match self.inspect(container_id).await? {
            ContainerState::Provisioning | ContainerState::Running => Ok(None),
            ContainerState::Gone => Ok(Some(pc::ExitStatus {
                code: None,
                signaled: true,
            })),
        }
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        let _ = (container_id, signal);
        Err(backend(
            "Kubernetes environment deletion requires the persisted Pod UID; signal the attached process handle instead",
        ))
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band: read from the outputs PVC, not through the API server.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        _container_id: &str,
        _artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        Err(backend(
            "k8s artifacts are read out-of-band from the outputs PVC",
        ))
    }

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // The owner controller patches the native Lease renewTime. This adapter
        // must not duplicate that authority or infer disposal from Pod age.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        super::pod_removal::remove_bound(self, container_id, None, None, None, false, None).await
    }

    async fn remove_unreceipted_scope(&self, scope: &str) -> Result<(), RuntimeError> {
        // Kubernetes names the Pod from the scope, so the cancelled creator's
        // object is addressable without the durable handle it never produced.
        // Absent is success: the create may have been dropped before the API
        // server ever saw it.
        let pod_name = awaken_worker_fleet_kubernetes::managed_names::pod_name(
            &awaken_worker_fleet_kubernetes::managed_names::k8s_runtime_id(scope)?,
        );
        let observed = match self.pods().get(&pod_name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        // The Pod's own observed UID is the only incarnation evidence that can
        // exist here, and re-reading it is not a weaker gate than the handle
        // path: this scope was minted by one warm pool that holds no Session
        // receipt, so there is no durable claim for a stale UID to contradict.
        let pod_uid = observed
            .metadata
            .uid
            .clone()
            .ok_or_else(|| backend("Kubernetes warm capacity Pod has no incarnation UID"))?;
        let claim_uid = continuation::bound_claim_uid(&observed).map(str::to_owned);
        super::pod_removal::remove_bound(
            self,
            &pod_name,
            Some(&pod_uid),
            claim_uid.as_deref(),
            None,
            false,
            None,
        )
        .await
    }

    async fn remove_with_handle(
        &self,
        container_id: &str,
        runtime_handle: Option<&awaken_sandbox_container::ContainerContinuationHandle>,
    ) -> Result<(), RuntimeError> {
        let (pod_uid, claim_uid) = deletion_incarnation(runtime_handle)?;
        super::pod_removal::remove_bound(
            self,
            container_id,
            pod_uid,
            claim_uid,
            runtime_handle.and_then(
                awaken_sandbox_container::ContainerContinuationHandle::projected_participants,
            ),
            true,
            None,
        )
        .await
    }

    async fn remove_exact_incarnation(
        &self,
        container_id: &str,
        runtime_handle: Option<&awaken_sandbox_container::ContainerContinuationHandle>,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), RuntimeError> {
        if let Some(
            awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(guarded),
        ) = runtime_handle
        {
            return guarded_claim::remove_guarded_claim(
                self,
                container_id,
                guarded,
                authorization,
                authority,
            )
            .await;
        }
        let (pod_uid, claim_uid) = deletion_incarnation(runtime_handle)?;
        super::pod_removal::remove_bound(
            self,
            container_id,
            pod_uid,
            claim_uid,
            runtime_handle.and_then(
                awaken_sandbox_container::ContainerContinuationHandle::projected_participants,
            ),
            true,
            Some((authorization, authority)),
        )
        .await
    }
}

#[cfg(test)]
mod unmaterialized_absence_tests {
    use super::*;
    use crate::ephemeral_plan;

    #[tokio::test]
    async fn exact_attempt_close_joins_in_flight_calls_and_rejects_late_reentry() {
        // UA2: no observation creates closure evidence. Once the exact effect
        // has entered the runtime, close waits for its read-side critical
        // section, installs a process-lifetime tombstone, and every late retry
        // is rejected before another backend call.
        let runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let fence = SandboxEffectFence::new(
            "create-operation",
            "worker-owner",
            "worker-boot",
            7,
            u64::MAX,
        )
        .unwrap();
        assert!(
            runtime
                .close_observed_creation_attempt("scope", &fence)
                .await
                .is_none(),
            "unseen attempt has no closure witness"
        );
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(serde_json::json!("fingerprint")).unwrap();
        let intent = ContainerRealizationIntent::Create;
        let attempt = awaken_sandbox_container::engine::fresh_create_attempt();
        let context = ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            Some(&fence),
            &intent,
            &attempt,
        );
        let plan = ephemeral_plan();
        let admitted = runtime
            .admit_observed_creation_attempt(&context, &plan)
            .await
            .unwrap()
            .expect("exact attempt is tracked");
        let mut closing = Box::pin(runtime.close_observed_creation_attempt("scope", &fence));
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(closing.as_mut(), cx).is_pending(),
                "close cannot pass the in-flight call"
            );
            std::task::Poll::Ready(())
        })
        .await;
        drop(admitted);
        let closed = closing.await.expect("observed attempt closes");
        drop(closed);
        assert!(
            matches!(
                runtime
                    .admit_observed_creation_attempt(&context, &plan)
                    .await,
                Err(RuntimeError::EffectFenceExpired)
            ),
            "late retry cannot cross the closed attempt"
        );
    }

    #[tokio::test]
    async fn replacement_runtime_does_not_inherit_an_unproven_creation_gate() {
        // Causes: C1 Worker A admits one exact Pod-create attempt; C2 Worker B
        // starts with the same scope and fence while A may still have a delayed
        // API request; C3 A later leaves its own gate. Effects: E1 B cannot
        // certify closure before or after C3; E2 only A may close its observed
        // attempt. Rules X1=C1+C2->E1, X2=X1+C3->E1+E2. This pins the current
        // cross-process fail-closed limit until a durable physical proof exists.
        let old = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let replacement = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let fence = SandboxEffectFence::new(
            "create-operation",
            "worker-owner",
            "worker-boot",
            7,
            u64::MAX,
        )
        .unwrap();
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(serde_json::json!("fingerprint")).unwrap();
        let intent = ContainerRealizationIntent::Create;
        let attempt = awaken_sandbox_container::engine::fresh_create_attempt();
        let context = ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            Some(&fence),
            &intent,
            &attempt,
        );
        let admitted = old
            .admit_observed_creation_attempt(&context, &ephemeral_plan())
            .await
            .unwrap()
            .expect("X1 old Worker has an admitted attempt");
        assert!(
            replacement
                .close_observed_creation_attempt("scope", &fence)
                .await
                .is_none(),
            "X1/E1 a new process cannot infer the old API request closed"
        );
        drop(admitted);
        assert!(
            replacement
                .close_observed_creation_attempt("scope", &fence)
                .await
                .is_none(),
            "X2/E1 old in-memory completion is not a durable witness"
        );
        assert!(
            old.close_observed_creation_attempt("scope", &fence)
                .await
                .is_some(),
            "X2/E2 the original process can close what it observed"
        );
    }
}
