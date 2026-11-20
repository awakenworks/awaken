use super::*;
use awaken_agent_contract::{
    agent::{
        run::{EndCause, RunState},
        thread::Id as ThreadId,
    },
    stream::{
        event::{Event, Observation},
        sink::{Error as SinkError, Sink},
    },
};
use awaken_dispatch_contract::{DispatchEnqueue as _, RunDispatch};
use awaken_dispatch_erasure::DispatchBinding;
use awaken_runtime_contract::{
    llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor},
    runtime_context::RuntimeRunContext,
};

struct Text;
#[async_trait::async_trait]
impl LlmExecutor for Text {
    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn awaken_runtime_contract::llm::DeltaSink,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        sink.on_text("native ").await;
        sink.on_text("S3").await;
        self.infer(request).await
    }
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("native S3"),
            usage: None,
            stop_reason: None,
        })
    }
}
#[derive(Default)]
struct Live(
    awaken_sync::LeafLock<Vec<Observation>>,
    awaken_sync::LeafLock<Vec<Option<awaken_lease_contract::fence::Epoch>>>,
);
#[async_trait::async_trait]
impl Sink for Live {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        self.send_observation(event.into()).await
    }
    async fn send_observation(&self, observation: Observation) -> Result<(), SinkError> {
        self.1
            .lock()
            .push(awaken_dispatch_contract::preview_claim_epoch());
        self.0.lock().push(observation);
        Ok(())
    }
}
async fn selected() -> (crate::ObjectAgentPersistence, awaken_store_s3::S3Storage) {
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_786_000_000_000));
    let engine = awaken_store_s3::test_support::storage("selected", clock);
    let pair = crate::object_agent_persistence(engine.clone(), "tenant").unwrap();
    pair.sessions()
        .create(
            "tenant",
            awaken_store_conformance::session_store::session("session", "native"),
            awaken_session_contract::IdempotencyRecord {
                key: "create".into(),
                payload_hash: "create-hash".into(),
            },
            Vec::new(),
        )
        .await
        .unwrap();
    (pair, engine)
}
fn request() -> RunDispatch {
    let mut activation = awaken_dispatch_testkit::worker_http::activation("object-runtime");
    activation.snapshot.fingerprint = activation
        .snapshot
        .resolved_spec
        .catalog_fingerprint
        .clone();
    activation.run_id = RunId("run".into());
    activation.thread_id = ThreadId("session".into());
    RunDispatch::new(activation).with_worker_codec_requirements()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_authority_delivers_native_previews_without_persisting_them() {
    let (pair, engine) = selected().await;
    let authority = pair.runtime_authority();
    let commit = authority.open_commit("session").await.unwrap();
    let queue = authority.dispatch_store();
    queue.enqueue(request()).await.unwrap();
    let runtime = Arc::new(awaken_runtime::Runtime::new().with_llm(Arc::new(Text)));
    let live = Arc::new(Live::default());
    let worker = awaken_session_run_delivery::session_dispatch_worker(
        runtime,
        DispatchBinding::coordinator(authority.clone()),
        Arc::new(awaken_session_run_delivery::SessionCommit::Local(
            commit.clone(),
        )),
        "native-worker",
    )
    .unwrap()
    .with_stream_sink(live.clone());
    let processed = worker.run_until_idle(engine.clock()).await.unwrap();
    assert_eq!(processed.len(), 1);
    assert!(matches!(
        processed[0].1,
        RunState::Ended(EndCause::NaturalEnd)
    ));
    let record = commit
        .authoritative_run(&RunId("run".into()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        record.state, processed[0].1,
        "the pre-claim read handle refreshes authoritative truth"
    );
    let session = engine.find_session("session").await.unwrap().unwrap();
    let replay = session
        .streams()
        .read_run_page(
            &awaken_store_s3::StreamReadCursor {
                key: session.key().clone(),
                run: "run".into(),
                after: 0,
            },
            4096,
            None,
        )
        .await
        .unwrap()
        .observations()
        .unwrap();
    assert!(
        !live.0.lock().is_empty(),
        "the preview still reaches its sink"
    );
    assert!(
        replay.is_empty(),
        "ordinary previews must not enter replay: {} persisted observations",
        replay.len()
    );
    assert!(
        matches!(
            session.streams().checkpoint("run").await,
            awaken_agent_contract::observation::Observed::Absent
        ),
        "terminal commit retires interrupted-stream state"
    );
    let page = authority
        .read_transcript_page(
            "session",
            &TranscriptPageRequest {
                thread_id: ThreadId("session".into()),
                view: awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
                snapshot: None,
                start: 0,
                limit: std::num::NonZeroUsize::new(100).unwrap(),
            },
        )
        .await
        .unwrap();
    assert!(
        !page.messages.is_empty(),
        "the real history query uses object ranges"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_read_wrappers_keep_parent_session_validation() {
    let (pair, _) = selected().await;
    let commit = pair
        .runtime_authority()
        .open_commit("session")
        .await
        .unwrap();
    assert!(
        commit
            .recovery_snapshot_in_session(
                &ThreadId("foreign-parent".into()),
                &ThreadId("session".into()),
                &RunId("run".into())
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_startup_keeps_the_paired_session_authority() {
    let (pair, _) = selected().await;
    let expected = pair.sessions();
    let root = tempfile::tempdir().unwrap();
    let mut config = super::tests::test_storage();
    config.storage_dir = Some(root.path().to_path_buf());
    let opened = crate::open_object_coordinator_persistence(
        &config,
        awaken_time::system_clock(),
        pair,
        false,
    )
    .await
    .unwrap();
    let actual: Arc<dyn awaken_session_application::SessionStore> = opened.agent_sessions.unwrap();
    assert!(Arc::ptr_eq(&actual, &expected));
    assert!(
        !root.path().join("dispatch.db").exists(),
        "object queue selection must not open a second SQL queue"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_remote_http_persists_checkpoint_stream_and_fences_takeover() {
    use awaken_dispatch_contract::DispatchClaims as _;
    use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
    use awaken_dispatch_testkit::worker_http::{fixed_identity, ready_workers};
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
    let engine = match std::env::var("AWAKEN_TEST_S3_ENDPOINT") {
        Ok(endpoint) => awaken_store_s3::S3Config {
            bucket: std::env::var("AWAKEN_TEST_S3_BUCKET").expect("task-owned test bucket"),
            prefix: format!("awaken-http-progress-{}", uuid::Uuid::now_v7()),
            default_scope: "tenant".into(),
            region: Some("us-east-1".into()),
            allow_http: endpoint.starts_with("http://"),
            endpoint: Some(endpoint),
        }
        .open(clock.clone())
        .unwrap(),
        Err(std::env::VarError::NotPresent) => {
            assert!(
                std::env::var_os("AWAKEN_TEST_REQUIRE_S3").is_none(),
                "live S3 is required"
            );
            awaken_store_s3::test_support::storage("http-ports", clock.clone())
        }
        Err(error) => panic!("invalid S3 endpoint: {error}"),
    };
    let pair = crate::object_agent_persistence(engine.clone(), "tenant").unwrap();
    let authority = pair.runtime_authority();
    let queue = authority.dispatch_store();
    queue.enqueue(request()).await.unwrap();
    let identity = fixed_identity("worker");
    let claim = awaken_worker_contract::RunClaim::from(
        &queue
            .claim(&identity.lease_owner(), 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let live = Arc::new(Live::default());
    let persistence =
        awaken_dispatch_erasure::runtime_persistence_transport(authority, live.clone());
    let mut dependencies = awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies(
        queue.clone(),
        ready_workers(&["worker"]),
        clock.clone(),
    );
    dependencies.stream_sink = live.clone();
    dependencies.claim_persistence =
        awaken_dispatch_http::WorkerClaimPersistence::Object(persistence);
    let router =
        dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(dependencies)));
    let checkpoint = awaken_agent_contract::stream::checkpoint::StreamCheckpoint {
        run_id: "run".into(),
        thread_id: "session".into(),
        model: "m".into(),
        partial_text: "partial".into(),
        partial_tools: vec![],
        retry_count: 1,
    };
    let (status, result) = object_post(
        &router,
        "/v1/worker/checkpoint/put",
        serde_json::json!({"claim":claim,"identity":identity,"checkpoint":checkpoint}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{result}");
    assert_eq!(result["applied"], true);
    let (status, result) = object_post(
        &router,
        "/v1/worker/checkpoint/get",
        serde_json::json!({"claim":claim,"identity":identity}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{result}");
    assert_eq!(result["checkpoint"]["partial_text"], "partial");
    let observation = Observation::assistant_delta(
        claim.run_id.clone(),
        ThreadId("session".into()),
        0,
        0,
        awaken_agent_contract::event::AgentEvent::Delta(
            awaken_agent_contract::event::Delta::TextDelta {
                delta: "remote".into(),
            },
        ),
    );
    let (status, result) = object_post(
        &router,
        "/v1/worker/dispatch/stream",
        serde_json::to_value(awaken_dispatch_contract::StreamObservationRequest {
            request: awaken_dispatch_contract::StreamEventRequest {
                claim: claim.clone(),
                identity: identity.clone(),
                event: observation.event.clone(),
            },
            assistant_response: observation.assistant_response.clone(),
            preview_sequence: observation.preview_sequence,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{result}");
    assert_eq!(result["accepted"], true);
    use awaken_dispatch_contract::{AppendRunProgress as _, ProgressBatch, ProgressError};
    let session = engine.find_session("session").await.unwrap().unwrap();
    let from_start = awaken_store_s3::StreamReadCursor {
        key: session.key().clone(),
        run: "run".into(),
        after: 0,
    };
    assert!(
        session
            .streams()
            .read_run_page(&from_start, 10, None)
            .await
            .unwrap()
            .batches
            .is_empty()
    );
    let batch = ProgressBatch {
        commit_ordinal: 0,
        response: observation.assistant_response.clone().unwrap(),
        batch_index: 0,
        observations: vec![
            observation.clone(),
            observation.clone(),
            observation.clone(),
        ],
    };
    // Publish through the actual router, then lose the successful body
    // before the remote Worker can read its receipt. The retry must use the
    // same position and bytes.
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let discard = dropped.clone();
    let wrapped = router.clone().layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let discard = discard.clone();
            async move {
                let response = next.run(request).await;
                if response.status().is_success()
                    && !discard.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    return axum::response::Response::new(axum::body::Body::empty());
                }
                response
            }
        },
    ));
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(socket, wrapped).await.unwrap() });
    let client = awaken_coordinator_client::HttpDispatchQueue::new(url, identity.clone());
    let before = engine.request_counts().put;
    let receipt = client
        .append_progress_batch(&claim, batch.clone())
        .await
        .unwrap();
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(receipt.next_batch, 1);
    let replay = session
        .streams()
        .read_run_page(&from_start, 10, None)
        .await
        .unwrap();
    assert_eq!(
        replay.batches.len(),
        1,
        "one acknowledged range after lost HTTP response"
    );
    assert_eq!(replay.observations().unwrap(), batch.observations);
    assert_eq!(
        engine.request_counts().put - before,
        2,
        "one body and one Head publication"
    );
    let mut changed = batch.clone();
    changed.observations.pop();
    assert!(matches!(
        client.append_progress_batch(&claim, changed).await,
        Err(ProgressError::Conflict)
    ));
    let mut gapped = batch.clone();
    gapped.batch_index = 2;
    assert!(matches!(
        client.append_progress_batch(&claim, gapped).await,
        Err(ProgressError::Conflict)
    ));
    assert_eq!(live.0.lock().as_slice(), &[observation]);
    assert_eq!(
        live.1.lock().as_slice(),
        &[awaken_lease_contract::fence::Epoch::from_persisted(
            claim.epoch
        )],
        "the S3 Coordinator preserves the verified preview epoch without appending progress"
    );
    let (status, result) = object_post(
        &router,
        "/v1/worker/recovery/snapshot",
        serde_json::json!({
            "claim": claim,
            "identity": identity,
            "cached_prefix": {"last_commit": 99, "thread_version": 99},
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{result}");
    use awaken_agent_contract::thread::read::recovery::LoadedRunRecovery;
    let LoadedRunRecovery::Full {
        snapshot,
        prefix: None,
    } = serde_json::from_value(result).unwrap()
    else {
        panic!("S3 recovery must return the authoritative full snapshot");
    };
    assert_eq!(snapshot.thread_id.0, "session");
    assert_eq!(snapshot.claimed_run_id.0, "run");
    clock.advance(1000);
    queue
        .claim("successor", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let (status, result) = object_post(
        &router,
        "/v1/worker/checkpoint/delete",
        serde_json::json!({"claim":claim,"identity":identity}),
    )
    .await;
    assert_ne!(status, axum::http::StatusCode::OK, "{result}");
    assert!(
        matches!(
            session.streams().checkpoint("run").await,
            awaken_agent_contract::observation::Observed::Present(_)
        ),
        "stale transport cannot delete successor-visible recovery state"
    );
    assert_eq!(
        client
            .append_progress_batch(&claim, batch.clone())
            .await
            .unwrap(),
        receipt
    );
    let late = ProgressBatch {
        batch_index: 1,
        ..batch
    };
    assert!(matches!(
        client.append_progress_batch(&claim, late).await,
        Err(ProgressError::Conflict)
    ));
    assert_eq!(
        session
            .streams()
            .read_run_page(&from_start, 10, None)
            .await
            .unwrap()
            .batches
            .len(),
        1
    );
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_remote_settlement_uses_committed_child_truth() {
    use awaken_agent_contract::thread::commit::{
        operation::{CommitOperation, CommitOperationId, commit_payload_hash},
        staged::{RunDisposition, ThreadCommit},
    };
    use awaken_dispatch_contract::{ClaimedCommitCommand, DispatchClaims as _};
    use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
    use awaken_dispatch_testkit::worker_http::{fixed_identity, ready_workers};

    let (pair, engine) = selected().await;
    let authority = pair.runtime_authority();
    let queue = authority.dispatch_store();
    let mut activation = request().activation;
    activation.thread_id = ThreadId("child".into());
    let mut child = RunDispatch::new(activation).with_worker_codec_requirements();
    child.session_thread_id = Some(ThreadId("session".into()));
    queue.enqueue(child).await.unwrap();
    let identity = fixed_identity("worker");
    let claim = awaken_worker_contract::RunClaim::from(
        &queue
            .claim(&identity.lease_owner(), 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let terminal = ThreadCommit::assemble(
        ThreadId("child".into()),
        RunDisposition::ended(claim.run_id.clone(), EndCause::NaturalEnd),
        true,
        vec![],
        vec![],
        vec![],
    );
    authority
        .clone()
        .run_commits()
        .commit_operation(ClaimedCommitCommand {
            claim: claim.clone(),
            operation: CommitOperation {
                operation_id: CommitOperationId::new(claim.run_id.clone(), 0),
                expected_thread_version: 0,
                payload_hash: commit_payload_hash(&terminal).unwrap(),
                commit: terminal,
            },
        })
        .await
        .unwrap();
    let mut deps = awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies(
        queue.clone(),
        ready_workers(&["worker"]),
        engine.clock(),
    );
    deps.recovery = authority.open_commit("session").await.unwrap();
    let router = dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(deps)));
    let body = serde_json::json!({
        "run_id": claim.run_id.0, "epoch": claim.epoch,
        "outcome": "Done", "consumed": [], "identity": identity,
    });
    let (status, result) = object_post(&router, "/v1/worker/dispatch/settle", body.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{result}");
    assert_eq!(result["settled"], true, "{result}");
    let (status, result) = object_post(&router, "/v1/worker/dispatch/settle", body).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{result}");
    assert_eq!(
        result["settled"], false,
        "a retry cannot settle a second time"
    );
}

async fn object_post(
    router: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    use tower::ServiceExt as _;
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("x-awaken-worker-id", "worker")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

struct AcpProcess;
#[async_trait::async_trait]
impl awaken_provisioning_contract::ProcessHandle for AcpProcess {
    fn id(&self) -> &str {
        "object-acp-process"
    }
    async fn wait(
        &self,
    ) -> Result<awaken_provisioning_contract::ExitStatus, awaken_provisioning_contract::SandboxError>
    {
        Ok(awaken_provisioning_contract::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }
    async fn poll(
        &self,
    ) -> Result<
        Option<awaken_provisioning_contract::ExitStatus>,
        awaken_provisioning_contract::SandboxError,
    > {
        Ok(Some(self.wait().await?))
    }
    async fn signal(
        &self,
        _: awaken_provisioning_contract::Signal,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }
}
struct AcpAgent;
#[async_trait::async_trait]
impl awaken_run_executor_acp::AgentChannelSource for AcpAgent {
    fn check_tool_narrowing(
        &self,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> Result<(), awaken_run_executor_acp::OpenError> {
        awaken_run_executor_acp::opaque_tool_narrowing(narrowing)
    }
    async fn open(
        &self,
        _: &awaken_runtime_contract::activation::RunActivation,
        _: &RuntimeRunContext,
        _: awaken_run_executor_acp::ChannelOpenDoor,
    ) -> Result<awaken_run_executor_acp::AgentSession, awaken_run_executor_acp::OpenError> {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
        let (ours, mut agent) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let mut prompt = String::new();
            BufReader::new(&mut agent)
                .read_line(&mut prompt)
                .await
                .unwrap();
            agent.write_all(b"{\"type\":\"message\",\"text\":\"ACP S3\"}\n{\"type\":\"turn_end\",\"reason\":\"natural_end\"}\n").await.unwrap();
            agent.flush().await.unwrap();
        });
        Ok(awaken_run_executor_acp::AgentSession {
            channel: Box::new(ours),
            process: Arc::new(AcpProcess),
            codec: awaken_run_executor_acp::Codec::Newline,
            workspace_cwd: None,
            mcp_session_servers: vec![],
            session_model: None,
            session_mode: None,
            tool_aliases: Default::default(),
            session_config_options: vec![],
            expected_capability: None,
            runtime_identity: None,
            instructions: Default::default(),
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_authority_delivers_acp_previews_without_persisting_them() {
    let (pair, engine) = selected().await;
    let authority = pair.runtime_authority();
    let commit = authority.open_commit("session").await.unwrap();
    let mut dispatch = request();
    dispatch.activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::parse("", "", "acp:test").unwrap(),
        );
    authority.dispatch_store().enqueue(dispatch).await.unwrap();
    let live = Arc::new(Live::default());
    let worker = awaken_session_run_delivery::session_dispatch_worker(
        Arc::new(awaken_runtime::Runtime::new()),
        DispatchBinding::coordinator(authority.clone()),
        Arc::new(awaken_session_run_delivery::SessionCommit::Local(
            commit.clone(),
        )),
        "acp-worker",
    )
    .unwrap()
    .with_stream_sink(live.clone());
    worker.install_attempt_executor(Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(
        Arc::new(AcpAgent),
    )));
    let results = worker.run_until_idle(engine.clock()).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0].1, RunState::Ended(EndCause::NaturalEnd)),
        "{results:?}"
    );
    let session = engine.find_session("session").await.unwrap().unwrap();
    let replay = session
        .streams()
        .read_run_page(
            &awaken_store_s3::StreamReadCursor {
                key: session.key().clone(),
                run: "run".into(),
                after: 0,
            },
            4096,
            None,
        )
        .await
        .unwrap()
        .observations()
        .unwrap();
    assert!(
        !live.0.lock().is_empty(),
        "the ACP preview still reaches its sink"
    );
    assert!(
        replay.is_empty(),
        "ordinary ACP previews must not enter replay: {} persisted observations",
        replay.len()
    );
    assert_eq!(
        commit
            .authoritative_committed_messages(&ThreadId("session".into()))
            .await
            .unwrap()
            .last()
            .unwrap()
            .text_content(),
        "ACP S3"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_object_session_assignment_consumes_the_dispatch_condition() {
    use awaken_dispatch_contract::{
        ClaimAuthority, DispatchClaims as _, DispatchCommitAuthority as _,
    };
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1000));
    let engine = awaken_store_s3::test_support::storage("assignment", clock.clone());
    let pair = crate::object_agent_persistence(engine, "tenant").unwrap();
    let repository = pair.sessions();
    let mut root = awaken_protocol_managed_testkit::sample_persisted("session");
    root.resources = awaken_session_contract::SessionResourceState::from_active(Default::default());
    root.mcp = Default::default();
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut root.baseline
    else {
        panic!("frozen baseline")
    };
    baseline.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    awaken_protocol_managed_testkit::create_session_fixture(repository.as_ref(), "tenant", root)
        .await;
    let queue = pair.runtime_authority().dispatch_store();
    queue.enqueue(request()).await.unwrap();
    let old = awaken_worker_contract::RunClaim::from(
        &queue
            .claim("old-worker", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    let ClaimAuthority::Conditional(old_authority) =
        queue.acquire_commit_authority(&old).await.unwrap().unwrap()
    else {
        panic!("conditional authority")
    };
    let runtime = Arc::new(awaken_protocol_managed_testkit::RehydrateFake::default());
    let probe = Arc::new(awaken_session_application::ExecutionObservationProbe::default());
    let application = awaken_session_application::SessionApplicationBuilder::new(
        awaken_session_execution_contract::SessionExecutionRoles::colocated(
            runtime.clone(),
            runtime,
        ),
        pair.session_persistence(clock.clone()),
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
        awaken_session_application::SessionExecutionAdmission::new(
            queue.clone(),
            probe.clone(),
            probe,
        ),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .unwrap();
    let command = |owner: &str| awaken_session_execution_contract::BeginSessionRealization {
        session_id: "session".into(),
        expected_resources: None,
        target: awaken_session_execution_contract::SessionRealizationTarget {
            owner: owner.into(),
            runtime_incarnation: owner.into(),
            lease_expires_at_unix_ms: 20_000,
            reassign_existing_lease: true,
        },
    };
    // The invariant under test is the assignment CAS, before physical work is
    // driven. The fixture deliberately supplies no real Worker/sandbox execution.
    let _ = application
        .realizations()
        .begin_session_realization_conditionally(
            command("old-worker"),
            Some(&old_authority.condition),
        )
        .await;
    let assigned = repository
        .get(&"session".into())
        .await
        .unwrap()
        .realization
        .unwrap();
    assert_eq!(assigned.runtime_incarnation, "old-worker");
    clock.advance(1000);
    let new = awaken_worker_contract::RunClaim::from(
        &queue
            .claim("new-worker", 1000, &Default::default())
            .await
            .unwrap()
            .unwrap()
            .lease,
    );
    use awaken_session_execution_contract::ConditionalSessionRealizationRenewal as _;
    let stale_renewal = application
        .realizations()
        .renew_session_realization_conditionally(
            awaken_session_execution_contract::RenewSessionRealization {
                session_id: "session".into(),
                asserted_lease: assigned.clone(),
                requested_expires_at_unix_ms: assigned.expires_at_unix_ms + 500,
            },
            &old_authority.condition,
        )
        .await;
    assert!(stale_renewal.is_err());
    let stale = application
        .realizations()
        .begin_session_realization_conditionally(
            command("old-worker"),
            Some(&old_authority.condition),
        )
        .await;
    assert!(stale.is_err());
    assert_eq!(
        repository
            .get(&"session".into())
            .await
            .unwrap()
            .realization
            .as_ref(),
        Some(&assigned)
    );
    let ClaimAuthority::Conditional(current) =
        queue.acquire_commit_authority(&new).await.unwrap().unwrap()
    else {
        panic!("conditional authority")
    };
    let _ = application
        .realizations()
        .begin_session_realization_conditionally(command("new-worker"), Some(&current.condition))
        .await;
    let replaced = repository
        .get(&"session".into())
        .await
        .unwrap()
        .realization
        .unwrap();
    assert_eq!(replaced.runtime_incarnation, "new-worker");
    assert!(replaced.epoch > assigned.epoch);
}

struct CrashBetweenRetries {
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for CrashBetweenRetries {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        panic!("the recovery fixture must use the real streaming path");
    }
    async fn infer_streaming(
        &self,
        _: ChatRequest,
        sink: &dyn awaken_runtime_contract::llm::DeltaSink,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        assert_eq!(
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            0,
            "the parent must interrupt the worker during the retry backoff"
        );
        sink.on_text("prefix ").await;
        Err(awaken_runtime_contract::llm::Error::RateLimited {
            message: "fixture provider confirmed termination".into(),
            retry_after: Some(std::time::Duration::from_secs(5)),
        }
        .with_execution_evidence(
            awaken_runtime_contract::model_operation::ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "failed",
                ))
                .unwrap(),
            ),
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicit task-owned MinIO endpoint, bucket and credentials"]
async fn live_object_cold_restart_recovers_session_dispatch_runtime_and_checkpoint_together() {
    use awaken_agent_contract::observation::Observed;
    use awaken_dispatch_contract::{
        AttemptAdmission, DispatchCommitAuthority as _, DispatchExecution as _,
    };
    let old_clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000));
    let config = awaken_store_s3::S3Config {
        bucket: std::env::var("AWAKEN_TEST_S3_BUCKET").expect("task-owned test bucket"),
        prefix: format!("awaken-cross-domain-{}", uuid::Uuid::now_v7()),
        default_scope: "tenant".into(),
        region: Some(std::env::var("AWAKEN_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into())),
        endpoint: Some(std::env::var("AWAKEN_TEST_S3_ENDPOINT").expect("live MinIO endpoint")),
        allow_http: true,
    };
    let engine = config.open(old_clock.clone()).unwrap();
    let pair = crate::object_agent_persistence(engine.clone(), "tenant").unwrap();
    pair.sessions()
        .create(
            "tenant",
            awaken_store_conformance::session_store::session("session", "native"),
            awaken_session_contract::IdempotencyRecord {
                key: "create".into(),
                payload_hash: "create-hash".into(),
            },
            vec![],
        )
        .await
        .unwrap();
    let authority = pair.runtime_authority();
    let queue = authority.dispatch_store();
    queue.enqueue(request()).await.unwrap();
    let commit = authority.open_commit("session").await.unwrap();
    let partial = Arc::new(CrashBetweenRetries {
        calls: Default::default(),
    });
    let old_live = Arc::new(Live::default());
    let worker = awaken_session_run_delivery::session_dispatch_worker(
        Arc::new(awaken_runtime::Runtime::new().with_llm(partial.clone())),
        DispatchBinding::coordinator(authority.clone()),
        Arc::new(awaken_session_run_delivery::SessionCommit::Local(commit)),
        "old-native-worker",
    )
    .unwrap()
    // Remote expiry is driven by ManualClock below. Keep the real monotonic
    // renewal proof at an ordinary TTL so service jitter cannot move this
    // checkpoint-crash drill to a pre-provider lease-safety stop.
    .with_lease_ms(30_000)
    .with_context(RuntimeRunContext::new().with_stream_sink(old_live.clone()));
    let claimed = worker
        .claim_run(&RunId("run".into()))
        .await
        .unwrap()
        .unwrap();
    let old_claim = awaken_worker_contract::RunClaim::from(&claimed.lease);
    let source_clock = old_clock.clone();
    let mut running =
        tokio::spawn(async move { worker.drive_claimed(claimed, source_clock).await });
    // claim_run reserves ownership; drive_claimed must activate execution before
    // a checkpoint read is authorized. Observe the provider boundary first.
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while partial.calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            if running.is_finished() {
                let outcome = (&mut running).await;
                panic!("Worker ended before provider invocation: {outcome:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Runtime must enter the first provider invocation");
    let persistence =
        awaken_dispatch_erasure::runtime_persistence_transport(authority, old_live.clone());
    let checkpoint = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Some(value) = persistence
                .checkpoints
                .load_stream_checkpoint(&old_claim)
                .await
                .unwrap()
                && value.partial_text == "prefix "
            {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the real Runtime must persist its confirmed partial before retrying");
    assert_eq!(partial.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());

    // The first provider invocation returned a terminal receipt, and the abort
    // happened in retry backoff. No model, tool, Sandbox, or child future remains.
    let new_clock = Arc::new(awaken_lease_testkit::ManualClock::new(31_001));
    let cold_engine = config.open(new_clock.clone()).unwrap();
    let cold_pair = crate::object_agent_persistence(cold_engine.clone(), "tenant").unwrap();
    assert_eq!(
        cold_pair.sessions().owner(&"session".into()).await.unwrap(),
        "tenant"
    );
    cold_pair.sessions().get(&"session".into()).await.unwrap();
    let cold_authority = cold_pair.runtime_authority();
    let cold_queue = cold_authority.dispatch_store();
    let cold_commit = cold_authority.open_commit("session").await.unwrap();
    let new_live = Arc::new(Live::default());
    let successor = awaken_session_run_delivery::session_dispatch_worker(
        Arc::new(awaken_runtime::Runtime::new().with_llm(Arc::new(Text))),
        DispatchBinding::coordinator(cold_authority.clone()),
        Arc::new(awaken_session_run_delivery::SessionCommit::Local(
            cold_commit.clone(),
        )),
        "new-native-worker",
    )
    .unwrap()
    .with_context(RuntimeRunContext::new().with_stream_sink(new_live.clone()));
    let claimed = successor
        .claim_run(&RunId("run".into()))
        .await
        .unwrap()
        .unwrap();
    let new_claim = awaken_worker_contract::RunClaim::from(&claimed.lease);
    assert!(new_claim.epoch > old_claim.epoch);
    assert!(claimed.recovered);
    assert_eq!(
        cold_queue.begin_attempt(&new_claim, None).await.unwrap(),
        AttemptAdmission::Blocked
    );
    assert!(
        cold_queue
            .acquire_commit_authority(&old_claim)
            .await
            .unwrap()
            .is_none()
    );
    let current = cold_engine.canonical_checkpoints(new_claim.clone(), cold_queue.clone());
    use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore as _;
    assert!(
        matches!(current.get("run").await, Observed::Present(ref value) if value.partial_text=="prefix ")
    );
    assert!(
        persistence
            .checkpoints
            .put_stream_checkpoint(&old_claim, checkpoint)
            .await
            .is_err()
    );
    assert!(
        matches!(current.get("run").await, Observed::Present(ref value) if value.partial_text=="prefix ")
    );
    cold_queue.finish_attempt(&old_claim, None).await.unwrap();

    let outcome = successor
        .drive_claimed(claimed, new_clock)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome.1, RunState::Ended(EndCause::NaturalEnd)));
    let session = cold_engine.find_session("session").await.unwrap().unwrap();
    assert!(matches!(
        session.streams().checkpoint("run").await,
        awaken_agent_contract::observation::Observed::Absent
    ));
    let page = cold_authority
        .read_transcript_page(
            "session",
            &TranscriptPageRequest {
                thread_id: ThreadId("session".into()),
                view: awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
                snapshot: None,
                start: 0,
                limit: std::num::NonZeroUsize::new(100).unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.messages
            .iter()
            .filter(|message| message.text_content() == "prefix native S3")
            .count(),
        1,
        "the confirmed checkpoint prefix is stitched exactly once into the committed answer: {:?}",
        page.messages
            .iter()
            .map(|message| message.text_content())
            .collect::<Vec<_>>()
    );
    let replay = session
        .streams()
        .read_run_page(
            &awaken_store_s3::StreamReadCursor {
                key: session.key().clone(),
                run: "run".into(),
                after: 0,
            },
            4096,
            None,
        )
        .await
        .unwrap();
    assert!(
        replay.batches.is_empty(),
        "checkpoint recovery never depends on preview replay"
    );
    assert!(!old_live.0.lock().is_empty());
    assert!(!new_live.0.lock().is_empty());
    assert!(
        cold_commit
            .authoritative_run(&RunId("run".into()))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        cold_queue
            .acquire_commit_authority(&old_claim)
            .await
            .unwrap()
            .is_none()
    );
    session.collect_garbage().await.unwrap();
}
