//! Decorator lifetime decision rows at the real RunAttemptExecutor entry points.
//! No slot table, execution lease, physical-stop signal or shutdown flag is added.

use super::*;
use crate::application::attempt_context::SessionContextAttemptExecutor;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

struct AttemptProbe {
    calls: AtomicUsize,
    cancels: AtomicUsize,
    destroyed_futures: AtomicUsize,
    entered: Notify,
    release: Semaphore,
    blocked: bool,
    fails: bool,
}

struct AttemptDropReceipt<'a>(&'a AtomicUsize);

impl Drop for AttemptDropReceipt<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl AttemptProbe {
    fn new(blocked: bool, fails: bool) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            destroyed_futures: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Semaphore::new(0),
            blocked,
            fails,
        }
    }

    async fn attempt(&self) -> ExecutionResult<RunState> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _receipt = AttemptDropReceipt(&self.destroyed_futures);
        self.entered.notify_one();
        if self.blocked {
            // awaken-allow(wait): suspend the physical adapter at a controlled
            // boundary while the test drops the last external owner.
            self.release.acquire().await.expect("test release").forget();
        }
        if self.fails {
            Err(ExecutionError::Execution("injected attempt failure".into()))
        } else {
            Ok(RunState::Ended(EndCause::Stopped("probe complete".into())))
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl RunExecutor for AttemptProbe {
    async fn execute(&self, _: RunActivation, _: RuntimeRunContext) -> ExecutionResult<RunState> {
        self.attempt().await
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl RunAttemptExecutor for AttemptProbe {
    async fn resume(
        &self,
        _: RunActivation,
        _: ResumeCommand,
        _: RuntimeRunContext,
    ) -> ExecutionResult<RunState> {
        self.attempt().await
    }

    async fn cancel(&self, _: RunActivation, _: RuntimeRunContext) -> ExecutionResult<()> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("lifetime-run".into()),
        ThreadId("lifetime-thread".into()),
        ExecutableAgentSnapshot::builder("lifetime-agent")
            .model(ModelBinding::parse("provider", "stub", "native").expect("literal binding"))
            .max_steps(1)
            .build(),
        Vec::new(),
    )
}

fn resume(activation: &RunActivation) -> ResumeCommand {
    ResumeCommand {
        operation_id: None,
        correlation_id: "lifetime-correlation".into(),
        run_id: activation.run_id.clone(),
        thread_id: activation.thread_id.clone(),
        snapshot_id: activation.snapshot.id.clone(),
        catalog_fingerprint: activation
            .snapshot
            .resolved_spec
            .catalog_fingerprint
            .clone(),
        result: awaken_runtime_contract::resume::ResumeResult::Input("continue".into()),
        context_messages: Vec::new(),
        now_ms: 1,
    }
}

fn start(
    executor: &dyn RunAttemptExecutor,
    is_resume: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecutionResult<RunState>> + Send + '_>> {
    let activation = activation();
    if is_resume {
        Box::pin(executor.resume(
            activation.clone(),
            resume(&activation),
            RuntimeRunContext::new(),
        ))
    } else {
        Box::pin(executor.execute(activation, RuntimeRunContext::new()))
    }
}

async fn entered(
    probe: &AttemptProbe,
    running: &mut std::pin::Pin<
        Box<dyn std::future::Future<Output = ExecutionResult<RunState>> + Send + '_>,
    >,
) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            _ = probe.entered.notified() => {},
            result = running.as_mut() => panic!("attempt completed before the suspension: {result:?}"),
        }
    }).await.expect("attempt reaches controlled boundary");
}

#[tokio::test]
async fn session_projection_without_owner_refuses_start_and_resume_but_preserves_cancel() {
    // Cause/effect graph: C1 original slots owner is released; C2 operation is
    // execute/resume/cancel. Effects: E1 new execution refuses before inner I/O;
    // E2 cancellation reaches its original physical port without slot access.
    // Decision rows S1=C1+execute => E1; S2=C1+resume => E1; S3=C1+cancel => E2.
    let slots = Arc::new(crate::installed_session::InstalledSessions::default());
    let weak_slots = Arc::downgrade(&slots);
    let probe = Arc::new(AttemptProbe::new(false, false));
    let executor = SessionContextAttemptExecutor::new(
        probe.clone(),
        Arc::downgrade(&slots),
        "lifetime-thread",
        awaken_time::system_clock(),
    );
    drop(slots);
    assert!(
        weak_slots.upgrade().is_none(),
        "decorator has no owner allocation"
    );
    for is_resume in [false, true] {
        let error = start(&executor, is_resume).await.expect_err("owner loss");
        assert!(matches!(&error, ExecutionError::Resolution(message)
                if message == "Session context owner is unavailable"));
    }
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "zero model/tool adapter entries"
    );
    executor
        .cancel(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert_eq!(
        probe.cancels.load(Ordering::SeqCst),
        1,
        "old cancel control survives"
    );
}

#[tokio::test]
async fn session_projection_active_future_holds_original_slots_until_success_or_error() {
    // Cause/effect graph: C1 execute/resume; C2 inner succeeds/fails; C3 the last
    // external owner drops after inner entry. All four C1 x C2 rows under C3
    // retain the original slots until completion, preserve the exact result,
    // then release them. A subsequent attempt refuses without a second I/O.
    // This exercises the real entry point, not only Arc reference counts.
    for is_resume in [false, true] {
        for fails in [false, true] {
            let slots = Arc::new(crate::installed_session::InstalledSessions::default());
            let weak_slots = Arc::downgrade(&slots);
            let probe = Arc::new(AttemptProbe::new(true, fails));
            let executor = SessionContextAttemptExecutor::new(
                probe.clone(),
                Arc::downgrade(&slots),
                "lifetime-thread",
                awaken_time::system_clock(),
            );
            let mut running = start(&executor, is_resume);
            entered(&probe, &mut running).await;
            drop(slots);
            assert!(
                weak_slots.upgrade().is_some(),
                "active future owns original slots"
            );
            probe.release.add_permits(1);
            let result = running.await;
            assert_eq!(result.is_err(), fails, "original physical result preserved");
            assert!(
                weak_slots.upgrade().is_none(),
                "completed future releases original owner"
            );
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert!(
                start(&executor, is_resume).await.is_err(),
                "a later attempt cannot borrow expired ownership"
            );
            assert_eq!(
                probe.calls.load(Ordering::SeqCst),
                1,
                "no second adapter call"
            );
        }
    }
}

#[tokio::test]
async fn artifact_decorator_without_harvest_owner_refuses_effects_even_if_slots_live() {
    // Causes: C1 harvest owner is gone; C2 the canonical slots remain alive;
    // C3 execute/resume/cancel. Effects: E1 execute/resume refuse before I/O;
    // E2 cancel still forwards. A1=C1+C2+execute=>E1;
    // A2=C1+C2+resume=>E1; A3=C1+C2+cancel=>E2. C2 proves that readable state
    // cannot substitute for the independent original publication operation.
    let host =
        crate::HostDependencies::ephemeral(Arc::new(crate::NoModelConfiguredExecutor), "stub")
            .await
            .build()
            .unwrap();
    let slots = host.installed_sessions.clone();
    let weak_slots = Arc::downgrade(&slots);
    let owner = host.realizer.cleanup.artifacts.clone();
    let weak_owner = Arc::downgrade(&owner);
    let probe = Arc::new(AttemptProbe::new(false, false));
    let executor = ArtifactHarvestAttemptExecutor::new(probe.clone(), Arc::downgrade(&owner));
    drop(owner);
    drop(host);
    assert!(
        weak_slots.upgrade().is_some(),
        "read-only state can still exist"
    );
    assert!(
        weak_owner.upgrade().is_none(),
        "slots do not retain independent harvest authority"
    );
    for is_resume in [false, true] {
        let error = start(&executor, is_resume).await.expect_err("owner loss");
        assert!(matches!(&error, ExecutionError::Resolution(message)
                if message == "Artifact harvest owner is unavailable"));
    }
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "zero inner I/O without output owner"
    );
    executor
        .cancel(activation(), RuntimeRunContext::new())
        .await
        .unwrap();
    assert_eq!(probe.cancels.load(Ordering::SeqCst), 1);
    drop(slots);
    assert!(weak_slots.upgrade().is_none());
}

#[tokio::test]
async fn artifact_decorator_active_future_holds_owner_through_attempt_success_or_error() {
    // Causes: C1 execute/resume; C2 inner success/failure; C3 drop the last
    // external Host/harvest owner after inner entry. The four C1 x C2 rules
    // under C3 retain the same operation and slots, preserve the inner result,
    // then release both. No new claim or publication authority is created.
    for is_resume in [false, true] {
        for fails in [false, true] {
            let host = crate::HostDependencies::ephemeral(
                Arc::new(crate::NoModelConfiguredExecutor),
                "stub",
            )
            .await
            .build()
            .unwrap();
            let weak_slots = Arc::downgrade(&host.installed_sessions);
            let owner = host.realizer.cleanup.artifacts.clone();
            let weak_owner = Arc::downgrade(&owner);
            let probe = Arc::new(AttemptProbe::new(true, fails));
            let executor =
                ArtifactHarvestAttemptExecutor::new(probe.clone(), Arc::downgrade(&owner));
            let mut running = start(&executor, is_resume);
            entered(&probe, &mut running).await;
            drop(owner);
            drop(host);
            assert!(
                weak_owner.upgrade().is_some(),
                "physical future holds harvest operation"
            );
            assert!(
                weak_slots.upgrade().is_some(),
                "same operation retains original state"
            );
            probe.release.add_permits(1);
            let result = running.await;
            assert_eq!(
                result.is_err(),
                fails,
                "original inner failure is preserved"
            );
            if let Err(error) = result {
                assert!(error.to_string().contains("injected attempt failure"));
            }
            assert!(weak_owner.upgrade().is_none(), "no resident wrapper owner");
            assert!(weak_slots.upgrade().is_none(), "no original state cycle");
        }
    }
}

struct BlockingCapture {
    inner: Arc<dyn awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>>,
    entered: Notify,
    release: Semaphore,
}

#[cfg(test)]
#[async_trait::async_trait]
impl awaken_resource_contract::ArtifactPublisher<crate::ResourceOperationFence>
    for BlockingCapture
{
    async fn capture(
        &self,
        workspace: &str,
        session: &str,
        fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ArtifactCapture>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.entered.notify_one();
        // awaken-allow(wait): stop the real post-attempt publication path after
        // physical execution has returned; release is bounded by the test.
        self.release
            .acquire()
            .await
            .expect("capture release")
            .forget();
        self.inner.capture(workspace, session, fence).await
    }
    async fn replace_capture(
        &self,
        request: awaken_resource_contract::ReplaceArtifactCapture,
        fence: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::ReplaceArtifactCaptureOutcome,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.inner.replace_capture(request, fence).await
    }
    async fn publish(
        &self,
        publication: awaken_resource_contract::ArtifactPublication<crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.inner.publish(publication).await
    }
    async fn recover(
        &self,
        recovery: awaken_resource_contract::ArtifactRecovery<crate::ResourceOperationFence>,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.inner.recover(recovery).await
    }
}

#[tokio::test]
async fn artifact_decorator_holds_same_owner_until_real_publication_finishes() {
    // Causes: C1 execute/resume; C2 physical success/failure; C3 real capture
    // suspends after the physical result; C4 all external owners then drop.
    // The four C1 x C2 rows with C3+C4 retain the original owner until capture
    // and publication finish, commit exact output bytes on both result paths,
    // preserve the physical result, and release slots/owner after completion.
    for is_resume in [false, true] {
        for fails in [false, true] {
            let storage = tempfile::tempdir().unwrap();
            let mut dependencies = crate::HostDependencies::ephemeral(
                Arc::new(crate::NoModelConfiguredExecutor),
                "stub",
            )
            .await;
            let publisher = Arc::new(BlockingCapture {
                inner: dependencies.resources.artifact_publisher.clone(),
                entered: Notify::new(),
                release: Semaphore::new(0),
            });
            dependencies.resources.artifact_publisher = publisher.clone();
            let host = dependencies.build().unwrap();
            let spec = crate::provisioning::agent_run_sandbox_spec("lifetime-thread");
            let environment = Arc::new(
                awaken_session_environment::SandboxEnvironment::workdir(
                    awaken_sandbox_local::LocalProvider::new(storage.path())
                        .create_sandbox(&spec)
                        .await
                        .unwrap(),
                )
                .into_session_environment(),
            );
            host.install_test_resident_session_environment("lifetime-thread", environment);
            let path = storage
                .path()
                .join("lifetime-thread")
                .join(spec.outputs_path.trim_start_matches('/'))
                .join("proof.txt");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"post-attempt lifetime proof").unwrap();
            let application = host.file_application().unwrap();
            let owner = host.realizer.cleanup.artifacts.clone();
            let weak_owner = Arc::downgrade(&owner);
            let weak_slots = Arc::downgrade(&host.installed_sessions);
            let workspace = host.thread_workspace("lifetime-thread");
            let probe = Arc::new(AttemptProbe::new(false, fails));
            let executor =
                ArtifactHarvestAttemptExecutor::new(probe.clone(), Arc::downgrade(&owner));
            let mut running = start(&executor, is_resume);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::select! {
                    _ = publisher.entered.notified() => {},
                    result = running.as_mut() => panic!("capture did not suspend: {result:?}"),
                }
            })
            .await
            .expect("real harvest reaches capture");
            assert_eq!(
                probe.calls.load(Ordering::SeqCst),
                1,
                "inner attempt already completed"
            );
            drop(owner);
            drop(host);
            assert!(
                weak_owner.upgrade().is_some(),
                "harvest await retains original operation"
            );
            assert!(
                weak_slots.upgrade().is_some(),
                "harvest await retains original state"
            );
            publisher.release.add_permits(1);
            let result = running.await;
            assert_eq!(result.is_err(), fails);
            let published = application
                .list(&workspace, Some("lifetime-thread"))
                .await
                .unwrap();
            assert_eq!(
                published.len(),
                1,
                "completed output remains durable on both branches"
            );
            assert_eq!(published[0].filename, "proof.txt");
            assert_eq!(
                application
                    .bytes(&workspace, &published[0].id)
                    .await
                    .unwrap()
                    .unwrap()
                    .1,
                b"post-attempt lifetime proof",
                "the committed object retains the exact captured bytes",
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"post-attempt lifetime proof"
            );
            assert!(
                weak_owner.upgrade().is_none(),
                "finished harvest releases operation"
            );
            assert!(
                weak_slots.upgrade().is_none(),
                "finished harvest releases original state"
            );
        }
    }
}

#[tokio::test]
async fn abandoning_the_original_attempt_drops_both_decorator_owners_and_inner_future() {
    // Causes: C1 execute/resume; C2 both decorators upgrade their original owners;
    // C3 the real inner future enters and blocks; C4 all external owners drop;
    // C5 the caller drops its future before release. Rules R1/R2=C1+C2+C3+C4+C5
    // -> destroy the entered inner future exactly once, release both owners,
    // reject a later attempt before I/O, and still forward explicit cancel once.
    // Future destruction does not establish external-process physical closure.
    for is_resume in [false, true] {
        let host =
            crate::HostDependencies::ephemeral(Arc::new(crate::NoModelConfiguredExecutor), "stub")
                .await
                .build()
                .unwrap();
        let weak_slots = Arc::downgrade(&host.installed_sessions);
        let owner = host.realizer.cleanup.artifacts.clone();
        let weak_owner = Arc::downgrade(&owner);
        let probe = Arc::new(AttemptProbe::new(true, false));
        let inner: Arc<dyn RunAttemptExecutor> = Arc::new(ArtifactHarvestAttemptExecutor::new(
            probe.clone(),
            Arc::downgrade(&owner),
        ));
        let executor = SessionContextAttemptExecutor::new(
            inner,
            Arc::downgrade(&host.installed_sessions),
            "lifetime-thread",
            host.clock(),
        );
        let mut running = start(&executor, is_resume);
        entered(&probe, &mut running).await;
        assert_eq!(probe.destroyed_futures.load(Ordering::SeqCst), 0);
        drop(owner);
        drop(host);
        assert!(
            weak_owner.upgrade().is_some() && weak_slots.upgrade().is_some(),
            "the original attempt holds both owners while blocked"
        );
        drop(running);
        assert_eq!(
            probe.destroyed_futures.load(Ordering::SeqCst),
            1,
            "the entered original inner future was destroyed without release"
        );
        assert!(
            weak_owner.upgrade().is_none() && weak_slots.upgrade().is_none(),
            "no cached decorator retains either owner after abandonment"
        );
        assert!(start(&executor, is_resume).await.is_err());
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            1,
            "no second physical attempt"
        );
        executor
            .cancel(activation(), RuntimeRunContext::new())
            .await
            .unwrap();
        assert_eq!(
            probe.cancels.load(Ordering::SeqCst),
            1,
            "explicit cancellation remains a separate original control operation"
        );
    }
}
