//! `ContainerRuntime` adapter for the Kubernetes backend.
//!
//! Kubernetes object construction, continuation, and lifecycle primitives remain
//! owned by the adjacent focused modules; this file owns only the neutral runtime
//! port projection and its attached-agent transport sequence.

use super::*;

pub(super) fn deletion_incarnation(
    runtime_handle: Option<&pc::ContainerContinuationHandle>,
) -> Result<(Option<&str>, Option<&str>), RuntimeError> {
    match runtime_handle {
        Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, claim_uid }) => {
            Ok((Some(pod_uid.as_str()), claim_uid.as_deref()))
        }
        Some(pc::ContainerContinuationHandle::KubernetesContinuation { .. }) => Err(backend(
            "legacy Kubernetes handle has no Pod UID deletion fence",
        )),
        Some(pc::ContainerContinuationHandle::HostBindRestoration(_)) => Err(backend(
            "Kubernetes removal received a host-bind continuation handle",
        )),
        None => Ok((None, None)),
    }
}

#[async_trait]
impl ContainerRuntime for K8sRuntime {
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
    ) -> Result<Option<pc::SandboxControlIncarnation>, RuntimeError> {
        sandbox_control::bind(self, container_id, request).await
    }

    async fn open_sandbox_control_channel(
        &self,
        container_id: &str,
        binding: &pc::SandboxControlIncarnation,
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
        self.admit_plan(plan).await?;
        self.create_decision(context, plan, realization_fingerprint)
            .await
            .map(drop)
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let fingerprint = legacy_unfenced_fingerprint(id);
        let attempt = ContainerCreateAttempt::fresh();
        let intent = ContainerRealizationIntent::Create;
        let context = ContainerRealizationContext::new(id, &fingerprint, None, &intent, &attempt);
        self.create_for_effect(&context, plan, &fingerprint).await
    }

    async fn create_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<String, RuntimeError> {
        self.admit_plan(plan).await?;
        if !plan.memory_mounts.is_empty() && context.effect_fence.is_none() {
            return Err(backend(
                "Kubernetes Memory projection requires a durable Environment effect fence",
            ));
        }
        let runtime_id = self.realization_runtime_id(context.scope)?;
        let mut decision = self
            .create_decision(context, plan, Some(realization_fingerprint))
            .await?;
        for _ in 0..4 {
            let before = decision.clone();
            let outcome = match decision {
                ExistingRealizationDecision::Create => {
                    creation::create(self, context, plan, realization_fingerprint).await
                }
                ExistingRealizationDecision::ConvergeCreating(observed)
                | ExistingRealizationDecision::ReuseReady(observed) => {
                    let recovery_attempt = ContainerCreateAttempt(
                        observed.attempt_id.clone().ok_or_else(|| {
                            backend(
                                "Kubernetes Sandbox realization has no projected-content attempt fence",
                            )
                        })?,
                    );
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
                ExistingRealizationDecision::ReplaceExact(observed) => self
                    .replace_exact_realization(&observed)
                    .await
                    .map(|()| String::new()),
                ExistingRealizationDecision::ValidateExisting(_) => {
                    return Err(backend(
                        "Kubernetes create reached a fingerprint-deferred decision",
                    ));
                }
            };
            match outcome {
                Ok(name) if !name.is_empty() => return Ok(name),
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
        Err(backend("Kubernetes exact realization did not converge").after_mutation())
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
    ) -> Result<(), RuntimeError> {
        restore::dispose(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn handle_extra(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::ContainerContinuationHandle>, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        continuation::handle_extra(&pod)
    }

    async fn observe(
        &self,
        expectation: ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        let expected_pod_uid = match expectation.runtime_handle {
            Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, .. }) => {
                Some(pod_uid.as_str())
            }
            Some(pc::ContainerContinuationHandle::HostBindRestoration(_)) => {
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
                Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
                    claim_uid: Some(_),
                    ..
                })
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
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
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
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
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
            None => TcpAgentTransport::new(self.agent_addr)
                .open_channel()
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

    async fn remove_with_handle(
        &self,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
    ) -> Result<(), RuntimeError> {
        let (pod_uid, claim_uid) = deletion_incarnation(runtime_handle)?;
        self.remove_bound(container_id, pod_uid, claim_uid, None)
            .await
    }

    async fn remove_exact_incarnation(
        &self,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), RuntimeError> {
        let (pod_uid, claim_uid) = deletion_incarnation(runtime_handle)?;
        self.remove_bound(container_id, pod_uid, claim_uid, Some(authorization))
            .await
    }
}
