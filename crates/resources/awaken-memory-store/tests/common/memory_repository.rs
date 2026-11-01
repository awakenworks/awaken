// @formal evidence memory.cas_has_one_winner_per_generation
// @formal evidence memory.rename_replace_preserves_source_identity
//! One backend-neutral Memory conformance authority, shared by unit and integration tests.
#![allow(dead_code)] // Different feature selections exercise different backend rows.

use awaken_lease_testkit::ManualClock;
use awaken_resource_contract::{
    MAX_MEMORY_BYTES, MAX_PATH_BYTES, MemErr, MemoryContentPurge, MemoryCopyConflictPreservation,
    MemoryCopyConflictQueries, MemoryCopyWriteback, MemoryFilesystem, MemoryGeneration,
    MemoryQueries, MemorySnapshotSource, MemoryVersionOperation, MemoryVersionQueries,
    MemoryVersionRedaction, MemoryWriter, memory_sha256_hex as sha256_hex,
};
use std::sync::Arc;

pub(crate) async fn timestamp_mutation_conformance<M>(fs: &M, clock: &ManualClock)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // Cause/effect table TM: C1=create, C2=changed content, C3=exact replay,
    // C4=stale generation, C5=rename replacing another head,
    // C6=conditional delete, C7=redaction, C8=redaction replay,
    // C9=ordinary delete of a present head.
    // The ManualClock is the same handle injected into the repository. Each
    // admitted mutation stamps its head and history with that reading; C3
    // preserves both; C4's displaced and moved history share one reading.
    // No-op/failed admissions do not mint a new version or change timestamps.
    const START_MS: u64 = 1_700_000_000_123;
    let nanos = |ms| u128::from(ms) * 1_000_000;
    clock.set(START_MS);
    let first = MemoryWriter::create(fs, "timestamp-conformance", "/a.md", "one")
        .await
        .unwrap();
    assert_eq!(first.created_unix_nanos, nanos(START_MS), "TM1");
    assert_eq!(first.updated_unix_nanos, nanos(START_MS), "TM1");
    let first_version = MemoryVersionQueries::list_versions(fs, "timestamp-conformance")
        .await
        .unwrap();
    assert_eq!(first_version[0].created_unix_nanos, nanos(START_MS), "TM1");

    clock.advance(7);
    let changed = MemoryWriter::update(fs, "timestamp-conformance", &first.generation(), "two")
        .await
        .unwrap();
    assert_eq!(changed.created_unix_nanos, nanos(START_MS), "TM2");
    assert_eq!(changed.updated_unix_nanos, nanos(START_MS + 7), "TM2");
    clock.advance(1);
    let replay = MemoryWriter::update(fs, "timestamp-conformance", &changed.generation(), "two")
        .await
        .unwrap();
    assert_eq!(replay, changed, "TM3");
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, "timestamp-conformance")
            .await
            .unwrap()
            .len(),
        2,
        "TM3"
    );
    assert!(
        matches!(
            MemoryWriter::update(fs, "timestamp-conformance", &first.generation(), "stale").await,
            Err(MemErr::Conflict { .. })
        ),
        "TM4"
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, "timestamp-conformance")
            .await
            .unwrap()
            .len(),
        2,
        "TM4"
    );

    let displaced = MemoryWriter::create(fs, "timestamp-conformance", "/b.md", "other")
        .await
        .unwrap();
    clock.advance(1);
    let renamed = MemoryFilesystem::rename_as(fs, "timestamp-conformance", "/a.md", "/b.md", None)
        .await
        .unwrap();
    assert_eq!(renamed.id, first.id, "TM5");
    assert_eq!(renamed.updated_unix_nanos, nanos(START_MS + 9), "TM5");
    let history = MemoryVersionQueries::list_versions(fs, "timestamp-conformance")
        .await
        .unwrap();
    let displaced_delete = history
        .iter()
        .rev()
        .find(|v| v.memory_id == displaced.id && v.operation == MemoryVersionOperation::Deleted)
        .unwrap();
    let moved = history
        .iter()
        .rev()
        .find(|v| v.memory_id == first.id && v.path == "/b.md")
        .unwrap();
    assert_eq!(
        displaced_delete.created_unix_nanos,
        nanos(START_MS + 9),
        "TM5"
    );
    assert_eq!(moved.created_unix_nanos, nanos(START_MS + 9), "TM5");

    clock.advance(1);
    assert!(
        MemoryWriter::delete_if_match(fs, "timestamp-conformance", "/b.md", &renamed.generation())
            .await
            .unwrap()
    );
    let history = MemoryVersionQueries::list_versions(fs, "timestamp-conformance")
        .await
        .unwrap();
    let deleted = history.last().unwrap();
    assert_eq!(deleted.created_unix_nanos, nanos(START_MS + 10), "TM6");
    clock.advance(1);
    let redacted =
        MemoryVersionRedaction::redact_version(fs, "timestamp-conformance", &first_version[0].id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        redacted.redacted_unix_nanos,
        Some(nanos(START_MS + 11)),
        "TM7"
    );
    clock.advance(1);
    assert_eq!(
        MemoryVersionRedaction::redact_version(fs, "timestamp-conformance", &first_version[0].id)
            .await
            .unwrap(),
        Some(redacted),
        "TM8"
    );
    clock.advance(1);
    MemoryWriter::create(fs, "timestamp-conformance", "/ordinary.md", "ordinary")
        .await
        .unwrap();
    clock.advance(1);
    MemoryWriter::delete_by_path(fs, "timestamp-conformance", "/ordinary.md")
        .await
        .unwrap();
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, "timestamp-conformance")
            .await
            .unwrap()
            .last()
            .unwrap()
            .created_unix_nanos,
        nanos(START_MS + 14),
        "TM9"
    );
}

pub(crate) async fn nanosecond_timestamp_conformance<M>(fs: &M, expected: u128)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // Cause/effect rule TN: an injected sub-millisecond reading on create
    // remains exact in both the head and its durable history; the Memory port's
    // u128 nanoseconds must never pass through a millisecond projection.
    let created = MemoryWriter::create(fs, "timestamp-nanos", "/precise.md", "one")
        .await
        .unwrap();
    assert_eq!(created.created_unix_nanos, expected, "TN head created");
    assert_eq!(created.updated_unix_nanos, expected, "TN head updated");
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, "timestamp-nanos")
            .await
            .unwrap()[0]
            .created_unix_nanos,
        expected,
        "TN history"
    );
}

pub(crate) async fn sql_timestamp_range_conformance<M>(representable: &M, overflow: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // Cause/effect table TS: clock=i64::MAX nanos accepts the SQL boundary;
    // clock=i64::MAX+1 with unchanged content or stale generation returns the
    // existing no-op/conflict before reading time; a changed write or new head
    // fails as Storage and rolls back head and history.
    let head = MemoryWriter::create(representable, "timestamp-range", "/a.md", "one")
        .await
        .unwrap();
    assert_eq!(head.created_unix_nanos, i64::MAX as u128, "TS1");
    let before_heads = MemorySnapshotSource::snapshot_heads(representable, "timestamp-range")
        .await
        .unwrap();
    let before_history = MemoryVersionQueries::list_versions(representable, "timestamp-range")
        .await
        .unwrap();
    assert_eq!(
        MemoryWriter::update(overflow, "timestamp-range", &head.generation(), "one")
            .await
            .unwrap(),
        head,
        "TS2"
    );
    // TS2b an authored same-path rename reaches the original no-op before
    // the SQL clock range check; actor presence must not append a history row.
    assert_eq!(
        MemoryFilesystem::rename_as(
            overflow,
            "timestamp-range",
            "/a.md",
            "/a.md",
            Some(&awaken_resource_contract::MemoryActor::UserActor {
                user_id: "bob".into()
            })
        )
        .await
        .unwrap(),
        head,
        "TS2b"
    );
    let stale = MemoryGeneration::new(
        head.id.clone(),
        head.version + 1,
        head.content_sha256.clone(),
    );
    assert!(
        matches!(
            MemoryWriter::update(overflow, "timestamp-range", &stale, "two").await,
            Err(MemErr::Conflict { .. })
        ),
        "TS3"
    );
    assert!(
        matches!(
            MemoryWriter::update(overflow, "timestamp-range", &head.generation(), "two").await,
            Err(MemErr::Storage(_))
        ),
        "TS4"
    );
    assert!(
        matches!(
            MemoryWriter::create(overflow, "timestamp-range", "/new.md", "two").await,
            Err(MemErr::Storage(_))
        ),
        "TS5"
    );
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(representable, "timestamp-range")
            .await
            .unwrap(),
        before_heads,
        "TS6"
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(representable, "timestamp-range")
            .await
            .unwrap(),
        before_history,
        "TS6"
    );
}

pub(crate) async fn copy_conflict_conformance<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    use awaken_resource_contract::{MemoryCopyBase, MemoryCopyConflict};
    // Retention decision table RC: valid={create,update,delete,empty}; same
    // candidate={new,retry,concurrent}; store={same,other}. RC1 stores exactly
    // one proposal per identity/store; RC2 changes neither heads nor versions;
    // RC3 invalid path/base/delete-without-base/size => reject with no writes;
    // RC4 existing whole-store purge removes proposals too and is idempotent.
    let store = "copy-conflict-conformance";
    let base = MemoryCopyBase {
        id: "original-id".into(),
        version: 1,
        content_sha256: sha256_hex("base"),
    };
    let candidates = [
        MemoryCopyConflict {
            path: "/a".into(),
            base: None,
            content: Some("created".into()),
        },
        MemoryCopyConflict {
            path: "/a".into(),
            base: Some(base.clone()),
            content: Some("updated".into()),
        },
        MemoryCopyConflict {
            path: "/a".into(),
            base: Some(base.clone()),
            content: None,
        },
        MemoryCopyConflict {
            path: "/a".into(),
            base: Some(base),
            content: Some(String::new()),
        },
    ];
    let mut expected = candidates.to_vec();
    expected.sort_by_key(MemoryCopyConflict::id);
    for candidate in &candidates {
        let (a, b) = tokio::join!(
            MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, candidate),
            MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, candidate)
        );
        a.unwrap();
        b.unwrap();
        MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, candidate)
            .await
            .unwrap();
    }
    assert_eq!(
        MemoryCopyConflictQueries::list_copy_conflicts(fs, store)
            .await
            .unwrap(),
        expected,
        "RC1"
    );
    assert!(
        MemorySnapshotSource::snapshot_heads(fs, store)
            .await
            .unwrap()
            .is_empty(),
        "RC2"
    );
    assert!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap()
            .is_empty(),
        "RC2"
    );
    assert!(
        MemoryCopyConflictQueries::list_copy_conflicts(fs, "copy-other")
            .await
            .unwrap()
            .is_empty()
    );
    MemoryCopyConflictPreservation::preserve_copy_conflict(fs, "copy-other", &candidates[0])
        .await
        .unwrap();
    for candidate in [
        MemoryCopyConflict {
            path: "../escape".into(),
            ..candidates[0].clone()
        },
        MemoryCopyConflict {
            base: None,
            content: None,
            ..candidates[0].clone()
        },
        MemoryCopyConflict {
            base: Some(MemoryCopyBase {
                id: "".into(),
                version: 0,
                content_sha256: "invalid".into(),
            }),
            ..candidates[0].clone()
        },
        MemoryCopyConflict {
            content: Some("x".repeat(MAX_MEMORY_BYTES + 1)),
            ..candidates[0].clone()
        },
    ] {
        assert!(
            MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, &candidate)
                .await
                .is_err(),
            "RC3"
        );
        assert_eq!(
            MemoryCopyConflictQueries::list_copy_conflicts(fs, store)
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        MemoryContentPurge::purge_store(fs, store)
            .await
            .unwrap()
            .copy_conflicts_deleted,
        4,
        "RC4"
    );
    assert_eq!(
        MemoryContentPurge::purge_store(fs, store)
            .await
            .unwrap()
            .copy_conflicts_deleted,
        0
    );
    assert!(
        MemoryCopyConflictQueries::list_copy_conflicts(fs, store)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        MemoryCopyConflictQueries::list_copy_conflicts(fs, "copy-other")
            .await
            .unwrap(),
        vec![candidates[0].clone()]
    );
    MemoryContentPurge::purge_store(fs, "copy-other")
        .await
        .unwrap();
    // RC5 capacity admission is store-serialized: a new identity at capacity
    // fails, but an exact retry succeeds. RC6 two distinct proposals competing
    // for the last slot => exactly one commit and one AtCapacity, never an
    // overshoot. Purge releases the same capacity. All backends share this row.
    for index in 0..awaken_resource_contract::MAX_MEMORIES_PER_STORE - 1 {
        let candidate = MemoryCopyConflict {
            path: format!("/capacity-{index}"),
            ..candidates[0].clone()
        };
        MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, &candidate)
            .await
            .unwrap();
    }
    let contenders = ["/last-slot-a", "/last-slot-b"].map(|path| MemoryCopyConflict {
        path: path.into(),
        ..candidates[0].clone()
    });
    let (first, second) = tokio::join!(
        MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, &contenders[0]),
        MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, &contenders[1]),
    );
    assert!(
        matches!(
            (&first, &second),
            (Ok(()), Err(MemErr::AtCapacity)) | (Err(MemErr::AtCapacity), Ok(()))
        ),
        "RC6: {first:?}, {second:?}"
    );
    let retained = MemoryCopyConflictQueries::list_copy_conflicts(fs, store)
        .await
        .unwrap();
    assert_eq!(
        retained.len(),
        awaken_resource_contract::MAX_MEMORIES_PER_STORE
    );
    assert_eq!(retained.contains(&contenders[0]), first.is_ok());
    assert_eq!(retained.contains(&contenders[1]), second.is_ok());
    assert!(matches!(
        MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, &candidates[0]).await,
        Err(MemErr::AtCapacity)
    ));
    MemoryCopyConflictPreservation::preserve_copy_conflict(
        fs,
        store,
        &MemoryCopyConflict {
            path: "/capacity-0".into(),
            ..candidates[0].clone()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        MemoryContentPurge::purge_store(fs, store)
            .await
            .unwrap()
            .copy_conflicts_deleted,
        awaken_resource_contract::MAX_MEMORIES_PER_STORE as u64
    );
    MemoryCopyConflictPreservation::preserve_copy_conflict(fs, store, &candidates[0])
        .await
        .unwrap();
    MemoryContentPurge::purge_store(fs, store).await.unwrap();
}

pub(crate) async fn materialized_path_cas<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // Cause/effect table MP: P=original path and G=complete id/version/hash token.
    // MP1 P+G+changed => update; MP2 P+current-G+same => replay; MP3 !P+G =>
    // conflict before idempotence; MP4 unknown identity => NotFound; MP5 P+!G =>
    // conflict. Every rejection preserves all heads/history. Path and generation
    // are checked in the same transaction, never a read-then-write.
    let store = "materialized-path";
    let base = MemoryWriter::create(fs, store, "/original.md", "base")
        .await
        .unwrap();
    // MP8 invalid original path => InvalidPath before lookup; MP9 oversized
    // candidate => TooLarge before path/identity admission. Neither may mutate.
    let heads = MemorySnapshotSource::snapshot_heads(fs, store)
        .await
        .unwrap();
    let history = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap();
    for path in ["relative.md", "/", "/a/../b.md"] {
        for id in [base.id.as_str(), "absent-id"] {
            let head = base.generation();
            let generation = MemoryGeneration::new(id, head.version(), head.content_sha256());
            assert!(
                matches!(
                    MemoryCopyWriteback::update_materialized(fs, store, path, &generation, "edit")
                        .await,
                    Err(MemErr::InvalidPath(_))
                ),
                "MP8"
            );
        }
    }
    assert!(
        matches!(
            MemoryCopyWriteback::update_materialized(
                fs,
                store,
                "invalid",
                &MemoryGeneration::new("absent-id", 1, "stale"),
                &"x".repeat(MAX_MEMORY_BYTES + 1)
            )
            .await,
            Err(MemErr::TooLarge)
        ),
        "MP9"
    );
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(fs, store)
            .await
            .unwrap(),
        heads
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap(),
        history
    );
    let applied =
        MemoryCopyWriteback::update_materialized(fs, store, &base.path, &base.generation(), "edit")
            .await
            .unwrap();
    let replay = MemoryCopyWriteback::update_materialized(
        fs,
        store,
        &applied.path,
        &applied.generation(),
        "edit",
    )
    .await
    .unwrap();
    assert_eq!(applied, replay, "MP1/MP2");
    MemoryFilesystem::rename_as(fs, store, "/original.md", "/renamed.md", None)
        .await
        .unwrap();
    MemoryWriter::create(fs, store, "/original.md", "replacement")
        .await
        .unwrap();
    let heads = MemorySnapshotSource::snapshot_heads(fs, store)
        .await
        .unwrap();
    let history = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap();
    for content in ["later", "edit"] {
        assert!(
            matches!(MemoryCopyWriteback::update_materialized(fs, store, &base.path, &base.generation(), content).await,
            Err(MemErr::Conflict { current }) if current.path == "/renamed.md"),
            "MP3"
        );
        assert_eq!(
            MemorySnapshotSource::snapshot_heads(fs, store)
                .await
                .unwrap(),
            heads
        );
        assert_eq!(
            MemoryVersionQueries::list_versions(fs, store)
                .await
                .unwrap(),
            history
        );
    }
    assert!(
        matches!(
            MemoryCopyWriteback::update_materialized(
                fs,
                store,
                "/renamed.md",
                &base.generation(),
                "other"
            )
            .await,
            Err(MemErr::Conflict { .. })
        ),
        "MP5"
    );
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(fs, store)
            .await
            .unwrap(),
        heads
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap(),
        history
    );
    MemoryWriter::delete_by_path(fs, store, "/renamed.md")
        .await
        .unwrap();
    MemoryWriter::create(fs, store, "/renamed.md", "edit")
        .await
        .unwrap();
    let heads = MemorySnapshotSource::snapshot_heads(fs, store)
        .await
        .unwrap();
    let history = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap();
    assert!(
        matches!(
            MemoryCopyWriteback::update_materialized(
                fs,
                store,
                "/renamed.md",
                &applied.generation(),
                "other"
            )
            .await,
            Err(MemErr::NotFound(_))
        ),
        "MP4 ABA"
    );
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(fs, store)
            .await
            .unwrap(),
        heads
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap(),
        history
    );
    // MP6 ordinary API/FUSE updates still follow an open identity after rename.
    let renamed = MemoryFilesystem::rename_as(fs, store, "/renamed.md", "/again.md", None)
        .await
        .unwrap();
    assert_eq!(
        MemoryWriter::update(fs, store, &renamed.generation(), "fd-write")
            .await
            .unwrap()
            .path,
        "/again.md"
    );
}

pub(crate) async fn materialized_update_races_rename<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // MP7 both operations ready: linearization update->rename carries edit;
    // rename->update returns conflict and carries base. No third outcome or
    // destination resurrection is allowed. Repeat on distinct stores, not sleeps.
    for index in 0..16 {
        let store = format!("path-race-{index}");
        let base = MemoryWriter::create(fs, &store, "/a.md", "base")
            .await
            .unwrap();
        let generation = base.generation();
        let (update, rename) = tokio::join!(
            MemoryCopyWriteback::update_materialized(fs, &store, "/a.md", &generation, "edit"),
            MemoryFilesystem::rename_as(fs, &store, "/a.md", "/b.md", None),
        );
        let renamed = rename.unwrap();
        let expected = match update {
            Ok(_) => "edit",
            Err(MemErr::Conflict { .. }) => "base",
            other => panic!("MP7 illegal outcome: {other:?}"),
        };
        assert_eq!(renamed.content.as_deref(), Some(expected));
        assert!(
            MemoryQueries::get_by_path(fs, &store, "/a.md")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            MemoryVersionQueries::list_versions(fs, &store)
                .await
                .unwrap()
                .len(),
            if expected == "edit" { 3 } else { 2 }
        );
    }
}

pub(crate) async fn complete_generation_rejects_same_content_aba<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // Complete-generation cause/effect graph:
    // C1 immutable id matches; C2 version matches; C3 content SHA matches;
    // C4 materialized path matches; C5 candidate equals current bytes/path.
    // E1 all C1-C4 + C5 => Current with no version/history change; E2 all
    // C1-C4 + !C5 => Apply once; E3 any of C1-C4 false => Conflict carrying
    // the exact current head. Rename A->B->A preserves C1/C3/C4 but advances
    // C2, so an old same-content token is rejected and cannot pass as retry.
    // Decision rules exercised: G1 current retry, G2 path ABA, G3 explicit
    // stale-version/same-SHA, G4 current id-following write after rename.
    let store = "complete-generation-aba";
    let first = MemoryWriter::create(fs, store, "/a.md", "same")
        .await
        .unwrap();
    let moved = MemoryFilesystem::rename_as(fs, store, "/a.md", "/b.md", None)
        .await
        .unwrap();
    assert_eq!(moved.content_sha256, first.content_sha256);
    let current = MemoryFilesystem::rename_as(fs, store, "/b.md", "/a.md", None)
        .await
        .unwrap();
    assert_eq!(current.content_sha256, first.content_sha256);
    assert!(current.version > first.version);
    let before = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap();

    for stale in [
        first.generation(),
        MemoryGeneration::new(
            current.id.clone(),
            current.version - 1,
            current.content_sha256.clone(),
        ),
    ] {
        assert!(matches!(
            MemoryCopyWriteback::update_materialized(fs, store, "/a.md", &stale, "same").await,
            Err(MemErr::Conflict { current: conflict }) if conflict.as_ref() == &current
        ));
        assert_eq!(
            MemoryVersionQueries::list_versions(fs, store)
                .await
                .unwrap(),
            before,
            "G2/G3"
        );
    }

    let retry =
        MemoryCopyWriteback::update_materialized(fs, store, "/a.md", &current.generation(), "same")
            .await
            .unwrap();
    assert_eq!(retry, current, "G1");
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap(),
        before,
        "G1"
    );

    let applied = MemoryWriter::update(fs, store, &current.generation(), "changed")
        .await
        .unwrap();
    assert_eq!(applied.version, current.version + 1, "G4");
    assert_eq!(applied.path, "/a.md", "G4");
}

pub(crate) async fn rename_replace_orphans_the_destination_id<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    // Rename-replace cause/effect table: R1 source+different destination ->
    // source id/content survive at destination, both old paths cease naming the
    // displaced identity; R2 now-absent source -> NotFound and the committed
    // destination stays unchanged. Pure move and same-path success are covered
    // by extended_conformance, so all path-presence partitions reuse this owner.
    let store = "rename_replace_identity";
    let dst = MemoryWriter::create(fs, store, "/dst.md", "old-dst")
        .await
        .unwrap();
    let src = MemoryWriter::create(fs, store, "/src.md", "src")
        .await
        .unwrap();
    assert_ne!(dst.id, src.id);
    let moved = MemoryFilesystem::rename_as(fs, store, "/src.md", "/dst.md", None)
        .await
        .unwrap();
    assert_eq!(moved.id, src.id, "R1 keeps the source id");
    assert_eq!(moved.content.as_deref(), Some("src"));
    assert!(
        matches!(
            MemoryWriter::update(fs, store, &dst.generation(), "zombie").await,
            Err(MemErr::NotFound(_))
        ),
        "R1 orphans the displaced destination id"
    );
    let at_dst = MemoryQueries::get_by_path(fs, store, "/dst.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(at_dst.id, src.id);
    assert_eq!(at_dst.content.as_deref(), Some("src"));
    assert!(matches!(
        MemoryFilesystem::rename_as(fs, store, "/src.md", "/dst.md", None).await,
        Err(MemErr::NotFound(_))
    ));
    assert_eq!(
        MemoryQueries::get_by_path(fs, store, "/dst.md")
            .await
            .unwrap(),
        Some(at_dst),
        "R2 leaves the committed survivor unchanged"
    );
}

pub(crate) async fn ids_stay_monotonic_across_delete<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    let a = MemoryWriter::create(fs, "s", "/a.md", "a").await.unwrap();
    let b = MemoryWriter::create(fs, "s", "/b.md", "b").await.unwrap();
    assert_eq!((a.id.as_str(), b.id.as_str()), ("mem_1", "mem_2"));
    // Delete the highest-ordinal memory, then create again.
    MemoryWriter::delete_by_path(fs, "s", "/b.md")
        .await
        .unwrap();
    let c = MemoryWriter::create(fs, "s", "/c.md", "c").await.unwrap();
    assert_eq!(
        c.id, "mem_3",
        "a create after deleting the top id must not reuse it"
    );
}

/// Run the same conformance suite over any `MemoryRepository` backend.
pub(crate) async fn conformance<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    copy_conflict_conformance(fs).await;
    let store = "memstore_1";

    // create → version 1, sha set, content round-trips.
    let m = MemoryWriter::create(fs, store, "/notes/today.md", "alpha")
        .await
        .unwrap();
    assert_eq!(m.path, "/notes/today.md");
    assert_eq!(m.version, 1);
    assert_eq!(m.content_sha256, sha256_hex("alpha"));
    assert_eq!(m.content.as_deref(), Some("alpha"));
    assert!(m.created_unix_nanos > 0 && m.updated_unix_nanos >= m.created_unix_nanos);

    // duplicate path → PathConflict.
    assert!(matches!(
        MemoryWriter::create(fs, store, "/notes/today.md", "x").await,
        Err(MemErr::PathConflict(_))
    ));

    // get_by_path.
    let got = MemoryQueries::get_by_path(fs, store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.content.as_deref(), Some("alpha"));
    assert!(
        MemoryQueries::get_by_path(fs, store, "/nope.md")
            .await
            .unwrap()
            .is_none()
    );

    // update with the right base sha → version bumps, content changes.
    let up = MemoryWriter::update(fs, store, &m.generation(), "beta")
        .await
        .unwrap();
    assert_eq!(up.version, 2);
    assert_eq!(up.content.as_deref(), Some("beta"));

    // update with a STALE base sha (the original) → Conflict carrying live "beta".
    match MemoryWriter::update(fs, store, &m.generation(), "gamma").await {
        Err(MemErr::Conflict { current }) => {
            assert_eq!(current.content.as_deref(), Some("beta"));
            assert_eq!(current.content_sha256, sha256_hex("beta"));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // ...and the store still holds "beta" (no clobber).
    assert_eq!(
        MemoryQueries::get_by_path(fs, store, "/notes/today.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("beta")
    );

    // idempotent update: the exact current generation and content → Ok.
    let current = MemoryQueries::get_by_path(fs, store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    let idem = MemoryWriter::update(fs, store, &current.generation(), "beta")
        .await
        .unwrap();
    assert_eq!(idem.content.as_deref(), Some("beta"));

    // list under a prefix.
    MemoryWriter::create(fs, store, "/notes/other.md", "o")
        .await
        .unwrap();
    MemoryWriter::create(fs, store, "/root.md", "r")
        .await
        .unwrap();
    let mut notes: Vec<_> = MemoryQueries::list(fs, store, "/notes")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    notes.sort();
    assert_eq!(notes, vec!["/notes/other.md", "/notes/today.md"]);
    assert_eq!(MemoryQueries::list(fs, store, "/").await.unwrap().len(), 3);

    // rename-replace: move over an existing target atomically, id preserved.
    let before = MemoryQueries::get_by_path(fs, store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    let moved = MemoryFilesystem::rename_as(fs, store, "/notes/today.md", "/notes/other.md", None)
        .await
        .unwrap();
    assert_eq!(moved.id, before.id, "rename preserves the memory id");
    assert_eq!(moved.path, "/notes/other.md");
    assert_eq!(moved.content.as_deref(), Some("beta"));
    // RA3 system/NoActor rename records no invented principal on either row.
    assert!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.path == "/notes/other.md"
                && row.operation != MemoryVersionOperation::Created)
            .all(|row| row.created_by.is_none()),
        "RA3"
    );
    assert!(
        MemoryQueries::get_by_path(fs, store, "/notes/today.md")
            .await
            .unwrap()
            .is_none(),
        "the source path is gone after rename"
    );
    assert_eq!(
        MemoryQueries::get_by_path(fs, store, "/notes/other.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("beta"),
        "the destination was atomically replaced with the source content"
    );

    // delete (idempotent).
    MemoryWriter::delete_by_path(fs, store, "/notes/other.md")
        .await
        .unwrap();
    assert!(
        MemoryQueries::get_by_path(fs, store, "/notes/other.md")
            .await
            .unwrap()
            .is_none()
    );
    MemoryWriter::delete_by_path(fs, store, "/notes/other.md")
        .await
        .unwrap(); // absent → Ok

    // path validation.
    assert!(matches!(
        MemoryWriter::create(fs, store, "relative.md", "x").await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(matches!(
        MemoryWriter::create(fs, store, "/a/../b.md", "x").await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(matches!(
        MemoryWriter::create(fs, store, "/a//b.md", "x").await,
        Err(MemErr::InvalidPath(_))
    ));

    // size cap.
    let big = "x".repeat(MAX_MEMORY_BYTES + 1);
    assert!(matches!(
        MemoryWriter::create(fs, store, "/big.md", &big).await,
        Err(MemErr::TooLarge)
    ));
}

/// Cause-effect-graph cases beyond `conformance`: the remaining validation
/// branches, CAS/rename **precedence** (who masks whom), and prefix-boundary
/// correctness. Runs over any backend.
pub(crate) async fn extended_conformance<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    let store = "ext";

    // G-C1: the other validate_path rejections — root, a control char, a `.`
    // segment, and a path one byte over the cap.
    let over_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES)); // len == cap + 1
    for bad in ["/", "/ctrl\nseg.md", "/a/./b.md", over_cap.as_str()] {
        assert!(
            matches!(
                MemoryWriter::create(fs, store, bad, "x").await,
                Err(MemErr::InvalidPath(_))
            ),
            "expected InvalidPath for {bad:?}"
        );
    }

    // G-C2: size boundary — content exactly at the cap is accepted (only `> cap`
    // is TooLarge, which `conformance` already checks).
    let at_cap = "x".repeat(MAX_MEMORY_BYTES);
    let m_cap = MemoryWriter::create(fs, store, "/at-cap.md", &at_cap)
        .await
        .unwrap();
    assert_eq!(m_cap.content_size, MAX_MEMORY_BYTES as u64);

    // G-U1: validate_size runs BEFORE the id lookup, so an oversized update to a
    // nonexistent id reports TooLarge, not NotFound (precedence pin).
    let big = "x".repeat(MAX_MEMORY_BYTES + 1);
    assert!(matches!(
        MemoryWriter::update(
            fs,
            store,
            &MemoryGeneration::new("mem_does_not_exist", 1, "sha"),
            &big
        )
        .await,
        Err(MemErr::TooLarge)
    ));

    // G-U2: an idempotent update with the current generation must
    // NOT bump the version.
    let m = MemoryWriter::create(fs, store, "/idem.md", "v0")
        .await
        .unwrap();
    let up = MemoryWriter::update(fs, store, &m.generation(), "v1")
        .await
        .unwrap();
    assert_eq!(up.version, 2);
    let idem = MemoryWriter::update(fs, store, &up.generation(), "v1")
        .await
        .unwrap();
    assert_eq!(
        idem.version, 2,
        "idempotent write leaves the version unchanged"
    );
    assert_eq!(idem.content.as_deref(), Some("v1"));

    // G-R1: a rename to an invalid path fails before touching the source.
    MemoryWriter::create(fs, store, "/src.md", "s")
        .await
        .unwrap();
    assert!(matches!(
        MemoryFilesystem::rename_as(fs, store, "/src.md", "relative", None).await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(
        MemoryQueries::get_by_path(fs, store, "/src.md")
            .await
            .unwrap()
            .is_some(),
        "a rejected rename must not destroy the source"
    );

    // G-R2: rename with from == to returns the current memory and does NOT bump
    // the version (the success shape of from==to, distinct from the NotFound one).
    let src = MemoryQueries::get_by_path(fs, store, "/src.md")
        .await
        .unwrap()
        .unwrap();
    let same = MemoryFilesystem::rename_as(fs, store, "/src.md", "/src.md", None)
        .await
        .unwrap();
    assert_eq!(same.id, src.id);
    assert_eq!(
        same.version, src.version,
        "from==to leaves the version unchanged"
    );

    // G-R3: a pure move (destination absent) preserves the id and bumps version.
    let moved = MemoryFilesystem::rename_as(fs, store, "/src.md", "/moved.md", None)
        .await
        .unwrap();
    assert_eq!(moved.id, src.id, "a move preserves the memory id");
    assert_eq!(moved.version, src.version + 1);
    assert!(
        MemoryQueries::get_by_path(fs, store, "/src.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        MemoryQueries::get_by_path(fs, store, "/moved.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("s")
    );

    // G-L1: under_prefix respects the path boundary — "/notes" must match "/notes"
    // and "/notes/x.md" but NOT the sibling "/notesbar".
    let pstore = "prefix";
    MemoryWriter::create(fs, pstore, "/notes", "a")
        .await
        .unwrap();
    MemoryWriter::create(fs, pstore, "/notes/x.md", "b")
        .await
        .unwrap();
    MemoryWriter::create(fs, pstore, "/notesbar", "c")
        .await
        .unwrap();
    let mut under: Vec<_> = MemoryQueries::list(fs, pstore, "/notes")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    under.sort();
    assert_eq!(
        under,
        vec!["/notes", "/notes/x.md"],
        "a prefix must not leak across a path boundary"
    );

    // G-L2: an empty prefix lists everything, exactly like "/".
    assert_eq!(MemoryQueries::list(fs, pstore, "").await.unwrap().len(), 3);

    // delete on a store that was never created is a no-op Ok (idempotent).
    MemoryWriter::delete_by_path(fs, "never_created_store", "/x.md")
        .await
        .unwrap();
}

/// The `NotFound` paths of `update`/`rename`, over any backend.
pub(crate) async fn not_found_paths<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    let store = "s";
    // update on a store that does not exist yet → NotFound.
    assert!(matches!(
        MemoryWriter::update(fs, store, &MemoryGeneration::new("no_id", 1, "sha"), "x").await,
        Err(MemErr::NotFound(_))
    ));
    // update an unknown id in an existing store → NotFound.
    MemoryWriter::create(fs, store, "/seed.md", "s")
        .await
        .unwrap();
    assert!(matches!(
        MemoryWriter::update(
            fs,
            store,
            &MemoryGeneration::new("no_such_id", 1, "sha"),
            "x"
        )
        .await,
        Err(MemErr::NotFound(_))
    ));
    // RA4 invalid destination still precedes source absence and author/history work.
    assert!(matches!(
        MemoryFilesystem::rename_as(
            fs,
            store,
            "/gone.md",
            "relative",
            Some(&awaken_resource_contract::MemoryActor::UserActor {
                user_id: "bob".into()
            })
        )
        .await,
        Err(MemErr::InvalidPath(_))
    ));
    // rename a source that does not exist → NotFound (both the from==to and the
    // from!=to shapes).
    assert!(matches!(
        MemoryFilesystem::rename_as(fs, store, "/gone.md", "/x.md", None).await,
        Err(MemErr::NotFound(_))
    ));
    assert!(matches!(
        MemoryFilesystem::rename_as(fs, store, "/gone.md", "/gone.md", None).await,
        Err(MemErr::NotFound(_))
    ));
}

/// A public API head update is one aggregate transaction: path validation and
/// CAS happen before any write; content + rename-replace + history commit once.
pub(crate) async fn atomic_head_update_conformance<M>(fs: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + ?Sized,
{
    let store = "atomic-head";
    let source = MemoryWriter::create(fs, store, "/source.md", "v1")
        .await
        .unwrap();
    let destination = MemoryWriter::create(fs, store, "/destination.md", "old")
        .await
        .unwrap();
    let initial_versions = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap()
        .len();

    assert!(matches!(
        MemoryWriter::update_head(
            fs,
            store,
            &source.generation(),
            "must-not-commit",
            Some("relative.md"),
        )
        .await,
        Err(MemErr::InvalidPath(_))
    ));
    assert_eq!(
        MemoryQueries::get_by_path(fs, store, "/source.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v1")
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap()
            .len(),
        initial_versions
    );

    let head = source.generation();
    let stale = MemoryGeneration::new(head.memory_id(), head.version(), "stale");
    assert!(matches!(
        MemoryWriter::update_head(fs, store, &stale, "v1", Some("/moved.md"),).await,
        Err(MemErr::Conflict { .. })
    ));
    assert!(
        MemoryQueries::get_by_path(fs, store, "/source.md")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        MemoryQueries::get_by_path(fs, store, "/moved.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap()
            .len(),
        initial_versions
    );

    let updated = MemoryWriter::update_head(
        fs,
        store,
        &source.generation(),
        "v2",
        Some("/destination.md"),
    )
    .await
    .unwrap();
    assert_eq!(updated.id, source.id);
    assert_eq!(updated.path, "/destination.md");
    assert_eq!(updated.content.as_deref(), Some("v2"));
    assert_eq!(updated.version, source.version + 1);
    assert!(
        MemoryQueries::get_by_path(fs, store, "/source.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_ne!(updated.id, destination.id);
    let versions = MemoryVersionQueries::list_versions(fs, store)
        .await
        .unwrap();
    assert_eq!(versions.len(), initial_versions + 2);
    assert_eq!(
        versions[initial_versions].operation,
        MemoryVersionOperation::Deleted
    );
    assert_eq!(versions[initial_versions].memory_id, destination.id);
    assert_eq!(
        versions[initial_versions + 1].operation,
        MemoryVersionOperation::Modified
    );
    assert_eq!(versions[initial_versions + 1].memory_id, source.id);

    let idempotent = MemoryWriter::update_head(
        fs,
        store,
        &updated.generation(),
        "v2",
        Some("/destination.md"),
    )
    .await
    .unwrap();
    assert_eq!(idempotent.version, updated.version);
    assert_eq!(
        MemoryVersionQueries::list_versions(fs, store)
            .await
            .unwrap()
            .len(),
        versions.len()
    );
}

pub(crate) async fn concurrent_create_one_winner<F>(fs: Arc<F>)
where
    F: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + 'static,
{
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let fs = fs.clone();
        handles.push(tokio::spawn(async move {
            MemoryWriter::create(fs.as_ref(), "s", "/race.md", &format!("v{i}")).await
        }));
    }
    let (mut oks, mut conflicts) = (0, 0);
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => oks += 1,
            Err(MemErr::PathConflict(_)) => conflicts += 1,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(oks, 1, "exactly one create wins the path");
    assert_eq!(conflicts, 7);
}

pub(crate) async fn concurrent_cas_one_winner<F>(fs: Arc<F>)
where
    F: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge
        + 'static,
{
    // CAS concurrency cause/effect table: C1 eight writers share one observed
    // complete generation token; C2 every candidate differs. Under the backend's one
    // lock/transaction serialization boundary, E1 exactly one state-changing
    // Apply wins, E2 generation advances exactly once, E3 every loser returns
    // Conflict carrying that winner, E4 exactly one Modified history row is
    // appended. C3 unrelated history may already exist (R1 absent / R2 present);
    // both rules require the complete pre-CAS prefix to remain unchanged and
    // exactly one new row bound to the winner. list_versions is store-scoped,
    // so a fixed total or filtering away other memories cannot prove E4.
    // This is real adapter cross-evidence; mutex/SQL atomicity remains TCB and
    // is not promoted by the scalar HeadUpdateAdmission proof.
    let m = MemoryWriter::create(fs.as_ref(), "s", "/c.md", "v0")
        .await
        .unwrap();
    let before = MemoryVersionQueries::list_versions(fs.as_ref(), "s")
        .await
        .unwrap();
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let (fs, base) = (fs.clone(), m.generation());
        handles.push(tokio::spawn(async move {
            MemoryWriter::update(fs.as_ref(), "s", &base, &format!("w{i}")).await
        }));
    }
    let mut winner = None;
    let mut conflicts = Vec::new();
    for h in handles {
        match h.await.unwrap() {
            Ok(memory) => assert!(winner.replace(memory).is_none(), "E1"),
            Err(MemErr::Conflict { current }) => conflicts.push(*current),
            other => panic!("unexpected: {other:?}"),
        }
    }
    let winner = winner.expect("E1 exactly one CAS write wins");
    assert_eq!(winner.version, m.version + 1, "E2");
    assert!(
        (0..8u32)
            .map(|i| format!("w{i}"))
            .any(|candidate| winner.content.as_deref() == Some(candidate.as_str())),
        "E1"
    );
    assert_eq!(conflicts, vec![winner.clone(); 7], "E3");
    assert_eq!(
        MemoryQueries::get_by_path(fs.as_ref(), "s", "/c.md")
            .await
            .unwrap(),
        Some(winner.clone()),
        "E1/E2"
    );
    let history = MemoryVersionQueries::list_versions(fs.as_ref(), "s")
        .await
        .unwrap();
    assert_eq!(history.len(), before.len() + 1, "E4");
    assert_eq!(&history[..before.len()], before.as_slice(), "E4 prefix");
    let appended = &history[before.len()];
    assert_eq!(appended.memory_id, winner.id, "E4");
    assert_eq!(appended.operation, MemoryVersionOperation::Modified, "E4");
    assert_eq!(appended.path, winner.path, "E4");
    assert_eq!(appended.content, winner.content, "E4");
}

/// Roles observe the same head/history/conflict state while each keeps its own capability.
pub(crate) async fn role_views_share_the_same_atomic_memory<M>(backend: &M)
where
    M: MemoryQueries
        + MemorySnapshotSource
        + MemoryWriter
        + MemoryFilesystem
        + MemoryCopyWriteback
        + MemoryCopyConflictQueries
        + MemoryCopyConflictPreservation
        + MemoryVersionQueries
        + MemoryVersionRedaction
        + MemoryContentPurge,
{
    use awaken_resource_contract::{MemoryActor, MemoryCopyConflict};
    let queries: &dyn MemoryQueries = backend;
    let snapshots: &dyn MemorySnapshotSource = backend;
    let writer: &dyn MemoryWriter = backend;
    let filesystem: &dyn MemoryFilesystem = backend;
    let copy: &dyn MemoryCopyWriteback = backend;
    let conflict_queries: &dyn MemoryCopyConflictQueries = backend;
    let conflict_preservation: &dyn MemoryCopyConflictPreservation = backend;
    let versions: &dyn MemoryVersionQueries = backend;
    let redaction: &dyn MemoryVersionRedaction = backend;
    let purge: &dyn MemoryContentPurge = backend;
    let actor = MemoryActor::ApiActor {
        api_key_id: "role-actor".into(),
    };
    let initial = writer
        .create_as("role-store", "/source", "same", Some(&actor))
        .await
        .unwrap();
    let displaced = writer
        .create("role-store", "/target", "other")
        .await
        .unwrap();
    assert_eq!(
        queries.get_by_path("role-store", "/source").await.unwrap(),
        Some(initial.clone())
    );
    assert_eq!(
        snapshots.snapshot_heads("role-store").await.unwrap().len(),
        2
    );
    // RA1 source creation and rename are different actions: the rename's Bob
    // must author BOTH the displaced deletion and moved head, never inherit
    // the source's original actor. All three backend role cases use this table.
    let bob = MemoryActor::UserActor {
        user_id: "bob".into(),
    };
    let before_rename = versions.list_versions("role-store").await.unwrap();
    let renamed = filesystem
        .rename_as("role-store", "/source", "/target", Some(&bob))
        .await
        .unwrap();
    assert_eq!(renamed.id, initial.id);
    assert_ne!(renamed.id, displaced.id);
    let after_rename = versions.list_versions("role-store").await.unwrap();
    assert_eq!(
        &after_rename[..before_rename.len()],
        before_rename.as_slice(),
        "RA1 old history"
    );
    let appended = &after_rename[before_rename.len()..];
    assert_eq!(appended.len(), 2, "RA1 exactly one replace transaction");
    assert!(
        appended
            .iter()
            .all(|row| row.created_by.as_ref() == Some(&bob)),
        "RA1 both Bob"
    );
    assert!(
        appended
            .iter()
            .any(|row| row.memory_id == displaced.id
                && row.operation == MemoryVersionOperation::Deleted),
        "RA1 destination"
    );
    assert!(
        appended
            .iter()
            .any(|row| row.memory_id == initial.id
                && row.operation == MemoryVersionOperation::Modified),
        "RA1 source"
    );
    // RA2 changing only the actor on from==to is still an original no-op:
    // no new generation/history; it does not rewrite an old author.
    let same = filesystem
        .rename_as("role-store", "/target", "/target", Some(&actor))
        .await
        .unwrap();
    assert_eq!(same.generation(), renamed.generation(), "RA2");
    assert_eq!(
        versions.list_versions("role-store").await.unwrap(),
        after_rename,
        "RA2"
    );
    assert!(
        queries
            .get_by_path("role-store", "/source")
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        copy.update_materialized("role-store", "/source", &initial.generation(), "loser")
            .await,
        Err(MemErr::Conflict { .. }) | Err(MemErr::NotFound(_))
    ));
    assert!(matches!(
        writer
            .update("role-store", &initial.generation(), "same")
            .await,
        Err(MemErr::Conflict { .. })
    ));
    let head = copy
        .update_materialized(
            "role-store",
            "/target",
            &renamed.generation(),
            "copy-winner",
        )
        .await
        .unwrap();
    let proposal = MemoryCopyConflict {
        path: "/source".into(),
        base: None,
        content: Some("loser".into()),
    };
    conflict_preservation
        .preserve_copy_conflict("role-store", &proposal)
        .await
        .unwrap();
    conflict_preservation
        .preserve_copy_conflict("role-store", &proposal)
        .await
        .unwrap();
    assert_eq!(
        conflict_queries
            .list_copy_conflicts("role-store")
            .await
            .unwrap(),
        vec![proposal]
    );
    assert_eq!(
        snapshots.snapshot_heads("role-store").await.unwrap(),
        vec![head.clone()]
    );
    let log = versions.list_versions("role-store").await.unwrap();
    assert_eq!(log[0].created_by.as_ref(), Some(&actor));
    assert_eq!(log.iter().filter(|v| v.memory_id == initial.id).count(), 3);
    let old = log.iter().find(|v| v.memory_id == initial.id).unwrap();
    assert!(
        redaction
            .redact_version_as("role-store", &old.id, Some(&actor))
            .await
            .unwrap()
            .unwrap()
            .content
            .is_none()
    );
    let mut expected_history = versions.list_versions("role-store").await.unwrap();
    let live = expected_history
        .iter_mut()
        .rev()
        .find(|version| version.memory_id == head.id)
        .unwrap();
    let redacted_live = redaction
        .redact_version_as("role-store", &live.id, Some(&actor))
        .await
        .unwrap()
        .unwrap();
    assert!(redacted_live.redacted_unix_nanos.is_some());
    live.content = None;
    live.redacted_unix_nanos = redacted_live.redacted_unix_nanos;
    live.redacted_by = Some(actor.clone());
    assert_eq!(redacted_live, *live);
    assert_eq!(
        versions.list_versions("role-store").await.unwrap(),
        expected_history
    );
    assert_eq!(
        snapshots.snapshot_heads("role-store").await.unwrap(),
        vec![head.clone()]
    );
    assert_eq!(
        queries.get_by_path("role-store", "/target").await.unwrap(),
        Some(head)
    );
    purge.purge_store("role-store").await.unwrap();
    assert!(
        snapshots
            .snapshot_heads("role-store")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        versions
            .list_versions("role-store")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        conflict_queries
            .list_copy_conflicts("role-store")
            .await
            .unwrap()
            .is_empty()
    );
}

/// Check finite observed operation histories against complete committed heads.
/// The writer's order is A1,B1,A2,B2,... . A snapshot must equal one whole
/// prefix that fits its invocation/return interval, including identity, hash,
/// content, version and metadata. Per-file list/get stitching can produce no
/// such prefix. Channels arrange one partial state and a final state; every
/// semantic assertion observes real committed heads/history, never a signal.
/// This exercises observed interleavings, not an exhaustive concurrency proof.
pub(crate) async fn snapshot_heads_follow_one_committed_cross_file_prefix<M>(fs: std::sync::Arc<M>)
where
    M: MemoryWriter + MemorySnapshotSource + MemoryVersionQueries + Send + Sync + 'static,
{
    snapshot_heads_follow_one_committed_cross_file_prefix_through(fs.clone(), fs).await;
}

/// As above, with the writes and the snapshots on separately supplied handles
/// (for a durable backend: two independently opened clients of one database).
pub(crate) async fn snapshot_heads_follow_one_committed_cross_file_prefix_through<W, R>(
    writer: std::sync::Arc<W>,
    reader: std::sync::Arc<R>,
) where
    W: MemoryWriter + Send + Sync + 'static,
    R: MemorySnapshotSource + MemoryVersionQueries + Send + Sync + 'static,
{
    use awaken_resource_contract::Memory;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use tokio::sync::oneshot;
    const STORE: &str = "snapshot-history";
    const FOREIGN: &str = "snapshot-history-foreign";
    const ROUNDS: usize = 8;
    type Write = (u64, u64, Memory);
    type Observation = (u64, u64, Vec<Memory>);
    async fn observe<M: MemorySnapshotSource>(fs: &M, sequence: &AtomicU64) -> Observation {
        let start = sequence.fetch_add(1, Ordering::SeqCst);
        let heads = MemorySnapshotSource::snapshot_heads(fs, STORE)
            .await
            .unwrap();
        let end = sequence.fetch_add(1, Ordering::SeqCst);
        (start, end, heads)
    }
    let a = MemoryWriter::create(writer.as_ref(), STORE, "/a.md", "0")
        .await
        .unwrap();
    let b = MemoryWriter::create(writer.as_ref(), STORE, "/b.md", "0")
        .await
        .unwrap();
    MemoryWriter::create(writer.as_ref(), FOREIGN, "/foreign.md", "sentinel")
        .await
        .unwrap();
    let foreign = MemorySnapshotSource::snapshot_heads(reader.as_ref(), FOREIGN)
        .await
        .unwrap();
    let initial = vec![a.clone(), b.clone()];
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(reader.as_ref(), STORE)
            .await
            .unwrap(),
        initial
    );
    let sequence = Arc::new(AtomicU64::new(1));
    let (first_committed, first_observed) = oneshot::channel();
    let (continue_writer, continue_received) = oneshot::channel();
    let (all_committed, all_observed) = oneshot::channel();
    let writer_fs = writer.clone();
    let writer_sequence = sequence.clone();
    let writer = tokio::spawn(async move {
        let (mut a, mut b) = (a, b);
        let (mut first, mut proceed) = (Some(first_committed), Some(continue_received));
        let mut writes: Vec<Write> = Vec::new();
        for round in 1..=ROUNDS {
            let start = writer_sequence.fetch_add(1, Ordering::SeqCst);
            a = MemoryWriter::update(
                writer_fs.as_ref(),
                STORE,
                &a.generation(),
                &round.to_string(),
            )
            .await
            .unwrap();
            let end = writer_sequence.fetch_add(1, Ordering::SeqCst);
            writes.push((start, end, a.clone()));
            if round == 1 {
                first
                    .take()
                    .unwrap()
                    .send(())
                    .expect("reader remains present");
                proceed
                    .take()
                    .unwrap()
                    .await
                    .expect("first committed snapshot observed");
            }
            tokio::task::yield_now().await;
            let start = writer_sequence.fetch_add(1, Ordering::SeqCst);
            b = MemoryWriter::update(
                writer_fs.as_ref(),
                STORE,
                &b.generation(),
                &round.to_string(),
            )
            .await
            .unwrap();
            let end = writer_sequence.fetch_add(1, Ordering::SeqCst);
            writes.push((start, end, b.clone()));
            tokio::task::yield_now().await;
        }
        all_committed.send(()).expect("reader remains present");
        writes
    });
    let reader_fs = reader.clone();
    let reader_task = tokio::spawn(async move {
        first_observed.await.expect("first A write committed");
        let mut observations = vec![observe(reader_fs.as_ref(), &sequence).await];
        continue_writer.send(()).expect("writer remains present");
        for _ in 0..64 {
            observations.push(observe(reader_fs.as_ref(), &sequence).await);
            tokio::task::yield_now().await;
        }
        all_observed.await.expect("all writes committed");
        observations.push(observe(reader_fs.as_ref(), &sequence).await);
        observations
    });
    let writes = writer.await.unwrap();
    let observations = reader_task.await.unwrap();
    assert_eq!(writes.len(), ROUNDS * 2);
    assert_eq!(observations.len(), 66);
    for (index, (start, end, heads)) in observations.iter().enumerate() {
        let lower = writes
            .iter()
            .filter(|(_, finished, _)| finished < start)
            .count();
        let upper = writes.iter().filter(|(began, _, _)| began < end).count();
        if index == 0 {
            assert_eq!((lower, upper), (1, 1));
        }
        if index == observations.len() - 1 {
            assert_eq!((lower, upper), (writes.len(), writes.len()));
        }
        let mut model = initial.clone();
        let mut matching_prefix = None;
        for prefix in 0..=writes.len() {
            if prefix > 0 {
                let head = &writes[prefix - 1].2;
                let slot = if head.path == "/a.md" {
                    0
                } else {
                    assert_eq!(head.path, "/b.md");
                    1
                };
                model[slot] = head.clone();
            }
            if (lower..=upper).contains(&prefix) && model == *heads {
                matching_prefix = Some(prefix);
                break;
            }
        }
        assert!(
            matching_prefix.is_some(),
            "snapshot {index} has no committed prefix in {lower}..={upper}: {heads:?}"
        );
    }
    assert_eq!(
        MemoryVersionQueries::list_versions(reader.as_ref(), STORE)
            .await
            .unwrap()
            .len(),
        ROUNDS * 2 + 2
    );
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(reader.as_ref(), FOREIGN)
            .await
            .unwrap(),
        foreign
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(reader.as_ref(), FOREIGN)
            .await
            .unwrap()
            .len(),
        1
    );
}
