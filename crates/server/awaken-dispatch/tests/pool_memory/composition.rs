//! Pool composition retains one queue owner and the claimed Run's identity.
//! Construction and both resolution paths share one entry-recording fixture.
use super::*;

/// Record the one worker_for entry and its claimed Agent identity, then resolve
/// through the existing exact Thread map. Composition failures must leave this
/// record empty; a cold Worker must receive the Run's own Agent, not the Host default.
struct RecordingResolver {
    inner: MapResolver,
    seen: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl WorkerResolver<MemoryDispatchStore> for RecordingResolver {
    fn dispatch_store(&self) -> Result<Arc<MemoryDispatchStore>, Error> {
        self.inner.dispatch_store()
    }

    async fn worker_for(
        &self,
        thread_id: &ThreadId,
        agent_id: Option<&str>,
    ) -> Result<Arc<MemWorker>, Error> {
        self.seen.lock().unwrap().push(agent_id.map(str::to_string));
        self.inner.worker_for(thread_id, agent_id).await
    }
}

/// The pool must hand a claimed run's OWN agent identity (its activation snapshot's
/// `root_agent_id`) to `worker_for`, so a cold worker opens the session bound to that
/// agent's published config instead of the host default. The harness snapshot's
/// `root_agent_id = "agent-1"`.
#[tokio::test]
async fn the_pool_forwards_a_claimed_runs_agent_to_the_resolver() {
    // A1 exact declared queue + admitted Run carrying agent-1 + resolver entry
    // -> the Run drains and that same Agent identity reaches the exact Thread map.
    // The binding tests below cover no-entry rejection; this positive must not
    // substitute a default Agent just because the queue identity is correct.
    let store = Arc::new(MemoryDispatchStore::new(system_clock()));
    let commit = Arc::new(MemoryCommitCoordinator::new(system_clock()));
    let worker = worker_over(text_runtime(), store.clone(), commit.clone());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let resolver = Arc::new(RecordingResolver {
        inner: MapResolver {
            store: store.clone(),
            workers: HashMap::from([("thread-m".to_string(), worker)]),
        },
        seen: seen.clone(),
    });
    let pool = DispatchPool::spawn(
        store.clone(),
        system_clock(),
        "pool",
        DEFAULT_LEASE_MS,
        DispatchServiceConfig::default(),
        resolver,
        2,
    )
    .expect("valid test Pool binding");

    pool.submit(activation_on("run-m", "thread-m"))
        .await
        .unwrap();
    assert!(
        wait_for(|| commit.commit_count() >= 1).await,
        "the run drained"
    );
    pool.shutdown().await;

    // The pool extracted the activation snapshot's root_agent_id ("agent-1") and
    // forwarded it, so the worker opens the session bound to that agent.
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|a| a.as_deref() == Some("agent-1")),
        "worker_for received the run's agent id; got {seen:?}"
    );
}

#[tokio::test]
async fn pool_binding_rejects_foreign_resolvers_and_empty_owners_before_effects() {
    // B1 same queue + named owner -> construct and join; B2 foreign queue;
    // B3 empty owner; B4 whitespace owner -> reject before task/claim/resolver
    // entry. The same worker_for entry record used by the Agent-identity test
    // counts resolution: every rejection leaves it empty, not merely both queues.
    for (same_queue, owner, accepted) in [
        (true, "pool-owner", true),
        (false, "pool-owner", false),
        (true, "", false),
        (true, " ", false),
    ] {
        let store = Arc::new(MemoryDispatchStore::new(system_clock()));
        let foreign = Arc::new(MemoryDispatchStore::new(system_clock()));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let declared = if same_queue {
            store.clone()
        } else {
            foreign.clone()
        };
        let worker = worker_over(
            text_runtime(),
            declared.clone(),
            Arc::new(MemoryCommitCoordinator::new(system_clock())),
        );
        let result = DispatchPool::spawn(
            store.clone(),
            system_clock(),
            owner,
            DEFAULT_LEASE_MS,
            DispatchServiceConfig::default(),
            Arc::new(RecordingResolver {
                inner: MapResolver {
                    store: declared,
                    workers: HashMap::from([("binding-thread".into(), worker)]),
                },
                seen: seen.clone(),
            }),
            1,
        );
        assert_eq!(
            result.is_ok(),
            accepted,
            "queue={same_queue}, owner={owner:?}"
        );
        if let Ok(pool) = result {
            pool.shutdown().await;
        }
        assert_eq!(seen.lock().unwrap().len(), 0);
        assert_eq!(store.dispatch_count(), 0);
        assert_eq!(foreign.dispatch_count(), 0);
    }
}

#[tokio::test]
async fn pool_validates_dynamic_workers_for_execution_and_retry_terminalization() {
    // D1 ordinary / D2 retry-exhausted claim x exact/foreign Worker queue.
    // Exact Workers use the Pool's full claim owner, even when their default
    // independent-claim owner differs. Foreign Workers must never commit or
    // settle on either queue; the original durable request is retained. The
    // ordinary and retry-exhausted defaults both enter this one recording
    // resolver. Its exact Thread map cannot hide a wrong routing coordinate.
    for exhausted in [false, true] {
        for same_queue in [false, true] {
            let clock = Arc::new(ManualClock::new(0));
            let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
            let foreign = Arc::new(MemoryDispatchStore::new(clock.clone()));
            let run = RunId("binding-run".into());
            store
                .enqueue(RunDispatch::new(activation_on(&run.0, "binding-thread")))
                .await
                .unwrap();
            if exhausted {
                store
                    .claim("predecessor", 1, &Default::default())
                    .await
                    .unwrap()
                    .unwrap();
                clock.set(2);
            }
            let commit = Arc::new(MemoryCommitCoordinator::new(system_clock()));
            let selected = if same_queue {
                store.clone()
            } else {
                foreign.clone()
            };
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let worker = worker_over(text_runtime(), selected, commit.clone());
            let pool = DispatchPool::spawn_with_wake(
                store.clone(),
                clock,
                "binding-owner",
                DEFAULT_LEASE_MS,
                DispatchServiceConfig {
                    max_attempts: if exhausted { 1 } else { 5 },
                    terminal_reconciliation_interval: None,
                    sample_dispatch_queue_depth: false,
                    ..Default::default()
                },
                Arc::new(RecordingResolver {
                    inner: MapResolver {
                        store: store.clone(),
                        workers: HashMap::from([("binding-thread".into(), worker)]),
                    },
                    seen: seen.clone(),
                }),
                1,
                Arc::new(BlackholeWake),
            )
            .expect("the declared immutable queue is correct");
            assert!(wait_for(|| !seen.lock().unwrap().is_empty()).await);
            if same_queue {
                assert!(wait_for(|| store.dispatch_count() == 0).await);
                assert!(commit.commit_count() > 0);
            }
            pool.shutdown().await;
            if !same_queue {
                assert_eq!(
                    commit.commit_count(),
                    0,
                    "foreign Worker cannot author truth"
                );
                assert_eq!(store.dispatch_count(), 1, "the original request survives");
            }
            assert_eq!(foreign.dispatch_count(), 0);
        }
    }
}
