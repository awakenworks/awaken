// @formal evidence session_execution.one_physical_attempt_per_thread
//! Scoped views over the original operation journal, never another inventory.
#![cfg(feature = "durable")]
mod harness;

use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
use awaken_dispatch::{
    DispatchAttemptRecordReads, DispatchCursor, DispatchOperation, PhysicalAttempt,
    PostgresDispatchStore, RunClaim, SqliteDispatchStore,
};
use serde_json::{Value, json};

fn original_rows() -> Vec<(u64, String, Value)> {
    // Decision table: I1 delivery facts + foreign entries precede own entries;
    // I2 root OR frozen member selects original identity, including mismatched
    // affinity (the recovery consumer must reject that mismatch); I3 excluded
    // closure rows cannot consume LIMIT or advance an empty view's cursor.
    let entry = |run: &str, thread: &str, parent: Option<&str>| PhysicalAttempt {
        claim: RunClaim {
            run_id: RunId(run.into()),
            owner: "fixture".into(),
            epoch: 1,
        },
        binding: None,
        thread_id: Some(ThreadId(thread.into())),
        session_thread_id: parent.map(|id| ThreadId(id.into())),
        closing: false,
    };
    let admitted = |sequence, attempt: PhysicalAttempt| {
        (
            sequence,
            attempt.claim.run_id.0.clone(),
            serde_json::to_value(DispatchOperation::AttemptAdmitted { attempt }).unwrap(),
        )
    };
    let own = entry("root-run", "root", None);
    vec![
        (
            11,
            "delivery".into(),
            serde_json::to_value(DispatchOperation::Claimed {
                claim: RunClaim {
                    run_id: RunId("delivery".into()),
                    owner: "fixture".into(),
                    epoch: 1,
                },
            })
            .unwrap(),
        ),
        admitted(
            29,
            entry("foreign-run", "foreign-thread", Some("foreign-root")),
        ),
        admitted(47, own.clone()),
        admitted(83, entry("child-run", "child", Some("root"))),
        admitted(109, entry("unfrozen-run", "unfrozen-child", Some("root"))),
        admitted(
            149,
            entry("wrong-parent-run", "child", Some("wrong-parent")),
        ),
        (
            197,
            "root-run".into(),
            serde_json::to_value(awaken_dispatch::AttemptPhysicalFact::Acknowledgement {
                record: awaken_dispatch::AttemptAcknowledgementRecord::try_new(
                    own,
                    awaken_dispatch::AttemptReturnCause::StructuredReturnObserved,
                )
                .unwrap(),
            })
            .unwrap(),
        ),
    ]
}

async fn assert_scoped_view(store: &impl DispatchAttemptRecordReads) {
    let root = ThreadId("root".into());
    let members = [root.clone(), ThreadId("child".into())];
    let mut cursor = DispatchCursor::default();
    for expected in [47, 83, 109, 149] {
        let page = store
            .attempt_admissions_after(cursor, 1, &root, &members)
            .await
            .unwrap();
        assert_eq!(
            page.events.len(),
            1,
            "I1/I2 filtering precedes logical LIMIT"
        );
        assert_eq!(
            page.events[0].cursor,
            DispatchCursor(expected),
            "I2 original sparse cursor"
        );
        assert!(matches!(
            page.events[0].operation,
            DispatchOperation::AttemptAdmitted { .. }
        ));
        cursor = page.next_cursor;
    }
    let empty = store
        .attempt_admissions_after(cursor, 1, &root, &members)
        .await
        .unwrap();
    assert!(
        empty.events.is_empty(),
        "I3 physical ACK is not admission inventory"
    );
    assert_eq!(
        empty.next_cursor, cursor,
        "I3 empty retains requested cursor"
    );
    let absent = ThreadId("absent".into());
    assert!(
        store
            .attempt_admissions_after(DispatchCursor(0), 1, &absent, std::slice::from_ref(&absent))
            .await
            .unwrap()
            .events
            .is_empty(),
        "I4 no unrelated inventory"
    );
    assert!(
        store
            .attempt_admissions_after(DispatchCursor(0), 0, &root, &members)
            .await
            .unwrap()
            .events
            .is_empty(),
        "I5 zero limit"
    );
    assert!(
        store
            .attempt_admissions_after(DispatchCursor(0), 1, &root, &[])
            .await
            .is_err(),
        "I6 incomplete caller scope cannot prove empty"
    );
}

#[tokio::test]
async fn sqlite_scoped_admission_inventory_preserves_original_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inventory.db");
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let db = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    for (sequence, run, operation) in original_rows() {
        sqlx::query("INSERT INTO runtime_dispatch_operation (sequence,run_id,operation,recorded_at_ms) VALUES (?1,?2,?3,1)")
            .bind(sequence as i64).bind(run).bind(operation.to_string()).execute(&db).await.unwrap();
    }
    assert_scoped_view(&store).await;
    // I7 an unclassifiable original row stays visible to the canonical decoder;
    // it cannot be silently filtered into a successful empty or partial page.
    sqlx::query("INSERT INTO runtime_dispatch_operation (sequence,run_id,operation,recorded_at_ms) VALUES (211,'unknown',?1,1)")
        .bind(json!({"type":"unknown_original_operation"}).to_string()).execute(&db).await.unwrap();
    let root = ThreadId("root".into());
    assert!(
        store
            .attempt_admissions_after(
                DispatchCursor(0),
                20,
                &root,
                &[root.clone(), ThreadId("child".into())]
            )
            .await
            .is_err(),
        "I7"
    );
}

#[tokio::test]
async fn postgres_scoped_admission_inventory_preserves_original_selection() {
    let Some(pool) = harness::schema_pool("t_pg_scoped_admission_inventory").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    for (sequence, run, operation) in original_rows() {
        sqlx::query("INSERT INTO runtime_dispatch_operation (sequence,run_id,operation,recorded_at_ms) VALUES ($1,$2,$3,1)")
            .bind(sequence as i64).bind(run).bind(sqlx::types::Json(operation)).execute(&pool).await.unwrap();
    }
    assert_scoped_view(&store).await;
    sqlx::query("INSERT INTO runtime_dispatch_operation (sequence,run_id,operation,recorded_at_ms) VALUES (211,'unknown',$1,1)")
        .bind(sqlx::types::Json(json!({"type":"unknown_original_operation"}))).execute(&pool).await.unwrap();
    let root = ThreadId("root".into());
    assert!(
        store
            .attempt_admissions_after(
                DispatchCursor(0),
                20,
                &root,
                &[root.clone(), ThreadId("child".into())]
            )
            .await
            .is_err(),
        "I7"
    );
}
