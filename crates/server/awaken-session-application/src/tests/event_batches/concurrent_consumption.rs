//! T11-C03 (并发消费): two independent clients answer the same pending tool
//! ticket concurrently, one response is lost, and the callers retry.
//!
//! Oracles (groups-2.json order):
//! - O0 a legal consumption happens at most once;
//! - O1 an identical retry returns the original receipt;
//! - O2 the conflict outcome is identical across file SQLite and PostgreSQL.
//!
//! Negative control: an in-process Mutex cannot replace the root CAS. Each
//! client runs behind its OWN process-local lock (as two processes would) and
//! both hold it at the shared pre-CAS barrier at the same time; the clients
//! use two independently opened repositories (no shared handle or pool). A
//! lock-based exclusion would admit both replies; only the root CAS can pick
//! the single winner.
//!
//! | Row | Keys | Replies | Fault | Expected |
//! |---|---|---|---|---|
//! | C1 | a / b | equal | each winner's CAS response is lost once | one receipt, one occupied rejection, one root batch |
//! | C2 | a / b | different | same | same as C1 |
//! | C3 | winner key | winner reply | caller lost the receipt, retries via the OTHER client | the original receipt; root and outbox unchanged |
//! | C4 | new key | any | after consumption | occupied rejection; root and outbox unchanged |
//! | C5 | shared key | equal | both clients concurrently | both return the one canonical batch |
//! | C6 | unrelated Session | lifecycle fact | between replay snapshots | tested Session's outbox unchanged; unrelated fact retained |
//! | X | SQLite vs PostgreSQL | rows C1-C5 | - | identical normalized outcome signatures |
use super::*;

type Store = Arc<dyn crate::SessionStore>;

async fn session_outbox(
    store: &Store,
    session: &SessionId,
) -> Vec<awaken_session_contract::ManagedLifecycleFact> {
    store
        .pending_lifecycle()
        .await
        .unwrap()
        .into_iter()
        .filter(|fact| fact.object_id == session.as_str())
        .collect()
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

struct Client {
    faulting: Arc<FaultingSessionRepository>,
    runtime: Arc<EventBatchRuntime>,
    app: SessionApplication,
    // NC: this client's own process-local lock.
    lock: tokio::sync::Mutex<()>,
}

fn client(store: Store, barrier: Option<Arc<tokio::sync::Barrier>>) -> Client {
    let faulting = Arc::new(FaultingSessionRepository::new(store));
    let runtime = Arc::new(EventBatchRuntime::default());
    *runtime.admission_barrier.lock().unwrap() = barrier;
    let app = application_with_runtime(
        runtime.clone(),
        faulting.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    Client {
        faulting,
        runtime,
        app,
        lock: tokio::sync::Mutex::new(()),
    }
}

async fn append(
    client: &Client,
    session: &SessionId,
    input: SessionEventInput,
    key: &str,
) -> Result<awaken_session_contract::SessionEventBatch, RunError> {
    let _process_local = client.lock.lock().await;
    client
        .app
        .event_batches
        .admit_session_event_batch_idempotent(
            session,
            vec![input],
            None,
            None,
            Some((key.into(), format!("fingerprint-{key}"))),
        )
        .await
        .map(|result| {
            result
                .into_batch()
                .expect("reply fixture retains its Event batch")
        })
}

fn normalized(result: &Result<awaken_session_contract::SessionEventBatch, RunError>) -> String {
    match result {
        Ok(_) => "accepted".into(),
        Err(error) => format!("{:?}:{}:{}", error.kind, error.code, error.message),
    }
}

/// Run C1-C5 over two independent stores and return one normalized signature
/// per row for the cross-backend comparison.
async fn consumption_table(prefix: &str, store_a: Store, store_b: Store) -> Vec<String> {
    assert!(
        !std::ptr::addr_eq(Arc::as_ptr(&store_a), Arc::as_ptr(&store_b)),
        "NC the clients use independently opened repositories"
    );
    let mut signatures = Vec::new();
    for changed in [false, true] {
        let id = unique(&format!("{prefix}-reply-{changed}"));
        let session = SessionId::from(id.as_str());
        create(store_a.as_ref(), persisted(&id, false, "idle")).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let a = client(store_a.clone(), Some(barrier.clone()));
        let b = client(store_b.clone(), Some(barrier));
        // Each winner's in-call CAS response is lost once; the loser's CAS
        // conflicts for real. Neither may consume twice.
        a.faulting.commit_then_conflict_once("append-event-batch");
        b.faulting.commit_then_conflict_once("append-event-batch");
        let left = tool_reply_input("one-call");
        let mut right = left.clone();
        if changed {
            let SessionEventInput::ToolReply(reply) = &mut right else {
                unreachable!()
            };
            reply.reply = SessionEventToolReplyKind::ToolResult {
                content: Some(vec![ContentBlock::text("other")]),
                is_error: true,
            };
        }
        let (ra, rb) = tokio::join!(
            append(&a, &session, left.clone(), "a"),
            append(&b, &session, right.clone(), "b"),
        );
        let outcomes = [ra, rb];
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
            1,
            "C1/C2/O0 one consumption despite both process-local locks being held: {outcomes:?}"
        );
        let rejection = outcomes
            .iter()
            .find_map(|outcome| outcome.as_ref().err())
            .expect("C1/C2 one rejection");
        assert_eq!(
            rejection.kind,
            awaken_runtime_contract::runs::RunErrorKind::BadRequest,
            "C1/C2/O0 the loser is an occupancy rejection"
        );
        let winner_index = usize::from(outcomes[0].is_err());
        let receipt = outcomes[winner_index].as_ref().unwrap().clone();
        let (winner_input, winner_key) = if winner_index == 0 {
            (left.clone(), "a")
        } else {
            (right.clone(), "b")
        };
        let root = store_b.get(&session).await.unwrap();
        assert_eq!(root.event_batches.len(), 1, "C1/C2/O0 one root batch");
        assert_eq!(
            root.event_batches[0], receipt,
            "C1/C2 the receipt is the root batch"
        );
        let outbox = session_outbox(&store_b, &session).await;

        // C6: a shared PostgreSQL fixture legitimately receives facts from
        // other tests. Make that interleaving deterministic on BOTH backends;
        // C3/C4 must protect this Session rather than freeze the whole database.
        let unrelated = awaken_session_contract::ManagedLifecycleFact {
            id: unique(&format!("{prefix}-unrelated-fact")),
            object_id: unique(&format!("{prefix}-unrelated-session")),
            workspace_id: Some("workspace".into()),
            event_type: "session.status_idled".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        store_b.append_lifecycle(unrelated.clone()).await.unwrap();

        // C3: the caller lost the receipt and retries the identical request
        // through the OTHER independent client.
        let retry_client = if winner_index == 0 { &b } else { &a };
        let replay = append(retry_client, &session, winner_input, winner_key).await;
        assert_eq!(
            replay.as_ref().ok(),
            Some(&receipt),
            "C3/O1 the identical retry returns the original receipt"
        );
        // C4: a new key cannot consume the occupied ticket.
        let late = append(retry_client, &session, left.clone(), "late").await;
        assert!(late.is_err(), "C4/O0 occupied ticket rejects a new key");
        assert_eq!(
            store_a.get(&session).await.unwrap(),
            root,
            "C3/C4 root unchanged"
        );
        assert_eq!(
            session_outbox(&store_a, &session).await,
            outbox,
            "C3/C4 this Session's outbox unchanged"
        );
        assert!(
            store_a
                .pending_lifecycle()
                .await
                .unwrap()
                .contains(&unrelated),
            "C6 unrelated Session fact remains durable"
        );
        assert_eq!(
            a.runtime.tool_reply_calls.load(Ordering::SeqCst)
                + b.runtime.tool_reply_calls.load(Ordering::SeqCst),
            0,
            "C1-C4 admission performs no Runtime delivery"
        );
        signatures.push(format!(
            "changed={changed} outcomes={} batches={} replay={} late={}",
            {
                let mut rendered = outcomes.iter().map(normalized).collect::<Vec<_>>();
                rendered.sort();
                rendered.join("|")
            },
            root.event_batches.len(),
            normalized(&replay),
            normalized(&late),
        ));
    }

    // C5: both clients submit the SAME key and reply concurrently.
    let id = unique(&format!("{prefix}-same-key"));
    let session = SessionId::from(id.as_str());
    create(store_a.as_ref(), persisted(&id, false, "idle")).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let a = client(store_a.clone(), Some(barrier.clone()));
    let b = client(store_b.clone(), Some(barrier));
    let input = tool_reply_input("same-key-call");
    let (ra, rb) = tokio::join!(
        append(&a, &session, input.clone(), "shared"),
        append(&b, &session, input.clone(), "shared"),
    );
    let (ra, rb) = (ra.expect("C5 left"), rb.expect("C5 right"));
    assert_eq!(
        ra, rb,
        "C5/O1 both callers receive the one canonical receipt"
    );
    assert_eq!(
        store_a.get(&session).await.unwrap().event_batches,
        vec![ra],
        "C5/O0 one consumption"
    );
    signatures.push("same-key=one-receipt".into());
    signatures
}

async fn sqlite_signatures() -> Vec<String> {
    let directory = tempfile::tempdir().expect("T11 SQLite directory");
    let path = directory.path().join("t11-consumption.db");
    let path = path.to_str().unwrap();
    let store_a: Store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(path)
            .await
            .expect("T11 left SQLite client"),
    );
    let store_b: Store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(path)
            .await
            .expect("T11 right SQLite client"),
    );
    consumption_table("t11-sqlite", store_a, store_b).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_independent_clients_consume_one_tool_ticket_at_most_once() {
    let signatures = sqlite_signatures().await;
    assert_eq!(signatures.len(), 3, "C1-C5 executed on file SQLite");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let store_a: Store = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("T11 left PostgreSQL pool"),
    );
    let store_b: Store = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("T11 right PostgreSQL pool"),
    );
    let postgres = consumption_table("t11-postgres", store_a, store_b).await;
    let sqlite = sqlite_signatures().await;
    assert_eq!(
        postgres, sqlite,
        "X/O2 the conflict outcome is identical across SQL backends"
    );
}

#[test]
fn postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::event_batches::concurrent_consumption::postgres_independent_clients_consume_one_tool_ticket_identically_to_sqlite",
    );
}
