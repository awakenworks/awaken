//! A registered Worker's Task attention through the actual client, HTTP route
//! and Worker registry check, against a recording Session application.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_coordinator_client::{WorkerControlClient, WorkerTaskAttentionClient};
use awaken_dispatch_http::worker_task_attention_router;
use awaken_dispatch_testkit::worker_http::ready_worker;
use awaken_lease_testkit::ManualClock;
use awaken_session_application_contract::{
    PrivateTaskInput, RunApplicationError, SessionRunBackgroundApplication, TaskAttention,
    TaskAttentionAck, TaskAttentionDelivery, registered_task_holder,
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

/// Abort-on-drop also closes the server on assertion failure.
struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(
    application: Arc<RecordingApplication>,
) -> (Server, String, WorkerTaskAttentionClient) {
    let (directory, identity) = ready_worker("attention-worker").await;
    let router = worker_task_attention_router(
        application,
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
        WorkerTaskAttentionClient::new(control, identity),
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
    let (_server, _, client) = serve(application.clone()).await;
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
    let (_flaky_server, _, flaky_client) = serve(flaky.clone()).await;
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
    let (_guarded_server, _, guarded_client) = serve(guarded.clone()).await;
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
    let (_refusing_server, _, refusing_client) = serve(refusing.clone()).await;
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
    let (_server, base_url, client) = serve(application.clone()).await;
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
