//! T28-C03: a native model request already went out when authority was
//! revoked or the network was cut; the recoverer reconnects the original
//! operation identity.
//!
//! Production units: `BrokeredCandidateExecutor::infer{,_streaming}` (grant,
//! native send through the real provider transport, close) and the runtime
//! reentry rule `ModelOperation::permits_reentry`. Fakes are collaborators
//! only: the broker control client (`RecordingClient`), the ownership
//! verifier, and a real TCP upstream that counts complete requests.
//!
//! Decision table (each row x streaming/non-streaming):
//! | rule | after the upstream received the full request | effect                     |
//! | U1   | ownership Lost, connection closed, no reply  | Unknown, no reentry, 1 send |
//! | U2   | ownership current, connection cut, no reply  | Unknown, no reentry, 1 send |
//! | U3   | ownership Lost, partial 200 body, then close | Unknown, no reentry, 1 send |
//! Every row: exactly one grant, that grant is closed, and the recoverer that
//! reconnects the same operation derives the identical idempotency key while
//! making zero further native sends.
//!
//! Negative control (executed on U2): an RPC-failure mapper that relabels the
//! failure as NotDispatched makes the recoverer resend (a second physical
//! send on the counting upstream), and the oracle check detects it.

use super::*;
use awaken_runtime_contract::model_operation::{
    ModelExecutionEvidence, ModelOperation, ModelOperationPhase, NoDispatchProof,
    with_model_operation_context,
};
use awaken_runtime_contract::{AttemptOwnershipError, AttemptOwnershipVerifier};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

struct MutableOwnership(Mutex<Result<(), AttemptOwnershipError>>);

#[async_trait]
impl AttemptOwnershipVerifier for MutableOwnership {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    /// Close after the full request without writing any response byte.
    NoResponse,
    /// Send a 200 head announcing more body than is written, then close.
    PartialBody,
}

struct Upstream {
    endpoint: String,
    sends: Arc<AtomicUsize>,
    received: Arc<tokio::sync::Notify>,
    proceed: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

/// Real TCP upstream: counts a send only after the complete request body was
/// read (so the effect provably left the executor), signals `received`, waits
/// for `proceed`, then cuts the connection per `cut`.
async fn upstream(cut: Cut) -> Upstream {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let sends = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    let (counter, on_received, gate) = (sends.clone(), received.clone(), proceed.clone());
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            {
                let mut reader = tokio::io::BufReader::new(&mut socket);
                let mut line = String::new();
                let mut content_length = None;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        break;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = Some(value.trim().parse::<u64>().unwrap());
                    }
                }
                let length = content_length.expect("native JSON request has a body length");
                // Read the whole body: on Windows unread input turns close
                // into a reset, and the send must be complete to count.
                assert_eq!(
                    tokio::io::copy(&mut reader.take(length), &mut tokio::io::sink())
                        .await
                        .unwrap(),
                    length
                );
            }
            counter.fetch_add(1, Ordering::SeqCst);
            on_received.notify_one();
            gate.notified().await;
            if cut == Cut::PartialBody {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 400\r\n\r\n{\"id\":\"resp-1\",",
                    )
                    .await;
            }
            let _ = socket.shutdown().await;
        }
    });
    Upstream {
        endpoint,
        sends,
        received,
        proceed,
        task,
    }
}

fn request_for(
    operation: &awaken_runtime_contract::model_operation::ModelOperationContext,
) -> ChatRequest {
    ChatRequest {
        model_binding: operation.binding().clone(),
        inference: Default::default(),
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

async fn run_once(
    model: &BrokeredCandidateExecutor,
    streaming: bool,
) -> Result<ChatResponse, LlmError> {
    let operation = model_operation(1);
    let request = request_for(&operation);
    with_model_operation_context(Some(operation), async {
        if streaming {
            model.infer_streaming(request, &crate::NoopDeltaSink).await
        } else {
            model.infer(request).await
        }
    })
    .await
}

/// The journal entry the runtime would hold for the returned call.
fn journal_entry(
    model: &BrokeredCandidateExecutor,
    evidence: ModelExecutionEvidence,
) -> ModelOperation {
    let operation = model_operation(1);
    let mut entry = ModelOperation::next(None, operation.binding().clone()).unwrap();
    entry.phase = ModelOperationPhase::Returned;
    entry.execution = evidence;
    entry.effect_scope = model.effect_scope(&request_for(&operation));
    entry
}

/// Recoverer for the original operation identity: re-enters the executor
/// only when the production reentry rule permits it for the journaled
/// evidence. Returns whether it re-sent.
async fn recover(
    model: &BrokeredCandidateExecutor,
    entry: &ModelOperation,
    streaming: bool,
) -> bool {
    for recovering_checkpoint in [true, false] {
        if ModelOperation::permits_reentry(Some(entry), recovering_checkpoint) {
            let _ = run_once(model, streaming).await;
            return true;
        }
    }
    false
}

/// The T28-C03 oracle on the observed outcome of one already-sent request.
fn check_sent_effect_kept_unknown(
    evidence: ModelExecutionEvidence,
    resent: bool,
    sends: usize,
) -> Result<(), String> {
    if evidence != ModelExecutionEvidence::Unknown {
        return Err(format!(
            "a request the upstream fully received was reported as {evidence:?}"
        ));
    }
    if resent || sends != 1 {
        return Err(format!(
            "unknown outcome was re-sent (resent={resent}, physical sends={sends})"
        ));
    }
    Ok(())
}

#[tokio::test]
async fn t28_c03_sent_native_request_stays_unknown_and_is_never_resent() {
    for (rule, revoke, cut) in [
        ("U1", true, Cut::NoResponse),
        ("U2", false, Cut::NoResponse),
        ("U3", true, Cut::PartialBody),
    ] {
        for streaming in [false, true] {
            let upstream = upstream(cut).await;
            let client = Arc::new(RecordingClient {
                gateway_base_url: Some(upstream.endpoint.clone()),
                ..Default::default()
            });
            let ownership = Arc::new(MutableOwnership(Mutex::new(Ok(()))));
            let model = executor(client.clone(), Some(ownership.clone()));
            let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(run_once(&model, streaming), async {
                    upstream.received.notified().await;
                    if revoke {
                        *ownership.0.lock().unwrap() = Err(AttemptOwnershipError::Lost);
                    }
                    upstream.proceed.notify_one();
                })
            })
            .await
            .unwrap_or_else(|_| panic!("{rule}/{streaming}: barrier-controlled call finishes"));
            let error = match outcome {
                Ok(_) => panic!("{rule}/{streaming}: a cut connection cannot be a success"),
                Err(error) => error,
            };
            let evidence = error.execution_evidence();
            assert_eq!(
                upstream.sends.load(Ordering::SeqCst),
                1,
                "{rule}/{streaming}"
            );
            assert_eq!(
                evidence,
                ModelExecutionEvidence::Unknown,
                "{rule}/{streaming}: sent request must not claim zero effect: {error}"
            );
            assert_eq!(
                client.requests.lock().unwrap().len(),
                1,
                "{rule}/{streaming} one grant"
            );
            assert_eq!(
                *client.closed.lock().unwrap(),
                ["grant-1"],
                "{rule}/{streaming}: the original grant keeps its close owner"
            );

            // Recoverer: production reentry rule over the journaled Unknown.
            let entry = journal_entry(&model, evidence);
            assert!(!entry.reentry().allows_request(), "{rule}/{streaming}");
            let resent = recover(&model, &entry, streaming).await;
            let sends = upstream.sends.load(Ordering::SeqCst);
            check_sent_effect_kept_unknown(evidence, resent, sends)
                .unwrap_or_else(|violation| panic!("{rule}/{streaming}: {violation}"));
            assert_eq!(
                client.requests.lock().unwrap().len(),
                1,
                "{rule}/{streaming}: no new grant"
            );

            // Reconnecting the original identity derives the same key.
            let observer = Arc::new(RecordingClient::default());
            let reconnect = executor(observer.clone(), Some(Arc::new(CurrentOwnership)));
            lease_operation(&reconnect, 1)
                .await
                .expect("reconnect lease");
            assert_eq!(
                observer.requests.lock().unwrap()[0].idempotency_key,
                client.requests.lock().unwrap()[0].idempotency_key,
                "{rule}/{streaming}: recoverer reconnects the original operation identity"
            );

            if rule == "U2" {
                // Negative control, executed: map the RPC failure to
                // NotDispatched and let the same recoverer act on it.
                let mutated = error
                    .with_execution_evidence(ModelExecutionEvidence::NotDispatched(
                        NoDispatchProof::executor_resolution(),
                    ))
                    .execution_evidence();
                let entry = journal_entry(&model, mutated);
                let (resent, ()) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        tokio::join!(recover(&model, &entry, streaming), async {
                            // The recoverer's resend reached the upstream: release it.
                            upstream.received.notified().await;
                            upstream.proceed.notify_one();
                        })
                    })
                    .await
                    .expect("NC: mutant resend settles");
                assert!(resent, "NC/{streaming}: mutant evidence permits reentry");
                let sends = upstream.sends.load(Ordering::SeqCst);
                assert_eq!(sends, 2, "NC/{streaming}: a second physical send happened");
                let violation = check_sent_effect_kept_unknown(mutated, resent, sends)
                    .expect_err("NC: relabelled RPC failure must be detected");
                assert!(
                    violation.contains("NotDispatched"),
                    "NC/{streaming}: {violation}"
                );
            }
            upstream.task.abort();
            let _ = upstream.task.await;
        }
    }
}
