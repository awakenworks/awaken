//! T12-C03 (暂停与事务失败), SQL rows: a budget-paused Session root receives
//! interleaved ordinary and interrupt-only Event batches, and a write fails
//! in the middle of the batch transaction.
//!
//! Oracle O1 (groups-2.json order index 1): records written in one
//! transaction roll back together. The policy rows (O0) run through the
//! application admission in
//! `awaken-session-application/src/tests/event_batches/pause_transaction.rs`.
//!
//! The failure is a real SQL trigger on `managed_session_idempotency`, which
//! the commit writes AFTER it updated the root row and synchronized the root
//! indexes and BEFORE the lifecycle outbox insert, so it fails mid-write.
//!
//! | Row | Batch | Fault | Expected |
//! |---|---|---|---|
//! | T1 | ordinary | - | applied: root batch + receipt + fact |
//! | T2 | interrupt-only | - | applied: root batch + receipt + fact |
//! | T3 | ordinary | idempotency INSERT trigger | error; root row, indexes, receipts and outbox byte-equal to before |
//! | T4 | exact T3 retry after the fault clears | - | applied once; replay returns the receipt; no duplicate fact |
//! | T5 | file reopen (SQLite) | - | T1/T2/T4 durable, T3's failed attempt absent |
//! | NC | complete T4 commit, then its receipt row removed (a half-committed batch) | - | the same atomicity checker rejects it |
use std::time::Duration;

use awaken_session_application::SessionStore;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionAggregate, SessionMutation,
    SessionMutationResult,
};
use awaken_session_contract::{
    SessionEventBatch, SessionEventInput, SessionEventInterrupt, SessionEventProjectionAnchor,
};
use awaken_session_contract::{SessionRepository as _, SessionRepositoryReads as _};
use awaken_session_store::{PostgresManagedSessionRepository, SqliteManagedSessionRepository};
use awaken_session_types::{SessionId, SessionRevision};
use awaken_store_conformance::session_store::{create_session, fact, session};
use sqlx::Executor as _;
use sqlx::postgres::PgPoolOptions;

const CONSUMED_MINOR: u64 = 2;

fn paused(id: &str) -> PersistedSession {
    let mut value = session(id, "paused");
    value.execution = awaken_session_contract::SessionExecutionState::Idle;
    value.budget = awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: u128::from(CONSUMED_MINOR)
            * awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot: awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "t12-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "t12-prices-v1-fingerprint".into(),
        },
        reach_transitions: Vec::new(),
    };
    value
}

fn ordinary(text: &str) -> SessionEventInput {
    SessionEventInput::UserMessage {
        content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
            text,
        )],
    }
}

fn interrupt_only() -> SessionEventInput {
    SessionEventInput::Interrupt(SessionEventInterrupt {
        requested_target: None,
        targets: vec![awaken_session_contract::SessionThreadTarget::Primary],
    })
}

struct BatchCommand {
    batch_id: String,
    key: String,
    fact: ManagedLifecycleFact,
    mutation: SessionMutation,
}

/// Append one batch through the Session aggregate law and compile the one
/// root mutation that carries the batch, its receipt and its lifecycle fact.
fn batch_command(root: &PersistedSession, row: &str, input: SessionEventInput) -> BatchCommand {
    let mut aggregate = SessionAggregate::try_from(root.clone()).expect("T12 aggregate");
    let next = SessionRevision(root.revision.0 + 1);
    let batch_id = awaken_session_contract::session_event_batch_id(&root.session_id, next)
        .expect("T12 batch id");
    let mut batch = SessionEventBatch::compile(&root.session_id, batch_id.clone(), vec![input])
        .expect("T12 grammar-valid batch");
    batch.admitted_revision = next;
    batch
        .anchor_admission_projection(SessionEventProjectionAnchor {
            source_commit_cursor: 0,
        })
        .expect("T12 admission anchor");
    aggregate
        .events()
        .append_batch(batch)
        .expect("T12 aggregate admits the batch");
    let key = format!("test:t12:{}:{row}", root.session_id);
    let fact = fact(
        &format!("session:{}:t12:{row}", root.session_id),
        root.session_id.as_str(),
        "session.status_idled",
    );
    let payload =
        awaken_session_contract::SessionMutationPayload::Replace(aggregate.snapshot().clone());
    let idempotency = IdempotencyRecord {
        key: key.clone(),
        payload_hash: payload.stable_hash(),
    };
    let mutation = aggregate.compile_mutation("ws_a", idempotency, vec![fact.clone()], 1_000);
    BatchCommand {
        batch_id,
        key,
        fact,
        mutation,
    }
}

/// Everything one batch commit may write, read back through the port and the
/// raw root indexes.
#[derive(Debug, PartialEq)]
struct Observed {
    root: PersistedSession,
    receipt: Option<awaken_session_contract::SessionIdempotencyReceipt>,
    facts: Vec<ManagedLifecycleFact>,
    raw: RawRows,
}

#[derive(Clone, Debug, PartialEq)]
struct RawRows {
    root_revision: i64,
    idempotency_rows: i64,
    reconciliation_rows: i64,
    outbox_rows: i64,
}

async fn observe(store: &dyn SessionStore, id: &SessionId, key: &str, raw: RawRows) -> Observed {
    Observed {
        root: store.get(id).await.expect("observe root"),
        receipt: store
            .idempotency_receipt(id, key)
            .await
            .expect("observe receipt"),
        facts: store
            .pending_lifecycle()
            .await
            .expect("observe outbox")
            .into_iter()
            .filter(|fact| fact.object_id == id.as_str())
            .collect(),
        raw,
    }
}

/// The shared all-or-nothing checker: a batch's root entry, receipt and fact
/// are either all present or all absent. Returns whether it is present.
fn batch_atomicity(observed: &Observed, command: &BatchCommand) -> Result<bool, String> {
    let in_root = observed
        .root
        .event_batches
        .iter()
        .any(|batch| batch.batch_id == command.batch_id);
    let receipt = observed.receipt.is_some();
    let fact = observed.facts.contains(&command.fact);
    if in_root == receipt && receipt == fact {
        Ok(in_root)
    } else {
        Err(format!(
            "half-committed batch: root={in_root} receipt={receipt} fact={fact}"
        ))
    }
}

trait Backend {
    async fn raw(&self, id: &SessionId) -> RawRows;
    async fn arm(&self, id: &SessionId);
    async fn disarm(&self);
    async fn delete_receipt(&self, id: &SessionId, key: &str);
}

async fn rollback_table<S: SessionStore, B: Backend>(
    store: &S,
    backend: &B,
    id: &str,
) -> PersistedSession {
    let session_id = SessionId::from(id);
    let created = create_session(store, "ws_a", paused(id), Vec::new()).await;
    assert!(
        awaken_session_contract::session_is_budget_paused(&created),
        "T12 root is budget paused"
    );

    // T1/T2: interleaved ordinary and interrupt-only batches commit fully.
    let mut root = created;
    let mut committed = Vec::new();
    for (row, input) in [("t1", ordinary("first")), ("t2", interrupt_only())] {
        let command = batch_command(&root, row, input);
        assert_eq!(
            store.commit("ws_a", command.mutation.clone()).await,
            Ok(SessionMutationResult::Applied {
                new_revision: SessionRevision(root.revision.0 + 1)
            }),
            "{row} applied"
        );
        root = store.get(&session_id).await.unwrap();
        let observed = observe(
            store,
            &session_id,
            &command.key,
            backend.raw(&session_id).await,
        )
        .await;
        assert_eq!(batch_atomicity(&observed, &command), Ok(true), "{row}/O1");
        committed.push(command);
    }

    // T3: the idempotency INSERT fails after the root UPDATE in the same
    // transaction.
    let failing = batch_command(&root, "t3", ordinary("third"));
    let before = observe(
        store,
        &session_id,
        &failing.key,
        backend.raw(&session_id).await,
    )
    .await;
    backend.arm(&session_id).await;
    let failed = store.commit("ws_a", failing.mutation.clone()).await;
    // T3/C3 is before COMMIT, so the actual SQLite/PostgreSQL transaction
    // certifies this attempt did not commit. The frame below independently
    // observes rollback; a generic unknown error cannot satisfy this rule.
    assert!(
        matches!(
            failed,
            Err(awaken_session_contract::SessionRepositoryError::NotApplied(
                _
            ))
        ),
        "T3 the mid-write fault is a proven rollback"
    );
    let after = observe(
        store,
        &session_id,
        &failing.key,
        backend.raw(&session_id).await,
    )
    .await;
    assert_eq!(
        after, before,
        "T3/O1 every record of the batch rolled back together"
    );
    assert_eq!(batch_atomicity(&after, &failing), Ok(false), "T3/O1 absent");

    // T4: the exact retry after the fault clears commits exactly once.
    backend.disarm().await;
    let next = SessionRevision(root.revision.0 + 1);
    assert_eq!(
        store.commit("ws_a", failing.mutation.clone()).await,
        Ok(SessionMutationResult::Applied { new_revision: next }),
        "T4 exact retry applies"
    );
    assert_eq!(
        store.commit("ws_a", failing.mutation.clone()).await,
        Ok(SessionMutationResult::Replayed { new_revision: next }),
        "T4 replay returns the receipt"
    );
    let retried = observe(
        store,
        &session_id,
        &failing.key,
        backend.raw(&session_id).await,
    )
    .await;
    assert_eq!(
        batch_atomicity(&retried, &failing),
        Ok(true),
        "T4/O1 complete"
    );
    assert_eq!(
        retried
            .facts
            .iter()
            .filter(|fact| **fact == failing.fact)
            .count(),
        1,
        "T4 no duplicate fact"
    );
    for command in &committed {
        let observed = observe(
            store,
            &session_id,
            &command.key,
            backend.raw(&session_id).await,
        )
        .await;
        assert_eq!(batch_atomicity(&observed, command), Ok(true), "T1/T2 kept");
    }

    no_op_receipt_table(store, backend, &session_id).await;
    no_op_deletion_table(store, &session_id).await;

    // NC: a half-committed batch (its receipt row removed out of band) must
    // be rejected by the same checker.
    backend.delete_receipt(&session_id, &failing.key).await;
    let half = observe(
        store,
        &session_id,
        &failing.key,
        backend.raw(&session_id).await,
    )
    .await;
    assert!(
        batch_atomicity(&half, &failing).is_err(),
        "NC the atomicity checker detects a half-committed batch"
    );
    retried.root
}

fn no_op_command(root: &PersistedSession, key: &str, fingerprint: &str) -> SessionMutation {
    SessionMutation {
        expected_revision: root.revision,
        idempotency: IdempotencyRecord::event_admission(key, fingerprint),
        payload: awaken_session_contract::SessionMutationPayload::EventNoOp {
            session_id: root.session_id.clone(),
            interrupts: vec![SessionEventInterrupt {
                requested_target: None,
                targets: vec![awaken_session_contract::SessionThreadTarget::Primary],
            }],
        },
        lifecycle_facts: Vec::new(),
    }
}

async fn no_op_deletion_table<S: SessionStore>(store: &S, original: &SessionId) {
    // DT-D: C1 accepted no-op has only a command-index receipt; C2 a later
    // canonical root transition completes terminal cleanup; C3 deletion commits
    // or replays. D1 C1+C2+C3 => the SAME owner-bound tombstone keeps explicit
    // known-empty NoOp; D2 changed fingerprint => conflict; D3 foreign owner =>
    // no proof. Root absence alone never satisfies D1. Both SQL engines run it.
    use awaken_session_contract::{SessionEventAdmission, resolve_deleted_event_admission};
    use awaken_store_conformance::session_store::{complete_terminal_cleanup, delete_mutation};
    let id = SessionId::from(format!("{}-noop-deletion", original.as_str()));
    let mut root = create_session(store, "ws_a", paused(id.as_str()), Vec::new()).await;
    let noop = no_op_command(&root, "deleted-noop", "deleted-fingerprint");
    assert!(matches!(
        store.commit("ws_a", noop).await.unwrap(),
        SessionMutationResult::AcknowledgedNoOp { .. }
    ));
    root.request_delete("ws_a", 0).unwrap().unwrap();
    complete_terminal_cleanup(&mut root);
    let mut aggregate = SessionAggregate::try_from(root).unwrap();
    let terminal = aggregate.compile_mutation(
        "ws_a",
        IdempotencyRecord {
            key: "test:noop-terminal".into(),
            payload_hash: "terminal".into(),
        },
        Vec::new(),
        1,
    );
    assert!(matches!(
        store.commit("ws_a", terminal).await.unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    let root = store.get(&id).await.unwrap();
    let delete = delete_mutation(&root, "test:noop-delete");
    for replay in [false, true] {
        let result = store.commit("ws_a", delete.clone()).await.unwrap();
        assert_eq!(
            matches!(result, SessionMutationResult::Replayed { .. }),
            replay
        );
        let proof = store
            .deleted_event_admissions("ws_a", &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            resolve_deleted_event_admission(&proof, "deleted-noop", "deleted-fingerprint").unwrap(),
            SessionEventAdmission::NoOp,
            "D1 explicit outcome survives deletion/replay"
        );
        assert_eq!(
            resolve_deleted_event_admission(&proof, "deleted-noop", "changed")
                .unwrap_err()
                .code,
            "idempotency_conflict",
            "D2"
        );
        assert!(
            store
                .deleted_event_admissions("foreign", &id)
                .await
                .unwrap()
                .is_none(),
            "D3"
        );
    }
}

async fn no_op_receipt_table<S: SessionStore, B: Backend>(store: &S, backend: &B, id: &SessionId) {
    // DT-N (both real engines): C1 current paused root + pure Interrupt;
    // C2 new/exact/changed receipt fingerprint; C3 two concurrent same-key
    // callers. N1 new => one EventNoOp receipt, unchanged root/index/outbox;
    // N2 exact => same acknowledgement, no extra row; N3 changed => conflict;
    // N4 C3 => both acknowledge one receipt although root revision never moves.
    // These rows would reject a fake empty batch or a no-op with no durable fact.
    use awaken_session_contract::SessionCommandReceiptKind;
    let before = store.get(id).await.unwrap();
    let raw = backend.raw(id).await;
    let outbox = store.pending_lifecycle().await.unwrap();
    let command = |key: &str| no_op_command(&before, key, "noop-fingerprint");
    let wanted = SessionMutationResult::AcknowledgedNoOp {
        observed_revision: before.revision,
    };
    let first = command("noop-first");
    assert_eq!(
        store.commit("ws_a", first.clone()).await.unwrap(),
        wanted,
        "N1"
    );
    assert_eq!(
        store.commit("ws_a", first.clone()).await.unwrap(),
        wanted,
        "N2"
    );
    let receipt = store
        .idempotency_receipt(id, &first.idempotency.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.kind(), SessionCommandReceiptKind::EventNoOp);
    assert_eq!(receipt.committed_revision, before.revision);
    assert_eq!(receipt.payload_hash, "noop-fingerprint");
    let after = backend.raw(id).await;
    assert_eq!(after.root_revision, raw.root_revision);
    assert_eq!(after.reconciliation_rows, raw.reconciliation_rows);
    assert_eq!(after.outbox_rows, raw.outbox_rows);
    assert_eq!(
        after.idempotency_rows,
        raw.idempotency_rows + 1,
        "N1/N2 one receipt"
    );
    let mut changed = first;
    changed.idempotency.payload_hash = "changed".into();
    assert_eq!(
        store.commit("ws_a", changed).await.unwrap(),
        SessionMutationResult::IdempotencyMismatch,
        "N3"
    );
    let race = command("noop-race");
    let (left, right) = tokio::join!(
        store.commit("ws_a", race.clone()),
        store.commit("ws_a", race.clone())
    );
    assert_eq!(left.unwrap(), wanted, "N4 left");
    assert_eq!(right.unwrap(), wanted, "N4 right");
    assert_eq!(
        backend.raw(id).await.idempotency_rows,
        raw.idempotency_rows + 2,
        "N4 one concurrent winner"
    );
    assert_eq!(
        store.get(id).await.unwrap(),
        before,
        "N1-N4 whole root frame"
    );
    assert_eq!(
        store.pending_lifecycle().await.unwrap(),
        outbox,
        "N1-N4 no lifecycle fact"
    );
}

async fn postgres_noop_waiter_table(
    store: &PostgresManagedSessionRepository,
    pool: &sqlx::PgPool,
    schema: &str,
    id: &SessionId,
) {
    // N5: hold the real root lock, then wait until BOTH independent writers
    // have passed their initial absent-receipt read and block on that root.
    // Releasing it must yield two acknowledgements and one fact despite no
    // revision increment. Removing the post-lock receipt lookup makes the
    // second INSERT fail its unique constraint, which this oracle rejects.
    let before = store.get(id).await.unwrap();
    let mutation = no_op_command(&before, "pg-gated-noop", "gated-fingerprint");
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT session_id FROM managed_session WHERE session_id=$1 FOR UPDATE")
        .bind(id.as_str())
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let callers = async {
        tokio::join!(
            store.commit("ws_a", mutation.clone()),
            store.commit("ws_a", mutation.clone())
        )
    };
    let release = async {
        awaken_reliability_testkit::wait_for_postgres_lock_waiters(
            pool,
            schema,
            2,
            Some("FOR UPDATE"),
            Duration::from_secs(10),
        )
        .await
        .expect("N5 both writers reach the real root lock");
        held.commit().await.unwrap();
    };
    let ((left, right), ()) = tokio::join!(callers, release);
    let expected = SessionMutationResult::AcknowledgedNoOp {
        observed_revision: before.revision,
    };
    assert_eq!(left.unwrap(), expected, "N5 left");
    assert_eq!(right.unwrap(), expected, "N5 right");
    assert_eq!(store.get(id).await.unwrap(), before, "N5 root frame");
}

struct Sqlite {
    pool: sqlx::SqlitePool,
}

impl Backend for Sqlite {
    async fn raw(&self, id: &SessionId) -> RawRows {
        raw_rows_sqlite(&self.pool, id).await
    }

    async fn arm(&self, id: &SessionId) {
        // SQL safety: DDL cannot bind trigger literals. Quote the literal's
        // apostrophes; every SQL identifier and the trigger body are static.
        let literal_id = id.as_str().replace('\'', "''");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER t12_reject_idempotency BEFORE INSERT ON managed_session_idempotency \
             WHEN NEW.session_id = '{literal_id}' \
             BEGIN SELECT RAISE(ABORT, 't12 injected mid-write failure'); END;"
        )))
        .execute(&self.pool)
        .await
        .expect("install SQLite T12 trigger");
    }

    async fn disarm(&self) {
        sqlx::raw_sql("DROP TRIGGER t12_reject_idempotency")
            .execute(&self.pool)
            .await
            .expect("drop SQLite T12 trigger");
    }

    async fn delete_receipt(&self, id: &SessionId, key: &str) {
        sqlx::query(
            "DELETE FROM managed_session_idempotency WHERE session_id = ?1 AND idempotency_key = ?2",
        )
        .bind(id.as_str())
        .bind(key)
        .execute(&self.pool)
        .await
        .expect("NC remove receipt");
    }
}

async fn raw_rows_sqlite(pool: &sqlx::SqlitePool, id: &SessionId) -> RawRows {
    let count = |sql: &'static str| {
        let pool = pool.clone();
        let id = id.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("raw SQLite row read")
        }
    };
    RawRows {
        root_revision: count("SELECT revision FROM managed_session WHERE session_id = ?1").await,
        idempotency_rows: count(
            "SELECT count(*) FROM managed_session_idempotency WHERE session_id = ?1",
        )
        .await,
        reconciliation_rows: count(
            "SELECT count(*) FROM managed_session_reconciliation_work WHERE session_id = ?1",
        )
        .await,
        outbox_rows: count("SELECT count(*) FROM managed_lifecycle_outbox WHERE fact_id LIKE 'session:' || ?1 || ':%'")
            .await,
    }
}

#[tokio::test]
async fn sqlite_mid_write_failure_rolls_back_the_whole_batch_transaction() {
    let directory = tempfile::tempdir().expect("T12 SQLite directory");
    let path = directory.path().join("t12-pause.db");
    let path_str = path.to_str().unwrap();
    let store = SqliteManagedSessionRepository::open(path_str)
        .await
        .expect("T12 SQLite store");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .busy_timeout(Duration::from_secs(30)),
        )
        .await
        .expect("raw SQLite fault connection");
    let backend = Sqlite { pool };
    let id = "sesn_t12_sqlite";
    let durable = rollback_table(&store, &backend, id).await;
    backend.pool.close().await;
    drop(store);
    // T5: reopen the file; the retried batch and both earlier batches are
    // durable, and the failed attempt left nothing behind.
    let reopened = SqliteManagedSessionRepository::open(path_str)
        .await
        .expect("T5 reopen");
    use awaken_session_contract::SessionRepositoryReads as _;
    assert_eq!(
        reopened.get(&SessionId::from(id)).await.unwrap(),
        durable,
        "T5 durable root after reopen"
    );
}

struct Postgres {
    pool: sqlx::PgPool,
}

impl Backend for Postgres {
    async fn raw(&self, id: &SessionId) -> RawRows {
        let count = |sql: &'static str| {
            let pool = self.pool.clone();
            let id = id.to_string();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("raw PostgreSQL row read")
            }
        };
        RawRows {
            root_revision: count("SELECT revision FROM managed_session WHERE session_id = $1")
                .await,
            idempotency_rows: count(
                "SELECT count(*) FROM managed_session_idempotency WHERE session_id = $1",
            )
            .await,
            reconciliation_rows: count(
                "SELECT count(*) FROM managed_session_reconciliation_work WHERE session_id = $1",
            )
            .await,
            outbox_rows: count(
                "SELECT count(*) FROM managed_lifecycle_outbox WHERE fact_id LIKE 'session:' || $1 || ':%'",
            )
            .await,
        }
    }

    async fn arm(&self, id: &SessionId) {
        // SQL safety: the interpolated trigger value is a quoted SQL literal;
        // identifiers and the separate PL/pgSQL function body stay static.
        let literal_id = id.as_str().replace('\'', "''");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE FUNCTION t12_reject_idempotency() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 't12 injected mid-write failure'; END $$; \
             CREATE TRIGGER t12_reject_idempotency BEFORE INSERT ON managed_session_idempotency \
             FOR EACH ROW WHEN (NEW.session_id = '{literal_id}') EXECUTE FUNCTION t12_reject_idempotency();"
        )))
        .execute(&self.pool)
        .await
        .expect("install PostgreSQL T12 trigger");
    }

    async fn disarm(&self) {
        sqlx::raw_sql(
            "DROP TRIGGER t12_reject_idempotency ON managed_session_idempotency; \
             DROP FUNCTION t12_reject_idempotency();",
        )
        .execute(&self.pool)
        .await
        .expect("drop PostgreSQL T12 trigger");
    }

    async fn delete_receipt(&self, id: &SessionId, key: &str) {
        sqlx::query(
            "DELETE FROM managed_session_idempotency WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(id.as_str())
        .bind(key)
        .execute(&self.pool)
        .await
        .expect("NC remove receipt");
    }
}

#[tokio::test]
async fn postgres_mid_write_failure_rolls_back_the_whole_batch_transaction() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("explicit T12 PostgreSQL fixture must connect");
    let schema = format!("t_session_t12_{}", uuid::Uuid::new_v4().simple());
    admin
        // SQL safety: fixture-owned identifiers are UUID/PID-derived or closed literals.
        .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .await
        .expect("create owned T12 schema");
    let owned = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let schema = owned.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(&schema)
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SELECT set_config('application_name', $1, false)")
                    .bind(&schema)
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect owned T12 schema");
    let store = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("T12 PostgreSQL migrations");
    let backend = Postgres { pool: pool.clone() };
    rollback_table(&store, &backend, "sesn_t12_postgres").await;
    postgres_noop_waiter_table(
        &store,
        &pool,
        &schema,
        &SessionId::from("sesn_t12_postgres"),
    )
    .await;
    drop(store);
    pool.close().await;
    admin
        // SQL safety: fixture-owned identifiers are UUID/PID-derived or closed literals.
        .execute(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .await
        .expect("drop owned T12 schema");
    admin.close().await;
}

#[test]
fn postgres_mid_write_failure_rolls_back_the_whole_batch_transaction_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_mid_write_failure_rolls_back_the_whole_batch_transaction",
    );
}
