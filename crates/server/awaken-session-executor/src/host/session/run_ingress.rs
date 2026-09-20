//! How a Session's runs reach an executor: the durable checkpoint store, the
//! foreground delivery choice (durable dispatch or a direct driver), and the
//! per-attempt inference materializer a database-less Worker carries.
//!
//! Split out of `session.rs` unchanged; these three are the ingress seam and
//! share no state with the snapshot/context assembly that remains there.

use super::*;

impl SharedHost {
    /// Build a thread's interrupted-stream checkpoint store, mirroring
    /// `build_commit`'s durability choice: a filesystem store under the configured
    /// directory (so a partial survives a process crash and resumes), or the
    /// checkpoint adapter paired with the shared Postgres dispatch authority.
    /// Only explicit test-support startup may select a process-local store.
    pub(super) fn build_stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<dyn StreamCheckpointStore>>, HostError> {
        // A database-less Worker persists checkpoints through its authenticated,
        // claim-fenced dispatch transport. Installing a local store here would
        // create a second authority that cannot survive Worker replacement.
        if self.upstream.is_some() {
            return Ok(None);
        }
        self.authority
            .as_ref()
            .ok_or_else(|| {
                HostError::internal(
                    "database-less Worker build cannot open a local checkpoint authority",
                )
            })?
            .stream_checkpoint(thread)
            .map(Some)
            .map_err(HostError::from)
    }

    /// Build the one claimed Worker for this Thread, then select its independent
    /// foreground-delivery policy. Durable foreground delivery retains the
    /// durable coordinator that owns that same Worker; direct foreground delivery
    /// keeps one concrete attempt driver while retaining the Worker for pool-routed claims.
    /// Runtime, commit, executor, observers, credentials, and recovery wiring are
    /// therefore configured once regardless of delivery durability.
    pub(super) async fn build_ingress(
        &self,
        runtime: Arc<Runtime>,
        attempt_executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
        commit: Arc<HostCommit>,
        stream_checkpoint: Option<Arc<dyn StreamCheckpointStore>>,
        run_context: awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<
        (
            ForegroundRunDelivery,
            Arc<awaken_dispatch::DispatchWorker<AnyDispatchStore>>,
        ),
        HostError,
    > {
        // The ONE process-shared dispatch queue (shared SQLite file, or the shared
        // Postgres pool for a fleet) plus this process's unique claim owner — both
        // arrive through the Coordinator-owned RuntimeAuthority (ADR-0019/0024).
        // Every session's worker shares this queue; the process-level `DispatchPool`
        // is its sole claimer and routes each run back to its owning session.
        let store = self.dispatch_store()?;
        // Carry the host's InferenceExecutorMaterializer into the worker as a neutral model→executor
        // closure, so a worker-driven run resolves its own configured model per attempt
        // (R1). `None` when no provider is installed — the worker stays on the host
        // default.
        let inference_materializer = self.worker_inference_materializer();
        // Transport presence does not remove installed adapters. Both local
        // claims and remote claims revalidate against this same Host evidence.
        let local_credential_capabilities = self.local_credential_realization_capabilities();
        let recovery_projection = commit.recovery_projection();
        let settlement_observer = self.dispatch_settlement_observer();
        let session_admission_observer = self.session_run_admission_observer();
        // The recovered dispatch a crash left mid-flight is re-executed by this
        // worker; giving it the same checkpoint store lets that re-execution resume
        // the interrupted step from its flushed partial (Phase 3 cross-process).
        let mut worker = awaken_dispatch::DispatchWorker::new(
            runtime.clone(),
            store,
            commit,
            self.deployment.dispatch_owner.clone(),
        );
        if let Some(checkpoint) = stream_checkpoint {
            worker = worker.with_stream_checkpoint(checkpoint);
        }
        if let Some(materializer) = inference_materializer {
            worker = worker.with_inference_materializer(materializer);
        }
        worker = worker.with_context(run_context);
        if let Some(observer) = settlement_observer {
            worker = worker.with_settlement_observer(observer);
        }
        if let Some(observer) = session_admission_observer {
            worker = worker.with_session_run_admission_observer(observer);
        }
        worker = match &self.worker_stream_publisher {
            Some(publisher) => worker.with_claimed_stream_publisher(publisher.clone()),
            None => worker.with_stream_sink(self.completion.clone()),
        };
        worker = worker.with_local_credential_capabilities(local_credential_capabilities);
        if let Some(resolver) = &self.worker_credential_resolver {
            worker = worker.with_worker_credential_resolver(resolver.clone());
        }
        if let Some(upstream) = &self.upstream {
            worker =
                worker.with_claimed_commit(crate::commit_ingest::remote_claimed_commit(upstream)?);
        }
        if let Some(projection) = recovery_projection {
            worker = worker.with_recovery_projection(projection);
        }
        let claimed_worker = Arc::new(worker);
        claimed_worker.install_attempt_executor(attempt_executor.clone());
        // No per-session recovery sweep here: this session's worker shares one queue
        // with every other, so a claim would grab foreign threads' runs. The
        // process-level `DispatchPool` owns recovery — it claims each crashed run and
        // routes it to the session (this one included) that owns its thread.
        if self.deployment.durable {
            Ok((ForegroundRunDelivery::Durable, claimed_worker))
        } else {
            let direct = Arc::new(DirectAttemptDriver::with_attempt_executor(
                runtime,
                attempt_executor,
            ));
            Ok((ForegroundRunDelivery::Direct(direct), claimed_worker))
        }
    }

    /// Wrap this host's `InferenceExecutorMaterializer` (if installed) into the neutral
    /// published-access → executor closure a worker's `WorkerContext` carries, so a
    /// database-less worker materializes the run's configured model per attempt (R1).
    /// `None` when no provider is installed — the worker stays on the runtime's bound
    /// default (a single-model deployment is unaffected).
    pub(crate) fn worker_inference_materializer(
        &self,
    ) -> Option<awaken_dispatch::InferenceMaterializerFn> {
        self.inference_routing.materializer().map(|materializer| {
            let resolve: awaken_dispatch::InferenceMaterializerFn =
                Arc::new(move |activation, context| materializer.materialize(activation, context));
            resolve
        })
    }
}
