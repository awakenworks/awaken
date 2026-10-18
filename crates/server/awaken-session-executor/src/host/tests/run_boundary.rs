// @formal evidence worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal evidence worker_drain.grace_expiry_retains_cancellation_observer
//! Run boundary: drain fence, interrupts, construction ports, capture, and the attempt decorator.

use super::*;

fn completed_outcome(progress: HostOutcomeDrive) -> HostOutcomeReport {
    match progress {
        HostOutcomeDrive::Completed(report) => report,
        HostOutcomeDrive::Awaiting => panic!("test Outcome unexpectedly awaited input"),
    }
}

#[tokio::test(start_paused = true)]
async fn runtime_drain_fence_obeys_the_callers_absolute_deadline() {
    // Cause/effect graph: C1 pool-fence acquisition completes or remains
    // pending; C2 the absolute drain deadline is before or after completion.
    // E1 return the exact fence value on completion; E2 retain and poll the
    // pending production future until C2; E3 report timeout without a fence.
    // Constraint: `Some(None)` remains the valid no-local-pool result and must
    // not be confused with E3's outer `None`.
    //
    // | Rule | C1 | C2 | Effect |
    // |---|---|---|---|
    // | F1 | complete | after completion | E1 (`Some(None)`) |
    // | F2 | pending | before deadline | E2 |
    // | F3 | pending | at deadline | E3 |
    let budget = std::time::Duration::from_secs(5);
    assert!(
        matches!(
            await_runtime_drain_fence_before(tokio::time::Instant::now() + budget, async {
                None::<crate::PoolDrainFence>
            })
            .await,
            Some(None)
        ),
        "F1/E1"
    );

    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let deadline = tokio::time::Instant::now() + budget;
    let task = tokio::spawn(await_runtime_drain_fence_before(deadline, {
        let dropped = dropped.clone();
        let started = started.clone();
        async move {
            struct FenceDrop(Arc<AtomicBool>);
            impl Drop for FenceDrop {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _fence = FenceDrop(dropped);
            started.notify_one();
            std::future::pending::<Option<crate::PoolDrainFence>>().await
        }
    }));
    started.notified().await;
    tokio::time::advance(budget - std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(!task.is_finished(), "F2/E2");
    assert!(!dropped.load(Ordering::SeqCst), "F2/E2");
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert!(task.await.unwrap().is_none(), "F3/E3");
    assert!(dropped.load(Ordering::SeqCst), "F3/E3");
}

/// Wait for the Provider-side gate or fail immediately if setup terminates the
/// Run first. This makes fixture drift observable instead of turning a missing
/// precondition into an unbounded `Notify` wait.
async fn await_interrupt_inference_gate<T>(
    reached: &tokio::sync::Notify,
    task: &mut tokio::task::JoinHandle<Result<T, HostError>>,
) {
    tokio::select! {
        () = reached.notified() => {}
        result = task => match result {
            Ok(Ok(_)) => panic!("Run completed before reaching the inference gate"),
            Ok(Err(error)) => panic!("Run failed before reaching the inference gate: {error}"),
            Err(error) => panic!("Run task failed before reaching the inference gate: {error}"),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_the_run_and_reports_interrupted() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = (crate::HostDependencies::ephemeral(model, "scripted").await)
        .build()
        .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);

    // Cause/effect rule: C0 the canonical Dispatch runtime is installed; C1 an
    // Outcome receives needs_revision; C2 its next Worker Run reaches blocked
    // inference; C3 interrupt is accepted. Effects: E0 fixture setup cannot
    // masquerade as a gate hang; E1 the prior Grade remains needs_revision; E2
    // the active cycle ends interrupted. R1=C0+C1+C2+C3=>E0+E1+E2.
    let driver = host.clone();
    let mut task =
        tokio::spawn(async move { driver.define_outcome("t1", "finish", "FINAL", 5).await });

    // Once the loop is blocked mid-run, interrupt it, then release the gate.
    await_interrupt_inference_gate(&reached, &mut task).await;
    host.interrupt("t1").await.expect("interrupt");
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    // Round 1 graded needs_revision; the interrupt ended the run before the
    // second round could conclude, so the outcome reports interrupted.
    assert_eq!(report.iterations[0].result, "needs_revision");
    assert_eq!(
        report.iterations.last().expect("a round").result,
        "interrupted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_acknowledgment_replaces_the_budget_terminal() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C0 the canonical Dispatch runtime is installed; C1
    // max_iterations=1 and a needs_revision Grade enter the stable
    // acknowledgment Run; C2 that Run owns the active cancellation slot; C3
    // user.interrupt lands while its model request is blocked. Effects: E0
    // fixture setup cannot masquerade as a gate hang; E1 the existing Host
    // cancellation path ends the Outcome as interrupted; E2 the one graded
    // cycle remains iteration 0; E3 its public terminal is interrupted, with
    // neither max_iterations_reached nor an invented cycle 1.
    //
    // | Rule | Runtime | At cap | Ack active | Interrupt | Effects |
    // | R1 | installed | yes | yes | yes | E0 + iteration 0 interrupted |
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = (crate::HostDependencies::ephemeral(model, "scripted").await)
        .build()
        .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);

    let driver = host.clone();
    let mut task = tokio::spawn(async move {
        driver
            .define_outcome("ack-interrupt", "finish", "FINAL", 1)
            .await
    });

    await_interrupt_inference_gate(&reached, &mut task).await;
    host.interrupt("ack-interrupt").await.expect("interrupt");
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    assert_eq!(report.iterations.len(), 1, "R1/E2-E3");
    assert_eq!(report.iterations[0].iteration, 0, "R1/E2");
    assert_eq!(report.iterations[0].result, "interrupted", "R1/E1+E3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_authority_loss_interrupts_active_session_before_revocation() {
    // Cause/effect rule: C0 the canonical Dispatch runtime is installed; C1 an
    // Outcome owns active Worker and Judge Session projections; C2 its next
    // Worker inference is blocked; C3 worker authority is lost. Effects: E0
    // setup errors fail before the gate wait; E1 both active projections are
    // interrupted before E2 both realizations are revoked. E3 every current
    // projection closes before the first asynchronous interruption/drain. This
    // is the same split Host operation consumed by the Worker loss handler.
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = (crate::HostDependencies::ephemeral(model, "scripted").await)
        .build()
        .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);

    let driver = host.clone();
    let mut task = tokio::spawn(async move {
        driver
            .define_outcome("authority-loss-active", "finish", "FINAL", 5)
            .await
    });

    await_interrupt_inference_gate(&reached, &mut task).await;
    // The outcome owns both its Worker Session and its Judge Session; authority
    // loss must fence every process-local projection, not only the caller thread.
    let revocation = host.run_assembly.begin_session_realization_revocation();
    assert!(
        host.installed_sessions.session_ids().iter().all(|id| {
            host.installed_sessions
                .realizations()
                .read_realization(id, |r| r.execution().authority.is_closed())
                .expect("installed Sessions")
                .unwrap()
        }),
        "E3 before asynchronous interruption"
    );
    assert_eq!(host.interrupt_all_session_runs().await, 2);
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    assert_eq!(
        report.iterations.last().expect("a round").result,
        "interrupted"
    );
    assert_eq!(
        host.run_assembly
            .finish_session_realization_revocation(revocation)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn worker_interrupt_uses_resident_claim_without_coordinator_queue_operations() {
    // Cause/effect graph: C1 Worker transport has no global queue authority;
    // C2 resident attempt is selected by direct active hint or installed claim;
    // C3 a different Run is registered in the same Runtime; C4 scope is absent.
    // D1 direct hint + C1 -> cancel that token, no transport I/O.
    // D2 no direct hint + claim + C1 -> cancel that token, never list/requeue.
    // D3 D1/D2 + C3 -> unrelated token unchanged; no physical ACK fabricated.
    // D4 C4 -> idempotent no-op, no cold Runtime or Session materialization.
    // D5 direct hint whose Runtime registration is absent -> the foreground
    // signal is cancelled and the request succeeds (the former live delivery
    // failed NotActive here and was ignored; nothing is delivered now).
    // A later registration of the same Run after selection is the
    // dispatch-execution selection test; it is never signalled.
    // Coordinator user interruption keeps its existing durable-intent tests.
    let local = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&local);
    let ctx = local
        .run_assembly
        .ctx_for("worker-stop", None)
        .await
        .unwrap();
    let worker = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_authority(crate::UpstreamAuthority::default()))
    .build()
    .expect("validated Host dependencies");
    for direct in [true, false] {
        let run_id = RunId(format!("worker-stop-{direct}"));
        let cancelled = awaken_runtime_contract::CancellationToken::new();
        let unrelated = awaken_runtime_contract::CancellationToken::new();
        let mut context = awaken_runtime_contract::RuntimeRunContext::new();
        context.cancellation = Some(cancelled.clone());
        let physical = context.physical_quiescence.clone();
        let scope = ctx.runtime.begin_active_attempt(
            &run_id,
            &ctx.thread_id,
            context,
            awaken_runtime_contract::execution::LiveInput::None,
        );
        let mut other = awaken_runtime_contract::RuntimeRunContext::new();
        other.cancellation = Some(unrelated.clone());
        let other_scope = ctx.runtime.begin_active_attempt(
            &RunId("foreign".into()),
            &ctx.thread_id,
            other,
            awaken_runtime_contract::execution::LiveInput::None,
        );
        ctx.foreground
            .install_for_test(direct.then(|| run_id.clone()));
        worker.installed_sessions.update_run("worker-stop", |run| {
            run.context = Some(ctx.clone());
            run.dispatch_claim = Some(awaken_dispatch_contract::RunClaim {
                run_id,
                owner: "worker-old".into(),
                epoch: 1,
            });
        });
        // The local signal must not clear pre-existing nested uncertainty.
        physical.mark_uncertain();
        worker.interrupt("worker-stop").await.unwrap();
        assert!(cancelled.is_cancelled(), "D1/D2 target attempt");
        assert!(!unrelated.is_cancelled(), "D3 foreign Run");
        assert!(
            !physical.can_acknowledge_return(),
            "D3 no fabricated return"
        );
        drop((scope, other_scope));
    }
    let unregistered = RunId("worker-stop-unregistered".into());
    let foreground_signal = ctx.foreground.arm(&unregistered).unwrap();
    worker.installed_sessions.update_run("worker-stop", |run| {
        run.dispatch_claim = Some(awaken_dispatch_contract::RunClaim {
            run_id: unregistered.clone(),
            owner: "worker-old".into(),
            epoch: 1,
        });
    });
    worker.interrupt("worker-stop").await.unwrap();
    assert!(foreground_signal.is_cancelled(), "D5 foreground signal");
    worker.interrupt("absent-worker-session").await.unwrap();
    assert!(
        !worker.installed_sessions.contains("absent-worker-session"),
        "D4 no materialization"
    );
}

#[tokio::test]
async fn interrupt_is_a_noop_when_nothing_runs() {
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(GatedModel {
            gate: Arc::new(tokio::sync::Notify::new()),
            reached: Arc::new(tokio::sync::Notify::new()),
            calls: AtomicUsize::new(0),
        }),
        "scripted",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    // No run in flight → interrupt succeeds and does nothing.
    host.interrupt("idle-thread")
        .await
        .expect("interrupt is a no-op");
}

#[tokio::test]
async fn runtime_constructor_installs_the_exact_file_content_source() {
    struct ConstructorFileSource;

    #[async_trait::async_trait]
    impl crate::FileContentSource<crate::ResourceOperationFence> for ConstructorFileSource {
        async fn read(
            &self,
            workspace_id: &str,
            file_id: &str,
            _purpose: &awaken_resource_contract::FileReadPurpose,
            _claim: Option<&crate::ResourceOperationFence>,
        ) -> Result<
            Option<awaken_resource_contract::ResolvedFileContent>,
            awaken_resource_contract::FileContentSourceError,
        > {
            let bytes = format!("{workspace_id}/{file_id}").into_bytes();
            Ok(Some(awaken_resource_contract::ResolvedFileContent {
                file_id: file_id.into(),
                content_id: awaken_resource_contract::content_id(&bytes),
                filename: "input.txt".into(),
                media_type: "text/plain".into(),
                bytes,
            }))
        }
    }

    // Cause/effect decision table: R1 production Runtime constructor + exact File content port ->
    // that same port serves immutable bytes; R2 no post-construction override -> the
    // default-feature CLI remains compilable without exposing the volatile fixture mutator.
    // Unavailable-source failure semantics are owned by awaken-resource-contract and tested there.
    let host = (crate::HostDependencies::for_coordinator(
        Arc::new(MemoryHostModel),
        "stub",
        {
            let memory = Arc::new(
                awaken_memory_store::SqliteMemoryRepository::open(
                    ":memory:",
                    awaken_time::system_clock(),
                )
                .await
                .expect("open test Memory repository"),
            );
            crate::WorkerContentAdapters {
                file_content_source: Arc::new(ConstructorFileSource),
                memory: awaken_resource_contract::MemoryExecutionContent::new(memory),
            }
        },
        SharedHost::test_memory_extraction_repository(None).await,
        crate::DeploymentConfig::ephemeral()
            .host_process(SharedHost::mint_local_workspace(), system_clock())
            .expect("ephemeral process"),
        Arc::new(crate::EphemeralRuntimeAuthority::new(system_clock())),
    ))
    .build()
    .expect("validated Host dependencies");
    let resolved = host
        .worker_file_content_source()
        .read(
            "workspace-runtime",
            "file-runtime",
            &awaken_resource_contract::FileReadPurpose::SessionResource,
            None,
        )
        .await
        .expect("read through exact constructor port")
        .expect("constructor source returns one File");

    assert_eq!(resolved.bytes, b"workspace-runtime/file-runtime");
}

#[tokio::test]
async fn attributed_run_meets_deployment_capture_with_control_consent() {
    struct SubjectConsent;
    #[derive(Default)]
    struct CountingCaptureSink(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_data_governance_contract::DataSubjectConsentSource for SubjectConsent {
        async fn consent_ceiling(
            &self,
            subject: &awaken_runtime_contract::DataSubjectId,
            _purpose: awaken_runtime_contract::Purpose,
        ) -> awaken_runtime_contract::ContentCapture {
            if subject.as_str() == "granted" {
                awaken_runtime_contract::ContentCapture::Full
            } else {
                awaken_runtime_contract::ContentCapture::Structured
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_runtime_contract::CaptureSink for CountingCaptureSink {
        async fn record(
            &self,
            _subject: &awaken_runtime_contract::DataSubjectId,
            _purpose: awaken_runtime_contract::Purpose,
            _kind: awaken_runtime_contract::ContentKind,
            _content: &str,
        ) -> Result<(), awaken_runtime_contract::CaptureError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // Cause/effect decision table: deployment Full + granted subject -> prompt
    // and completion are persisted; deployment Full + absent/withdrawn subject
    // -> no content row. The single attempt-executor decorator owns this meet for
    // both direct and durable delivery; this unit partition proves the direct
    // route and management_capture_erasure_loop_e2e proves durable delivery.
    let mut deployment = crate::DeploymentConfig::ephemeral();
    deployment.content_capture.level = awaken_runtime_contract::ContentCapture::Full;
    let captured = Arc::new(CountingCaptureSink::default());
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral_with_deployment(
            Arc::new(MemoryHostModel),
            "stub",
            deployment,
        )
        .await
        .with_capture_sink(captured.clone())
        .with_capture_policy_sources(
            awaken_data_governance_contract::CapturePolicySources {
                consent: Arc::new(SubjectConsent),
                ..Default::default()
            },
        ))
        .build()
        .expect("validated Host dependencies"),
    );

    host.run_attributed(
        None,
        "consent-granted",
        user("hello granted"),
        Some(awaken_runtime_contract::DataSubjectId("granted".into())),
    )
    .await
    .expect("R1 attributed run");
    let after_granted = captured.0.load(Ordering::SeqCst);
    assert!(after_granted >= 2, "R1 prompt and completion captured");

    host.run_attributed(
        None,
        "consent-unknown",
        user("hello unknown"),
        Some(awaken_runtime_contract::DataSubjectId("unknown".into())),
    )
    .await
    .expect("R2 attributed run");
    assert_eq!(
        captured.0.load(Ordering::SeqCst),
        after_granted,
        "R2 consent clamps capture"
    );
}

#[tokio::test]
async fn workspace_narrowing_tightens_the_deployment_ceiling_for_each_run() {
    use awaken_data_governance_contract::{
        TelemetryNarrowing, TelemetryNarrowingSource, TelemetryPolicyError,
    };
    use std::collections::BTreeMap;

    /// Control's authored narrowing per Workspace; `None` = unreadable store.
    #[derive(Default)]
    struct WorkspaceNarrowing(std::sync::Mutex<BTreeMap<String, Option<TelemetryNarrowing>>>);

    impl WorkspaceNarrowing {
        fn set(&self, workspace: &str, value: Option<TelemetryNarrowing>) {
            self.0.lock().unwrap().insert(workspace.into(), value);
        }
    }

    #[async_trait::async_trait]
    impl TelemetryNarrowingSource for WorkspaceNarrowing {
        async fn workspace_narrowing(
            &self,
            workspace: &str,
        ) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
            match self.0.lock().unwrap().get(workspace) {
                None => Ok(TelemetryNarrowing::default()),
                Some(Some(narrowing)) => Ok(*narrowing),
                Some(None) => Err(TelemetryPolicyError::StoreUnavailable),
            }
        }
    }

    #[derive(Default)]
    struct CountingCaptureSink(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_runtime_contract::CaptureSink for CountingCaptureSink {
        async fn record(
            &self,
            _subject: &awaken_runtime_contract::DataSubjectId,
            _purpose: awaken_runtime_contract::Purpose,
            _kind: awaken_runtime_contract::ContentKind,
            _content: &str,
        ) -> Result<(), awaken_runtime_contract::CaptureError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // Cause/effect decision table. C1 deployment ceiling = Full (the top
    // layer); C2 the Run's Workspace narrowing: unset, Structured, unreadable;
    // C3 the subject consent grants Full (NullResolver), so only C1/C2 decide.
    // | Rule | Workspace (narrowing)    | Effect                              |
    // | W1   | ws-open (unset)          | prompt + completion captured        |
    // | W2   | ws-closed (Structured)   | no content row                      |
    // | W3   | ws-open, now Structured  | next Run of the same Thread: none   |
    // | W4   | ws-broken (unreadable)   | fail closed: no content row         |
    // W1 and W2 run concurrently-configured Workspaces of one self-hosted
    // deployment: each Workspace's narrowing applies only to its own Runs.
    let mut deployment = crate::DeploymentConfig::ephemeral();
    deployment.content_capture.level = awaken_runtime_contract::ContentCapture::Full;
    let captured = Arc::new(CountingCaptureSink::default());
    let narrowing = Arc::new(WorkspaceNarrowing::default());
    narrowing.set(
        "ws-closed",
        Some(TelemetryNarrowing {
            content_capture: Some(awaken_runtime_contract::ContentCapture::Structured),
        }),
    );
    narrowing.set("ws-broken", None);
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral_with_deployment(
            Arc::new(MemoryHostModel),
            "stub",
            deployment,
        )
        .await
        .with_capture_sink(captured.clone())
        .with_capture_policy_sources(
            awaken_data_governance_contract::CapturePolicySources {
                narrowing: narrowing.clone(),
                ..Default::default()
            },
        ))
        .build()
        .expect("validated Host dependencies"),
    );
    let subject = || Some(awaken_runtime_contract::DataSubjectId("subject".into()));
    for (thread, workspace) in [
        ("narrow-open", "ws-open"),
        ("narrow-closed", "ws-closed"),
        ("narrow-broken", "ws-broken"),
    ] {
        host.run_assembly
            .register_thread_workspace(thread, workspace);
    }

    host.run_attributed(None, "narrow-open", user("hello open"), subject())
        .await
        .expect("W1 run");
    let after_open = captured.0.load(Ordering::SeqCst);
    assert!(after_open >= 2, "W1 prompt and completion captured");

    host.run_attributed(None, "narrow-closed", user("hello closed"), subject())
        .await
        .expect("W2 run");
    assert_eq!(captured.0.load(Ordering::SeqCst), after_open, "W2");

    narrowing.set(
        "ws-open",
        Some(TelemetryNarrowing {
            content_capture: Some(awaken_runtime_contract::ContentCapture::Structured),
        }),
    );
    host.run_attributed(None, "narrow-open", user("hello again"), subject())
        .await
        .expect("W3 run");
    assert_eq!(captured.0.load(Ordering::SeqCst), after_open, "W3");

    host.run_attributed(None, "narrow-broken", user("hello broken"), subject())
        .await
        .expect("W4 run");
    assert_eq!(captured.0.load(Ordering::SeqCst), after_open, "W4");
}

#[tokio::test]
async fn bound_executor_is_the_ordinary_run_execution_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::RunExecutor;

    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub").await)
            .build()
            .expect("validated Host dependencies"),
    );
    let ctx = host
        .run_assembly
        .ctx_for("snapshot-run", None)
        .await
        .expect("context");
    let before = ctx.commit.committed_messages(&ctx.thread_id).len();
    let activation = RunActivation::new(
        RunId("snapshot-run-1".into()),
        ctx.thread_id.clone(),
        ctx.config.clone(),
        user("hello"),
    );
    let state = BoundRunExecutor::new(&host, ctx.clone())
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("snapshot run");

    assert!(matches!(state, RunState::Ended(EndCause::NaturalEnd)));
    let messages = ctx.commit.committed_messages(&ctx.thread_id);
    assert_eq!(before, 0);
    assert_eq!(messages.len(), 2, "user + assistant delta");
    assert_eq!(messages.last().unwrap().text_content(), "ok");
}

#[tokio::test]
async fn host_application_decorator_wraps_the_complete_session_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::{
        Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resume::ResumeCommand;
    use std::sync::atomic::Ordering;

    struct ObservingAttemptExecutor {
        calls: Arc<AtomicUsize>,
        inner: Arc<dyn RunAttemptExecutor>,
    }

    #[async_trait::async_trait]
    impl RunExecutor for ObservingAttemptExecutor {
        async fn execute(
            &self,
            activation: RunActivation,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.execute(activation, context).await
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for ObservingAttemptExecutor {
        async fn resume(
            &self,
            activation: RunActivation,
            command: ResumeCommand,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.resume(activation, command, context).await
        }

        async fn cancel(
            &self,
            activation: RunActivation,
            context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.cancel(activation, context).await
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let decorator_calls = calls.clone();
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
            .await
            .with_attempt_decorator(Arc::new(move |inner| {
                Arc::new(ObservingAttemptExecutor {
                    calls: decorator_calls.clone(),
                    inner,
                })
            })))
        .build()
        .expect("validated Host dependencies"),
    );
    let ctx = host
        .run_assembly
        .ctx_for("injected-attempt", None)
        .await
        .unwrap();
    let activation = RunActivation::new(
        RunId("injected-attempt-run".into()),
        ctx.thread_id.clone(),
        ctx.config.clone(),
        user("go"),
    );
    let state = BoundRunExecutor::new(&host, ctx)
        .execute(activation, RuntimeRunContext::new())
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tool_bearing_snapshot_can_be_restricted_at_the_run_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::RunExecutor;
    use awaken_runtime_contract::resolved::ToolDescriptor;

    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub").await)
            .build()
            .expect("validated Host dependencies"),
    );
    let ctx = host
        .run_assembly
        .ctx_for("unsafe-grader", None)
        .await
        .expect("context");
    let mut snapshot = ctx.config.clone();
    snapshot
        .resolved_spec
        .tool_descriptors
        .push(ToolDescriptor::pinned(
            "test",
            "write",
            "writes",
            serde_json::json!({"type": "object"}),
        ));

    let activation = RunActivation::new(
        RunId("unsafe-grader-1".into()),
        ctx.thread_id.clone(),
        snapshot,
        user("grade"),
    )
    .without_tools();
    // Cause graph: C1 the snapshot has a colliding model-facing tool catalog;
    // C2 this attempt narrows authority to DenyAll. E1 no tool or discovery
    // prompt is projected, E2 the model can answer normally, and E3 a recovered
    // or injected call remains blocked by the same closed narrowing enum.
    let state = BoundRunExecutor::new(&host, ctx)
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("declared tools do not bypass a per-Run deny-all restriction");

    assert!(
        matches!(state, RunState::Ended(EndCause::NaturalEnd)),
        "unexpected restricted Run state: {state:?}"
    );
}

/// A model that blocks on its first inference until released, so a concurrent
/// `interrupt` lands while a plain Run is in flight.
struct BlockOnceModel {
    reached: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl LlmExecutor for BlockOnceModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.reached.notify_one();
        self.gate.notified().await;
        Ok(ChatResponse {
            output: AssistantOutput::text("too late"),
            usage: None,
            stop_reason: None,
        })
    }
}

/// The real-Run interrupt the conformance matrix flagged as unasserted: a plain
/// `run` (a Managed Session's normal Run), interrupted while its inference is in
/// flight, ends `Cancelled` promptly instead of running to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_ends_an_in_flight_run_as_cancelled() {
    // Test design. Causes: C0 the canonical Dispatch runtime is installed; C1 a
    // Managed Run is blocked in inference; C2 interrupt is accepted before
    // inference returns. Effects: E0 setup errors fail before the gate wait; E1
    // C2 ends the exact Run as Cancelled rather than committing the late model
    // reply. Constraint/Invariant: interruption targets the active Run generation
    // only. Decision rule: C0, block, interrupt, release inference, require E0+E1.
    let reached = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(BlockOnceModel {
            reached: reached.clone(),
            gate: gate.clone(),
        }),
        "scripted",
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);

    let driver = host.clone();
    let mut task = tokio::spawn(async move { driver.run(None, "t-int", user("go")).await });

    // The Run is blocked mid-inference; interrupt it, then release the gate.
    await_interrupt_inference_gate(&reached, &mut task).await;
    host.interrupt("t-int").await.expect("interrupt");
    gate.notify_one();

    let result = task.await.expect("join").expect("run");
    assert!(
        matches!(result.state, RunState::Ended(EndCause::Cancelled)),
        "an interrupted in-flight Run ends Cancelled, not run to completion: {:?}",
        result.state
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_interrupt_returns_after_intent_before_the_blocked_attempt_finishes() {
    // Causes: C0 canonical durable Dispatch composition; C1 inference entered
    // without return evidence; C2 accepted cancellation replaces the claim.
    // C1+C2 -> prompt intent ACK, then predecessor-unconfirmed Indeterminate,
    // retaining the exact Run's physical slot and publishing no model reply.
    // ThreadCommit owns logical state; Dispatch owns physical exclusion. Neither
    // a new claim nor a returned Host future proves remote termination.
    // Direct cooperative cancellation is the adjacent test's Cancelled row;
    // lease_semantics::native_cancelled_external_future owns the lower tool seam.
    // SessionRunProtocol's ExternalAttemptRetainsExactSlot and unsafe-ACK mutant
    // cover the abstract exclusion rule, not provider termination or liveness.
    //
    // | Rule | Runtime | durable Run | Provider | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | R1 | installed | active | blocked | new claim | prompt ACK + Indeterminate + occupied |
    // | R2 | installed | absent | n/a | replay/no-op | immediate success (sibling test) |
    let reached = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("dispatch store"),
    );
    let host = (crate::HostDependencies::ephemeral(
        Arc::new(BlockOnceModel {
            reached: reached.clone(),
            gate: gate.clone(),
        }),
        "scripted",
    )
    .await
    .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool()
        .expect("valid test dispatch composition");

    let driver = host.clone();
    let mut run =
        tokio::spawn(async move { driver.run(None, "durable-interrupt", user("go")).await });
    await_interrupt_inference_gate(&reached, &mut run).await;

    let before = dispatch.list_dispatches().await.unwrap();
    assert_eq!(before.len(), 1, "R1: exactly one admitted Run");
    let original = &before[0];
    assert_eq!(original.thread_id, ThreadId("durable-interrupt".into()));
    assert!(original.physical_attempt_active, "R1: model really entered");

    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        host.interrupt("durable-interrupt"),
    )
    .await
    .expect("R1 interrupt admission must not await Provider completion")
    .expect("R1 interrupt admission");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("R1 predecessor-uncertain cancellation drainer")
        .expect("R1 join")
        .expect("R1 durable Run");
    assert!(
        matches!(result.state, RunState::Ended(EndCause::Indeterminate)),
        "R1: {:?}",
        result.state
    );
    let after = dispatch.list_dispatches().await.unwrap();
    assert_eq!(after.len(), 1, "R1: uncertain occupant cannot be removed");
    assert_eq!(after[0].run_id, original.run_id);
    assert!(
        after[0].cancellation_requested,
        "R1: exact cancellation intent retained"
    );
    assert!(
        after[0].physical_attempt_active,
        "R1: dropping inference cannot acknowledge physical termination"
    );
    let ctx = host
        .run_assembly
        .ctx_for("durable-interrupt", None)
        .await
        .unwrap();
    assert_eq!(
        ctx.commit.run_state(&original.run_id),
        Some(RunState::Ended(EndCause::Indeterminate)),
        "R1: Host result is the exact committed terminal fact"
    );
    assert!(
        ctx.commit
            .committed_messages(&ctx.thread_id)
            .iter()
            .all(|message| message.role != Role::Assistant),
        "R1: abandoned inference publishes no reply or proposed tool call"
    );
    gate.notify_waiters();
}

#[tokio::test]
async fn live_only_durable_control_never_materializes_a_cold_session() {
    // Cause/effect table for the Host boundary: C1 typed durable capability is
    // enabled; C2 a process-resident Session context exists; C3 the Runtime owns
    // the exact active attempt. Effects: E1 reject with the capability error;
    // E2 reject as NoSubscriber; E3 deliver through the existing live-control
    // service; E4 create no Session/Environment/dispatch fact.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | W0 | F | any | any | E1 + E4 |
    // | W1 | T | F | F | E2 + E4 |
    // | W2 | T | T | F | E2 (LiveRunControlService test) |
    // | W3 | T | T | T | E3 (Runtime active-attempt test) |
    //
    // This test owns W0/W1, where the regression previously called `ctx_for`
    // and surfaced an unrelated Environment failure. The cited lower-owner
    // tests retain W2/W3, so this Host test adds no parallel attempt registry.
    let cold_thread = "cold-live-only-control";
    let run_id = "cold-live-only-run";
    let direct = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let unsupported = direct
        .wake_durable(cold_thread, run_id)
        .await
        .expect_err("W0 direct ingress rejects durable control");
    assert_eq!(unsupported.kind, HostErrorKind::BadRequest, "W0/E1");
    assert!(
        unsupported.message.contains("durable ingress not enabled"),
        "W0/E1: {}",
        unsupported.message
    );
    assert!(
        direct
            .installed_sessions
            .read(cold_thread, |_| ())
            .is_none(),
        "W0/E4 direct control must not create a Session slot"
    );

    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("live-control dispatch store"),
    );
    let durable = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    let wake = durable
        .wake_durable(cold_thread, run_id)
        .await
        .expect_err("W1 cold wake has no subscriber");
    assert_eq!(wake.kind, HostErrorKind::BadRequest, "W1/E2");
    assert_eq!(
        wake.message,
        format!("no live subscriber for run: {run_id}"),
        "W1/E2"
    );

    let explicit_pause = durable
        .pause_durable(cold_thread, Some(run_id))
        .await
        .expect_err("W1 cold explicit pause has no subscriber");
    assert_eq!(explicit_pause.kind, HostErrorKind::BadRequest, "W1/E2");
    assert_eq!(explicit_pause.message, wake.message, "W1/E2");
    let implicit_pause = durable
        .pause_durable(cold_thread, None)
        .await
        .expect_err("W1 cold implicit pause has no active attempt");
    assert_eq!(implicit_pause.kind, HostErrorKind::BadRequest, "W1/E2");
    assert_eq!(
        implicit_pause.message, "thread has no locally owned active run",
        "W1/E2"
    );
    assert!(
        durable
            .installed_sessions
            .read(cold_thread, |_| ())
            .is_none(),
        "W1/E4 live-only control must not materialize a Session or Environment"
    );
    assert!(
        dispatch
            .list_dispatches()
            .await
            .expect("W1 inspect dispatch authority")
            .is_empty(),
        "W1/E4 live-only control must not create a dispatch fact"
    );
}

#[tokio::test]
async fn managed_interrupt_cancels_cold_dispatch_without_realizing_an_environment() {
    use awaken_dispatch::RunDispatch;

    // Cause/effect graph: C1 one Session-affined dispatch exhausted retries; C2
    // no Runtime context/Environment is resident; C3 Environment realization
    // may be unavailable; C4 user.interrupt is accepted. Effects: E1 select and
    // requeue the exact dead-letter row; E2 persist its cancellation so the
    // ordinary Worker cancellation claim can settle it; E3 do not construct a
    // Runtime context or touch Environment realization. Constraint: this test
    // observes the canonical Session slot and dispatch store only; it adds no
    // control registry or force-delete path.
    //
    // | Rule | dispatch | resident context | Environment | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | C1 | unique dead letter | absent | unavailable/unknown | accepted | E1+E2+E3 |
    // | C2 | absent | absent | unavailable/unknown | accepted | no-op+E3 (idle sibling) |
    // | C3 | multiple exact Session rows | absent | any | accepted | cancel each (sibling) |
    let thread = "cold-managed-interrupt";
    let run_id = RunId("cold-managed-interrupt-run".into());
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                thread, &run_id.0,
            ))
            .for_session(ThreadId(thread.into()))
            .with_session_activity_epoch(1),
        )
        .await
        .expect("C1 executable Session dispatch");
    clock.set(0);
    let claimed = dispatch
        .claim("failed-worker", 1, &Default::default())
        .await
        .expect("C1 claim")
        .expect("C1 exact claim");
    assert_eq!(
        dispatch
            .quarantine_retry_exhausted(0, claimed.lease.expires_ms().saturating_add(1))
            .await
            .expect("C1 quarantine retry exhaustion"),
        1,
        "C1 exact dead letter"
    );
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    assert!(
        host.installed_sessions
            .read_run(thread, |run| run.context.clone())
            .flatten()
            .is_none(),
        "C2 precondition"
    );

    host.interrupt(thread).await.expect("C4 interrupt accepted");

    let rows = dispatch
        .list_dispatches()
        .await
        .expect("E1 inspect canonical dispatch");
    assert_eq!(rows.len(), 1, "E1 exact row");
    assert_eq!(
        rows[0].state,
        awaken_dispatch_contract::DispatchState::Pending,
        "E1 dead letter reuses the same runnable row"
    );
    assert!(rows[0].cancellation_requested, "E2 durable cancellation");
    assert!(
        host.installed_sessions
            .read_run(thread, |run| run.context.clone())
            .flatten()
            .is_none(),
        "E3 interrupt must not realize a cold Runtime/Environment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_primary_interrupt_recovers_the_active_dispatch_without_a_local_run_hint() {
    // Test design. Causes: C0 the canonical Dispatch runtime is installed; C1
    // the primary Session has no local run/cancel hint; C2 exactly one executable
    // durable dispatch is active; C3 a remote attempt owns it. Effects: E1
    // interrupt selects C2, persists cancellation, invokes remote cancel, and
    // settles Cancelled. Constraint/Invariant: selection uses durable dispatch
    // truth, never guessed process state. Decision rule: C0+unique C2+C3 yields
    // E1; ambiguity is owned by the fail-closed sibling test.
    use awaken_dispatch::RunDispatch;
    use awaken_runtime_contract::execution::{
        Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resume::ResumeCommand;

    struct RecordingRemoteAttempt {
        cancelled: Mutex<Vec<RunId>>,
        cancellation_seen: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl RunExecutor for RecordingRemoteAttempt {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution(
                "the replacement cancellation claim must not execute the remote Run".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for RecordingRemoteAttempt {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution(
                "the replacement cancellation claim must not resume the remote Run".into(),
            ))
        }

        async fn cancel(
            &self,
            activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            self.cancelled.lock().unwrap().push(activation.run_id);
            self.cancellation_seen.notify_one();
            Ok(())
        }
    }

    // Cause/effect graph: C1 a Managed primary Run is already leased from the
    // one durable Dispatch store; C2 its immutable backend is remote; C3 the Run
    // bypassed BoundRunExecutor, so SessionCtx has no `active_run`; C4 the same
    // accepted user interrupt reaches SharedHost. Effects: E1 C4 resolves the
    // unique executable Session-affined row instead of succeeding as a no-op;
    // E2 the ordinary pool cancellation claim invokes the installed remote
    // executor exactly once with that Run id; E3 the claim commits Cancelled and
    // settles the row. Constraints: the interrupt caller records intent only;
    // the old lease is fenced and may not perform remote cancellation; no second
    // queue, live-control bus, Session cache, or synchronous driver is created.
    //
    // | Rule | leased row | backend | local hint | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | M1 | unique + Session-affined | remote | absent | accepted | E1+E2+E3 |
    // | M2 | absent | any | absent | accepted | no-op (existing idle test) |
    // | M3 | multiple executable primary rows | any | absent | accepted | cancel each exact row |
    // | M4 | exact local hint | any | present | accepted | existing exact-id path |
    let thread = "managed-active-remote-interrupt";
    let run_id = RunId("managed-active-remote-run".into());
    let mut activation =
        crate::host::worker_resolver::test_support::test_activation(thread, &run_id.0);
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "remote",
                "",
                "a2a:http://remote.invalid",
            ),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "sha256:active-remote",
        )
        .expect("coherent active remote candidate");
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    dispatch
        .enqueue(
            RunDispatch::new(activation)
                .for_session(ThreadId(thread.into()))
                .with_session_activity_epoch(1),
        )
        .await
        .expect("M1 Managed Run admission");
    let previous = dispatch
        .claim("previous-worker", DEFAULT_LEASE_MS, &Default::default())
        .await
        .expect("M1 old Worker claim")
        .expect("M1 active dispatch");
    assert_eq!(previous.lease.run_id(), &run_id, "M1/C1");

    let remote = Arc::new(RecordingRemoteAttempt {
        cancelled: Mutex::new(Vec::new()),
        cancellation_seen: tokio::sync::Notify::new(),
    });
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch.clone())
        .with_remote_attempt_executor(RemoteAttemptInstallation {
            executor: remote.clone(),
            credential_realization: Default::default(),
        }))
    .build()
    .expect("validated Host dependencies");
    install_recording_session_application(&host);
    let _managed = install_test_dispatch_runtime(&host);
    let ctx = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .expect("M1 resident Session");
    assert!(
        !ctx.foreground.has_run().unwrap(),
        "M1/C3 Managed admission has no foreground hint"
    );
    host.ensure_dispatch_pool()
        .expect("valid test dispatch composition");

    host.interrupt(thread)
        .await
        .expect("M1/E1 interrupt intent");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        remote.cancellation_seen.notified(),
    )
    .await
    .expect("M1/E2 canonical cancellation claim");
    assert_eq!(
        *remote.cancelled.lock().unwrap(),
        std::slice::from_ref(&run_id),
        "M1/E2"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if dispatch
                .list_dispatches()
                .await
                .expect("M1 inspect settlement")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("M1/E3 cancellation settlement");
    assert_eq!(
        host.thread_reads
            .commit_for_read(thread)
            .await
            .expect("M1 committed truth")
            .run_state(&run_id),
        Some(RunState::Ended(EndCause::Cancelled)),
        "M1/E3"
    );
}

#[tokio::test]
async fn managed_primary_interrupt_cancels_every_exact_session_dispatch() {
    use awaken_dispatch::{DispatchOutcome, RunDispatch};

    // Cause/effect graph: C1 the local active Run hint is absent; C2 the store
    // has an Awaiting customer-tool Run and a queued child-report continuation
    // on the same primary Session; C3 another Session/child has a queued Run;
    // C4 interrupt is retried. Effects: E1 both exact primary rows receive
    // durable cancellation; E2 unrelated rows remain untouched; E3 replay is
    // idempotent. Constraint: selection uses the existing Session affinity and
    // committed-terminal filter, never a new registry or a guessed latest Run.
    //
    // | Rule | exact rows | foreign rows | retry | Effects |
    // | A1 | two | two | no | E1+E2 |
    // | A2 | two | two | yes | E1+E2+E3 |
    let thread = "queued-managed-primary-interrupt";
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
            .await
            .expect("dispatch store"),
    );
    for run in ["awaiting-primary", "queued-report"] {
        dispatch
            .enqueue(
                RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                    thread, run,
                ))
                .for_session(ThreadId(thread.into()))
                .with_session_activity_epoch(if run == "awaiting-primary" { 5 } else { 11 }),
            )
            .await
            .expect("A1 legal queued primary admission");
        if run == "awaiting-primary" {
            let claimed = dispatch
                .claim("previous-worker", 30_000, &Default::default())
                .await
                .expect("A1 claim predecessor")
                .expect("A1 predecessor row");
            dispatch
                .settle(
                    claimed.lease.run_id(),
                    claimed.lease.epoch(),
                    DispatchOutcome::Awaiting,
                    &[],
                )
                .await
                .expect("A1 predecessor awaits customer");
        }
    }
    for (owner, run) in [
        ("other-session", "foreign-primary"),
        (thread, "foreign-child"),
    ] {
        let logical = if run == "foreign-child" {
            "child-thread"
        } else {
            owner
        };
        dispatch
            .enqueue(
                RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                    logical, run,
                ))
                .for_session(ThreadId(owner.into()))
                .with_session_activity_epoch(11),
            )
            .await
            .expect("A1 foreign admission");
    }
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_ephemeral_dispatch(dispatch.clone()))
    .build()
    .expect("validated Host dependencies");
    host.interrupt(thread)
        .await
        .expect("A1 exact Session interrupt");
    host.interrupt(thread).await.expect("A2 idempotent retry");
    let rows = dispatch
        .list_dispatches()
        .await
        .expect("A1 inspect durable rows");
    assert_eq!(rows.len(), 4, "A1/C2+C3");
    for row in rows {
        assert_eq!(
            row.cancellation_requested,
            matches!(row.run_id.0.as_str(), "awaiting-primary" | "queued-report"),
            "A1-A2/E1-E3 {}",
            row.run_id.0
        );
    }
}

#[tokio::test]
async fn run_capture_level_is_the_shared_decision_for_every_cause() {
    // Equivalence with the transparency reads: the Run boundary records
    // exactly `decide_capture(requested = Full, top, Workspace answer,
    // consent)` for every top level, every Workspace answer (unset, each
    // level, unreadable) and every consent (unattributed, each level).
    // Effects: E1 the attempt's capture level is that decision's
    // `effective`; E2 an attributed request keeps its subject; E3 an
    // unattributed request never reads consent.
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    use awaken_data_governance_contract::{
        CapturePolicyInputs, CapturePolicySources, DataSubjectConsentSource, TelemetryNarrowing,
        TelemetryNarrowingSource, TelemetryPolicyError, decide_capture,
    };
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::execution::{
        Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resume::ResumeCommand;
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_runtime_contract::{ContentCapture, DataSubjectId, Purpose};
    use std::sync::Mutex;

    struct Narrowing(Result<TelemetryNarrowing, TelemetryPolicyError>);
    #[async_trait::async_trait]
    impl TelemetryNarrowingSource for Narrowing {
        async fn workspace_narrowing(
            &self,
            workspace: &str,
        ) -> Result<TelemetryNarrowing, TelemetryPolicyError> {
            assert_eq!(workspace, "ws-capture");
            self.0
        }
    }
    struct Consent(Option<ContentCapture>);
    #[async_trait::async_trait]
    impl DataSubjectConsentSource for Consent {
        async fn consent_ceiling(
            &self,
            _subject: &DataSubjectId,
            purpose: Purpose,
        ) -> ContentCapture {
            assert_eq!(purpose, Purpose::TelemetryContent);
            self.0.expect("E3 an unattributed Run never reads consent")
        }
    }
    #[derive(Default)]
    struct Observed(Mutex<Option<(ContentCapture, Option<DataSubjectId>)>>);
    #[async_trait::async_trait]
    impl RunExecutor for Observed {
        async fn execute(
            &self,
            _activation: RunActivation,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            *self.0.lock().unwrap() =
                Some((context.capture.decision.level, context.capture.subject));
            Ok(RunState::Ended(EndCause::Stopped("observed".into())))
        }
    }
    #[async_trait::async_trait]
    impl RunAttemptExecutor for Observed {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            unreachable!("only execute is observed")
        }

        async fn cancel(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            unreachable!("only execute is observed")
        }
    }

    let levels = [
        ContentCapture::Off,
        ContentCapture::Structured,
        ContentCapture::Full,
    ];
    let mut workspace_answers = vec![
        Ok(TelemetryNarrowing::default()),
        Err(TelemetryPolicyError::StoreUnavailable),
        Err(TelemetryPolicyError::AuthorityUnreachable),
    ];
    workspace_answers.extend(levels.map(|level| {
        Ok(TelemetryNarrowing {
            content_capture: Some(level),
        })
    }));
    let consents = [None, Some(levels[0]), Some(levels[1]), Some(levels[2])];
    let mut rows = 0;
    for top in levels {
        for workspace in workspace_answers.iter().copied() {
            for consent in consents {
                let capability = crate::host::capability::ContentCaptureCapability {
                    capture_sink: None,
                    capture_decision: awaken_runtime_contract::CaptureDecision::new(top),
                    policy_sources: CapturePolicySources {
                        consent: Arc::new(Consent(consent)),
                        narrowing: Arc::new(Narrowing(workspace)),
                    },
                };
                let observed = Arc::new(Observed::default());
                let executor = crate::run_exec::CaptureContextAttemptExecutor::new(
                    observed.clone(),
                    &capability,
                    "ws-capture".into(),
                );
                let mut run = crate::run_exec::tests::activation("awaken");
                run.data_subject_id = consent.map(|_| DataSubjectId("dsub".into()));
                executor
                    .execute(run, RuntimeRunContext::new())
                    .await
                    .unwrap();
                let (level, subject) = observed.0.lock().unwrap().take().unwrap();
                let expected = decide_capture(CapturePolicyInputs {
                    requested: ContentCapture::Full,
                    top,
                    workspace,
                    consent,
                });
                assert_eq!(
                    level, expected.effective,
                    "E1 {top:?} {workspace:?} {consent:?}"
                );
                assert_eq!(subject.is_some(), consent.is_some(), "E2");
                rows += 1;
            }
        }
    }
    assert_eq!(rows, 3 * 6 * 4);
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchMaintenance as _, DispatchReads as _,
    DispatchSettlement as _,
};
