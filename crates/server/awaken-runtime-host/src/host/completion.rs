//! Durable-foreground completion: the [`SharedHost`] pool-submit/await methods
//! and the [`CompletionRegistry`] event-wakeup machinery.

use super::*;
use awaken_run_ingress::{
    HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, PlacementRequirements,
};
use awaken_runtime_contract::CredentialMaterialSource;
use std::collections::{BTreeSet, HashMap};

pub(crate) fn model_realization_capability(
    candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
) -> Option<&'static str> {
    match &candidate.provisioning {
        awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
            Some(HOST_EXECUTOR_CAPABILITY)
        }
        awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. } => {
            Some(awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        }
        awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        }
        | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: Some(credential),
            ..
        } if credential.material_source == CredentialMaterialSource::WorkerReference => {
            Some(awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        }
        awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. }
        | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: Some(_),
            ..
        } => Some(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
        awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: None, ..
        } => None,
    }
}

fn worker_local_credentials(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_run_ingress::WorkerCredentialRevision> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match &candidate.provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
                credential,
                ..
            } => Some(credential.clone()),
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                credential: Some(credential),
                ..
            }
            | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
                credential: Some(credential),
                ..
            } if credential.material_source == CredentialMaterialSource::WorkerReference => {
                Some(credential.credential.clone())
            }
            _ => None,
        })
        .collect()
}

fn worker_acp_capabilities(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_run_ingress::WorkerAcpCapabilityRequirement> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match &candidate.provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { acp, .. }
                if !acp.capability_fingerprint.trim().is_empty() =>
            {
                Some(awaken_run_ingress::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding.backend_ref.clone(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                acp: Some(acp),
                ..
            } if !acp.capability_fingerprint.trim().is_empty() => {
                Some(awaken_run_ingress::WorkerAcpCapabilityRequirement {
                    backend_ref: candidate.binding.backend_ref.clone(),
                    fingerprint: acp.capability_fingerprint.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// Whether any publication-pinned execution candidate needs the Session's local
/// Environment. This is deliberately a set-wide decision: an A2A primary with a
/// Native/ACP fallback is not A2A-only and must retain one realizable Environment.
pub(crate) fn requires_local_environment(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> bool {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .any(|candidate| {
            !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &candidate.binding.backend_ref
                ),
                awaken_runtime_contract::resolved::Backend::Remote { .. }
            )
        })
}

/// Resolve the canonical cold-start inference holder from immutable candidate
/// backends. Embedded applications that author their own `RunDispatch` use this
/// same decision instead of duplicating the self-hosted boundary mapping.
pub fn self_hosted_inference_holder(
    activation: &RunActivation,
) -> Result<Option<awaken_runtime_contract::PlaintextHolder>, HostError> {
    let mut boundary = None;
    for candidate in activation
        .snapshot
        .resolved_spec
        .execution_candidates(activation.model_ref_override.as_deref())
    {
        let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(_),
            ..
        } = &candidate.provisioning
        else {
            continue;
        };
        let candidate_boundary = match awaken_runtime_contract::resolved::Backend::from_ref(
            &candidate.binding.backend_ref,
        ) {
            awaken_runtime_contract::resolved::Backend::Acp { .. } => {
                awaken_runtime_contract::PlaintextBoundary::Workload
            }
            awaken_runtime_contract::resolved::Backend::Native
            | awaken_runtime_contract::resolved::Backend::Remote { .. } => {
                awaken_runtime_contract::PlaintextBoundary::Worker
            }
        };
        if boundary.is_some_and(|existing| existing != candidate_boundary) {
            return Err(HostError::bad_request(
                "one execution candidate set cannot require multiple credential plaintext holders",
            ));
        }
        boundary = Some(candidate_boundary);
    }
    Ok(boundary.map(|boundary| match boundary {
        awaken_runtime_contract::PlaintextBoundary::Workload => {
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp()
                .inference_holder
        }
        awaken_runtime_contract::PlaintextBoundary::Worker
        | awaken_runtime_contract::PlaintextBoundary::Platform => {
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
                .inference_holder
        }
    }))
}

/// Compile the complete immutable Worker claim requirements for one resolved
/// model set. Application adapters that author their own [`RunDispatch`] must
/// use this function instead of projecting backend or credential capabilities
/// independently.
#[must_use]
pub fn remote_worker_placement(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
    environment: Option<&awaken_session_contract::EnvironmentSnapshot>,
    resources: Option<&awaken_session_contract::SessionResourceManifest>,
    remote_required: bool,
) -> PlacementRequirements {
    let required_credentials = worker_local_credentials(models);
    let mut placement = if remote_required || !required_credentials.is_empty() {
        PlacementRequirements::remote_required()
    } else {
        PlacementRequirements::default()
    };
    placement.required_credentials = required_credentials;
    placement.required_acp_capabilities = worker_acp_capabilities(models);
    let requires_local_environment = requires_local_environment(models);
    let requires_opaque_process = std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .any(|candidate| {
            matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    &candidate.binding.backend_ref
                ),
                awaken_runtime_contract::resolved::Backend::Acp { .. }
            ) && !matches!(
                candidate.provisioning,
                awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. }
            )
        });
    if requires_local_environment {
        placement.sandbox = environment.map_or_else(
            || awaken_provisioning_contract::SandboxRequirements {
                ..Default::default()
            },
            |environment| {
                crate::provisioning::sandbox_requirements(environment, requires_opaque_process)
            },
        );
    }
    for candidate in std::iter::once(&models.model_binding).chain(models.model_candidates.iter()) {
        placement.required_capabilities.insert(
            awaken_runtime_contract::execution::execution_capability(
                &candidate.binding.backend_ref,
            ),
        );
        if let Some(capability) = model_realization_capability(candidate) {
            placement
                .required_capabilities
                .insert(capability.to_string());
        }
    }
    if let Some(resources) = resources {
        placement.sandbox.enforced_readonly |= resources
            .resources
            .inputs
            .iter()
            .any(|input| input.access == awaken_resource_contract::ResourceAccess::ReadOnly);
        placement
            .required_capabilities
            .insert(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY.to_string());
        let credentialed_repository = resources.resources.inputs.iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::Repository { config, .. }
                    if config.credential_binding.is_some()
            )
        });
        if credentialed_repository {
            placement
                .required_capabilities
                .insert(awaken_run_ingress::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
    placement
}

impl SharedHost {
    /// Resolve the one exact inference plaintext holder used by both direct and
    /// dispatched attempts. A prepared Environment projection is authoritative;
    /// a cold/restarted host derives the same self-hosted boundary from the
    /// immutable candidate backend instead of silently omitting admission input.
    pub(crate) fn inference_plaintext_holder(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<awaken_runtime_contract::PlaintextHolder>, HostError> {
        match self.thread_credential_realization(&activation.thread_id.0) {
            Some(profile) => Ok(Some(profile.inference_holder)),
            None => self_hosted_inference_holder(activation),
        }
    }

    pub(crate) fn resolved_dispatch(
        &self,
        activation: RunActivation,
    ) -> Result<RunDispatch, HostError> {
        let thread = activation.thread_id.0.clone();
        let resources = self.thread_resource_manifest(&thread);
        let runtime_projection = self
            .session_slots
            .read(&thread, |slot| {
                slot.environment_snapshot
                    .clone()
                    .map(|environment| (environment, slot.toolsets.clone()))
            })
            .flatten();
        let environment_snapshot = runtime_projection
            .as_ref()
            .map(|(environment, _)| environment);
        let inference_holder = self.inference_plaintext_holder(&activation)?;
        // A mixed deployment may have both the local pool and remote workers.
        // Any carried manifest still needs capability admission: an explicit empty
        // successor can be the operation that removes a prior projection.
        let worker_local = !worker_local_credentials(&activation.snapshot.resolved_spec).is_empty();
        let placement = (self.deployment.disable_local_pool || resources.is_some() || worker_local)
            .then(|| {
                remote_worker_placement(
                    &activation.snapshot.resolved_spec,
                    environment_snapshot,
                    resources.as_ref(),
                    self.deployment.disable_local_pool,
                )
            });
        let mut request = RunDispatch::new(activation)
            .with_traceparent(awaken_observability::current_traceparent());
        if let Some(holder) = inference_holder {
            request = request.with_inference_plaintext_holder(holder);
        }
        if let Some(resources) = resources {
            let envelope = crate::provisioning::encode_session_resource_envelope(&resources)
                .map_err(|error| {
                    HostError::internal(format!("serialize Session resource manifest: {error}"))
                })?;
            request = request
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(resources.workspace_id.clone()),
                ))
                .with_session_resources(envelope);
        }
        if let Some((environment, toolsets)) = runtime_projection {
            let envelope =
                crate::provisioning::encode_session_runtime_envelope(environment, toolsets)
                    .map_err(|error| {
                        HostError::internal(format!(
                            "serialize Session runtime projection: {error}"
                        ))
                    })?;
            request = request.with_session_runtime(envelope);
        }
        if let Some(placement) = placement {
            request = request.with_placement(placement);
        }
        Ok(request)
    }

    /// The process dispatch pool, or a fail-closed error when durable ingress (and
    /// thus the pool) is not enabled.
    pub(crate) fn dispatch_pool_or_err(
        &self,
    ) -> Result<&Arc<DispatchPool<AnyDispatchStore>>, HostError> {
        self.dispatch_pool.get().ok_or_else(|| {
            HostError::bad_request(
                "durable dispatch not enabled (set typed durable ingress to run the pool)",
            )
        })
    }

    /// Submit a durable run and wait for the pool to drive it to a settled state.
    /// Under the shared queue a session's own worker must not claim (it would grab
    /// foreign threads' runs), so the foreground durable path enqueues, nudges the
    /// pool, and waits for the pool to signal completion — **by event**, not by
    /// polling committed truth, so it pays no poll-interval latency. `supersede`
    /// marks the thread's prior pending work superseded first (ADR-0022).
    pub(crate) async fn submit_durable_foreground(
        &self,
        ctx: &Arc<SessionCtx>,
        activation: RunActivation,
        supersede: bool,
    ) -> Result<awaken_agent_contract::agent::run::RunState, HostError> {
        let run_id = activation.run_id.clone();
        // Register for the settle event BEFORE enqueue, so the pool cannot drive and
        // settle the run before this caller is listening (no lost wakeup). The guard
        // removes the waiter if this future is dropped (client disconnect) before it
        // settles — held to the end of this method.
        let (settled, _waiter_guard) = self.completion.register(&run_id);
        let request = self.resolved_dispatch(activation)?;
        // Enqueue only — never drive here; the pool is the sole claimer. The common
        // path goes through `pool.submit` (which stamps the trace); a superseding
        // submit needs the supersede option, so it enqueues on the shared store and
        // nudges the pool directly.
        let ingress = ctx
            .durable_ingress
            .as_ref()
            .ok_or_else(|| HostError::internal("durable submit requires durable ingress"))?;
        if supersede {
            ingress
                .worker()
                .store()
                .enqueue_with(
                    request,
                    SubmitOptions {
                        supersede: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            if let Some(pool) = self.dispatch_pool.get() {
                pool.notify().await;
            }
        } else if let Some(pool) = self.dispatch_pool.get() {
            pool.submit_dispatch(request)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        } else {
            // A coordinator-only cell still authors the same durable dispatch;
            // registered remote Workers are its only claimers. Their authenticated
            // settle route wakes this Host's completion registry from committed
            // Run truth, so no local executor or polling compatibility path exists.
            ingress
                .worker()
                .store()
                .enqueue(request)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
        }
        self.await_settled_event(ctx, &run_id, None, settled).await
    }

    /// Deliver one foreground answer through the durable inbox and wait until the
    /// dispatch worker settles the exact resumed ticket. The queue remains the sole
    /// durable execution/settlement authority; this method only authors input and
    /// observes the resulting committed Run fact.
    pub(crate) async fn resume_durable_foreground(
        &self,
        ctx: &Arc<SessionCtx>,
        command: ResumeCommand,
    ) -> Result<RunState, HostError> {
        let run_id = command.run_id.clone();
        let correlation_id = command.correlation_id.clone();
        // Register before append so a local pool cannot settle between input
        // publication and waiter installation.
        let (settled, _waiter_guard) = self.completion.register(&run_id);
        let input = durable_resume_input(command);
        let ingress = ctx
            .durable_ingress
            .as_ref()
            .ok_or_else(|| HostError::internal("durable resume requires durable ingress"))?;
        if let Some(pool) = self.dispatch_pool.get() {
            pool.deliver(input)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        } else {
            // Coordinator-only cells publish to the same shared store. Remote
            // Workers claim it on their ordinary wake/poll path and committed-truth
            // reconciliation below observes their settlement.
            ingress
                .worker()
                .store()
                .append(input)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        }
        self.await_settled_event(ctx, &run_id, Some(&correlation_id), settled)
            .await
    }

    /// Wait for the pool's settle signal for `run_id` (sub-millisecond wakeup), with
    /// committed-truth reconciliation for peer Coordinators. A foreground transport
    /// lifetime is not a Run deadline: long-running work remains `Running` until the
    /// runtime commits `Awaiting` or `Ended`, and client cancellation drops this
    /// future and its waiter guard.
    async fn await_settled_event(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: &RunId,
        answered_correlation: Option<&str>,
        settled: tokio::sync::oneshot::Receiver<RunState>,
    ) -> Result<RunState, HostError> {
        await_completion_state(settled, std::time::Duration::from_millis(250), || {
            self.read_settled_phase(ctx, run_id, answered_correlation)
        })
        .await
    }

    /// One committed-truth read: the run's state if it has settled (`Ended` or
    /// `Awaiting`), else `None`. The fallback path for `await_settled_event`.
    async fn read_settled_phase(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: &RunId,
        answered_correlation: Option<&str>,
    ) -> Result<Option<RunState>, HostError> {
        let record = ctx
            .commit
            .authoritative_run(run_id)
            .await
            .map_err(HostError::internal)?;
        match record {
            Some(
                record @ awaken_agent_contract::agent::run::Record {
                    state: RunState::Ended(_),
                    ..
                },
            ) => Ok(Some(record.state)),
            Some(
                record @ awaken_agent_contract::agent::run::Record {
                    state: RunState::Awaiting,
                    ..
                },
            ) => {
                let Some(answered_correlation) = answered_correlation else {
                    return Ok(Some(record.state));
                };
                // Before a remote Worker consumes the answer, committed truth is
                // still Awaiting on the answered ticket. That is not a settlement
                // of this resume. Only a newly committed ticket (or Ended above)
                // releases the foreground caller.
                let current = ctx
                    .commit
                    .open_wait_for_thread(&ctx.thread_id)
                    .await
                    .map_err(HostError::internal)?;
                Ok(awaiting_ticket_advanced(
                    run_id,
                    answered_correlation,
                    current
                        .as_ref()
                        .map(|(current_run, ticket)| (current_run, ticket.correlation_id.as_str())),
                )
                .then_some(record.state))
            }
            _ => Ok(None),
        }
    }
}

fn awaiting_ticket_advanced(
    observed_run: &RunId,
    answered_correlation: &str,
    current_wait: Option<(&RunId, &str)>,
) -> bool {
    current_wait.is_some_and(|(current_run, current_correlation)| {
        current_run == observed_run && current_correlation != answered_correlation
    })
}

/// A resume ticket is a one-answer idempotency boundary. Encoding the exact
/// `(run, correlation)` pair with a run-length prefix is collision-free for
/// arbitrary string contents; replaying another payload for that same ticket is
/// rejected by the Inbox's existing idempotency-conflict rule.
fn durable_resume_input(command: ResumeCommand) -> PendingInput {
    let message_id = format!(
        "foreground-resume:{}:{}{}",
        command.run_id.0.len(),
        command.run_id.0,
        command.correlation_id
    );
    PendingInput {
        message_id,
        run_id: command.run_id,
        thread_id: command.thread_id,
        correlation_id: command.correlation_id,
        available_at_ms: None,
        result: command.result,
    }
}

/// Await the existing completion signal while reconciling the one committed Run
/// authority. `None` means the Run is still live and must never be projected as a
/// timeout/error by this transport helper. Dispatch lease recovery and dead-letter
/// policy own genuinely abandoned execution; cancellation owns caller departure.
async fn await_completion_state<Read, ReadFuture>(
    settled: tokio::sync::oneshot::Receiver<RunState>,
    reconciliation_interval: std::time::Duration,
    mut read_settled: Read,
) -> Result<RunState, HostError>
where
    Read: FnMut() -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<Option<RunState>, HostError>>,
{
    let mut settled = std::pin::pin!(settled);
    // The local event is the fast path. A peer Coordinator can commit the same
    // shared PostgreSQL Run without owning this process's oneshot sender, so
    // committed-truth reconciliation is also required. One foreground waiter
    // performs one narrow read per interval; it never claims, settles, times out,
    // or creates a second completion authority.
    loop {
        tokio::select! {
            result = &mut settled => {
                if let Ok(state) = result {
                    return Ok(state);
                }
                return read_settled().await?.ok_or_else(|| {
                    HostError::internal(
                        "durable completion signal closed before committed Run settlement",
                    )
                });
            }
            _ = tokio::time::sleep(reconciliation_interval) => {
                if let Some(state) = read_settled().await? {
                    return Ok(state);
                }
            }
        }
    }
}

/// Wakes a foreground durable submitter the instant the pool settles its run, so
/// the durable foreground path waits by **event** rather than polling committed
/// truth — removing the poll-interval latency floor. Keyed by run id; a run with no
/// registered waiter (a fire-and-forget background submit) settles as a no-op.
#[derive(Default)]
pub(crate) struct CompletionRegistry {
    waiters: std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<RunState>>>,
}

impl CompletionRegistry {
    /// Register interest in `run_id` BEFORE it is enqueued, so the pool cannot
    /// settle it before this caller is listening (no lost wakeup). Returns the
    /// receiver plus a [`WaiterGuard`] that removes the waiter if the caller's
    /// future is dropped before the run settles (e.g. a client disconnect), so an
    /// unwaited entry never lingers in the map.
    fn register(
        self: &Arc<Self>,
        run_id: &RunId,
    ) -> (tokio::sync::oneshot::Receiver<RunState>, WaiterGuard) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiters
            .lock()
            .expect("completion registry poisoned")
            .insert(run_id.0.clone(), tx);
        let guard = WaiterGuard {
            registry: Arc::downgrade(self),
            run_id: run_id.0.clone(),
        };
        (rx, guard)
    }
}

/// Removes a completion waiter on drop, so a foreground submit whose future is
/// dropped (client disconnect) or which timed out never leaves a stale sender in
/// the registry. On normal completion the sender is already gone (consumed by
/// [`CompletionSink::settled`]), so the removal is a harmless no-op.
struct WaiterGuard {
    registry: std::sync::Weak<CompletionRegistry>,
    run_id: String,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut waiters) = registry.waiters.lock()
        {
            waiters.remove(&self.run_id);
        }
    }
}

impl CompletionSink for CompletionRegistry {
    fn settled(&self, run_id: &RunId, state: &RunState) {
        if let Some(tx) = self
            .waiters
            .lock()
            .expect("completion registry poisoned")
            .remove(&run_id.0)
        {
            // The receiver may have already gone (timed out) — a dropped send is fine.
            let _ = tx.send(state.clone());
        }
    }
}

#[cfg(test)]
mod completion_tests {
    use super::{
        CompletionRegistry, HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, RunId,
        await_completion_state, awaiting_ticket_advanced, durable_resume_input,
        remote_worker_placement,
    };
    use awaken_agent_contract::agent::run::RunState;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_run_ingress::CompletionSink;
    use awaken_runtime_contract::resolved::{
        CatalogFingerprint, ModelBinding, ResolvedModelCandidate,
    };
    use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
    use std::sync::Arc;

    use crate::{NoModelConfiguredExecutor, SharedHost, UNCONFIGURED_MODEL_REF};

    fn host_models() -> awaken_runtime_contract::resolved::ResolvedSpec {
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("test")
            .model(ModelBinding::new("host", "primary", "native"))
            .build()
            .resolved_spec
    }

    fn environment() -> awaken_session_contract::EnvironmentSnapshot {
        awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env-1".into(),
            revision: awaken_session_contract::EnvironmentRevision(3),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("fp-env".into()),
            sandbox: serde_json::json!({
                "isolation": "container",
                "limits": {"memory_bytes": 67108864}
            }),
            sandbox_provisioning: Default::default(),
            packages: awaken_session_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                ..Default::default()
            },
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example.test".into()],
            },
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    fn resume_command(run_id: &str, correlation_id: &str, answer: &str) -> ResumeCommand {
        ResumeCommand {
            correlation_id: correlation_id.into(),
            run_id: RunId(run_id.into()),
            thread_id: ThreadId("thread-1".into()),
            snapshot_id: ExecutableAgentSnapshotId("snapshot-1".into()),
            catalog_fingerprint: CatalogFingerprint("catalog-1".into()),
            result: ResumeResult::Input(answer.into()),
            now_ms: 10,
        }
    }

    #[test]
    fn durable_resume_identity_is_exactly_the_answered_ticket() {
        // Cause/effect graph: C1 exact retry of one (Run, correlation, payload);
        // C2 same ticket with a conflicting payload; C3 delimiter-ambiguous raw
        // strings belonging to different tickets. Effects: E1 exact retry yields
        // one identical PendingInput; E2 conflict keeps the same message id but a
        // different payload so the canonical Inbox rejects it; E3 distinct ticket
        // pairs never alias. Constraint: caller time is not delivery identity.
        //
        // | Rule | ticket pair | payload | Effect |
        // | R1 | same | same | E1 identical input |
        // | R2 | same | different | E2 same id, conflicting input |
        // | R3 | different but delimiter-ambiguous | any | E3 different id |
        let exact = durable_resume_input(resume_command("run-a", "corr-a", "yes"));
        let retry = durable_resume_input(resume_command("run-a", "corr-a", "yes"));
        assert_eq!(exact, retry, "R1");

        let conflict = durable_resume_input(resume_command("run-a", "corr-a", "no"));
        assert_eq!(exact.message_id, conflict.message_id, "R2 identity");
        assert_ne!(exact, conflict, "R2 payload conflict");

        let left = durable_resume_input(resume_command("a", "bc", "yes"));
        let right = durable_resume_input(resume_command("ab", "c", "yes"));
        assert_ne!(left.message_id, right.message_id, "R3");
    }

    #[test]
    fn peer_completion_waits_for_exact_ticket_advancement() {
        // Cause/effect graph: C1 initial durable submit vs resumed wait; C2 the
        // committed Run is still Awaiting; C3 open ticket is the answered ticket,
        // a new ticket, absent, or belongs to another Run. Effects: E1 old truth
        // cannot prematurely release a resume waiter; E2 a new ticket releases it;
        // E3 missing/inconsistent truth remains fail-closed. Ended and initial
        // Awaiting are handled by the enclosing read-settled state match.
        //
        // | Rule | observed Run | current ticket | Effect |
        // | R1 | run-1 | same correlation | E1 false |
        // | R2 | run-1 | new correlation | E2 true |
        // | R3 | run-1 | absent | E3 false |
        // | R4 | run-1 | ticket for run-2 | E3 false |
        let run_1 = RunId("run-1".into());
        let run_2 = RunId("run-2".into());
        assert!(
            !awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_1, "ticket-1"))),
            "R1"
        );
        assert!(
            awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_1, "ticket-2"))),
            "R2"
        );
        assert!(!awaiting_ticket_advanced(&run_1, "ticket-1", None), "R3");
        assert!(
            !awaiting_ticket_advanced(&run_1, "ticket-1", Some((&run_2, "ticket-2"))),
            "R4"
        );
    }

    #[test]
    fn environment_and_backend_compile_one_worker_sandbox_requirement_vector() {
        use awaken_provisioning_contract::IsolationClass;
        use awaken_runtime_contract::resolved::{
            BackendModelSelection, ModelProvisioning, ResolvedModelCandidate,
        };

        // Cause/effect graph:
        // C1=Native; C2=projected ACP opaque process; C3=trusted BackendOwned ACP;
        // C4=A2A-only; C5=frozen Environment isolation/network/limits/packages;
        // C6=prepared image. Effects: E1=one PlacementRequirements.sandbox vector;
        // E2=opaque ACP adds transparent Namespace semantics; E3=A2A adds no local
        // Sandbox demand; E4=image replaces package provisioning with rootfs demand.
        // Constraint: candidates are conjunctive for admission; any local candidate
        // keeps the local Environment requirement.
        //
        // Decision table:
        // R1 C1+C5 -> exact Environment enforcement, cooperative Hand paths.
        // R2 C2+C5 -> R1 plus transparent/path-fidelity.
        // R3 C3+C5 -> trusted Workdir semantics; no artificial Namespace demand.
        // R4 C4+C5 -> default (no local Environment) vector.
        // R5 C1+C5+C6 -> custom-rootfs=true, package-provisioning=false.
        // R6 C4 plus any Native fallback -> R1 (the candidate set is not A2A-only).
        let frozen = environment();

        let native = remote_worker_placement(&host_models(), Some(&frozen), None, true);
        assert_eq!(native.sandbox.isolation, IsolationClass::Container, "R1");
        assert!(native.sandbox.network_isolation, "R1 network");
        assert!(native.sandbox.enforced_network_allowlist, "R1 allowlist");
        assert!(native.sandbox.resource_limits, "R1 limits");
        assert!(native.sandbox.package_provisioning, "R1 packages");
        assert!(
            !native.sandbox.tool_transparent && !native.sandbox.path_fidelity,
            "R1 hand"
        );

        let mut projected_acp = host_models();
        projected_acp.model_binding = ResolvedModelCandidate::provider(
            ModelBinding::new("provider", "model", "acp:claude"),
            "provider@1",
            "route@1",
            "workspace",
            None,
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://example.test".into(),
                upstream_model: "model".into(),
            },
        );
        let acp = remote_worker_placement(&projected_acp, Some(&frozen), None, true);
        assert!(
            acp.sandbox.tool_transparent && acp.sandbox.path_fidelity,
            "R2"
        );

        let mut trusted = host_models();
        trusted.model_binding.provisioning = ModelProvisioning::BackendOwned {
            credential: awaken_runtime_contract::CredentialRef {
                id: "local".into(),
                revision: 1,
            },
            model_selection: BackendModelSelection::Default,
            acp: Default::default(),
        };
        trusted.model_binding.binding.backend_ref = "acp:codex".into();
        let mut workdir = frozen.clone();
        workdir.sandbox = serde_json::json!({});
        workdir.network = awaken_session_contract::SessionNetworkPolicy::Unrestricted;
        workdir.packages = Default::default();
        let trusted = remote_worker_placement(&trusted, Some(&workdir), None, true);
        assert_eq!(trusted.sandbox.isolation, IsolationClass::Workdir, "R3");
        assert!(!trusted.sandbox.tool_transparent, "R3");

        let mut remote = host_models();
        remote.model_binding = ResolvedModelCandidate::remote(
            ModelBinding::new("agent", "", "a2a:https://agent.test"),
            "workspace",
            None,
            "security-fp",
        );
        let remote_placement = remote_worker_placement(&remote, Some(&frozen), None, true);
        assert_eq!(remote_placement.sandbox, Default::default(), "R4");

        remote.model_candidates.push(host_models().model_binding);
        let mixed = remote_worker_placement(&remote, Some(&frozen), None, true);
        assert_eq!(mixed.sandbox.isolation, IsolationClass::Container, "R6");

        let mut prepared = frozen;
        prepared.prepared_image = Some("image@sha256:abc".into());
        let image = remote_worker_placement(&host_models(), Some(&prepared), None, true);
        assert!(image.sandbox.custom_rootfs, "R5 rootfs");
        assert!(!image.sandbox.package_provisioning, "R5 packages");
    }

    #[test]
    fn exported_dispatch_completion_sink_is_the_host_owned_projection() {
        // Cause graph: one SharedHost owns one CompletionRegistry; an embedding
        // requests the Worker-settle projection; cloning must preserve that exact
        // registry rather than constructing a peer notification path.
        // Decision table:
        // | host mode | exported sink | effect |
        // | local pool | exact host registry | local settle and export converge |
        // | coordinator-only | exact host registry | remote settle wakes Session |
        // Mode does not alter sink identity, so pointer identity covers both rules
        // without manufacturing two deployment configurations in this unit test.
        let host = SharedHost::new(Arc::new(NoModelConfiguredExecutor), UNCONFIGURED_MODEL_REF);
        let expected = host.completion.clone() as Arc<dyn CompletionSink>;
        let exported = host.dispatch_completion_sink();

        assert!(Arc::ptr_eq(&expected, &exported));
    }

    /// A3: dropping the guard (caller future dropped / timed out) removes the
    /// waiter, so a run that never settles does not leak an entry.
    #[tokio::test]
    async fn dropping_the_guard_removes_the_registration() {
        let registry = Arc::new(CompletionRegistry::default());
        let (rx, guard) = registry.register(&RunId("r".into()));
        assert_eq!(registry.waiters.lock().unwrap().len(), 1);
        drop(guard);
        drop(rx);
        assert!(
            registry.waiters.lock().unwrap().is_empty(),
            "the guard removed the leaked waiter"
        );
    }

    /// The happy path: `settled` delivers the state to the waiter and clears the
    /// slot, so the later guard drop is a no-op.
    #[tokio::test]
    async fn settled_delivers_the_state_and_clears_the_slot() {
        let registry = Arc::new(CompletionRegistry::default());
        let (rx, _guard) = registry.register(&RunId("r".into()));
        registry.settled(&RunId("r".into()), &RunState::Awaiting);
        assert!(matches!(rx.await, Ok(RunState::Awaiting)));
        assert!(registry.waiters.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_live_run_outlives_foreground_reconciliation_without_a_transport_timeout() {
        // Durable foreground completion cause/effect table:
        // C1=the local completion sender remains open; C2=committed Run truth is
        // still Running (`read_settled -> None`); C3=multiple reconciliation
        // intervals pass; C4=the runtime later commits/sends Awaiting. Effects:
        // E1=the foreground waiter remains pending through C3; E2=no transport
        // timeout invents Error/Ended; E3=C4 alone releases the waiter with the
        // exact authoritative state. Rule F1=C1+C2+C3=>E1+E2;
        // F2=F1+C4=>E3. Caller cancellation is covered by the existing guard-drop
        // test and is intentionally independent of Run terminal authority.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_reads = reads.clone();
        let future =
            await_completion_state(receiver, std::time::Duration::from_millis(2), move || {
                observed_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(None))
            });
        tokio::pin!(future);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(15), &mut future)
                .await
                .is_err(),
            "F1/F2: live committed truth must keep the foreground wait open"
        );
        assert!(
            reads.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "F1: reconciliation observed live truth repeatedly"
        );

        sender
            .send(RunState::Awaiting)
            .expect("waiter remains live");
        let state = tokio::time::timeout(std::time::Duration::from_millis(100), &mut future)
            .await
            .expect("authoritative settlement wakes promptly")
            .expect("settlement succeeds");
        assert!(matches!(state, RunState::Awaiting), "F2");
    }

    #[test]
    fn coordinator_admission_pins_protocol_and_every_materialization_capability() {
        let mut models = host_models();
        models
            .model_candidates
            .push(ResolvedModelCandidate::provider(
                ModelBinding::new("provider", "fallback", "native"),
                "provider@1",
                "route@1",
                "workspace",
                None,
                awaken_runtime_contract::InferenceEndpoint {
                    adapter_kind: "openai".into(),
                    api_dialect: "open_ai_chat".into(),
                    base_url: "https://example.invalid".into(),
                    upstream_model: "fallback".into(),
                },
            ));
        let placement = remote_worker_placement(&models, None, None, true);
        assert_eq!(placement.contract_version, 1);
        assert_eq!(placement.dispatch_contract_version, 1);
        assert_eq!(placement.runtime_protocol_version, 1);
        assert!(placement.required_capabilities.contains("native-runtime"));
        assert!(
            placement
                .required_capabilities
                .contains(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY)
        );
        assert!(
            placement
                .required_capabilities
                .contains(HOST_EXECUTOR_CAPABILITY)
        );
    }

    #[test]
    fn coordinator_admission_requires_exact_acp_and_generic_a2a_capabilities() {
        let mut models = host_models();
        models.model_binding.binding.backend_ref = "acp:claude".to_string();
        let mut remote = models.model_binding.clone();
        remote.binding.backend_ref = "a2a:https://agent.example".to_string();
        models.model_candidates.push(remote);

        let placement = remote_worker_placement(&models, None, None, true);
        assert!(
            placement.required_capabilities.contains("acp:claude"),
            "ACP CLI identity is part of claim compatibility"
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY),
            "the generic A2A transport is part of claim compatibility"
        );
        assert!(
            !placement.required_capabilities.contains("native-runtime"),
            "native is not advertised as a substitute for exact external backends"
        );
    }

    #[test]
    fn worker_local_candidates_pin_every_exact_credential_for_claim_admission() {
        use awaken_runtime_contract::{
            CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
            CredentialUsage, InferenceEndpoint,
        };

        let worker_candidate = |model: &str, credential: &str, revision: u64| {
            ResolvedModelCandidate::provider(
                ModelBinding::new("provider", model, "genai"),
                "provider@1",
                "route@1",
                "workspace-a",
                Some(CredentialAccess::new(
                    CredentialRef {
                        id: credential.into(),
                        revision,
                    },
                    CredentialMaterialSource::WorkerReference,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )),
                InferenceEndpoint {
                    adapter_kind: "openai".into(),
                    api_dialect: "open_ai_chat".into(),
                    base_url: "https://example.invalid".into(),
                    upstream_model: model.into(),
                },
            )
        };
        let mut models = host_models();
        models.model_binding = worker_candidate("primary", "cred:primary", 2);
        models
            .model_candidates
            .push(worker_candidate("fallback", "cred:fallback", 5));

        let placement = remote_worker_placement(&models, None, None, false);
        assert_eq!(
            placement.location,
            awaken_run_ingress::ExecutionLocation::RemoteRequired
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        );
        assert_eq!(
            placement.required_credentials,
            std::collections::BTreeSet::from([
                awaken_run_ingress::WorkerCredentialRevision {
                    id: "cred:fallback".into(),
                    revision: 5,
                },
                awaken_run_ingress::WorkerCredentialRevision {
                    id: "cred:primary".into(),
                    revision: 2,
                },
            ])
        );
    }

    #[test]
    fn backend_owned_candidate_uses_the_same_exact_worker_placement_fence() {
        // Cause graph: BackendOwned candidate -> WorkerLocal capability + exact
        // credential revision -> RemoteRequired placement -> existing heartbeat
        // selection and pre-launch revalidation. There is no materialization cause.
        //
        // Decision table: BackendOwned always requires its one exact observation;
        // HostExecutor requires neither this capability nor credential revision.
        let mut models = host_models();
        models.model_binding = ResolvedModelCandidate::backend_owned(
            ModelBinding::new("cred:local", "", "acp:codex"),
            awaken_runtime_contract::CredentialRef {
                id: "cred:local".into(),
                revision: 7,
            },
            awaken_runtime_contract::resolved::BackendModelSelection::Default,
            "test",
            "sha256:test-capability",
            Default::default(),
        );

        let placement = remote_worker_placement(&models, None, None, false);
        assert_eq!(
            placement.location,
            awaken_run_ingress::ExecutionLocation::RemoteRequired
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        );
        assert_eq!(
            placement.required_credentials,
            std::collections::BTreeSet::from([awaken_run_ingress::WorkerCredentialRevision {
                id: "cred:local".into(),
                revision: 7,
            },])
        );
        assert_eq!(
            placement.required_acp_capabilities,
            std::collections::BTreeSet::from([
                awaken_run_ingress::WorkerAcpCapabilityRequirement {
                    backend_ref: "acp:codex".into(),
                    fingerprint: "sha256:test-capability".into(),
                },
            ])
        );
    }

    #[test]
    fn resource_placement_requires_resource_and_repository_credential_capabilities() {
        use awaken_resource_contract::{
            BindingId, ClonePolicy, ConfigVersion, RepositoryConfigVersion, RepositoryId,
            ResourceAccess,
        };
        use awaken_session_contract::{
            ResolvedInput, ResolvedInputSource, ResolvedSessionResources, SessionResourceManifest,
        };

        // Cause/effect graph: C1=resource manifest exists; C2=repository has a
        // credential; C3=input is read-only. Effects: E1=resource realization
        // capability; E2=repository credential capability; E3=enforced read-only
        // Sandbox capability. One row with C1+C2+C3 covers all conjunctive effects;
        // the following empty-manifest test owns the !C2/!C3 revocation row.
        let resources = SessionResourceManifest::new(
            "workspace-a",
            ResolvedSessionResources {
                inputs: vec![ResolvedInput {
                    binding_id: BindingId::new("repo-binding"),
                    source: ResolvedInputSource::Repository {
                        repository_id: RepositoryId::from("repo-a"),
                        config: RepositoryConfigVersion {
                            repository_id: "repo-a".into(),
                            version: ConfigVersion::INITIAL,
                            remote_url: "https://example.invalid/repo.git".into(),
                            credential_binding: Some("credential-a".into()),
                            initial_branch: None,
                            initial_commit: None,
                            clone_policy: ClonePolicy::default(),
                        },
                        credential: None,
                    },
                    mount_path: "/workspace/repo".into(),
                    access: ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );
        let placement = remote_worker_placement(&host_models(), None, Some(&resources), true);
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY)
        );
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
        assert!(placement.sandbox.enforced_readonly);
    }

    #[test]
    fn explicit_empty_manifest_still_requires_the_revocation_capability() {
        let resources = awaken_session_contract::SessionResourceManifest::new(
            "workspace-a",
            awaken_session_contract::ResolvedSessionResources {
                inputs: Vec::new(),
                skills: Some(Vec::new()),
            },
        );
        let placement = remote_worker_placement(&host_models(), None, Some(&resources), true);
        assert!(
            placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY),
            "an empty successor manifest may need to remove prior sandbox material"
        );
        assert!(
            !placement
                .required_capabilities
                .contains(awaken_run_ingress::REPOSITORY_CREDENTIALS_CAPABILITY)
        );
    }
}
