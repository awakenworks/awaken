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
use awaken_session_application_contract::{
    PrivateTaskInput, RegisteredTaskControl, RunApplicationError, SessionRunBackgroundApplication,
    TaskAttention, TaskAttentionAck, TaskReadPartition, TaskTenureRead, registered_task_holder,
};
use awaken_session_types::{RunError, RunErrorKind};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

/// The Coordinator's Session application, recording what it admits.
struct RecordingApplication {
    unavailable_first: AtomicUsize,
    refuse: bool,
    attentions: Mutex<Vec<TaskAttention>>,
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
        self.attentions.lock().unwrap().clone()
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
    ) -> Result<RunId, RunApplicationError> {
        panic!("Task attention never submits an ordinary background Run")
    }

    async fn submit_task_attention(
        &self,
        attention: TaskAttention,
    ) -> Result<TaskAttentionAck, RunApplicationError> {
        self.attentions.lock().unwrap().push(attention.clone());
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
    root_committed(Some(Vec::new()))
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
        thread: "attention-thread".into(),
        holder: holder.into(),
        message: Message::text(
            MessageId("background-task-attention-message-fixed".into()),
            Role::System,
            "Background task task-1 has produced a terminal completion candidate.",
        ),
        input: Some(
            PrivateTaskInput::new(vec![serde_json::json!({"version": 1})])
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
    let sent = attention(client.holder());
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
    let sent = attention(flaky_client.holder());
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
    serde_json::json!({"id": task, "lifecycle": lifecycle, "revision": 1})
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
    let held = slot(
        "held",
        serde_json::json!({"state": "running", "attempt": attempt(&holder)}),
    );
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
    with_epoch["epoch"] = serde_json::json!(1);
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
