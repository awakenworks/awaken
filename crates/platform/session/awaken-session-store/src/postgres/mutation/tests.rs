// @formal surface exclude=test_only
// @formal owner crates/platform/session/awaken-session-store/src/postgres/mutation.rs
// @formal reason: Caller-owned transaction, receipt replay and retained privacy controls on the original PostgreSQL mutation kernel.
use super::PostgresManagedSessionRepository;
use awaken_session_contract::{
    IdempotencyRecord, LifecycleOutbox as _, ManagedListPriceSnapshot, PersistedSession,
    SessionAggregate, SessionBudgetState, SessionCommandReceiptKind, SessionEventAdmission,
    SessionEventInterrupt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionPrivateBinding, SessionRepository as _, SessionRepositoryError,
    SessionRepositoryReads as _, SessionThreadTarget, resolve_deleted_event_admission,
};
use awaken_session_types::SessionRevision;
use awaken_store_conformance::session_store::{
    complete_terminal_cleanup, create_session, delete_mutation, fact, record, record_update,
    session,
};
use sqlx::{Executor as _, PgPool};
use std::time::Duration;

struct Fixture {
    repository: PostgresManagedSessionRepository,
    admin: PgPool,
    schema: String,
}

impl Fixture {
    async fn open() -> Option<Self> {
        let (url, admin) = crate::tests::postgres_test_admin().await?;
        let schema = format!("t_borrowed_root_{}", uuid::Uuid::new_v4().simple());
        // Only this fixture's UUID-derived identifier enters DDL. All business
        // values below are bound, and production writes use commit_on/commit.
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .await
            .unwrap();
        let pool = awaken_sql_storage::postgres_pool_options()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&crate::tests::scoped_postgres_url(&url, &schema))
            .await
            .unwrap();
        let repository = PostgresManagedSessionRepository::with_pool(pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE borrowed_mutation_sentinel (marker INTEGER PRIMARY KEY)")
            .execute(&repository.pool)
            .await
            .unwrap();
        Some(Self {
            repository,
            admin,
            schema,
        })
    }

    async fn close(self) {
        self.repository.pool.close().await;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .await
            .unwrap();
        self.admin.close().await;
    }
}

fn replacement(root: &PersistedSession, key: &str) -> SessionMutation {
    let mut candidate = root.clone();
    candidate.title = Some("borrowed replacement".into());
    let payload =
        SessionMutationPayload::Replace(record_update(root, candidate, 0).into_snapshot());
    SessionMutation {
        expected_revision: root.revision,
        idempotency: record(key, &payload),
        payload,
        lifecycle_facts: vec![fact(
            "borrowed-replaced",
            root.session_id.as_str(),
            "session.status_idled",
        )],
    }
}

fn replaced_root(mutation: &SessionMutation) -> PersistedSession {
    let SessionMutationPayload::Replace(mut expected) = mutation.payload.clone() else {
        panic!("replacement fixture carries a Replace payload");
    };
    expected.revision = SessionRevision(mutation.expected_revision.0 + 1);
    expected
}

#[tokio::test]
async fn postgres_borrowed_commit_keeps_caller_transaction_and_rolls_back_late_sql_failure() {
    // B1 cap1 + caller-held connection => the original kernel returns without
    // another checkout, and its root/receipt/fact share the sentinel's txid.
    // B2 caller rollback => none of those writes survives. B3 an actual outbox
    // INSERT failure occurs after Root/index/receipt writes => AuthorityUnavailable,
    // never a premature NotApplied certificate; the outer rollback proves B2.
    // B4 the owned entry alone certifies its failed transaction, then retry
    // delegates to the same kernel and persists exactly one complete mutation.
    let Some(fixture) = Fixture::open().await else {
        return;
    };
    let repository = &fixture.repository;
    let root = create_session(
        repository,
        "ws_a",
        session("borrowed-rollback", "before"),
        Vec::new(),
    )
    .await;
    let mutation = replacement(&root, "borrowed-replace");
    let expected = replaced_root(&mutation);
    let mut transaction = repository.pool.begin().await.unwrap();
    let txid: i64 = sqlx::query_scalar("SELECT txid_current()")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("INSERT INTO borrowed_mutation_sentinel (marker) VALUES (1)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(10),
            PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", mutation.clone(),)
        )
        .await
        .expect("B1 cap1 borrowed kernel must not check out again")
        .unwrap(),
        SessionMutationResult::Applied {
            new_revision: expected.revision
        },
    );
    assert_eq!(
        super::super::postgres_session_root_on(&mut transaction, &root.session_id)
            .await
            .unwrap(),
        expected
    );
    assert!(
        PostgresManagedSessionRepository::idempotency_receipt_on(
            &mut transaction,
            &root.session_id,
            &mutation.idempotency.key,
        )
        .await
        .unwrap()
        .is_some()
    );
    let same_txid: i64 = sqlx::query_scalar("SELECT txid_current()")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(
        same_txid, txid,
        "B1 the callee did not complete the caller transaction"
    );
    let staged_facts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM managed_lifecycle_outbox")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(
        staged_facts, 1,
        "B1 fact shares the caller's uncommitted transaction"
    );
    let staged_revisions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM managed_session_revision WHERE session_id = $1")
            .bind(root.session_id.as_str())
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(
        staged_revisions, 2,
        "B1 the root history append shares this transaction"
    );
    transaction.rollback().await.unwrap();
    assert_eq!(repository.get(&root.session_id).await.unwrap(), root);
    assert!(
        repository
            .idempotency_receipt(&root.session_id, &mutation.idempotency.key)
            .await
            .unwrap()
            .is_none()
    );
    assert!(repository.pending_lifecycle().await.unwrap().is_empty());
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM borrowed_mutation_sentinel")
        .fetch_one(&repository.pool)
        .await
        .unwrap();
    assert_eq!(retained, 0, "B2 only the outer owner chose rollback");

    // This trigger injects a real late SQL error; it performs no production
    // mutation itself and cannot make a partial application look successful.
    sqlx::raw_sql(
        "CREATE FUNCTION borrowed_reject_outbox() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'borrowed late outbox failure'; END $$; \
         CREATE TRIGGER borrowed_reject_outbox BEFORE INSERT ON managed_lifecycle_outbox \
         FOR EACH ROW EXECUTE FUNCTION borrowed_reject_outbox();",
    )
    .execute(&repository.pool)
    .await
    .unwrap();
    let mut transaction = repository.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO borrowed_mutation_sentinel (marker) VALUES (2)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let error =
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", mutation.clone())
            .await
            .unwrap_err();
    assert!(
        matches!(error, SessionRepositoryError::AuthorityUnavailable(_)),
        "B3 outer outcome is still unknown to the borrowed kernel: {error:?}"
    );
    transaction.rollback().await.unwrap();
    assert_eq!(repository.get(&root.session_id).await.unwrap(), root);
    assert!(
        repository
            .idempotency_receipt(&root.session_id, &mutation.idempotency.key)
            .await
            .unwrap()
            .is_none()
    );
    assert!(repository.pending_lifecycle().await.unwrap().is_empty());
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM borrowed_mutation_sentinel")
        .fetch_one(&repository.pool)
        .await
        .unwrap();
    assert_eq!(
        retained, 0,
        "B3 late failure and sentinel roll back together"
    );
    let retained_revisions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM managed_session_revision WHERE session_id = $1")
            .bind(root.session_id.as_str())
            .fetch_one(&repository.pool)
            .await
            .unwrap();
    assert_eq!(
        retained_revisions, 1,
        "B3 the failed history append rolls back too"
    );
    assert!(
        matches!(
            repository.commit("ws_a", mutation.clone()).await,
            Err(SessionRepositoryError::NotApplied(_))
        ),
        "B4 owned adapter alone adds the no-commit certificate"
    );
    sqlx::raw_sql(
        "DROP TRIGGER borrowed_reject_outbox ON managed_lifecycle_outbox; \
         DROP FUNCTION borrowed_reject_outbox();",
    )
    .execute(&repository.pool)
    .await
    .unwrap();
    assert_eq!(
        repository.commit("ws_a", mutation.clone()).await.unwrap(),
        SessionMutationResult::Applied {
            new_revision: expected.revision
        }
    );
    assert_eq!(repository.get(&root.session_id).await.unwrap(), expected);
    assert_eq!(
        repository.pending_lifecycle().await.unwrap(),
        mutation.lifecycle_facts
    );
    fixture.close().await;
}

#[tokio::test]
async fn postgres_borrowed_commit_replays_exact_receipt_after_lost_acknowledgement() {
    // A1 a real outer COMMIT succeeds but its result is not delivered => an
    // exact retry returns that original receipt, Root and lifecycle fact once.
    // A2 replay must not commit/rollback a caller's unrelated sentinel: the
    // second transaction can observe both markers and roll its own one back.
    // This is an explicit lost-return simulation, not a claim of network ACK-loss
    // injection; the original process-death/COMMIT qualification remains separate.
    let Some(fixture) = Fixture::open().await else {
        return;
    };
    let repository = &fixture.repository;
    let root = create_session(
        repository,
        "ws_a",
        session("borrowed-lost-ack", "before"),
        Vec::new(),
    )
    .await;
    let mutation = replacement(&root, "borrowed-lost-ack");
    let expected = replaced_root(&mutation);
    let mut transaction = repository.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO borrowed_mutation_sentinel (marker) VALUES (1)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let undelivered =
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", mutation.clone())
            .await
            .unwrap();
    assert_eq!(
        undelivered,
        SessionMutationResult::Applied {
            new_revision: expected.revision
        }
    );
    transaction.commit().await.unwrap();
    let _ = undelivered; // The retry below has no application acknowledgement.
    let receipt = repository
        .idempotency_receipt(&root.session_id, &mutation.idempotency.key)
        .await
        .unwrap()
        .unwrap();
    let mut transaction = repository.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO borrowed_mutation_sentinel (marker) VALUES (2)")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let txid: i64 = sqlx::query_scalar("SELECT txid_current()")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", mutation.clone())
            .await
            .unwrap(),
        SessionMutationResult::Replayed {
            new_revision: expected.revision
        }
    );
    let same_txid: i64 = sqlx::query_scalar("SELECT txid_current()")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(
        same_txid, txid,
        "A2 replay never completes its caller transaction"
    );
    let staged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM borrowed_mutation_sentinel")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(staged, 2, "A2 unrelated caller write is still live");
    transaction.rollback().await.unwrap();
    assert_eq!(
        repository.commit("ws_a", mutation.clone()).await.unwrap(),
        SessionMutationResult::Replayed {
            new_revision: expected.revision
        }
    );
    assert_eq!(repository.get(&root.session_id).await.unwrap(), expected);
    assert_eq!(
        repository
            .idempotency_receipt(&root.session_id, &mutation.idempotency.key)
            .await
            .unwrap(),
        Some(receipt)
    );
    assert_eq!(
        repository.pending_lifecycle().await.unwrap(),
        mutation.lifecycle_facts
    );
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM borrowed_mutation_sentinel")
        .fetch_one(&repository.pool)
        .await
        .unwrap();
    assert_eq!(
        retained, 1,
        "A1/A2 only the acknowledged outer commit survives"
    );
    fixture.close().await;
}

#[tokio::test]
async fn postgres_borrowed_commit_preserves_private_binding_noop_and_delete_receipts() {
    // C1 decoded replacement strips private binding => typed refusal with no
    // Root/receipt/fact change. C2 accepted EventNoOp + exact replay => one receipt,
    // unchanged Root revision/outbox. C3 original hidden/completed cleanup =>
    // Delete retains that exact NoOp evidence and private-content bit; exact
    // borrowed/owned Delete replay cannot create another lifecycle fact.
    // Storage-only cleanup uses the existing conformance constructor, not a
    // new Runtime completion/authority path.
    let Some(fixture) = Fixture::open().await else {
        return;
    };
    let repository = &fixture.repository;
    let mut seed = session("borrowed-private-noop", "private paused");
    seed.budget = SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: 2
            * SessionBudgetState::MICROS_PER_MINOR_USD
            * SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: Default::default(),
        snapshot: ManagedListPriceSnapshot {
            snapshot_id: "borrowed-price".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "borrowed-price".into(),
        },
        reach_transitions: Vec::new(),
    };
    let seed = SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "ws_a".into(),
        1,
    )
    .unwrap()
    .bind_fresh(seed)
    .unwrap();
    let root = create_session(repository, "ws_a", seed, Vec::new()).await;
    let mut forged = serde_json::to_value(&root).unwrap();
    forged.as_object_mut().unwrap().remove("private_binding");
    let payload = SessionMutationPayload::Replace(serde_json::from_value(forged).unwrap());
    let forged = SessionMutation {
        expected_revision: root.revision,
        idempotency: record("borrowed-strip-private", &payload),
        payload,
        lifecycle_facts: Vec::new(),
    };
    let mut transaction = repository.pool.begin().await.unwrap();
    assert!(matches!(
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", forged.clone()).await,
        Err(SessionRepositoryError::InvalidMutation(_))
    ));
    assert_eq!(
        super::super::postgres_session_root_on(&mut transaction, &root.session_id)
            .await
            .unwrap(),
        root
    );
    assert!(
        PostgresManagedSessionRepository::idempotency_receipt_on(
            &mut transaction,
            &root.session_id,
            &forged.idempotency.key
        )
        .await
        .unwrap()
        .is_none()
    );
    transaction.rollback().await.unwrap();
    assert!(repository.pending_lifecycle().await.unwrap().is_empty());
    let noop = SessionMutation {
        expected_revision: root.revision,
        idempotency: IdempotencyRecord::event_admission("borrowed-noop", "borrowed-interrupt"),
        payload: SessionMutationPayload::EventNoOp {
            session_id: root.session_id.clone(),
            interrupts: vec![SessionEventInterrupt {
                requested_target: None,
                targets: vec![SessionThreadTarget::Primary],
            }],
        },
        lifecycle_facts: Vec::new(),
    };
    let mut transaction = repository.pool.begin().await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", noop.clone())
                .await
                .unwrap(),
            SessionMutationResult::AcknowledgedNoOp {
                observed_revision: root.revision
            }
        );
    }
    assert_eq!(
        super::super::postgres_session_root_on(&mut transaction, &root.session_id)
            .await
            .unwrap(),
        root
    );
    let receipt = PostgresManagedSessionRepository::idempotency_receipt_on(
        &mut transaction,
        &root.session_id,
        &noop.idempotency.key,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(receipt.kind(), SessionCommandReceiptKind::EventNoOp);
    assert_eq!(receipt.committed_revision, root.revision);
    transaction.commit().await.unwrap();
    assert_eq!(repository.get(&root.session_id).await.unwrap(), root);
    assert!(repository.pending_lifecycle().await.unwrap().is_empty());
    let mut terminal = root.clone();
    terminal.request_delete("ws_a", 0).unwrap().unwrap();
    complete_terminal_cleanup(&mut terminal);
    let mut aggregate = SessionAggregate::try_from(terminal).unwrap();
    let terminal = aggregate.compile_mutation(
        "ws_a",
        IdempotencyRecord {
            key: "borrowed-terminal".into(),
            payload_hash: "borrowed-terminal".into(),
        },
        Vec::new(),
        1,
    );
    let mut transaction = repository.pool.begin().await.unwrap();
    assert!(matches!(
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", terminal)
            .await
            .unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    transaction.commit().await.unwrap();
    let terminal = repository.get(&root.session_id).await.unwrap();
    let mut deletion = delete_mutation(&terminal, "borrowed-delete");
    deletion.lifecycle_facts.push(fact(
        "borrowed-deleted",
        root.session_id.as_str(),
        "session.deleted",
    ));
    let deleted_revision = SessionRevision(terminal.revision.0 + 1);
    let mut transaction = repository.pool.begin().await.unwrap();
    assert_eq!(
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", deletion.clone())
            .await
            .unwrap(),
        SessionMutationResult::Applied {
            new_revision: deleted_revision
        }
    );
    let private_content: i64 = sqlx::query_scalar(
        "SELECT private_content FROM managed_session_tombstone WHERE session_id = $1",
    )
    .bind(root.session_id.as_str())
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    assert_eq!(
        private_content, 1,
        "C3 private identity survives into the same tombstone transaction"
    );
    transaction.commit().await.unwrap();
    let proof = repository
        .deleted_event_admissions("ws_a", &root.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resolve_deleted_event_admission(&proof, "borrowed-noop", "borrowed-interrupt").unwrap(),
        SessionEventAdmission::NoOp
    );
    assert!(
        repository
            .deleted_event_admissions("foreign", &root.session_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        repository.get(&root.session_id).await,
        Err(SessionRepositoryError::NotFound)
    ));
    assert!(
        !repository
            .workspace_content_is_public("ws_a", &root.session_id)
            .await
            .unwrap()
    );
    let mut transaction = repository.pool.begin().await.unwrap();
    assert_eq!(
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", deletion.clone())
            .await
            .unwrap(),
        SessionMutationResult::Replayed {
            new_revision: deleted_revision
        }
    );
    transaction.rollback().await.unwrap();
    assert_eq!(
        repository.commit("ws_a", deletion.clone()).await.unwrap(),
        SessionMutationResult::Replayed {
            new_revision: deleted_revision
        }
    );
    assert_eq!(
        repository.pending_lifecycle().await.unwrap(),
        deletion.lifecycle_facts
    );
    fixture.close().await;
}

#[tokio::test]
async fn postgres_root_lock_serialization_failure_refuses_before_writing() {
    let Some(fixture) = Fixture::open().await else {
        return;
    };
    let repository = &fixture.repository;
    let root = create_session(
        repository,
        "ws_a",
        session("borrowed-serialization", "before"),
        Vec::new(),
    )
    .await;
    let mutation = replacement(&root, "borrowed-serialization-replace");
    let mut transaction = repository.pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await
        .unwrap();
    super::super::postgres_session_root_on(&mut transaction, &root.session_id)
        .await
        .unwrap();
    // Force a new PostgreSQL tuple while retaining the same logical revision
    // so the original mutation can be retried unchanged after rollback.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {}.managed_session SET revision = revision WHERE session_id = $1",
        fixture.schema
    )))
    .bind(root.session_id.as_str())
    .execute(&fixture.admin)
    .await
    .unwrap();
    let refused =
        PostgresManagedSessionRepository::commit_on(&mut transaction, "ws_a", mutation.clone())
            .await
            .unwrap_err();
    assert!(matches!(refused, SessionRepositoryError::NotApplied(_)));
    transaction.rollback().await.unwrap();
    assert_eq!(repository.get(&root.session_id).await.unwrap(), root);
    assert!(repository.pending_lifecycle().await.unwrap().is_empty());
    assert!(matches!(
        repository.commit("ws_a", mutation).await.unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    assert_eq!(repository.pending_lifecycle().await.unwrap().len(), 1);
    fixture.close().await;
}
