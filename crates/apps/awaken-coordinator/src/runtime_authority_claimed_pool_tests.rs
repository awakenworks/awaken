//! Independent pool/scope/rollback outcomes on the production-selected writer.
use super::*;
use awaken_dispatch::RecoveryProjection;
use awaken_dispatch_contract::DispatchSettlement as _;

const PROGRESS: Duration = Duration::from_secs(10);

#[cfg(test)]
async fn measured_fixture(capacity: u32) -> (String, Arc<DurableRuntimeAuthority>, PgPool) {
    let url = awaken_reliability_testkit::postgres_fixture_url()
        .expect("explicitly admitted PostgreSQL fixture");
    let setup = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    drop(authority_for_pool(setup.clone()).await);
    setup.close().await;
    // Migration and cold SCRAM belong to setup. Only the original warmed pool
    // has the unchanged 100ms contention budget; no authentication time is
    // charged as a missing connection. This original owner requires current_thread.
    tokio::time::pause();
    let pool = {
        let connecting = awaken_sql_storage::postgres_pool_options()
            .max_connections(capacity)
            .min_connections(capacity)
            .acquire_timeout(Duration::from_millis(100))
            .connect(&url);
        tokio::pin!(connecting);
        loop {
            tokio::select! {
                result = &mut connecting => break result.expect("measurement connections authenticate"),
                () = tokio::task::yield_now() => {}
            }
        }
    };
    tokio::time::resume();
    let authority = authority_for_pool(pool.clone()).await;
    assert!(
        std::ptr::eq(
            pool.options(),
            authority
                .postgres_run_commits
                .as_ref()
                .unwrap()
                .pool
                .options(),
        ),
        "the selected global writer owns THIS pool, not a second resource"
    );
    (url, authority, pool)
}

#[cfg(test)]
async fn projection_for(
    authority: &DurableRuntimeAuthority,
    command: &ClaimedCommitCommand,
) -> RecoveryProjection {
    let snapshot = authority
        .shared_recovery_source()
        .unwrap()
        .recovery_snapshot(&command.operation.commit.thread_id, &command.claim.run_id)
        .await
        .unwrap();
    let projection = RecoveryProjection::new();
    projection
        .install(
            &command.claim.run_id,
            &command.operation.commit.thread_id,
            snapshot,
        )
        .unwrap();
    projection
}

#[cfg(test)]
async fn shared_pool_progress(capacity: u32) {
    // C1 same actual Dispatch/commit pool, capacity={1,2}; C2 distinct exact
    // claims; C3 each real writer transaction waits on its original Thread CAS
    // lock. R1/R2 both writer PIDs must reach that lock concurrently, then
    // release => all operations/receipts progress on the capped original pool.
    // The peer is a diagnostic lock/observation connection only; it never serves
    // a business writer checkout. Thread locks precede the store's global dense
    // sequence lock, so this does not demand an impossible dual receipt trigger.
    // R3 success reaches SQL before Worker projection; only apply_receipt may
    // advance it. R4 duplicate SQL receipt + second local application stutter.
    let (url, authority, pool) = measured_fixture(capacity).await;
    let observer = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
    let mut commands = Vec::new();
    let mut projections = Vec::new();
    for index in 0..capacity {
        let mut activation =
            awaken_dispatch_testkit::worker_http::activation(&format!("pool-{capacity}-{index}"));
        activation.thread_id = ThreadId(format!(
            "pool-thread-{}-{capacity}-{index}",
            std::process::id()
        ));
        let command = claim_dispatch(&authority, RunDispatch::new(activation)).await;
        projections.push(projection_for(&authority, &command).await);
        commands.push(command);
    }
    let mut connections = Vec::new();
    let mut writer_pids = Vec::new();
    for _ in 0..capacity {
        let mut connection = pool.acquire().await.unwrap();
        writer_pids.push(
            sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
                .fetch_one(&mut *connection)
                .await
                .unwrap(),
        );
        connections.push(connection);
    }
    drop(connections);
    let mut blocker = observer.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    for command in &commands {
        // Same lock key as the original store's lock_thread_version. This is
        // scheduling only; Dispatch still owns claim admission in the writer TX.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&command.operation.commit.thread_id.0)
            .execute(&mut *blocker)
            .await
            .unwrap();
    }
    let writer = authority.clone().run_commits();
    let mut tasks = Vec::new();
    for command in &commands {
        let writer = writer.clone();
        let command = command.clone();
        tasks.push(tokio::spawn(async move {
            writer.commit_operation(command).await
        }));
    }
    tokio::time::timeout(PROGRESS, async {
        for pid in writer_pids {
            wait_for_blocker(&observer, pid, blocker_pid).await;
        }
    })
    .await
    .expect("all actual capped writer connections reached their Thread lock");
    assert!(pool.size() <= capacity);
    assert_commit_rows(&observer, 0).await;
    blocker.rollback().await.unwrap();
    for ((task, command), projection) in tasks.into_iter().zip(&commands).zip(&projections) {
        let receipt = tokio::time::timeout(PROGRESS, task)
            .await
            .expect("original shared-pool progress budget")
            .unwrap()
            .unwrap();
        assert!(!receipt.duplicate);
        let before = projection.current().unwrap();
        assert_eq!(
            before.thread_version, 0,
            "R3 SQL success does not push a Worker cache"
        );
        assert!(before.messages.is_empty());
        projection
            .apply_receipt(command.operation.clone(), &receipt)
            .unwrap();
        let duplicate = writer.commit_operation(command.clone()).await.unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(
            (duplicate.commit_sequence, duplicate.thread_version),
            (receipt.commit_sequence, receipt.thread_version)
        );
        projection
            .apply_receipt(command.operation.clone(), &duplicate)
            .unwrap();
        let after = projection.current().unwrap();
        assert_eq!(after.thread_version, receipt.thread_version);
        assert_eq!(after.messages, command.operation.commit.messages);
        assert_eq!(after.state, command.operation.commit.state);
        assert_eq!(after.next_commit_ordinal, 1);
    }
    assert_commit_rows(&pool, i64::from(capacity)).await;
    assert_eq!(
        authority.postgres_commit.as_ref().unwrap().commit_count(),
        0
    );
    assert_eq!(pool.options().get_max_connections(), capacity);
    assert!(pool.size() <= capacity);
    observer.close().await;
    pool.close().await;
}

#[cfg(test)]
#[tokio::test]
#[ignore = "requires explicitly admitted PostgreSQL"]
async fn claimed_commit_shared_pool_cap_one_progresses() {
    shared_pool_progress(1).await;
}

#[cfg(test)]
#[tokio::test]
#[ignore = "requires explicitly admitted PostgreSQL"]
async fn claimed_commit_shared_pool_cap_two_concurrent_progresses() {
    shared_pool_progress(2).await;
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly admitted PostgreSQL"]
async fn claimed_postgres_cold_child_scope_replay_and_ordinary_binding() {
    // C1 exact child Dispatch with parent affinity; C2 corrupt physical binding
    // unrelated to ordinary commit; C3 malformed old message prevents ordinary
    // hydration. R1 C1+C2 => child SQL append; R2 C3 => the SAME global selected
    // writer still appends the next exact operation without decoding old history.
    // R3 wrong parent/logical Thread/Run or retired claim => no append. R4 exact
    // replay stutters, and parent/global synchronous projections never become
    // the child's truth. There is no caller-provided physical partition to forge.
    let (_, authority, pool) = fixture().await.expect("explicit live PostgreSQL");
    let parent = ThreadId("pool-physical-parent".into());
    let child = ThreadId("pool-logical-child".into());
    let mut activation = awaken_dispatch_testkit::worker_http::activation("pool-child");
    activation.thread_id = child.clone();
    let command = claim_dispatch(
        &authority,
        RunDispatch::new(activation).for_session(parent.clone()),
    )
    .await;
    let dispatch = authority.dispatch_store();
    assert_eq!(
        dispatch.begin_attempt(&command.claim, None).await.unwrap(),
        awaken_dispatch_contract::AttemptAdmission::Applied
    );
    let original_binding: Option<String> =
        sqlx::query_scalar("SELECT active_attempt_binding FROM runtime_dispatch WHERE run_id=$1")
            .bind(&command.claim.run_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        original_binding.is_none(),
        "C2 original admitted None binding"
    );
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding='not-json' WHERE run_id=$1")
        .bind(&command.claim.run_id.0)
        .execute(&pool)
        .await
        .unwrap();
    let writer = authority.clone().run_commits();
    let projection = projection_for(&authority, &command).await;
    let first = writer.commit_operation(command.clone()).await.unwrap();
    assert!(!first.duplicate);
    projection
        .apply_receipt(command.operation.clone(), &first)
        .unwrap();
    let (row, original): (i64, serde_json::Value) = sqlx::query_as(
        "SELECT id, data FROM runtime_message WHERE thread_id=$1 ORDER BY id LIMIT 1",
    )
    .bind(&child.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE runtime_message SET data=jsonb_set(data, '{role}', $1) WHERE id=$2")
        .bind(serde_json::json!("owned-invalid-role"))
        .bind(row)
        .execute(&pool)
        .await
        .unwrap();
    let failure = match authority.open_commit(&child.0).await {
        Ok(_) => panic!("R2 ordinary full view must reject the poisoned historical role"),
        Err(error) => error,
    };
    assert!(failure.to_string().contains("owned-invalid-role"));
    let commit = ThreadCommit::assemble(
        child.clone(),
        RunDisposition::ended(command.claim.run_id.clone(), EndCause::NaturalEnd),
        true,
        vec![Message::text(
            MessageId("pool-child-tail".into()),
            Role::Assistant,
            "cold child tail",
        )],
        Vec::new(),
        Vec::new(),
    );
    let next = ClaimedCommitCommand {
        claim: command.claim.clone(),
        operation: CommitOperation {
            operation_id: CommitOperationId::new(command.claim.run_id.clone(), 1),
            expected_thread_version: first.thread_version,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    };
    let second = writer
        .commit_operation(next.clone())
        .await
        .expect("R2 SQL writer is independent from ordinary hydration");
    assert_eq!(
        projection.current().unwrap().thread_version,
        first.thread_version
    );
    projection
        .apply_receipt(next.operation.clone(), &second)
        .unwrap();
    assert_eq!(
        sqlx::query("UPDATE runtime_message SET data=$1 WHERE id=$2")
            .bind(original)
            .bind(row)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    let replay = writer.commit_operation(next.clone()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(
        (replay.commit_sequence, replay.thread_version),
        (second.commit_sequence, second.thread_version)
    );
    projection
        .apply_receipt(next.operation.clone(), &replay)
        .unwrap();
    for wrong in [parent.clone(), ThreadId("pool-foreign-logical".into())] {
        let mut forged = next.clone();
        forged.operation.commit.thread_id = wrong;
        forged.operation.payload_hash = commit_payload_hash(&forged.operation.commit).unwrap();
        let rejected = writer.commit_operation(forged).await.unwrap_err();
        assert_eq!(rejected.kind, RunErrorKind::BadRequest);
        assert!(
            rejected
                .message
                .contains("does not match claimed dispatch thread")
        );
    }
    let foreign = RunId("pool-foreign-run".into());
    let commit = ThreadCommit::assemble(
        child.clone(),
        RunDisposition::running(foreign.clone()),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let forged = ClaimedCommitCommand {
        claim: command.claim.clone(),
        operation: CommitOperation {
            operation_id: CommitOperationId::new(foreign, 0),
            expected_thread_version: 0,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    };
    let rejected = writer.commit_operation(forged).await.unwrap_err();
    assert_eq!(rejected.kind, RunErrorKind::BadRequest);
    assert!(
        rejected
            .message
            .contains("does not match claimed dispatch run")
    );
    // R5 C2's corrupt binding has remained in place for every ordinary write.
    // Restore only this fixture's captured value, under its exact physical
    // owner/epoch; leave slot/lease mutation to the original production ports.
    // No Model/tool/Sandbox executor was started here and all owned database
    // calls have returned, so the original ordinary finish(None) is valid.
    // Do not mark it closing or fabricate independent termination evidence.
    assert_eq!(
        sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding=$1 WHERE run_id=$2 AND active_attempt_owner=$3 AND active_attempt_epoch=$4 AND active_attempt_binding='not-json'")
            .bind(original_binding)
            .bind(&command.claim.run_id.0)
            .bind(&command.claim.owner)
            .bind(i64::try_from(command.claim.epoch).expect("fixture epoch fits durable storage"))
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected(),
        1,
        "R5 restore only the exact owned binding"
    );
    assert_eq!(
        dispatch.finish_attempt(&command.claim, None).await.unwrap(),
        SettleOutcome::Applied,
        "R5 original physical slot retires through its canonical owner"
    );
    assert_eq!(
        dispatch
            .settle(
                &command.claim.run_id,
                command.claim.epoch,
                awaken_dispatch_contract::DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap(),
        SettleOutcome::Applied,
        "R5 durable NaturalEnd consumes the exact claim after physical finish"
    );
    assert_eq!(
        writer.commit_operation(next).await.unwrap_err().code,
        awaken_dispatch_contract::COMMIT_FENCED
    );
    let durable = authority
        .shared_recovery_source()
        .unwrap()
        .recovery_snapshot(&child, &command.claim.run_id)
        .await
        .unwrap();
    let local = projection.current().unwrap();
    assert_eq!(durable.thread_version, second.thread_version);
    assert_eq!(local.messages, durable.messages);
    assert_eq!(local.runs, durable.runs);
    assert_eq!(durable.messages.len(), 2);
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM runtime_commit), (SELECT count(*) FROM runtime_commit_receipt), (SELECT count(*) FROM runtime_commit WHERE thread_id=$1)",
    ).bind(&parent.0).fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (2, 2, 0));
    assert_eq!(
        authority.postgres_commit.as_ref().unwrap().commit_count(),
        0
    );
    pool.close().await;
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly admitted PostgreSQL"]
async fn claimed_postgres_rollback_preserves_projection_and_receipt() {
    // R1 hash/owner refusal => no facts/receipt/cache. R2 failure at the last
    // receipt INSERT rolls back every staged append, while the actual Worker
    // RecoveryProjection remains its installed prefix. R3 removing that exact
    // fault then replaying the SAME operation gives one result; only the
    // canonical receipt advances projection, and duplicate application stutters.
    let (_, authority, pool) = fixture().await.expect("explicit live PostgreSQL");
    let command = initial_claim(&authority).await;
    let projection = projection_for(&authority, &command).await;
    let writer = authority.clone().run_commits();
    let mut invalid = command.clone();
    invalid.operation.payload_hash.0 = "forged".into();
    assert_eq!(
        writer.commit_operation(invalid).await.unwrap_err().code,
        awaken_dispatch_contract::COMMIT_IDENTITY_MISMATCH
    );
    let mut foreign = command.clone();
    foreign.claim.owner = "foreign-worker".into();
    assert_eq!(
        writer.commit_operation(foreign).await.unwrap_err().code,
        awaken_dispatch_contract::COMMIT_FENCED
    );
    assert_commit_rows(&pool, 0).await;
    sqlx::query(
        "ALTER TABLE runtime_commit_receipt ADD CONSTRAINT reject_pool_receipt CHECK (false)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let failure = writer.commit_operation(command.clone()).await.unwrap_err();
    assert!(failure.message.contains("reject_pool_receipt"));
    assert_commit_rows(&pool, 0).await;
    let before = projection.current().unwrap();
    assert_eq!(before.thread_version, 0);
    assert!(before.messages.is_empty() && before.runs.is_empty() && before.state.is_empty());
    sqlx::query("ALTER TABLE runtime_commit_receipt DROP CONSTRAINT reject_pool_receipt")
        .execute(&pool)
        .await
        .unwrap();
    let receipt = writer.commit_operation(command.clone()).await.unwrap();
    assert!(!receipt.duplicate);
    assert_eq!(
        projection.current().unwrap().thread_version,
        0,
        "R3 SQL cannot push a Worker cache"
    );
    projection
        .apply_receipt(command.operation.clone(), &receipt)
        .unwrap();
    let replay = writer.commit_operation(command.clone()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(
        (replay.commit_sequence, replay.thread_version),
        (receipt.commit_sequence, receipt.thread_version)
    );
    projection
        .apply_receipt(command.operation.clone(), &replay)
        .unwrap();
    assert_eq!(
        projection.current().unwrap().messages,
        command.operation.commit.messages
    );
    assert_eq!(
        projection.current().unwrap().thread_version,
        receipt.thread_version
    );
    assert_eq!(
        authority.postgres_commit.as_ref().unwrap().commit_count(),
        0
    );
    assert_commit_rows(&pool, 1).await;
    pool.close().await;
}
