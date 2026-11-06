//! A registered Worker's Task attention and Task tenure reads through the
//! actual client, HTTP routes and Worker registry check, against a recording
//! Session application and scripted committed reads.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::{Action, Command, Key, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::background_task::background_task_state_key;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_coordinator_client::{WorkerControlClient, WorkerTaskControlClient};
use awaken_dispatch_http::worker_task_control_router;
use awaken_dispatch_testkit::worker_http::ready_worker;
use awaken_lease_testkit::ManualClock;
use awaken_runtime_contract::runs::{RunError, RunErrorKind};

use awaken_session_application_contract::{
    PrivateTaskInput, RegisteredTaskControl, SessionRunBackgroundApplication, TaskAttention,
    TaskAttentionAck, TaskReadPartition, TaskTenureRead, registered_task_holder,
};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

/// The Coordinator's Session application, recording what it admits.
struct RecordingApplication {
    unavailable_first: AtomicUsize,
    refuse: bool,
    attentions: Mutex<Vec<(TaskAttention, bool)>>,
}

impl RecordingApplication {
    fn new(unavailable_first: usize, refuse: bool) -> Arc<Self> {
        Arc::new(Self {
            unavailable_first: AtomicUsize::new(unavailable_first),
            refuse,
            attentions: Mutex::new(Vec::new()),
        })
    }

    fn admitted(&self) -> Vec<TaskAttention> {
        self.attentions
            .lock()
            .unwrap()
            .iter()
            .map(|(attention, _)| attention.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl SessionRunBackgroundApplication for RecordingApplication {
    async fn submit_session_run_background(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        _traceparent: Option<String>,
    ) -> Result<RunId, RunError> {
        panic!("Task attention never submits an ordinary background Run")
    }

    async fn task_attention_receipt(
        &self,
        attention: TaskAttention,
    ) -> Result<Option<TaskAttentionAck>, RunError> {
        for (original, acknowledged) in self.attentions.lock().unwrap().iter() {
            if *acknowledged && original.operation_id == attention.operation_id {
                if original != &attention {
                    return Err(RunError::conflict("Task attention identity was reused"));
                }
                return Ok(Some(TaskAttentionAck {
                    run_id: RunId(format!("attention-run-{}", attention.operation_id)),
                }));
            }
        }
        Ok(None)
    }

    async fn submit_task_attention(
        &self,
        attention: TaskAttention,
    ) -> Result<TaskAttentionAck, RunError> {
        let index = {
            let mut calls = self.attentions.lock().unwrap();
            let index = calls.len();
            calls.push((attention.clone(), false));
            index
        };
        if self.refuse {
            return Err(RunError::bad_request("the Session is gone"));
        }
        if self
            .unavailable_first
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(RunError::unavailable("activation store is busy"));
        }
        self.attentions.lock().unwrap()[index].1 = true;
        // The Run id is a function of the operation id, as in the real
        // application (row K2).
        Ok(TaskAttentionAck {
            run_id: RunId(format!("attention-run-{}", attention.operation_id)),
        })
    }
}

/// The Coordinator's committed reads: one Thread, committed in one Session
/// partition, whose State holds the given Task slots and whose origin Run is
/// committed there; or an unavailable store. Like a partitioned store, a read
/// of any other partition answers an empty Thread.
struct ScriptedCommitted {
    slots: Option<Vec<(String, serde_json::Value)>>,
    session_thread: &'static str,
    thread: &'static str,
}

#[async_trait::async_trait]
impl RunRecoverySource for ScriptedCommitted {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.recovery_snapshot_in_session(thread_id, thread_id, claimed_run_id)
            .await
    }

    async fn recovery_snapshot_in_session(
        &self,
        session_thread_id: &ThreadId,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let slots = self
            .slots
            .as_ref()
            .ok_or_else(|| RecoveryError::Rejected("committed store unavailable".into()))?;
        let committed_here =
            session_thread_id.0 == self.session_thread && thread_id.0 == self.thread;
        Ok(RunRecoverySnapshot {
            thread_id: thread_id.clone(),
            claimed_run_id: claimed_run_id.clone(),
            runs: if committed_here {
                vec![RunRecord {
                    id: claimed_run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: RunState::Running,
                }]
            } else {
                Vec::new()
            },
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state: if committed_here {
                slots
                    .iter()
                    .map(|(task, slot)| Command {
                        key: Key(background_task_state_key(task)),
                        scope: Scope::Thread,
                        merge: MergePolicy::Exclusive,
                        run_id: None,
                        action: Action::Set(slot.clone()),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: u64::from(committed_here),
            store_cursor: 0,
            next_commit_ordinal: 0,
        })
    }
}

/// A root Thread: its own Session partition.
fn root_committed(slots: Option<Vec<(String, serde_json::Value)>>) -> Arc<ScriptedCommitted> {
    Arc::new(ScriptedCommitted {
        slots,
        session_thread: "attention-thread",
        thread: "attention-thread",
    })
}

fn no_committed_reads() -> Arc<ScriptedCommitted> {
    // Attention now proves actual held Task coordinates even when its input is
    // empty. This existing fixture is the root Task held by this incarnation.
    let holder = registered_task_holder("attention-worker", "attention-worker-boot");
    root_committed(Some(vec![(
        "task-1".into(),
        slot(
            "task-1",
            serde_json::json!({"state":"running", "attempt": attempt(&holder)}),
        ),
    )]))
}

/// Abort-on-drop also closes the server on assertion failure.
struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(
    application: Arc<RecordingApplication>,
    committed: Arc<ScriptedCommitted>,
) -> (Server, String, WorkerTaskControlClient) {
    let (directory, identity) = ready_worker("attention-worker").await;
    serve_with_worker(application, committed, directory, identity).await
}

async fn serve_with_worker(
    application: Arc<RecordingApplication>,
    committed: Arc<ScriptedCommitted>,
    directory: Arc<dyn awaken_dispatch::WorkerDirectory>,
    identity: awaken_worker_contract::WorkerIdentity,
) -> (Server, String, WorkerTaskControlClient) {
    let router = worker_task_control_router(
        application,
        committed,
        directory,
        Arc::new(HeaderWorkerAuthenticator),
        Arc::new(ManualClock::new(0)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    let base_url = format!("http://{address}");
    let control = WorkerControlClient::new(
        WorkerUpstream::new(base_url.clone()).with_worker_identity(identity.clone()),
    );
    (
        server,
        base_url,
        WorkerTaskControlClient::new(control, identity),
    )
}

fn attention(holder: &str) -> TaskAttention {
    TaskAttention {
        operation_id: "background-task-attention-fixed".into(),
        tenure: tenure_read("task-1", holder),
        message: Message::text(
            MessageId("background-task-attention-message-fixed".into()),
            Role::System,
            "Background task task-1 has produced a terminal completion candidate.",
        ),
        input: Some(
            PrivateTaskInput::new(vec![serde_json::json!({"version": 1, "task_id":"task-1", "holder":holder, "epoch":3, "observation":{"kind":"ended", "end":{"outcome":"cancelled"}}})])
                .expect("one observation"),
        ),
    }
}

/// Decision table through the real transport:
/// | Rule | cause | effect |
/// |---|---|---|
/// | A2+K1 | this incarnation's holder; the application admits | ACK with the admitted Run; the application saw the exact attention |
/// | K5+K2 | the application is unavailable once | `Unavailable`; the identical resend returns the same Run |
/// | A3+K4 | another incarnation's holder | `BadRequest`; the application is never called |
/// | K4 | the application refuses definitively | `BadRequest` |
#[tokio::test]
async fn a_registered_worker_delivers_only_its_own_task_attention_and_retries_to_the_same_run() {
    // A2+K1.
    let application = RecordingApplication::new(0, false);
    let (_server, _, client) = serve(application.clone(), no_committed_reads()).await;
    assert_eq!(
        client.holder(),
        registered_task_holder("attention-worker", "attention-worker-boot"),
        "the delivery is authenticated as the incarnation it tenures Tasks to"
    );
    let sent = attention(client.holder()).bind_input().unwrap();
    let ack = client.deliver(sent.clone()).await.expect("A2+K1 ack");
    assert_eq!(
        ack.run_id,
        RunId(format!("attention-run-{}", sent.operation_id)),
        "K1"
    );
    assert_eq!(
        application.admitted(),
        vec![sent.clone()],
        "K1 exact attention"
    );

    // K5 then K2: the first delivery is not acknowledged; the resend is.
    let flaky = RecordingApplication::new(1, false);
    let (_flaky_server, _, flaky_client) = serve(flaky.clone(), no_committed_reads()).await;
    let sent = attention(flaky_client.holder()).bind_input().unwrap();
    let error = flaky_client.deliver(sent.clone()).await.unwrap_err();
    assert_eq!(error.kind, RunErrorKind::Unavailable, "K5");
    let retried = flaky_client.deliver(sent.clone()).await.expect("K2 ack");
    assert_eq!(retried, ack, "K2 the same operation reaches the same Run");
    assert_eq!(
        flaky.admitted(),
        vec![sent.clone(), sent],
        "K2 identical resend"
    );

    // A3+K4: a holder of another incarnation never reaches the application.
    let guarded = RecordingApplication::new(0, false);
    let (_guarded_server, _, guarded_client) = serve(guarded.clone(), no_committed_reads()).await;
    let error = guarded_client
        .deliver(attention(&registered_task_holder(
            "attention-worker",
            "previous-incarnation",
        )))
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::BadRequest, "A3+K4");
    assert!(guarded.admitted().is_empty(), "A3 no admission");

    // K4: a definitive application refusal stays definitive.
    let refusing = RecordingApplication::new(0, true);
    let (_refusing_server, _, refusing_client) =
        serve(refusing.clone(), no_committed_reads()).await;
    let error = refusing_client
        .deliver(attention(refusing_client.holder()))
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::BadRequest, "K4");
    assert_eq!(refusing.admitted().len(), 1, "K4 one admission attempt");
}

/// The request envelope admits only its current shape: a historical or
/// future body never reaches the application.
#[tokio::test]
async fn a_historical_task_attention_body_is_refused_before_admission() {
    let application = RecordingApplication::new(0, false);
    let (_server, base_url, client) = serve(application.clone(), no_committed_reads()).await;
    let identity = serde_json::json!({
        "worker_id": "attention-worker",
        "incarnation_id": "attention-worker-boot",
        "generation": 1,
    });
    let current = serde_json::to_value(attention(client.holder())).unwrap();
    let mut unknown_field = current.clone();
    unknown_field["legacy_holder"] = serde_json::json!("attention-worker");
    let mut bare_record_list = current.clone();
    bare_record_list["input"] = serde_json::json!([{"version": 1}]);
    let mut future_input = current.clone();
    future_input["input"]["version"] = serde_json::json!(2);
    let http = reqwest::Client::new();
    for (row, body) in [
        (
            "unknown attention field",
            serde_json::json!({ "identity": identity, "attention": unknown_field }),
        ),
        (
            "pre-codec input",
            serde_json::json!({ "identity": identity, "attention": bare_record_list }),
        ),
        (
            "future input version",
            serde_json::json!({ "identity": identity, "attention": future_input }),
        ),
        (
            "unknown envelope field",
            serde_json::json!({ "identity": identity, "attention": current, "version": 0 }),
        ),
    ] {
        let response = http
            .post(format!(
                "{base_url}{}",
                awaken_session_application_contract::WORKER_TASK_ATTENTION_PATH
            ))
            .header("x-awaken-worker-id", "attention-worker")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_client_error(),
            "{row}: {}",
            response.status()
        );
    }
    assert!(
        application.admitted().is_empty(),
        "no historical body is admitted"
    );
}

fn slot(task: &str, lifecycle: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"id": task, "origin":{"thread_id":"attention-thread", "run_id":"origin-run", "operation_id":"op"}, "lifecycle": lifecycle, "revision": 1})
}

fn attempt(holder: &str) -> serde_json::Value {
    serde_json::json!({
        "worker_id": holder, "epoch": 3, "lease_expires_at_ms": 900, "policy": {}
    })
}

fn tenure_read(task: &str, holder: &str) -> TaskTenureRead {
    partitioned_read(
        "attention-thread",
        TaskReadPartition::Thread {},
        task,
        holder,
    )
}

fn partitioned_read(
    thread: &str,
    partition: TaskReadPartition,
    task: &str,
    holder: &str,
) -> TaskTenureRead {
    TaskTenureRead {
        thread: thread.into(),
        partition,
        run: "origin-run".into(),
        task: task.into(),
        holder: holder.into(),
        epoch: awaken_lease_contract::fence::Epoch::from_persisted(3).expect("nonzero Task epoch"),
    }
}

/// Holder-bound Task tenure through the real transport:
/// | Rule | cause | effect |
/// |---|---|---|
/// | H2 | the committed attempt names this incarnation's holder | the exact committed slot |
/// | H3 | absent, Ended, or another holder's attempt | no slot |
/// | H4 | the read claims another incarnation's holder | `BadRequest` before any read |
/// | H5 | the committed store is unavailable, or the slot is malformed | `Unavailable` |
#[tokio::test]
async fn a_registered_worker_reads_only_the_committed_tenure_it_holds() {
    let holder = registered_task_holder("attention-worker", "attention-worker-boot");
    let previous = registered_task_holder("attention-worker", "previous-incarnation");
    let held = slot(
        "held",
        serde_json::json!({"state": "waiting", "attempt": attempt(&holder)}),
    );
    let committed = root_committed(Some(vec![
        ("held".into(), held.clone()),
        (
            "ended".into(),
            slot(
                "ended",
                serde_json::json!({"state": "ended", "end": {"outcome": "cancelled"}}),
            ),
        ),
        (
            "other".into(),
            slot(
                "other",
                serde_json::json!({"state": "running", "attempt": attempt(&previous)}),
            ),
        ),
        ("malformed".into(), serde_json::json!("not a task")),
    ]));
    let (_server, _, client) = serve(RecordingApplication::new(0, false), committed).await;
    assert_eq!(client.holder(), holder);

    // H2
    let record = client
        .read_tenure(tenure_read("held", &holder))
        .await
        .expect("H2")
        .expect("H2 slot");
    assert_eq!(record.slot(), &held, "H2 the exact committed slot");
    // H3
    for task in ["absent", "ended", "other"] {
        assert!(
            client
                .read_tenure(tenure_read(task, &holder))
                .await
                .expect("H3")
                .is_none(),
            "H3 {task}"
        );
    }
    // H4
    let error = client
        .read_tenure(tenure_read("other", &previous))
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::BadRequest, "H4");
    // H5: a malformed committed slot.
    let error = client
        .read_tenure(tenure_read("malformed", &holder))
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::Unavailable, "H5 malformed");

    // H5: the committed store is unavailable.
    let (_down, _, down_client) =
        serve(RecordingApplication::new(0, false), root_committed(None)).await;
    let error = down_client
        .read_tenure(tenure_read("held", &holder))
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::Unavailable, "H5 unavailable");
}

/// The partition a tenure read addresses through the real transport:
/// | Rule | cause | effect |
/// |---|---|---|
/// | P1 | a root-Thread Task read with `thread` | read (the H2 row above) |
/// | P2 | a child-Thread Task read in its Session's partition | the exact committed slot |
/// | P3 | `session` naming the Thread itself | `BadRequest` |
/// | P4 | a child-Thread Task read as a root Thread, or in another Session | `BadRequest`: refused, never an empty read |
#[tokio::test]
async fn a_child_thread_task_is_read_only_in_its_session_partition() {
    let holder = registered_task_holder("attention-worker", "attention-worker-boot");
    let mut held = slot(
        "held",
        serde_json::json!({"state": "running", "attempt": attempt(&holder)}),
    );
    held["origin"]["thread_id"] = serde_json::json!("child-thread");
    let committed = Arc::new(ScriptedCommitted {
        slots: Some(vec![("held".into(), held.clone())]),
        session_thread: "session-thread",
        thread: "child-thread",
    });
    let (_server, _, client) = serve(RecordingApplication::new(0, false), committed).await;
    let in_session = |session: &str| TaskReadPartition::Session {
        session_thread: session.into(),
    };

    // P2
    let record = client
        .read_tenure(partitioned_read(
            "child-thread",
            in_session("session-thread"),
            "held",
            &holder,
        ))
        .await
        .expect("P2")
        .expect("P2 slot");
    assert_eq!(record.slot(), &held, "P2 the exact committed slot");
    // P3
    let error = client
        .read_tenure(partitioned_read(
            "session-thread",
            in_session("session-thread"),
            "held",
            &holder,
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunErrorKind::BadRequest, "P3");
    // P4: the wrong partition answers an empty Thread; the read is refused.
    for (row, partition) in [
        ("P4 as a root Thread", TaskReadPartition::Thread {}),
        ("P4 another Session", in_session("other-session")),
    ] {
        let error = client
            .read_tenure(partitioned_read("child-thread", partition, "held", &holder))
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunErrorKind::BadRequest, "{row}");
    }
}

/// The tenure request admits only its current shape.
#[tokio::test]
async fn a_historical_task_tenure_body_is_refused() {
    let (_server, base_url, client) =
        serve(RecordingApplication::new(0, false), no_committed_reads()).await;
    let identity = serde_json::json!({
        "worker_id": "attention-worker",
        "incarnation_id": "attention-worker-boot",
        "generation": 1,
    });
    let current = serde_json::to_value(tenure_read("held", client.holder())).unwrap();
    let mut with_epoch = current.clone();
    with_epoch["epoch"] = serde_json::json!(0);
    let mut without_run = current.clone();
    without_run.as_object_mut().unwrap().remove("run");
    let mut without_partition = current.clone();
    without_partition
        .as_object_mut()
        .unwrap()
        .remove("partition");
    let mut unknown_partition = current.clone();
    unknown_partition["partition"] = serde_json::json!({"kind": "root"});
    let http = reqwest::Client::new();
    for (row, body) in [
        (
            "unknown read field",
            serde_json::json!({ "identity": identity, "read": with_epoch }),
        ),
        (
            "pre-run read",
            serde_json::json!({ "identity": identity, "read": without_run }),
        ),
        (
            "pre-partition read",
            serde_json::json!({ "identity": identity, "read": without_partition }),
        ),
        (
            "unknown partition",
            serde_json::json!({ "identity": identity, "read": unknown_partition }),
        ),
        (
            "unknown envelope field",
            serde_json::json!({ "identity": identity, "read": current, "version": 0 }),
        ),
    ] {
        let response = http
            .post(format!(
                "{base_url}{}",
                awaken_session_application_contract::WORKER_TASK_TENURE_PATH
            ))
            .header("x-awaken-worker-id", "attention-worker")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_client_error(),
            "{row}: {}",
            response.status()
        );
    }
}

/// ACK transport cause/effect table: J1 Coordinator admission returned an ACK,
/// but the HTTP200 body contains truncated JSON; J2 the body is not JSON.
/// Either is Unknown/Unavailable, not a definitive rejection: retry the same
/// operation and exact input, recover the original Run id, and retain both
/// admission observations. Existing K4 tests retain explicit4xx refusals.
#[tokio::test]
async fn an_undecodable_successful_ack_retries_the_same_attention_and_run() {
    use axum::extract::State;
    use axum::response::IntoResponse;

    async fn corrupt_first_ack(
        State((seen, malformed)): State<(Arc<AtomicUsize>, &'static str)>,
        response: axum::response::Response,
    ) -> axum::response::Response {
        if response.status().is_success() && seen.fetch_add(1, Ordering::SeqCst) == 0 {
            (axum::http::StatusCode::OK, malformed).into_response()
        } else {
            response
        }
    }

    for (rule, malformed) in [("J1", r#"{"ack":"#), ("J2", "not-json")] {
        let application = RecordingApplication::new(0, false);
        let (directory, identity) = ready_worker("attention-worker").await;
        let router = worker_task_control_router(
            application.clone(),
            no_committed_reads(),
            directory,
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(0)),
        )
        .layer(axum::middleware::map_response_with_state(
            (Arc::new(AtomicUsize::new(0)), malformed),
            corrupt_first_ack,
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _server = Server(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let control = WorkerControlClient::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
        );
        let client = WorkerTaskControlClient::new(control, identity);
        let mut sent = attention(client.holder());
        sent.input = Some(
            PrivateTaskInput::new(vec![serde_json::json!({
                "version": 1,
                "task_id": "task-1",
                "holder": client.holder(),
                "epoch": 3,
                "observation": {"kind": "ended", "end": {"outcome": "cancelled"}},
            })])
            .unwrap(),
        );
        let sent = sent.bind_input().unwrap();
        let error = client.deliver(sent.clone()).await.unwrap_err();
        assert_eq!(error.kind, RunErrorKind::Unavailable, "{rule} not an ACK");
        assert_eq!(
            application.admitted(),
            vec![sent.clone()],
            "{rule} already admitted"
        );
        let ack = client
            .deliver(sent.clone())
            .await
            .expect("same-operation retry ACK");
        assert_eq!(
            ack.run_id,
            RunId(format!("attention-run-{}", sent.operation_id)),
            "{rule} original Run identity"
        );
        assert_eq!(
            application.admitted(),
            vec![sent],
            "{rule} exact replay adds no admission"
        );
    }
}

/// Authentication decision table, before every fresh admission: A1 every
/// record names the same held Task/epoch -> ACK; A2 a foreign holder anywhere
/// in a batch -> refuse atomically; A3 unknown record version -> refuse; A4
/// the sender's own text but no held committed Task -> refuse. InputRequired
/// (A5) carries no observation but must still prove exactly the held tenure.
#[tokio::test]
async fn task_attention_binds_every_record_to_the_authenticated_holder_before_admission() {
    for row in 0..5 {
        let application = RecordingApplication::new(0, false);
        let committed = if row == 3 {
            root_committed(Some(Vec::new()))
        } else {
            no_committed_reads()
        };
        let (_server, _, client) = serve(application.clone(), committed).await;
        let mut sent = attention(client.holder());
        if row == 4 {
            sent.input = None;
        } else {
            let own = sent.input.as_ref().unwrap().observations()[0].clone();
            let mut second = own.clone();
            if row == 1 {
                second["holder"] =
                    serde_json::json!(registered_task_holder("another-worker", "boot"));
            }
            if row == 2 {
                second["version"] = serde_json::json!(2);
            }
            sent.input = Some(PrivateTaskInput::new(vec![own, second]).unwrap());
        }
        let result = client.deliver(sent.clone()).await;
        if row == 0 || row == 4 {
            assert!(result.is_ok(), "A1/A5 row={row}");
            assert_eq!(
                application.admitted(),
                vec![sent.clone().bind_input().unwrap()],
                "exact bound input"
            );
        } else {
            assert_eq!(
                result.unwrap_err().kind,
                RunErrorKind::BadRequest,
                "row={row}"
            );
            assert!(application.admitted().is_empty(), "row={row} no admission");
        }
    }
}

/// R1 exact original ACK precedes current held-task reads: Ended/another holder,
/// absent or unavailable current Task source cannot erase accepted input. R2 a
/// changed subject (including a meta-only input-required attention) conflicts
/// before any new admission. The recording application models the receipt port;
/// the real backend conformance separately tests its persisted identity law.
#[tokio::test]
async fn an_accepted_attention_ack_survives_task_phase_changes_without_new_admission() {
    for meta_only in [false, true] {
        let application = RecordingApplication::new(0, false);
        let (_first_server, _, first) = serve(application.clone(), no_committed_reads()).await;
        let mut sent = attention(first.holder());
        if meta_only {
            sent.input = None;
        }
        let sent = sent.bind_input().unwrap();
        let original = first.deliver(sent.clone()).await.unwrap();
        for committed in [
            root_committed(Some(vec![(
                "task-1".into(),
                slot(
                    "task-1",
                    serde_json::json!({"state":"ended", "end":{"outcome":"cancelled"}}),
                ),
            )])),
            root_committed(Some(Vec::new())),
            root_committed(None),
        ] {
            let (_server, _, replay) = serve(application.clone(), committed).await;
            assert_eq!(
                replay.deliver(sent.clone()).await.unwrap(),
                original,
                "R1 meta_only={meta_only}"
            );
        }
        let (_server, _, replay) = serve(application.clone(), no_committed_reads()).await;
        for axis in [
            "task",
            "thread",
            "run",
            "holder",
            "epoch",
            "partition",
            "message",
        ] {
            let mut wire = serde_json::to_value(&sent).unwrap();
            if axis == "message" {
                wire["message"]["id"] = serde_json::json!("changed-message");
            } else if axis == "epoch" {
                wire["tenure"][axis] = serde_json::json!(4);
            } else if axis == "partition" {
                wire["tenure"][axis] =
                    serde_json::json!({"kind":"session", "session_thread":"another-session"});
            } else {
                wire["tenure"][axis] = serde_json::json!(format!("another-{axis}"));
            }
            // The private input is bound by the same subject. Changing just the
            // outer read is also invalid and never allowed to reuse an ACK.
            wire["input"]["tenure"] = wire["tenure"].clone();
            let changed: TaskAttention = serde_json::from_value(wire).unwrap();
            assert_eq!(
                replay.deliver(changed).await.unwrap_err().kind,
                RunErrorKind::BadRequest,
                "R2 {axis} meta_only={meta_only}: invalid subject or exact command conflict is definitive, never an unavailable retry"
            );
        }
        assert_eq!(application.admitted(), vec![sent], "R1/R2 no new admission");
    }
}

#[tokio::test]
async fn registry_unavailability_is_retryable_without_task_admission_or_ack() {
    // U1 authentication succeeds, but the original current-incarnation read
    // fails =>503/Unavailable before Task reads/admission. U2 the SAME original
    // attention after recovery => one ACK/one admission. U3 immutable accepted
    // input changes =>409/BadRequest, no retry authority or second admission.
    // The existing directory's same record/read leaf is injected, not a second
    // parser, precomputed status or different directory on the retry. U4 a
    // foreign transport identity remains definitive before the current read.
    let (directory, identity) = ready_worker("attention-worker").await;
    let application = RecordingApplication::new(0, false);
    let (_server, base_url, client) = serve_with_worker(
        application.clone(),
        no_committed_reads(),
        directory.clone(),
        identity.clone(),
    )
    .await;
    let sent = attention(client.holder());
    directory.set_current_unavailable(true);
    assert_eq!(
        client.deliver(sent.clone()).await.unwrap_err().kind,
        RunErrorKind::Unavailable,
        "U1 unknown Registry"
    );
    assert_eq!(directory.current_reads(), 1, "U1 one original read");
    assert!(application.admitted().is_empty(), "U1 no admission/ACK");
    let refused = reqwest::Client::new()
        .post(format!(
            "{base_url}{}",
            awaken_session_application_contract::WORKER_TASK_ATTENTION_PATH
        ))
        .header("x-awaken-worker-id", "another-worker")
        .json(&serde_json::json!({"identity": &identity, "attention": &sent}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "U4 wrong authenticated transport/body identity"
    );
    assert_eq!(
        directory.current_reads(),
        1,
        "U4 definitive before Registry read"
    );
    assert!(application.admitted().is_empty(), "U4 no Task admission");
    directory.set_current_unavailable(false);
    let ack = client
        .deliver(sent.clone())
        .await
        .expect("U2 same operation retry");
    assert_eq!(
        ack.run_id,
        RunId(format!("attention-run-{}", sent.operation_id)),
        "U2 original Run identity"
    );
    assert_eq!(
        application.admitted(),
        vec![sent.clone().bind_input().unwrap()],
        "U2 one admission"
    );
    let mut changed = sent;
    changed.message.id.0.push_str("-conflict");
    let conflict = reqwest::Client::new()
        .post(format!(
            "{base_url}{}",
            awaken_session_application_contract::WORKER_TASK_ATTENTION_PATH
        ))
        .header("x-awaken-worker-id", &identity.worker_id)
        .json(&serde_json::json!({"identity": &identity, "attention": &changed}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        conflict.status(),
        reqwest::StatusCode::CONFLICT,
        "U3 known mismatch stays409"
    );
    assert_eq!(
        client.deliver(changed).await.unwrap_err().kind,
        RunErrorKind::BadRequest,
        "U3 known immutable conflict is definitive"
    );
    assert_eq!(
        directory.current_reads(),
        4,
        "U1/U2/U3 one current read each; U4 prelookup refusal"
    );
    assert_eq!(application.admitted().len(), 1, "U3 no new work");
}
