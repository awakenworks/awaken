// @formal evidence memory.cas_has_one_winner_per_generation
// @formal evidence memory.rename_replace_preserves_source_identity
//! One backend-neutral Memory conformance authority, shared by unit and integration tests.
#![allow(dead_code)] // Different feature selections exercise different backend rows.

use awaken_resource_contract::{
    MAX_MEMORY_BYTES, MAX_PATH_BYTES, MemErr, MemoryGeneration, MemoryRepository,
    MemoryVersionOperation, memory_sha256_hex as sha256_hex,
};
use std::sync::Arc;

pub(crate) async fn copy_conflict_conformance(fs: &dyn MemoryRepository) {
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
            fs.preserve_copy_conflict(store, candidate),
            fs.preserve_copy_conflict(store, candidate)
        );
        a.unwrap();
        b.unwrap();
        fs.preserve_copy_conflict(store, candidate).await.unwrap();
    }
    assert_eq!(
        fs.list_copy_conflicts(store).await.unwrap(),
        expected,
        "RC1"
    );
    assert!(fs.snapshot_heads(store).await.unwrap().is_empty(), "RC2");
    assert!(fs.list_versions(store).await.unwrap().is_empty(), "RC2");
    assert!(
        fs.list_copy_conflicts("copy-other")
            .await
            .unwrap()
            .is_empty()
    );
    fs.preserve_copy_conflict("copy-other", &candidates[0])
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
            fs.preserve_copy_conflict(store, &candidate).await.is_err(),
            "RC3"
        );
        assert_eq!(fs.list_copy_conflicts(store).await.unwrap(), expected);
    }
    assert_eq!(
        fs.purge_store(store).await.unwrap().copy_conflicts_deleted,
        4,
        "RC4"
    );
    assert_eq!(
        fs.purge_store(store).await.unwrap().copy_conflicts_deleted,
        0
    );
    assert!(fs.list_copy_conflicts(store).await.unwrap().is_empty());
    assert_eq!(
        fs.list_copy_conflicts("copy-other").await.unwrap(),
        vec![candidates[0].clone()]
    );
    fs.purge_store("copy-other").await.unwrap();
    // RC5 capacity admission is store-serialized: a new identity at capacity
    // fails, but an exact retry succeeds. RC6 two distinct proposals competing
    // for the last slot => exactly one commit and one AtCapacity, never an
    // overshoot. Purge releases the same capacity. All backends share this row.
    for index in 0..awaken_resource_contract::MAX_MEMORIES_PER_STORE - 1 {
        let candidate = MemoryCopyConflict {
            path: format!("/capacity-{index}"),
            ..candidates[0].clone()
        };
        fs.preserve_copy_conflict(store, &candidate).await.unwrap();
    }
    let contenders = ["/last-slot-a", "/last-slot-b"].map(|path| MemoryCopyConflict {
        path: path.into(),
        ..candidates[0].clone()
    });
    let (first, second) = tokio::join!(
        fs.preserve_copy_conflict(store, &contenders[0]),
        fs.preserve_copy_conflict(store, &contenders[1]),
    );
    assert!(
        matches!(
            (&first, &second),
            (Ok(()), Err(MemErr::AtCapacity)) | (Err(MemErr::AtCapacity), Ok(()))
        ),
        "RC6: {first:?}, {second:?}"
    );
    let retained = fs.list_copy_conflicts(store).await.unwrap();
    assert_eq!(
        retained.len(),
        awaken_resource_contract::MAX_MEMORIES_PER_STORE
    );
    assert_eq!(retained.contains(&contenders[0]), first.is_ok());
    assert_eq!(retained.contains(&contenders[1]), second.is_ok());
    assert!(matches!(
        fs.preserve_copy_conflict(store, &candidates[0]).await,
        Err(MemErr::AtCapacity)
    ));
    fs.preserve_copy_conflict(
        store,
        &MemoryCopyConflict {
            path: "/capacity-0".into(),
            ..candidates[0].clone()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        fs.purge_store(store).await.unwrap().copy_conflicts_deleted,
        awaken_resource_contract::MAX_MEMORIES_PER_STORE as u64
    );
    fs.preserve_copy_conflict(store, &candidates[0])
        .await
        .unwrap();
    fs.purge_store(store).await.unwrap();
}

pub(crate) async fn materialized_path_cas(fs: &dyn MemoryRepository) {
    // Cause/effect table MP: P=original path and G=complete id/version/hash token.
    // MP1 P+G+changed => update; MP2 P+current-G+same => replay; MP3 !P+G =>
    // conflict before idempotence; MP4 unknown identity => NotFound; MP5 P+!G =>
    // conflict. Every rejection preserves all heads/history. Path and generation
    // are checked in the same transaction, never a read-then-write.
    let store = "materialized-path";
    let base = fs.create(store, "/original.md", "base").await.unwrap();
    // MP8 invalid original path => InvalidPath before lookup; MP9 oversized
    // candidate => TooLarge before path/identity admission. Neither may mutate.
    let heads = fs.snapshot_heads(store).await.unwrap();
    let history = fs.list_versions(store).await.unwrap();
    for path in ["relative.md", "/", "/a/../b.md"] {
        for id in [base.id.as_str(), "absent-id"] {
            let mut generation = base.generation();
            generation.memory_id = id.into();
            assert!(
                matches!(
                    fs.update_materialized(store, path, &generation, "edit")
                        .await,
                    Err(MemErr::InvalidPath(_))
                ),
                "MP8"
            );
        }
    }
    assert!(
        matches!(
            fs.update_materialized(
                store,
                "invalid",
                &MemoryGeneration {
                    memory_id: "absent-id".into(),
                    version: 1,
                    content_sha256: "stale".into(),
                },
                &"x".repeat(MAX_MEMORY_BYTES + 1)
            )
            .await,
            Err(MemErr::TooLarge)
        ),
        "MP9"
    );
    assert_eq!(fs.snapshot_heads(store).await.unwrap(), heads);
    assert_eq!(fs.list_versions(store).await.unwrap(), history);
    let applied = fs
        .update_materialized(store, &base.path, &base.generation(), "edit")
        .await
        .unwrap();
    let replay = fs
        .update_materialized(store, &applied.path, &applied.generation(), "edit")
        .await
        .unwrap();
    assert_eq!(applied, replay, "MP1/MP2");
    fs.rename(store, "/original.md", "/renamed.md")
        .await
        .unwrap();
    fs.create(store, "/original.md", "replacement")
        .await
        .unwrap();
    let heads = fs.snapshot_heads(store).await.unwrap();
    let history = fs.list_versions(store).await.unwrap();
    for content in ["later", "edit"] {
        assert!(
            matches!(fs.update_materialized(store, &base.path, &base.generation(), content).await,
            Err(MemErr::Conflict { current }) if current.path == "/renamed.md"),
            "MP3"
        );
        assert_eq!(fs.snapshot_heads(store).await.unwrap(), heads);
        assert_eq!(fs.list_versions(store).await.unwrap(), history);
    }
    assert!(
        matches!(
            fs.update_materialized(store, "/renamed.md", &base.generation(), "other")
                .await,
            Err(MemErr::Conflict { .. })
        ),
        "MP5"
    );
    assert_eq!(fs.snapshot_heads(store).await.unwrap(), heads);
    assert_eq!(fs.list_versions(store).await.unwrap(), history);
    fs.delete_by_path(store, "/renamed.md").await.unwrap();
    fs.create(store, "/renamed.md", "edit").await.unwrap();
    let heads = fs.snapshot_heads(store).await.unwrap();
    let history = fs.list_versions(store).await.unwrap();
    assert!(
        matches!(
            fs.update_materialized(store, "/renamed.md", &applied.generation(), "other")
                .await,
            Err(MemErr::NotFound(_))
        ),
        "MP4 ABA"
    );
    assert_eq!(fs.snapshot_heads(store).await.unwrap(), heads);
    assert_eq!(fs.list_versions(store).await.unwrap(), history);
    // MP6 ordinary API/FUSE updates still follow an open identity after rename.
    let renamed = fs.rename(store, "/renamed.md", "/again.md").await.unwrap();
    assert_eq!(
        fs.update(store, &renamed.generation(), "fd-write")
            .await
            .unwrap()
            .path,
        "/again.md"
    );
}

pub(crate) async fn materialized_update_races_rename(fs: &dyn MemoryRepository) {
    // MP7 both operations ready: linearization update->rename carries edit;
    // rename->update returns conflict and carries base. No third outcome or
    // destination resurrection is allowed. Repeat on distinct stores, not sleeps.
    for index in 0..16 {
        let store = format!("path-race-{index}");
        let base = fs.create(&store, "/a.md", "base").await.unwrap();
        let generation = base.generation();
        let (update, rename) = tokio::join!(
            fs.update_materialized(&store, "/a.md", &generation, "edit"),
            fs.rename(&store, "/a.md", "/b.md"),
        );
        let renamed = rename.unwrap();
        let expected = match update {
            Ok(_) => "edit",
            Err(MemErr::Conflict { .. }) => "base",
            other => panic!("MP7 illegal outcome: {other:?}"),
        };
        assert_eq!(renamed.content.as_deref(), Some(expected));
        assert!(fs.get_by_path(&store, "/a.md").await.unwrap().is_none());
        assert_eq!(
            fs.list_versions(&store).await.unwrap().len(),
            if expected == "edit" { 3 } else { 2 }
        );
    }
}

pub(crate) async fn complete_generation_rejects_same_content_aba(fs: &dyn MemoryRepository) {
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
    let first = fs.create(store, "/a.md", "same").await.unwrap();
    let moved = fs.rename(store, "/a.md", "/b.md").await.unwrap();
    assert_eq!(moved.content_sha256, first.content_sha256);
    let current = fs.rename(store, "/b.md", "/a.md").await.unwrap();
    assert_eq!(current.content_sha256, first.content_sha256);
    assert!(current.version > first.version);
    let before = fs.list_versions(store).await.unwrap();

    for stale in [
        first.generation(),
        MemoryGeneration {
            memory_id: current.id.clone(),
            version: current.version - 1,
            content_sha256: current.content_sha256.clone(),
        },
    ] {
        assert!(matches!(
            fs.update_materialized(store, "/a.md", &stale, "same").await,
            Err(MemErr::Conflict { current: conflict }) if conflict.as_ref() == &current
        ));
        assert_eq!(fs.list_versions(store).await.unwrap(), before, "G2/G3");
    }

    let retry = fs
        .update_materialized(store, "/a.md", &current.generation(), "same")
        .await
        .unwrap();
    assert_eq!(retry, current, "G1");
    assert_eq!(fs.list_versions(store).await.unwrap(), before, "G1");

    let applied = fs
        .update(store, &current.generation(), "changed")
        .await
        .unwrap();
    assert_eq!(applied.version, current.version + 1, "G4");
    assert_eq!(applied.path, "/a.md", "G4");
}

pub(crate) async fn rename_replace_orphans_the_destination_id(fs: &dyn MemoryRepository) {
    // Rename-replace cause/effect table: R1 source+different destination ->
    // source id/content survive at destination, both old paths cease naming the
    // displaced identity; R2 now-absent source -> NotFound and the committed
    // destination stays unchanged. Pure move and same-path success are covered
    // by extended_conformance, so all path-presence partitions reuse this owner.
    let store = "rename_replace_identity";
    let dst = fs.create(store, "/dst.md", "old-dst").await.unwrap();
    let src = fs.create(store, "/src.md", "src").await.unwrap();
    assert_ne!(dst.id, src.id);
    let moved = fs.rename(store, "/src.md", "/dst.md").await.unwrap();
    assert_eq!(moved.id, src.id, "R1 keeps the source id");
    assert_eq!(moved.content.as_deref(), Some("src"));
    assert!(
        matches!(
            fs.update(store, &dst.generation(), "zombie").await,
            Err(MemErr::NotFound(_))
        ),
        "R1 orphans the displaced destination id"
    );
    let at_dst = fs.get_by_path(store, "/dst.md").await.unwrap().unwrap();
    assert_eq!(at_dst.id, src.id);
    assert_eq!(at_dst.content.as_deref(), Some("src"));
    assert!(matches!(
        fs.rename(store, "/src.md", "/dst.md").await,
        Err(MemErr::NotFound(_))
    ));
    assert_eq!(
        fs.get_by_path(store, "/dst.md").await.unwrap(),
        Some(at_dst),
        "R2 leaves the committed survivor unchanged"
    );
}

pub(crate) async fn ids_stay_monotonic_across_delete(fs: &dyn MemoryRepository) {
    let a = fs.create("s", "/a.md", "a").await.unwrap();
    let b = fs.create("s", "/b.md", "b").await.unwrap();
    assert_eq!((a.id.as_str(), b.id.as_str()), ("mem_1", "mem_2"));
    // Delete the highest-ordinal memory, then create again.
    fs.delete_by_path("s", "/b.md").await.unwrap();
    let c = fs.create("s", "/c.md", "c").await.unwrap();
    assert_eq!(
        c.id, "mem_3",
        "a create after deleting the top id must not reuse it"
    );
}

/// Run the same conformance suite over any `MemoryRepository` backend.
pub(crate) async fn conformance(fs: &dyn MemoryRepository) {
    copy_conflict_conformance(fs).await;
    let store = "memstore_1";

    // create → version 1, sha set, content round-trips.
    let m = fs.create(store, "/notes/today.md", "alpha").await.unwrap();
    assert_eq!(m.path, "/notes/today.md");
    assert_eq!(m.version, 1);
    assert_eq!(m.content_sha256, sha256_hex("alpha"));
    assert_eq!(m.content.as_deref(), Some("alpha"));
    assert!(m.created_unix_nanos > 0 && m.updated_unix_nanos >= m.created_unix_nanos);

    // duplicate path → PathConflict.
    assert!(matches!(
        fs.create(store, "/notes/today.md", "x").await,
        Err(MemErr::PathConflict(_))
    ));

    // get_by_path.
    let got = fs
        .get_by_path(store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.content.as_deref(), Some("alpha"));
    assert!(fs.get_by_path(store, "/nope.md").await.unwrap().is_none());

    // update with the right base sha → version bumps, content changes.
    let up = fs.update(store, &m.generation(), "beta").await.unwrap();
    assert_eq!(up.version, 2);
    assert_eq!(up.content.as_deref(), Some("beta"));

    // update with a STALE base sha (the original) → Conflict carrying live "beta".
    match fs.update(store, &m.generation(), "gamma").await {
        Err(MemErr::Conflict { current }) => {
            assert_eq!(current.content.as_deref(), Some("beta"));
            assert_eq!(current.content_sha256, sha256_hex("beta"));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    // ...and the store still holds "beta" (no clobber).
    assert_eq!(
        fs.get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("beta")
    );

    // idempotent update: the exact current generation and content → Ok.
    let current = fs
        .get_by_path(store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    let idem = fs
        .update(store, &current.generation(), "beta")
        .await
        .unwrap();
    assert_eq!(idem.content.as_deref(), Some("beta"));

    // list under a prefix.
    fs.create(store, "/notes/other.md", "o").await.unwrap();
    fs.create(store, "/root.md", "r").await.unwrap();
    let mut notes: Vec<_> = fs
        .list(store, "/notes")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    notes.sort();
    assert_eq!(notes, vec!["/notes/other.md", "/notes/today.md"]);
    assert_eq!(fs.list(store, "/").await.unwrap().len(), 3);

    // rename-replace: move over an existing target atomically, id preserved.
    let before = fs
        .get_by_path(store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    let moved = fs
        .rename(store, "/notes/today.md", "/notes/other.md")
        .await
        .unwrap();
    assert_eq!(moved.id, before.id, "rename preserves the memory id");
    assert_eq!(moved.path, "/notes/other.md");
    assert_eq!(moved.content.as_deref(), Some("beta"));
    assert!(
        fs.get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .is_none(),
        "the source path is gone after rename"
    );
    assert_eq!(
        fs.get_by_path(store, "/notes/other.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("beta"),
        "the destination was atomically replaced with the source content"
    );

    // delete (idempotent).
    fs.delete_by_path(store, "/notes/other.md").await.unwrap();
    assert!(
        fs.get_by_path(store, "/notes/other.md")
            .await
            .unwrap()
            .is_none()
    );
    fs.delete_by_path(store, "/notes/other.md").await.unwrap(); // absent → Ok

    // path validation.
    assert!(matches!(
        fs.create(store, "relative.md", "x").await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(matches!(
        fs.create(store, "/a/../b.md", "x").await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(matches!(
        fs.create(store, "/a//b.md", "x").await,
        Err(MemErr::InvalidPath(_))
    ));

    // size cap.
    let big = "x".repeat(MAX_MEMORY_BYTES + 1);
    assert!(matches!(
        fs.create(store, "/big.md", &big).await,
        Err(MemErr::TooLarge)
    ));
}

/// Cause-effect-graph cases beyond `conformance`: the remaining validation
/// branches, CAS/rename **precedence** (who masks whom), and prefix-boundary
/// correctness. Runs over any backend.
pub(crate) async fn extended_conformance(fs: &dyn MemoryRepository) {
    let store = "ext";

    // G-C1: the other validate_path rejections — root, a control char, a `.`
    // segment, and a path one byte over the cap.
    let over_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES)); // len == cap + 1
    for bad in ["/", "/ctrl\nseg.md", "/a/./b.md", over_cap.as_str()] {
        assert!(
            matches!(
                fs.create(store, bad, "x").await,
                Err(MemErr::InvalidPath(_))
            ),
            "expected InvalidPath for {bad:?}"
        );
    }

    // G-C2: size boundary — content exactly at the cap is accepted (only `> cap`
    // is TooLarge, which `conformance` already checks).
    let at_cap = "x".repeat(MAX_MEMORY_BYTES);
    let m_cap = fs.create(store, "/at-cap.md", &at_cap).await.unwrap();
    assert_eq!(m_cap.content_size, MAX_MEMORY_BYTES as u64);

    // G-U1: validate_size runs BEFORE the id lookup, so an oversized update to a
    // nonexistent id reports TooLarge, not NotFound (precedence pin).
    let big = "x".repeat(MAX_MEMORY_BYTES + 1);
    assert!(matches!(
        fs.update(
            store,
            &MemoryGeneration {
                memory_id: "mem_does_not_exist".into(),
                version: 1,
                content_sha256: "sha".into()
            },
            &big
        )
        .await,
        Err(MemErr::TooLarge)
    ));

    // G-U2: an idempotent update with the current generation must
    // NOT bump the version.
    let m = fs.create(store, "/idem.md", "v0").await.unwrap();
    let up = fs.update(store, &m.generation(), "v1").await.unwrap();
    assert_eq!(up.version, 2);
    let idem = fs.update(store, &up.generation(), "v1").await.unwrap();
    assert_eq!(
        idem.version, 2,
        "idempotent write leaves the version unchanged"
    );
    assert_eq!(idem.content.as_deref(), Some("v1"));

    // G-R1: a rename to an invalid path fails before touching the source.
    fs.create(store, "/src.md", "s").await.unwrap();
    assert!(matches!(
        fs.rename(store, "/src.md", "relative").await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(
        fs.get_by_path(store, "/src.md").await.unwrap().is_some(),
        "a rejected rename must not destroy the source"
    );

    // G-R2: rename with from == to returns the current memory and does NOT bump
    // the version (the success shape of from==to, distinct from the NotFound one).
    let src = fs.get_by_path(store, "/src.md").await.unwrap().unwrap();
    let same = fs.rename(store, "/src.md", "/src.md").await.unwrap();
    assert_eq!(same.id, src.id);
    assert_eq!(
        same.version, src.version,
        "from==to leaves the version unchanged"
    );

    // G-R3: a pure move (destination absent) preserves the id and bumps version.
    let moved = fs.rename(store, "/src.md", "/moved.md").await.unwrap();
    assert_eq!(moved.id, src.id, "a move preserves the memory id");
    assert_eq!(moved.version, src.version + 1);
    assert!(fs.get_by_path(store, "/src.md").await.unwrap().is_none());
    assert_eq!(
        fs.get_by_path(store, "/moved.md")
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
    fs.create(pstore, "/notes", "a").await.unwrap();
    fs.create(pstore, "/notes/x.md", "b").await.unwrap();
    fs.create(pstore, "/notesbar", "c").await.unwrap();
    let mut under: Vec<_> = fs
        .list(pstore, "/notes")
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
    assert_eq!(fs.list(pstore, "").await.unwrap().len(), 3);

    // delete on a store that was never created is a no-op Ok (idempotent).
    fs.delete_by_path("never_created_store", "/x.md")
        .await
        .unwrap();
}

/// The `NotFound` paths of `update`/`rename`, over any backend.
pub(crate) async fn not_found_paths(fs: &dyn MemoryRepository) {
    let store = "s";
    // update on a store that does not exist yet → NotFound.
    assert!(matches!(
        fs.update(
            store,
            &MemoryGeneration {
                memory_id: "no_id".into(),
                version: 1,
                content_sha256: "sha".into()
            },
            "x"
        )
        .await,
        Err(MemErr::NotFound(_))
    ));
    // update an unknown id in an existing store → NotFound.
    fs.create(store, "/seed.md", "s").await.unwrap();
    assert!(matches!(
        fs.update(
            store,
            &MemoryGeneration {
                memory_id: "no_such_id".into(),
                version: 1,
                content_sha256: "sha".into()
            },
            "x"
        )
        .await,
        Err(MemErr::NotFound(_))
    ));
    // rename a source that does not exist → NotFound (both the from==to and the
    // from!=to shapes).
    assert!(matches!(
        fs.rename(store, "/gone.md", "/x.md").await,
        Err(MemErr::NotFound(_))
    ));
    assert!(matches!(
        fs.rename(store, "/gone.md", "/gone.md").await,
        Err(MemErr::NotFound(_))
    ));
}

/// A public API head update is one aggregate transaction: path validation and
/// CAS happen before any write; content + rename-replace + history commit once.
pub(crate) async fn atomic_head_update_conformance(fs: &dyn MemoryRepository) {
    let store = "atomic-head";
    let source = fs.create(store, "/source.md", "v1").await.unwrap();
    let destination = fs.create(store, "/destination.md", "old").await.unwrap();
    let initial_versions = fs.list_versions(store).await.unwrap().len();

    assert!(matches!(
        fs.update_head(
            store,
            &source.generation(),
            "must-not-commit",
            Some("relative.md"),
        )
        .await,
        Err(MemErr::InvalidPath(_))
    ));
    assert_eq!(
        fs.get_by_path(store, "/source.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v1")
    );
    assert_eq!(
        fs.list_versions(store).await.unwrap().len(),
        initial_versions
    );

    let mut stale = source.generation();
    stale.content_sha256 = "stale".into();
    assert!(matches!(
        fs.update_head(store, &stale, "v1", Some("/moved.md"),)
            .await,
        Err(MemErr::Conflict { .. })
    ));
    assert!(fs.get_by_path(store, "/source.md").await.unwrap().is_some());
    assert!(fs.get_by_path(store, "/moved.md").await.unwrap().is_none());
    assert_eq!(
        fs.list_versions(store).await.unwrap().len(),
        initial_versions
    );

    let updated = fs
        .update_head(store, &source.generation(), "v2", Some("/destination.md"))
        .await
        .unwrap();
    assert_eq!(updated.id, source.id);
    assert_eq!(updated.path, "/destination.md");
    assert_eq!(updated.content.as_deref(), Some("v2"));
    assert_eq!(updated.version, source.version + 1);
    assert!(fs.get_by_path(store, "/source.md").await.unwrap().is_none());
    assert_ne!(updated.id, destination.id);
    let versions = fs.list_versions(store).await.unwrap();
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

    let idempotent = fs
        .update_head(store, &updated.generation(), "v2", Some("/destination.md"))
        .await
        .unwrap();
    assert_eq!(idempotent.version, updated.version);
    assert_eq!(fs.list_versions(store).await.unwrap().len(), versions.len());
}

pub(crate) async fn concurrent_create_one_winner<F>(fs: Arc<F>)
where
    F: MemoryRepository + Send + Sync + 'static,
{
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let fs = fs.clone();
        handles.push(tokio::spawn(async move {
            fs.create("s", "/race.md", &format!("v{i}")).await
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
    F: MemoryRepository + Send + Sync + 'static,
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
    let m = fs.create("s", "/c.md", "v0").await.unwrap();
    let before = fs.list_versions("s").await.unwrap();
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let (fs, base) = (fs.clone(), m.generation());
        handles.push(tokio::spawn(async move {
            fs.update("s", &base, &format!("w{i}")).await
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
        fs.get_by_path("s", "/c.md").await.unwrap(),
        Some(winner.clone()),
        "E1/E2"
    );
    let history = fs.list_versions("s").await.unwrap();
    assert_eq!(history.len(), before.len() + 1, "E4");
    assert_eq!(&history[..before.len()], before.as_slice(), "E4 prefix");
    let appended = &history[before.len()];
    assert_eq!(appended.memory_id, winner.id, "E4");
    assert_eq!(appended.operation, MemoryVersionOperation::Modified, "E4");
    assert_eq!(appended.path, winner.path, "E4");
    assert_eq!(appended.content, winner.content, "E4");
}
