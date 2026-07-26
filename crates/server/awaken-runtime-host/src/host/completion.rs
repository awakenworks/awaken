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
) -> &'static str {
    match &candidate.provisioning {
        awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
            HOST_EXECUTOR_CAPABILITY
        }
        awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        } if credential.material_source == CredentialMaterialSource::WorkerReference => {
            awaken_run_ingress::WORKER_LOCAL_CREDENTIALS_CAPABILITY
        }
        awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. } => {
            PROVIDER_CREDENTIAL_SOURCE_CAPABILITY
        }
    }
}

fn worker_local_credentials(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> BTreeSet<awaken_run_ingress::WorkerCredentialRevision> {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .filter_map(|candidate| match &candidate.provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                credential: Some(credential),
                ..
            } if credential.material_source == CredentialMaterialSource::WorkerReference => {
                Some(awaken_run_ingress::WorkerCredentialRevision {
                    source_id: credential.credential.id.clone(),
                    revision: credential.credential.revision,
                })
            }
            _ => None,
        })
        .collect()
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

pub(crate) fn remote_worker_placement(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
    resources: Option<&awaken_protocol_managed::SessionResourceManifest>,
    remote_required: bool,
) -> PlacementRequirements {
    let required_credentials = worker_local_credentials(models);
    let mut placement = if remote_required || !required_credentials.is_empty() {
        PlacementRequirements::remote_required()
    } else {
        PlacementRequirements::default()
    };
    placement.required_credentials = required_credentials;
    for candidate in std::iter::once(&models.model_binding).chain(models.model_candidates.iter()) {
        placement.required_capabilities.insert(
            awaken_runtime_contract::execution::execution_capability(
                &candidate.binding.backend_ref,
            ),
        );
        placement
            .required_capabilities
            .insert(model_realization_capability(candidate).to_string());
    }
    if let Some(resources) = resources {
        placement
            .required_capabilities
            .insert(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY.to_string());
        let credentialed_repository = resources.resources.inputs.iter().any(|input| {
            matches!(
                &input.source,
                awaken_protocol_managed::ResolvedInputSource::Repository { config, .. }
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
        let inference_holder = self.inference_plaintext_holder(&activation)?;
        // A mixed deployment may have both the local pool and remote workers.
        // Any carried manifest still needs capability admission: an explicit empty
        // successor can be the operation that removes a prior projection.
        let worker_local = !worker_local_credentials(&activation.snapshot.resolved_spec).is_empty();
        let placement = (self.deployment.disable_local_pool || resources.is_some() || worker_local)
            .then(|| {
                remote_worker_placement(
                    &activation.snapshot.resolved_spec,
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
                "durable dispatch not enabled (set AWAKEN_INGRESS=durable to run the pool)",
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
        self.await_settled_event(ctx, &run_id, settled).await
    }

    /// Wait for the pool's settle signal for `run_id` (sub-millisecond wakeup), with
    /// a bounded timeout after which a single committed-truth read is the safety net
    /// (in case the pool died mid-drive). The event path replaces the old poll loop,
    /// removing the poll-interval floor from every durable foreground turn.
    async fn await_settled_event(
        &self,
        ctx: &Arc<SessionCtx>,
        run_id: &RunId,
        settled: tokio::sync::oneshot::Receiver<RunState>,
    ) -> Result<RunState, HostError> {
        // ~60s ceiling — generous for a multi-step run's inference, bounded so a
        // stuck run surfaces as an error rather than hanging the request forever.
        match tokio::time::timeout(std::time::Duration::from_secs(60), settled).await {
            // The pool signalled the settled state the instant it settled.
            Ok(Ok(state)) => Ok(state),
            // Sender dropped without sending (pool died) or the wait timed out: fall
            // back to one committed-truth read, else surface a hard error. The
            // waiter entry is cleaned up by the caller's `WaiterGuard` on return.
            Ok(Err(_)) | Err(_) => self.read_settled_phase(ctx, run_id).ok_or_else(|| {
                HostError::internal(
                    "durable run did not settle: the dispatch pool never drove it to completion",
                )
            }),
        }
    }

    /// One committed-truth read: the run's state if it has settled (`Ended` or
    /// `Awaiting`), else `None`. The fallback path for `await_settled_event`.
    fn read_settled_phase(&self, ctx: &Arc<SessionCtx>, run_id: &RunId) -> Option<RunState> {
        use awaken_agent_contract::thread::read::run_store::RunStore;
        match RunStore::get(&*ctx.commit, run_id) {
            Some(record) if matches!(record.state, RunState::Ended(_) | RunState::Awaiting) => {
                Some(record.state)
            }
            _ => None,
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
        remote_worker_placement,
    };
    use awaken_agent_contract::agent::run::RunState;
    use awaken_run_ingress::CompletionSink;
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
    use std::sync::Arc;

    fn host_models() -> awaken_runtime_contract::resolved::ResolvedSpec {
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("test")
            .model(ModelBinding::new("host", "primary", "native"))
            .build()
            .resolved_spec
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
                    base_url: "https://example.invalid".into(),
                    upstream_model: "fallback".into(),
                },
            ));
        let placement = remote_worker_placement(&models, None, true);
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

        let placement = remote_worker_placement(&models, None, true);
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

        let placement = remote_worker_placement(&models, None, false);
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
                    source_id: "cred:fallback".into(),
                    revision: 5,
                },
                awaken_run_ingress::WorkerCredentialRevision {
                    source_id: "cred:primary".into(),
                    revision: 2,
                },
            ])
        );
    }

    #[test]
    fn resource_placement_requires_resource_and_repository_credential_capabilities() {
        use awaken_protocol_managed::resource_plane::{
            BindingId, ClonePolicy, ConfigVersion, RepositoryConfigVersion, RepositoryId,
            ResourceAccess,
        };
        use awaken_protocol_managed::{
            ResolvedInput, ResolvedInputSource, ResolvedSessionResources, SessionResourceManifest,
        };

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
                            clone_policy: ClonePolicy::default(),
                        },
                        credential: None,
                    },
                    mount_path: "/workspace/repo".into(),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );
        let placement = remote_worker_placement(&host_models(), Some(&resources), true);
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
    }

    #[test]
    fn explicit_empty_manifest_still_requires_the_revocation_capability() {
        let resources = awaken_protocol_managed::SessionResourceManifest::new(
            "workspace-a",
            awaken_protocol_managed::ResolvedSessionResources {
                inputs: Vec::new(),
                skills: Some(Vec::new()),
            },
        );
        let placement = remote_worker_placement(&host_models(), Some(&resources), true);
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
