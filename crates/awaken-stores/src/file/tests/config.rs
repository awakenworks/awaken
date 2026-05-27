use super::*;

// ── ConfigStore::put_if_revision ──

#[tokio::test]
async fn file_store_put_if_revision_basic() {
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;

    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());

    let value_r1 = serde_json::json!({"spec": {"id": "k"}, "meta": {"source": {"kind": "user"}, "revision": 1}});
    // Insert: no record, expected=0 → succeeds.
    store
        .put_if_revision("ns", "k", &value_r1, 0)
        .await
        .unwrap();
    let stored = ConfigStore::get(&store, "ns", "k").await.unwrap().unwrap();
    assert_eq!(stored["meta"]["revision"], 1);

    // Conflict: expected=0 again should fail.
    let err = store
        .put_if_revision("ns", "k", &value_r1, 0)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::VersionConflict {
            expected: 0,
            actual: 1
        }
    ));

    // Correct CAS: expected=1 → update to revision 2.
    let value_r2 = serde_json::json!({"spec": {"id": "k"}, "meta": {"source": {"kind": "user"}, "revision": 2}});
    store
        .put_if_revision("ns", "k", &value_r2, 1)
        .await
        .unwrap();
    let stored = ConfigStore::get(&store, "ns", "k").await.unwrap().unwrap();
    assert_eq!(stored["meta"]["revision"], 2);
}

#[tokio::test]
async fn file_store_put_if_absent_and_delete_if_revision() {
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;

    let td = TempDir::new().unwrap();
    let store = FileStore::new(td.path());

    let value = serde_json::json!({
        "spec": {"id": "k"},
        "meta": {"source": {"kind": "user"}, "revision": 7}
    });
    store.put_if_absent("ns", "k", &value).await.unwrap();

    let err = store.put_if_absent("ns", "k", &value).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(id) if id == "ns/k"));

    let err = store.delete_if_revision("ns", "k", 6).await.unwrap_err();
    assert!(matches!(
        err,
        StorageError::VersionConflict {
            expected: 6,
            actual: 7
        }
    ));
    assert!(ConfigStore::get(&store, "ns", "k").await.unwrap().is_some());

    store.delete_if_revision("ns", "k", 7).await.unwrap();
    assert!(ConfigStore::get(&store, "ns", "k").await.unwrap().is_none());
}

#[tokio::test]
async fn file_store_put_if_revision_is_atomic_across_store_instances() {
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::storage::StorageError;

    const WRITERS: usize = 16;
    let td = TempDir::new().unwrap();
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::with_capacity(WRITERS);

    for i in 0..WRITERS {
        let path = td.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            let store = FileStore::new(path);
            let value = serde_json::json!({
                "spec": {"id": "race", "winner": i},
                "meta": {"source": {"kind": "user"}, "revision": 1}
            });
            barrier.wait().await;
            store.put_if_revision("ns", "race", &value, 0).await
        }));
    }

    let results = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|result| result.expect("task join"))
        .collect::<Vec<_>>();
    let successes = results.iter().filter(|result| result.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|result| {
            matches!(
                result,
                Err(StorageError::VersionConflict {
                    expected: 0,
                    actual: 1
                })
            )
        })
        .count();

    assert_eq!(successes, 1, "exactly one concurrent create may win");
    assert_eq!(
        conflicts,
        WRITERS - 1,
        "every losing create must observe the winning revision"
    );

    let store = FileStore::new(td.path());
    let stored = ConfigStore::get(&store, "ns", "race")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored["meta"]["revision"], 1);
}

#[tokio::test]
async fn file_store_delete_and_update_same_revision_are_mutually_exclusive() {
    use awaken_contract::contract::config_store::ConfigStore;

    let td = TempDir::new().unwrap();
    let seed_store = FileStore::new(td.path());
    let value_r1 = serde_json::json!({
        "spec": {"id": "duel", "value": 1},
        "meta": {"source": {"kind": "user"}, "revision": 1}
    });
    seed_store
        .put_if_revision("ns", "duel", &value_r1, 0)
        .await
        .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let delete_path = td.path().to_path_buf();
    let update_path = td.path().to_path_buf();
    let delete_barrier = Arc::clone(&barrier);
    let update_barrier = Arc::clone(&barrier);

    let delete = tokio::spawn(async move {
        let store = FileStore::new(delete_path);
        delete_barrier.wait().await;
        store.delete_if_revision("ns", "duel", 1).await
    });
    let update = tokio::spawn(async move {
        let store = FileStore::new(update_path);
        let value_r2 = serde_json::json!({
            "spec": {"id": "duel", "value": 2},
            "meta": {"source": {"kind": "user"}, "revision": 2}
        });
        update_barrier.wait().await;
        store.put_if_revision("ns", "duel", &value_r2, 1).await
    });

    let delete_ok = delete.await.unwrap().is_ok();
    let update_ok = update.await.unwrap().is_ok();
    assert_ne!(
        delete_ok, update_ok,
        "same-revision delete and update must not both succeed or both fail"
    );

    let store = FileStore::new(td.path());
    let stored = ConfigStore::get(&store, "ns", "duel").await.unwrap();
    if delete_ok {
        assert!(stored.is_none(), "successful delete must remove the record");
    } else {
        assert_eq!(
            stored.expect("successful update must leave record")["meta"]["revision"],
            2
        );
    }
}
