// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
//! `ContainerRuntime` adapter for the Kubernetes backend.
//!
//! Kubernetes object construction, continuation, and lifecycle primitives remain
//! owned by the adjacent focused modules; this file owns only the neutral runtime
//! port projection and its attached-agent transport sequence.

use super::*;
use crate::PreparedContainer;

fn unmaterialized_absence_shape_is_closed(
    has_packages: bool,
    has_binds: bool,
    has_memory: bool,
    has_continuation_claim: bool,
) -> bool {
    !(has_packages || has_binds || has_memory || has_continuation_claim)
}

impl K8sRuntime {
    async fn admit_observed_creation_attempt(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>, RuntimeError> {
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

    async fn close_observed_creation_attempt(
        &self,
        scope: &str,
        fence: &ContainerEffectFence,
    ) -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
        let gate = self.existing_creation_attempt(scope, fence)?;
        let guard = gate.barrier.clone().write_owned().await;
        gate.closed.store(true, Ordering::Release);
        Some(guard)
    }
}

pub(super) fn deletion_incarnation(
    runtime_handle: Option<&crate::ContainerContinuationHandle>,
) -> Result<(Option<&str>, Option<&str>), RuntimeError> {
    match runtime_handle {
        Some(crate::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid,
        }) => Ok((Some(pod_uid.as_str()), claim_uid.as_deref())),
        Some(crate::ContainerContinuationHandle::KubernetesContinuation { .. }) => Err(backend(
            "legacy Kubernetes handle has no Pod UID deletion fence",
        )),
        Some(crate::ContainerContinuationHandle::HostBindRestoration(_)) => Err(backend(
            "Kubernetes removal received a host-bind continuation handle",
        )),
        None => Ok((None, None)),
    }
}

#[async_trait]
impl ContainerRuntime for K8sRuntime {
    async fn owned_process_binding(
        &self,
        container_id: &str,
        handle: Option<&crate::ContainerContinuationHandle>,
    ) -> Result<Option<String>, RuntimeError> {
        let Some(crate::ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, .. }) =
            handle
        else {
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
                != Some(egress_label(&crate::NetworkMode::None))
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
        access: crate::ContainerCheckpointAccess<'_>,
        mutation: crate::ContainerCheckpointMutation<'_>,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
        self.persist_checkpoint_upload(expectation, access, mutation)
            .await
    }

    fn historical_terminal_configuration(
        &self,
        handle: Option<&crate::ContainerContinuationHandle>,
    ) -> Result<Option<std::collections::BTreeMap<String, String>>, RuntimeError> {
        // A typed V2 absence is not legacy missing evidence. Do not infer an
        // old PVC's storage class/size from its UID or today's deployment.
        if !matches!(handle, Some(crate::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid, claim_uid: None,
        }) if !pod_uid.trim().is_empty())
        {
            return Ok(None);
        }
        let mut configuration = self.realization_configuration()?;
        configuration.insert("continuation_volume".into(), "none".into());
        Ok(Some(configuration))
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
        Ok(std::collections::BTreeMap::from([
            ("backend".into(), "kubernetes".into()),
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
        ]))
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
    ) -> Result<Option<crate::SandboxControlIncarnation>, RuntimeError> {
        sandbox_control::bind(self, container_id, request).await
    }

    async fn open_sandbox_control_channel(
        &self,
        container_id: &str,
        binding: &crate::SandboxControlIncarnation,
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
        self.admit_plan(plan).await?;
        self.create_decision(context, plan, realization_fingerprint)
            .await
            .map(drop)
    }

    async fn prove_unmaterialized_absence(
        &self,
        scope: &str,
        plan: &ContainerPlan,
        expected_effect_fence: Option<&ContainerEffectFence>,
    ) -> Result<bool, RuntimeError> {
        // Kubernetes creates the Pod before ConfigMaps, Secrets, Memory
        // projection, or control sidecars. For this deliberately narrow shape,
        // therefore, absence of the canonical Pod and PVC proves that no
        // independently governed participant was ever published. Richer plans
        // retain the fail-closed handle requirement.
        if !unmaterialized_absence_shape_is_closed(
            !plan.packages.is_empty(),
            !plan.binds.is_empty(),
            !plan.memory_mounts.is_empty(),
            continuation::claim_required(plan, self.continuation_volume.is_some()),
        ) {
            return Ok(false);
        }
        let Some(expected_effect_fence) = expected_effect_fence else {
            return Ok(false);
        };
        let Some(_closed_attempt) = self
            .close_observed_creation_attempt(scope, expected_effect_fence)
            .await
        else {
            // A replacement process cannot infer closure of a request it never
            // joined. Keep recovery pending until a durable primitive exists.
            return Ok(false);
        };
        self.admit_plan(plan).await?;
        let runtime_id = self.realization_runtime_id(scope)?;
        let runtime_pod_name = pod_name(&runtime_id);
        let pod = self
            .pods()
            .get_opt(&runtime_pod_name)
            .await
            .map_err(backend)?;
        if pod.is_some() {
            return Ok(false);
        }
        let claim = self
            .persistent_volume_claims()
            .get_opt(&continuation_claim_for_pod(&runtime_pod_name)?)
            .await
            .map_err(backend)?;
        Ok(claim.is_none())
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let fingerprint = legacy_unfenced_fingerprint(id);
        let attempt = crate::fresh_create_attempt();
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
        self.admit_plan(plan).await?;
        if !plan.memory_mounts.is_empty() && context.effect_fence.is_none() {
            return Err(backend(
                "Kubernetes Memory projection requires a durable Environment effect fence",
            ));
        }
        let mut decision = self
            .create_decision(context, plan, Some(realization_fingerprint))
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
                    self.replace_exact_realization(&observed).await.map(|()| {
                        PreparedContainer::fresh(String::new(), context.attempt.as_str().to_owned())
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
                    let after = self
                        .create_decision(context, plan, Some(realization_fingerprint))
                        .await
                        .map_err(RuntimeError::after_mutation)?;
                    if after == before {
                        return Err(error.after_mutation());
                    }
                    decision = after;
                    continue;
                }
            }
            decision = self
                .create_decision(context, plan, Some(realization_fingerprint))
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
        self.admit_plan(plan).await?;
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
        let decision = self
            .create_decision(context, plan, Some(realization_fingerprint))
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
            ContainerCreateAttempt::new(observed.attempt_id.clone().ok_or_else(|| {
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
    ) -> Result<Option<crate::RuntimeRestoreTarget>, RuntimeError> {
        self.admit_plan(plan).await?;
        restore::recover(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn restore_or_adopt(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<crate::RuntimeRestoreTarget, RuntimeError> {
        self.admit_plan(plan).await?;
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
    ) -> Result<Option<crate::ContainerContinuationHandle>, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        continuation::handle_extra(&pod)
    }

    async fn observe(
        &self,
        expectation: ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        let expected_pod_uid = match expectation.runtime_handle {
            Some(crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid, ..
            }) => Some(pod_uid.as_str()),
            Some(crate::ContainerContinuationHandle::HostBindRestoration(_)) => {
                return Err(backend(
                    "Kubernetes observation received a host-bind continuation handle",
                ));
            }
            _ => None,
        };
        let current_handle = expectation.realization_fingerprint.is_some();
        let effect_fenced_total_absence = expectation.effect_fence.is_some()
            && matches!(
                expectation.runtime_handle,
                Some(
                    crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                        claim_uid: Some(_),
                        ..
                    }
                )
            );
        let pod = match self.pods().get(expectation.container_id).await {
            Ok(pod) => Some(pod),
            Err(error) if api_not_found(&error) => None,
            Err(error) => return Err(backend(error)),
        };
        let continuation = if current_handle {
            self.observed_continuation_disposition(
                expectation.container_id,
                pod.as_ref(),
                expectation.runtime_handle,
                expectation.purpose,
                expectation.effect_fence.is_some(),
                effect_fenced_total_absence,
            )
            .await?
        } else {
            ContinuationObservationDisposition::Live
        };
        if continuation == ContinuationObservationDisposition::Incompatible {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: "Kubernetes Sandbox continuation state differs from its durable handle"
                    .into(),
            });
        }
        let mut observations = pod
            .as_ref()
            .map(observed_pod)
            .transpose()?
            .into_iter()
            .collect::<Vec<_>>();
        if continuation == ContinuationObservationDisposition::Disposing && !observations.is_empty()
        {
            observations[0].phase = ExistingRealizationPhase::Terminal;
        }
        let observation = sandbox_observation(
            expected_pod_uid,
            expectation.adoption_fingerprint,
            expectation.realization_fingerprint,
            expectation.effect_fence,
            &observations,
            expectation.retained_attempt,
        )?;
        let observation = if continuation == ContinuationObservationDisposition::Disposing {
            match observation {
                pc::SandboxObservation::Terminal {
                    physical_incarnation,
                } => pc::SandboxObservation::Disposing {
                    physical_incarnation,
                },
                pc::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: Some(physical_incarnation),
                } => pc::SandboxObservation::Disposing {
                    physical_incarnation,
                },
                pc::SandboxObservation::Incompatible { reason } => {
                    pc::SandboxObservation::Incompatible { reason }
                }
                other => {
                    return Err(backend(format!(
                        "Kubernetes disposing continuation projected an invalid observation: {other:?}"
                    )));
                }
            }
        } else {
            observation
        };
        if !current_handle
            && matches!(
                observation,
                pc::SandboxObservation::Provisioning | pc::SandboxObservation::Ready
            )
        {
            let pod = pod.as_ref().expect("live observation has one Pod");
            if self
                .observed_continuation_disposition(
                    expectation.container_id,
                    Some(pod),
                    expectation.runtime_handle,
                    expectation.purpose,
                    false,
                    false,
                )
                .await?
                == ContinuationObservationDisposition::Incompatible
            {
                return Ok(pc::SandboxObservation::Incompatible {
                    reason: "Kubernetes Sandbox continuation state differs from its durable handle"
                        .into(),
                });
            }
        }
        Ok(observation)
    }

    async fn spawn(
        &self,
        admitted: crate::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        let container_id = admitted.container_id();
        if command.stdio == pc::Stdio::Piped {
            return Err(backend(
                "piped container exec requires the agent-channel capability",
            ));
        }
        let id = format!(
            "k8s-exec-{}-{}",
            std::process::id(),
            EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let execution = k8s_exec_argv(&id, command)?;
        let pods = self.streaming_pods();
        let mut attached = pods
            .exec(
                container_id,
                execution.argv,
                &AttachParams::default()
                    .container("agent")
                    .stdin(!execution.secret_stdin.is_empty())
                    // kube requires at least one attached stdio stream. Keep stdout
                    // attached and drain it in the completion task so a noisy command
                    // cannot block before the remote status frame is delivered.
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(backend)?;
        if !execution.secret_stdin.is_empty() {
            let mut stdin = attached
                .stdin()
                .ok_or_else(|| backend("k8s secret prelude has no stdin"))?;
            for secret in execution.secret_stdin {
                stdin
                    .write_all(secret.expose().as_bytes())
                    .await
                    .map_err(backend)?;
            }
            stdin.shutdown().await.map_err(backend)?;
        }
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s exec has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| backend("k8s exec has no completion status"))?;
        let completion = tokio::spawn(async move {
            let mut ignored = Vec::new();
            let (_, status) = tokio::join!(stdout.read_to_end(&mut ignored), status);
            status
        });
        Ok(Box::new(K8sExecProcess {
            id,
            pod: container_id.to_string(),
            pid_file: execution.pid_file,
            pods,
            state: tokio::sync::Mutex::new(K8sExecState {
                completion: Some(completion),
                status: None,
            }),
        }))
    }

    async fn spawn_agent(
        &self,
        admitted: crate::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        let container_id = admitted.container_id();
        let id = format!(
            "k8s-agent-exec-{}-{}",
            std::process::id(),
            EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let execution = k8s_exec_argv(&id, command)?;
        let pods = self.streaming_pods();
        let mut attached = pods
            .exec(
                container_id,
                execution.argv,
                &AttachParams::default()
                    .container("agent")
                    .stdin(true)
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(backend)?;
        let stdin = attached
            .stdin()
            .ok_or_else(|| backend("k8s agent exec has no stdin"))?;
        let mut stdin = stdin;
        for secret in execution.secret_stdin {
            stdin
                .write_all(secret.expose().as_bytes())
                .await
                .map_err(backend)?;
        }
        stdin.flush().await.map_err(backend)?;
        let stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s agent exec has no stdout"))?;
        let completion = attached
            .take_status()
            .ok_or_else(|| backend("k8s agent exec has no completion status"))?;
        Ok(RuntimeAgentProcess {
            process: Box::new(K8sExecProcess {
                id,
                pod: container_id.to_string(),
                pid_file: execution.pid_file,
                pods,
                state: tokio::sync::Mutex::new(K8sExecState {
                    completion: Some(tokio::spawn(completion)),
                    status: None,
                }),
            }),
            channel: Box::new(SplitChannel::new(stdout, stdin)),
        })
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        if let Some(port) = self.pod_channel_port {
            return channel::open_pod_channel(&self.streaming_pods(), container_id, port).await;
        }
        match self.rendezvous {
            // Reverse-dial: the host listens, the egress-fenced Pod dials out to us.
            Some(addr) => channel::accept_reverse(addr).await,
            // Direct-dial the agent's stdio over its published Service.
            None => TokioChannelFactory
                .connect(&ConnectionPlan::tcp_dial(self.agent_addr.to_string()))
                .await
                .map_err(backend),
        }
    }

    async fn read_live_file(
        &self,
        container_id: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        let mut attached = self
            .streaming_pods()
            .exec(
                container_id,
                vec!["cat", "--", path],
                &AttachParams::default().container("agent").stderr(false),
            )
            .await
            .map_err(backend)?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s credential harvest has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| backend("k8s credential harvest has no completion status"))?;
        let mut bytes = Vec::new();
        let (read, status) = tokio::join!(stdout.read_to_end(&mut bytes), status);
        read.map_err(backend)?;
        k8s_live_file_result(status, bytes)
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
        self.remove_bound(container_id, None, None, None).await
    }

    async fn remove_unreceipted_scope(&self, scope: &str) -> Result<(), RuntimeError> {
        // Kubernetes names the Pod from the scope, so the cancelled creator's
        // object is addressable without the durable handle it never produced.
        // Absent is success: the create may have been dropped before the API
        // server ever saw it.
        let pod_name = names::pod_name(&names::k8s_runtime_id(scope)?);
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
        self.remove_bound(&pod_name, Some(&pod_uid), claim_uid.as_deref(), None)
            .await
    }

    async fn remove_with_handle(
        &self,
        container_id: &str,
        runtime_handle: Option<&crate::ContainerContinuationHandle>,
    ) -> Result<(), RuntimeError> {
        let (pod_uid, claim_uid) = deletion_incarnation(runtime_handle)?;
        self.remove_bound(container_id, pod_uid, claim_uid, None)
            .await
    }

    async fn remove_exact_incarnation(
        &self,
        container_id: &str,
        runtime_handle: Option<&crate::ContainerContinuationHandle>,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), RuntimeError> {
        let (pod_uid, claim_uid) = deletion_incarnation(runtime_handle)?;
        self.remove_bound(
            container_id,
            pod_uid,
            claim_uid,
            Some((authorization, authority)),
        )
        .await
    }
}

#[cfg(test)]
mod unmaterialized_absence_tests {
    use super::*;

    #[test]
    fn proof_shape_decision_table_is_total_and_non_widening() {
        // UA1: enumerate the complete four-cause state space. Only the row with
        // no independently governed participant may proceed to live Pod/PVC
        // observation; every richer topology remains handle-fenced.
        for bits in 0_u8..16 {
            let causes = [bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0];
            assert_eq!(
                unmaterialized_absence_shape_is_closed(causes[0], causes[1], causes[2], causes[3]),
                bits == 0,
                "UA1 row {bits:04b}"
            );
        }
    }

    #[tokio::test]
    async fn exact_attempt_close_joins_in_flight_calls_and_rejects_late_reentry() {
        // UA2: no observation creates closure evidence. Once the exact effect
        // has entered the runtime, close waits for its read-side critical
        // section, installs a process-lifetime tombstone, and every late retry
        // is rejected before another backend call.
        let runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let fence = ContainerEffectFence::new(
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
        let attempt = crate::fresh_create_attempt();
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
        let fence = ContainerEffectFence::new(
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
        let attempt = crate::fresh_create_attempt();
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
