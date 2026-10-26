// @formal surface exclude=test_only
// @formal owner crates/stores/awaken-session-store/src/extraction.rs
// @formal reason: Existing codec laws and native backend CAS/reopen witnesses; no new production authority.
use super::*;

/// The same observable law for both adapters. The caller seeds either actual
/// historical bare JSON or the versioned representation, then two independent
/// handles compete from the same committed revision. The full stored winner,
/// not a fixture counter or a scheduling signal, decides the result.
async fn assert_execution_selection_claim_race(
    first: &dyn MemoryExtractionRepository,
    second: &dyn MemoryExtractionRepository,
    initial: &MemoryExtractionIntent,
    order: &str,
) -> MemoryExtractionIntent {
    let selected = first
        .get_extraction(&initial.intent_id)
        .await
        .unwrap()
        .unwrap()
        .with_execution(awaken_ext_memory::MemoryExtractionExecution {
            commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::AuxiliaryThread,
            data_subject_id: Some(awaken_runtime_contract::DataSubjectId("subject-a".into())),
        })
        .unwrap();
    let mut claimed = second
        .get_extraction(&initial.intent_id)
        .await
        .unwrap()
        .unwrap();
    claimed.claim("old-extractor", 100, 50).unwrap();
    assert_eq!((selected.revision, claimed.revision), (1, 1));
    let (selection, claim) = match order {
        "selection-first" => {
            let selection = first.compare_and_swap_extraction(0, selected.clone()).await;
            (
                selection,
                second.compare_and_swap_extraction(0, claimed.clone()).await,
            )
        }
        "claim-first" => {
            let claim = second.compare_and_swap_extraction(0, claimed.clone()).await;
            (
                first.compare_and_swap_extraction(0, selected.clone()).await,
                claim,
            )
        }
        "concurrent" => tokio::join!(
            first.compare_and_swap_extraction(0, selected.clone()),
            second.compare_and_swap_extraction(0, claimed.clone()),
        ),
        _ => unreachable!(),
    };
    assert_ne!(selection.is_ok(), claim.is_ok(), "{order}");
    let (winner, loser) = if selection.is_ok() {
        (selected, claim)
    } else {
        (claimed, selection)
    };
    assert!(
        matches!(loser, Err(MemoryExtractionError::RevisionConflict(_))),
        "{order}"
    );
    let stored = first
        .get_extraction(&initial.intent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored, winner, "{order}: one complete context/claim winner");
    assert!(
        stored.clone().with_execution(Default::default()).is_err(),
        "{order}: first admission stays frozen"
    );
    stored
}

fn assert_versioned_winner(data: &str, winner: &MemoryExtractionIntent) {
    let encoded: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(encoded["format"], "awaken.memory-extraction.v1");
    assert_eq!(encoded["extraction"], serde_json::to_value(winner).unwrap());
}

#[tokio::test]
async fn extraction_execution_selection_and_first_claim_share_one_revision_fence() {
    // C1 legacy bare / canonical v1; C2 selection first / claim first / race.
    // S1 exactly one revision1 winner retains its complete frozen execution and
    // attribution; S2 the loser is a revision conflict; S3 either input format
    // becomes canonical v1 through the real SQLite CAS and cannot relocate.
    for representation in ["legacy", "versioned"] {
        for order in ["selection-first", "claim-first", "concurrent"] {
            let (_dir, path) = crate::tests::temp_database("extraction-selection.db");
            let first = SqliteManagedSessionRepository::open(&path).await.unwrap();
            let second = SqliteManagedSessionRepository::open(&path).await.unwrap();
            let key = format!("selection-{representation}-{order}");
            let initial = crate::tests::extraction(&key, &key);
            first
                .put_extraction_if_absent(initial.clone())
                .await
                .unwrap();
            if representation == "legacy" {
                // Only this test-owned row is changed; indexed identity and
                // scheduling metadata remain the same virgin revision0.
                sqlx::query("UPDATE managed_memory_extraction SET data = ?1 WHERE intent_id = ?2")
                    .bind(serde_json::to_string(&initial).unwrap())
                    .bind(&initial.intent_id)
                    .execute(&first.pool)
                    .await
                    .unwrap();
            }
            let winner =
                assert_execution_selection_claim_race(&first, &second, &initial, order).await;
            let data: String = sqlx::query_scalar(
                "SELECT data FROM managed_memory_extraction WHERE intent_id = ?1",
            )
            .bind(&initial.intent_id)
            .fetch_one(&first.pool)
            .await
            .unwrap();
            assert_versioned_winner(&data, &winner);
        }
    }
}

#[tokio::test]
async fn postgres_extraction_execution_selection_and_first_claim_share_one_revision_fence() {
    // The same C1xC2 matrix uses independent native PostgreSQL pools. Required
    // absence or configured failure rejects through the existing fixture seam.
    // P1 one complete winner, never mixed claim/context; P2 every persisted row
    // is canonical v1; P3 server-mode reopen (no DDL/repair) retains every exact
    // winner, including original scope, subject, lease, generation and revision.
    use sqlx::Executor as _;
    let Some((url, admin)) = crate::tests::postgres_test_admin().await else {
        return;
    };
    let schema = format!("t_extraction_select_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    let scoped = crate::tests::scoped_postgres_url(&url, &schema);
    let first = PostgresManagedSessionRepository::connect(&scoped)
        .await
        .unwrap();
    let second = PostgresManagedSessionRepository::connect_existing(&scoped)
        .await
        .unwrap();
    let mut winners = Vec::new();
    for representation in ["legacy", "versioned"] {
        for order in ["selection-first", "claim-first", "concurrent"] {
            let key = format!("selection-{representation}-{order}");
            let initial = crate::tests::extraction(&key, &key);
            first
                .put_extraction_if_absent(initial.clone())
                .await
                .unwrap();
            if representation == "legacy" {
                sqlx::query("UPDATE managed_memory_extraction SET data = $1 WHERE intent_id = $2")
                    .bind(serde_json::to_string(&initial).unwrap())
                    .bind(&initial.intent_id)
                    .execute(&first.pool)
                    .await
                    .unwrap();
            }
            let winner =
                assert_execution_selection_claim_race(&first, &second, &initial, order).await;
            let data: String = sqlx::query_scalar(
                "SELECT data FROM managed_memory_extraction WHERE intent_id = $1",
            )
            .bind(&initial.intent_id)
            .fetch_one(&first.pool)
            .await
            .unwrap();
            assert_versioned_winner(&data, &winner);
            winners.push(winner);
        }
    }
    first.pool.close().await;
    second.pool.close().await;
    let reopened = PostgresManagedSessionRepository::connect_existing(&scoped)
        .await
        .unwrap();
    for winner in winners {
        assert_eq!(
            reopened
                .get_extraction(&winner.intent_id)
                .await
                .unwrap()
                .unwrap(),
            winner,
            "P3 cold independent reopen"
        );
    }
    reopened.pool.close().await;
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}

#[test]
fn extraction_codec_accepts_legacy_and_one_versioned_authority() {
    // R1 the published bare intent => exact legacy read, no guessed facts;
    // R2 canonical v1, with Session/Auxiliary output and attribution => exact roundtrip through the same
    // codec used by SQLite and PostgreSQL. R3 the old bare-intent serde
    // reader cannot interpret the new envelope as a legacy job. Serialization
    // changes only representation, preserving identities, state and lease.
    let intent = crate::tests::extraction("codec-intent", "codec-key");
    let legacy = serde_json::to_string(&intent).unwrap();
    assert_eq!(decode(&legacy).unwrap(), intent, "R1");
    for execution in [
        awaken_ext_memory::MemoryExtractionExecution::default(),
        awaken_ext_memory::MemoryExtractionExecution {
            commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::Session,
            data_subject_id: Some(awaken_runtime_contract::DataSubjectId("subject-a".into())),
        },
        awaken_ext_memory::MemoryExtractionExecution {
            commit_scope: awaken_ext_memory::MemoryExtractionCommitScope::AuxiliaryThread,
            data_subject_id: Some(awaken_runtime_contract::DataSubjectId("subject-b".into())),
        },
    ] {
        let selected = intent.clone().with_execution(execution).unwrap();
        let canonical = encode(&selected).unwrap();
        assert_eq!(decode(&canonical).unwrap(), selected, "R2");
        let envelope: serde_json::Value = serde_json::from_str(&canonical).unwrap();
        assert_eq!(envelope["format"], EXTRACTION_FORMAT, "R2");
        assert_eq!(
            envelope["extraction"],
            serde_json::to_value(&selected).unwrap(),
            "R2"
        );
        assert!(
            serde_json::from_str::<MemoryExtractionIntent>(&canonical).is_err(),
            "R3"
        );
    }
}

#[test]
fn extraction_codec_refuses_unknown_partial_and_downgraded_formats() {
    // F1 unsupported version; F2 missing/non-object body; F3 envelope plus
    // competing legacy top-level identity; F4 unversioned or unknown
    // execution context; F5 unknown bare intent field. Every row must refuse
    // before a claim; no fallback to legacy or dropped routing metadata.
    let intent = crate::tests::extraction("codec-invalid", "codec-key");
    let bare = serde_json::to_value(&intent).unwrap();
    let canonical: serde_json::Value = serde_json::from_str(&encode(&intent).unwrap()).unwrap();
    let mut unknown = canonical.clone();
    unknown["format"] = serde_json::json!("awaken.memory-extraction.v999");
    let mut competing = canonical.clone();
    competing["intent_id"] = serde_json::json!("another-intent");
    let mut future_bare = bare.clone();
    future_bare["execution"] = serde_json::json!({"commit_scope":"auxiliary_thread"});
    let mut future_envelope = canonical.clone();
    future_envelope["extraction"]["execution"] = serde_json::json!({"commit_scope":"unknown"});
    let mut unknown_field = bare;
    unknown_field["future_authority"] = serde_json::json!(true);
    for encoded in [
        unknown,
        serde_json::json!({"format":EXTRACTION_FORMAT}),
        serde_json::json!({"format":EXTRACTION_FORMAT,"extraction":42}),
        competing,
        future_bare,
        future_envelope,
        unknown_field,
    ] {
        assert!(decode(&encoded.to_string()).is_err(), "{encoded}");
    }
    assert!(decode("not json").is_err());
}

#[test]
fn postgres_integer_conversion_rejects_clamping_and_lease_erasure() {
    assert!(matches!(
        postgres_integer(u64::MAX, "revision"),
        Err(MemoryExtractionError::Storage(message))
            if message.contains("revision") && message.contains("BIGINT")
    ));
    assert!(matches!(
        postgres_optional_integer(Some(u64::MAX), "lease"),
        Err(MemoryExtractionError::Storage(message))
            if message.contains("lease") && message.contains("BIGINT")
    ));
}
