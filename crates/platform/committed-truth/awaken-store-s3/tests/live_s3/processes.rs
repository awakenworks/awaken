// @formal evidence store_history.s3_publication_refines_committed_history
use super::{live, proxy, support};
use awaken_agent_contract::{
    agent::{run::Id as RunId, thread::Id as ThreadId},
    event::{AgentEvent, Delta},
    stream::{
        checkpoint::{StreamCheckpoint, StreamCheckpointStore},
        event::{Event, Observation},
        sink::Error as SinkError,
    },
    thread::read::recovery::RunRecoverySource,
};
use awaken_dispatch::ObjectDispatchPartition;
use awaken_dispatch_contract::{
    AttemptAdmission, ClaimedCommitCommand, ClaimedRunCommit, ClaimedStreamPublisher,
    DispatchClaims as _, DispatchEnqueue as _, DispatchExecution as _,
};
use awaken_runtime_contract::runs::RunError;
use awaken_store_s3::{Limits, S3Storage};
use awaken_worker_contract::RunClaim;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
};

#[derive(Serialize, Deserialize)]
struct Input {
    role: String,
    endpoint: String,
    prefix: String,
    now: u64,
    owner: String,
    ready: PathBuf,
    result: PathBuf,
    start: Option<PathBuf>,
    command: Option<ClaimedCommitCommand>,
}
struct Peer {
    child: Option<Child>,
    root: tempfile::TempDir,
    ready: PathBuf,
    result: PathBuf,
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Peer {
    fn spawn(
        role: &str,
        endpoint: &str,
        prefix: &str,
        now: u64,
        owner: &str,
        start: Option<PathBuf>,
        command: Option<ClaimedCommitCommand>,
    ) -> Self {
        let root = tempfile::Builder::new()
            .prefix("s3-peer-")
            .tempdir()
            .unwrap();
        let ready = root.path().join("ready.json");
        let result = root.path().join("result.json");
        let input = Input {
            role: role.into(),
            endpoint: endpoint.into(),
            prefix: prefix.into(),
            now,
            owner: owner.into(),
            ready: ready.clone(),
            result: result.clone(),
            start,
            command,
        };
        let input_path = root.path().join("input.json");
        std::fs::write(&input_path, serde_json::to_vec(&input).unwrap()).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "processes::peer", "--ignored", "--nocapture"])
            .env("AWAKEN_S3_PEER_INPUT", input_path)
            .stdout(Stdio::from(
                std::fs::File::create(root.path().join("stdout.log")).unwrap(),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(root.path().join("stderr.log")).unwrap(),
            ))
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            root,
            ready,
            result,
        }
    }
    async fn wait_file(&mut self, path: PathBuf) -> serde_json::Value {
        // awaken-allow(wait): the owned child writes this barrier after the exact operation under test.
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            loop {
                if path.exists() {
                    return serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                }
                let status = self.child.as_mut().unwrap().try_wait().unwrap();
                assert!(
                    status.is_none(),
                    "peer exited before barrier; diagnostics: {}",
                    self.root.path().display()
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("peer barrier deadline")
    }
    async fn ready(&mut self) -> serde_json::Value {
        self.wait_file(self.ready.clone()).await
    }
    async fn result(&mut self) -> serde_json::Value {
        self.wait_file(self.result.clone()).await
    }
    fn kill_wait(&mut self) {
        let child = self.child.as_mut().unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "the test must interrupt a live process"
        );
        child.kill().unwrap();
        let _terminated = child.wait().unwrap();
        self.child = None;
    }
    fn finish(&mut self) {
        assert!(
            self.child.take().unwrap().wait().unwrap().success(),
            "peer failed"
        );
    }
}
fn record(path: &Path, value: &impl Serialize) {
    let pending = path.with_extension("pending");
    let mut file = std::fs::File::create(&pending).unwrap();
    use std::io::Write as _;
    file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    file.sync_all().unwrap();
    std::fs::rename(pending, path).unwrap();
}
fn dispatch(
    engine: &S3Storage,
    clock: Arc<awaken_lease_testkit::ManualClock>,
) -> Arc<ObjectDispatchPartition> {
    Arc::new(ObjectDispatchPartition::new(
        Arc::new(engine.component_store("dispatch").unwrap()),
        "tenant".into(),
        "session".into(),
        clock,
    ))
}
#[derive(Default)]
struct Live;
#[async_trait::async_trait]
impl ClaimedStreamPublisher for Live {
    async fn publish(&self, _: &RunClaim, _: Event) -> Result<(), SinkError> {
        Ok(())
    }
}
fn checkpoint() -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: "run".into(),
        thread_id: "session".into(),
        model: "model".into(),
        partial_text: "interrupted next step".into(),
        partial_tools: vec![],
        retry_count: 1,
    }
}
fn command(claim: RunClaim, ordinal: u64, text: &str) -> ClaimedCommitCommand {
    ClaimedCommitCommand {
        claim,
        operation: support::operation("run", "session", ordinal, ordinal, text),
    }
}

/// Internal subprocess entry point. The live suite explicitly excludes this test.
#[test]
#[ignore = "internal peer; only the parent live-provider tests supply its configuration"]
fn peer() {
    let input: Input = serde_json::from_slice(
        &std::fs::read(
            std::env::var_os("AWAKEN_S3_PEER_INPUT").expect("internal peer configuration"),
        )
        .unwrap(),
    )
    .unwrap();
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
        let (clock, engine, session) = live::open(live::connect(&input.endpoint), &input.prefix,
            input.now, Limits { max_cas_attempts: 128, ..Default::default() });
        let queue = dispatch(&engine, clock);
        match input.role.as_str() {
            "canonical-commit" => {
                record(&input.ready, &serde_json::json!({"ready": true}));
                let receipt = <_ as ClaimedRunCommit<RunError>>::commit_operation(
                    &engine.canonical_commits(queue), input.command.unwrap(),
                ).await.unwrap();
                record(&input.result, &receipt);
            }
            "crash-writer" => {
                let claim = RunClaim::from(&queue.claim(&input.owner, 100, &Default::default())
                    .await.unwrap().unwrap().lease);
                assert_eq!(queue.begin_attempt(&claim, None).await.unwrap(), AttemptAdmission::Applied);
                let receipt = <_ as ClaimedRunCommit<RunError>>::commit_operation(
                    &engine.canonical_commits(queue.clone()), command(claim.clone(), 0, "committed before crash"),
                ).await.unwrap();
                engine.canonical_checkpoints(claim.clone(), queue.clone()).put(checkpoint()).await.unwrap();
                session.streams().canonical_progress_publisher(queue, Arc::new(Live))
                    .persist_batch(&claim, vec![Observation::assistant_delta(
                        RunId("run".into()), ThreadId("session".into()), 1, 0,
                        AgentEvent::Delta(Delta::TextDelta { delta: "partial before crash".into() }),
                    )]).await.unwrap();
                // All storage calls are complete. This peer has no remote model, tool or Sandbox.
                record(&input.ready, &serde_json::json!({"claim": claim, "receipt": receipt}));
                std::future::pending::<()>().await;
            }
            "takeover" => {
                record(&input.ready, &serde_json::json!({"ready": true}));
                let start = input.start.unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(120), async {
                    while !start.exists() { tokio::time::sleep(std::time::Duration::from_millis(5)).await; }
                }).await.unwrap();
                let claimed = queue.claim(&input.owner, 100_000, &Default::default()).await.unwrap();
                record(&input.result, &serde_json::json!({"claim": claimed.map(|item| RunClaim::from(&item.lease))}));
            }
            other => panic!("unknown test peer role {other}"),
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn process_death_after_real_put_reuses_postgres_lost_acknowledgement_contract() {
    let fixture = live::Fixture::new(Limits::default()).await;
    let queue = dispatch(&fixture.engine, fixture.clock.clone());
    queue
        .enqueue(support::dispatch("run", "session", "session"))
        .await
        .unwrap();
    let claim = RunClaim::from(
        &queue
            .claim("predecessor", 1_000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let writer = fixture.engine.canonical_commits(queue.clone());
    let proxy = proxy::Proxy::new(live::endpoint()).await;
    let trace = awaken_store_conformance::claimed_commit::lost_acknowledgement_and_takeover(
        &writer,
        &fixture.session,
        command(claim, 0, "one publication"),
        async |operation| {
            proxy.arm(proxy::HOLD_AFTER);
            let mut peer = Peer::spawn(
                "canonical-commit",
                &proxy.endpoint,
                &fixture.prefix,
                1_000,
                "predecessor",
                None,
                Some(operation),
            );
            peer.ready().await;
            proxy.wait_persisted().await;
            assert_eq!(fixture.session.head().await.unwrap().commit_sequence, 1);
            peer.kill_wait();
            proxy.fault.release.notify_one();
            assert_eq!(
                proxy.fault.after.load(std::sync::atomic::Ordering::SeqCst),
                1
            );
        },
        async |_| {
            fixture.clock.advance(1_000);
            RunClaim::from(
                &queue
                    .claim("successor", 100_000, &Default::default())
                    .await
                    .unwrap()
                    .unwrap()
                    .lease,
            )
        },
    )
    .await;
    awaken_reliability_testkit::write_observation_trace(
        "s3-wire-claimed-store",
        "claimed-store",
        serde_json::json!([{"rule": "lost-acknowledgement-takeover", "trace": trace}]),
    );
    drop(proxy);
    fixture.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned S3 endpoint, bucket and credentials"]
async fn independent_process_takeover_preserves_the_unfinished_physical_slot() {
    let fixture = live::Fixture::new(Limits::default()).await;
    let queue = dispatch(&fixture.engine, fixture.clock.clone());
    queue
        .enqueue(support::dispatch("run", "session", "session"))
        .await
        .unwrap();
    let mut old = Peer::spawn(
        "crash-writer",
        &live::endpoint(),
        &fixture.prefix,
        1_000,
        "old",
        None,
        None,
    );
    let ready = old.ready().await;
    let stale: RunClaim = serde_json::from_value(ready["claim"].clone()).unwrap();
    old.kill_wait();
    let barrier = tempfile::Builder::new()
        .prefix("s3-start-")
        .tempdir()
        .unwrap();
    let start = barrier.path().join("start");
    let mut contenders = (0..2)
        .map(|index| {
            Peer::spawn(
                "takeover",
                &live::endpoint(),
                &fixture.prefix,
                1_100,
                &format!("replacement-{index}"),
                Some(start.clone()),
                None,
            )
        })
        .collect::<Vec<_>>();
    for peer in &mut contenders {
        peer.ready().await;
    }
    std::fs::write(&start, b"go").unwrap();
    let mut winners = Vec::new();
    for peer in &mut contenders {
        let result = peer.result().await;
        if !result["claim"].is_null() {
            winners.push(serde_json::from_value::<RunClaim>(result["claim"].clone()).unwrap());
        }
        peer.finish();
    }
    assert_eq!(
        winners.len(),
        1,
        "independent processes cannot own the same live claim"
    );
    let current = winners.pop().unwrap();
    assert!(current.epoch > stale.epoch);
    let (clock, cold, session) = fixture.cold(1_100, Limits::default());
    let recovered = dispatch(&cold, clock);
    // Lease expiry and process exit do not silently retire the durable execution slot.
    assert_eq!(
        recovered.begin_attempt(&current, None).await.unwrap(),
        AttemptAdmission::Blocked
    );
    // This local-only fixture has no nested or remote executor; the joined peer is completely terminated.
    recovered.finish_attempt(&stale, None).await.unwrap();
    assert_eq!(
        recovered.begin_attempt(&current, None).await.unwrap(),
        AttemptAdmission::Applied
    );
    assert!(matches!(
        cold.canonical_checkpoints(current.clone(), recovered.clone())
            .get("run")
            .await,
        awaken_agent_contract::observation::Observed::Present(_)
    ));
    let writer = cold.canonical_commits(recovered.clone());
    let mut stale_command = command(stale.clone(), 1, "successor");
    let current_command = command(current.clone(), 1, "successor");
    awaken_store_conformance::claimed_commit::stale_claim_cannot_publish(
        &writer,
        &session,
        stale_command.clone(),
        current_command,
    )
    .await;
    assert!(matches!(
        session.streams().checkpoint("run").await,
        awaken_agent_contract::observation::Observed::Absent
    ));
    stale_command.operation = support::operation("run", "session", 0, 0, "committed before crash");
    let replay = <_ as ClaimedRunCommit<RunError>>::commit_operation(
        &writer,
        ClaimedCommitCommand {
            claim: current.clone(),
            operation: stale_command.operation,
        },
    )
    .await
    .unwrap();
    assert!(replay.duplicate);
    assert_eq!(
        replay.commit_sequence,
        ready["receipt"]["commit_sequence"].as_u64().unwrap()
    );
    let snapshot = session
        .recovery_snapshot(&ThreadId("session".into()), &RunId("run".into()))
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>(),
        vec!["committed before crash", "successor"]
    );
    recovered.finish_attempt(&current, None).await.unwrap();
    fixture.cleanup().await;
}
