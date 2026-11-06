use super::*;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _};
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, LocalCommitAdapter};
use awaken_runtime_contract::runs::RunError;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct OriginalSource {
    commit: Arc<SessionCommit>,
    calls: Arc<Mutex<Vec<String>>>,
    fail: bool,
}
#[async_trait::async_trait]
impl SessionCommitSource for OriginalSource {
    async fn open_for_read(&self, session: &str) -> Result<Arc<SessionCommit>, RunError> {
        self.calls.lock().unwrap().push(format!("open:{session}"));
        if self.fail {
            return Err(RunError::unavailable("original commit source refusal"));
        }
        Ok(self.commit.clone())
    }
    async fn durable_thread_exists(&self, _: &str) -> Result<bool, RunError> {
        unreachable!("raw factory open does not query or create a child partition")
    }
}
struct OriginalEffects {
    expected: Arc<SessionCommit>,
    calls: Arc<AtomicUsize>,
    reader: Mutex<Option<Arc<dyn CommittedThreadView>>>,
    fail: bool,
}
#[async_trait::async_trait]
impl SessionBoundaryEffects for OriginalEffects {
    async fn configure(
        self: Arc<Self>,
        worker: DispatchWorker<AnyDispatchStore>,
        _: &Claimed,
        commit: Arc<SessionCommit>,
        context: Option<RuntimeRunContext>,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
        assert!(
            Arc::ptr_eq(&self.expected, &commit),
            "configuration receives the exact source commit"
        );
        *self.reader.lock().unwrap() = Some(worker.committed_reader());
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(execution_error("original boundary effect refusal"));
        }
        Ok(match context {
            Some(context) => worker.with_context(context),
            None => worker,
        })
    }
}
async fn claimed() -> (DispatchBinding, Claimed) {
    let clock = awaken_time::system_clock();
    let store = Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone()));
    store
        .enqueue(
            awaken_dispatch_contract::RunDispatch::new(
                awaken_runtime_contract::RunActivation::new(
                    RunId("run".into()),
                    ThreadId("logical-child".into()),
                    crate::test_support::publication("agent"),
                    Vec::new(),
                ),
            )
            .for_session(ThreadId("physical-session".into())),
        )
        .await
        .unwrap();
    let claimed = store
        .claim("original-worker", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    (
        DispatchBinding::local(Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(
            store,
        ))),
        claimed,
    )
}
fn commit(remote: bool) -> Arc<SessionCommit> {
    Arc::new(if remote {
        SessionCommit::Remote(crate::WorkerRecoveryCommit::new())
    } else {
        SessionCommit::Local(Arc::new(LocalCommitAdapter::projected(
            awaken_store_inmem::MemoryCommitCoordinator::new(awaken_time::system_clock()),
        )))
    })
}

#[tokio::test]
async fn shared_commit_factory_retains_exact_source_reader_and_opened_lifetime() {
    // Causes: C1 authoritative Local / projected Remote source; C2 original
    // physical Session differs from the logical claimed Thread; C3 inherited
    // context supplies a foreign reader; C4 root factory/source/effect owners
    // drop while the opened boundary is retained; C5 opened boundary drops.
    // F1=C1+C2 -> one exact raw open, no effects before Worker construction.
    // F2=F1+C3 -> original commit/configuration and recovery reader survive
    // context installation, Remote never gains committed authority.
    // F3=C4 -> opened boundary retains required source/effect roles; F4=C5 ->
    // releases them. Both C1 rows exercise the real shared constructor; the
    // Remote row proves constructor/reader selection only, not local writes.
    for remote in [false, true] {
        let commit = commit(remote);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(OriginalSource {
            commit: commit.clone(),
            calls: calls.clone(),
            fail: false,
        });
        let configured = Arc::new(AtomicUsize::new(0));
        let effects = Arc::new(OriginalEffects {
            expected: commit.clone(),
            calls: configured.clone(),
            reader: Mutex::default(),
            fail: false,
        });
        let weak_source = Arc::downgrade(&source);
        let weak_effects = Arc::downgrade(&effects);
        let recovery: Arc<dyn RunRecoverySource> = commit.clone();
        let (binding, claimed) = claimed().await;
        let workers = SessionCommitWorkers::new(
            source.clone(),
            Some(binding),
            awaken_time::system_clock(),
            recovery.clone(),
            effects.clone(),
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "F1 cold construction opens nothing"
        );
        assert!(
            Arc::ptr_eq(&workers.recovery_source().unwrap(), &recovery),
            "F1 exact physical-attempt recovery source"
        );
        let opened = workers
            .open_commit(claimed.request.session_thread_id())
            .await
            .unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["open:physical-session"],
            "F1 parent partition only"
        );
        assert_eq!(
            configured.load(Ordering::SeqCst),
            0,
            "F1 late configuration"
        );
        assert_eq!(
            opened.committed() as *const dyn CommittedThreadView as *const (),
            Arc::as_ptr(&commit) as *const (),
            "F1 exact original commit"
        );
        let foreign: Arc<dyn CommittedThreadView> =
            Arc::new(SessionCommit::Remote(crate::WorkerRecoveryCommit::new()));
        let worker = opened
            .boundary_worker(
                &claimed,
                Some(RuntimeRunContext::new().with_reader(foreign.clone())),
            )
            .await
            .unwrap();
        let reader = worker.committed_reader();
        assert!(
            !Arc::ptr_eq(&reader, &foreign),
            "F2 inherited reader cannot replace commit"
        );
        let original = effects
            .reader
            .lock()
            .unwrap()
            .clone()
            .expect("F2 reader before context");
        assert!(
            Arc::ptr_eq(&reader, &original),
            "F2 exact original reader survives context configuration"
        );
        if let Some(projection) = commit.recovery_projection() {
            let expected: Arc<dyn CommittedThreadView> = projection;
            assert!(Arc::ptr_eq(&reader, &expected), "F2 exact Remote cache");
        } else {
            // new_bound keeps the original raw commit as its snapshot source
            // and exposes a claim-loaded recovery projection even locally.
            // A fresh write plus the real terminal-recovery entry proves that
            // the original source hydrates this exact retained projection.
            use awaken_agent_contract::agent::run::{EndCause, RunState};
            use awaken_agent_contract::thread::commit::{RunDisposition, commit_run};
            commit_run(
                commit.as_ref(),
                claimed.request.thread_id(),
                RunDisposition::ended(claimed.request.run_id().clone(), EndCause::NaturalEnd),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                reader.run_state(claimed.request.run_id()),
                None,
                "F2 local execution view stays unhydrated before its claim read"
            );
            let run = claimed.request.run_id().clone();
            assert_eq!(
                worker.settle_claimed_terminal(claimed).await.unwrap(),
                Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
                "F2 original physical commit hydrates terminal recovery"
            );
            assert_eq!(
                reader.run_state(&run),
                Some(RunState::Ended(EndCause::NaturalEnd)),
                "F2 the exact retained local projection observes its original source"
            );
        }
        assert_eq!(
            commit.committed_authority().is_some(),
            !remote,
            "F2 no new committed authority"
        );
        assert_eq!(
            configured.load(Ordering::SeqCst),
            1,
            "F2 one active configuration"
        );
        drop(worker);
        drop(workers);
        drop(source);
        drop(effects);
        assert!(
            weak_source.upgrade().is_some() && weak_effects.upgrade().is_some(),
            "F3 opened roles retained"
        );
        drop(opened);
        assert!(
            weak_source.upgrade().is_none() && weak_effects.upgrade().is_none(),
            "F4 opened roles released"
        );
    }
}

#[tokio::test]
async fn shared_commit_factory_preserves_source_binding_and_effect_failure_order() {
    // Cause/effect decision table: S1 original source refuses -> same diagnostic,
    // no configuration. S2 absent dispatch binding -> original binding refusal
    // before the effect. S3 bound source plus effect refusal -> same diagnostic,
    // exactly one configuration. No test introduces another recovery registry;
    // each uses the same real Worker constructor and exact held commit.
    for rule in ["S1", "S2", "S3"] {
        let commit = commit(false);
        let configured = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(OriginalSource {
            commit: commit.clone(),
            calls: calls.clone(),
            fail: rule == "S1",
        });
        let effects = Arc::new(OriginalEffects {
            expected: commit.clone(),
            calls: configured.clone(),
            reader: Mutex::default(),
            fail: true,
        });
        let (binding, claimed) = claimed().await;
        let workers = SessionCommitWorkers::new(
            source,
            (rule != "S2").then_some(binding),
            awaken_time::system_clock(),
            commit,
            effects,
        );
        let result = match workers
            .open_commit(claimed.request.session_thread_id())
            .await
        {
            Err(error) => Err(error),
            Ok(opened) => opened.boundary_worker(&claimed, None).await,
        };
        let error = result
            .err()
            .expect("the original failure must remain visible");
        let message = match rule {
            "S1" => "original commit source refusal",
            "S2" => "claimed recovery requires a dispatch binding",
            _ => "original boundary effect refusal",
        };
        assert_eq!(
            error.to_string(),
            format!("runtime execution failed: {message}"),
            "{rule} unchanged diagnostic"
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["open:physical-session"],
            "{rule} one source open"
        );
        assert_eq!(
            configured.load(Ordering::SeqCst),
            usize::from(rule == "S3"),
            "{rule} original effect order"
        );
    }
}
