// Shared numeric-SQL/partitioned-object feed laws.
pub async fn assert_portable_dispatch_scan_conformance<
    S: ConformanceDispatch + DispatchOperationalFeed,
>(
    store: &S,
    ns: &str,
) {
    use awaken_dispatch_contract::DispatchScanCursor;
    let a = dispatch(ns, "portable-a", "portable-thread-a");
    let b = dispatch(ns, "portable-b", "portable-thread-b");
    // Establish a checkpoint independently from individual event sequences.
    let mut completions: Option<DispatchScanCursor> = None;
    loop {
        let page = store
            .scan_completions(completions.as_ref(), 64)
            .await
            .unwrap();
        completions = Some(page.next_cursor);
        if page.events.is_empty() {
            break;
        }
    }
    let mut operations: Option<DispatchScanCursor> = None;
    loop {
        let page = store
            .scan_events(operations.as_ref(), 64, None)
            .await
            .unwrap();
        operations = Some(page.next_cursor);
        if page.events.is_empty() {
            break;
        }
    }
    store.enqueue(a.clone()).await.unwrap();
    let first = store
        .claim_run(
            a.run_id(),
            "portable-a-owner",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .settle(a.run_id(), first.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    let first_page = store
        .scan_completions(completions.as_ref(), 1)
        .await
        .unwrap();
    assert_eq!(first_page.events.len(), 1);
    assert_eq!(&first_page.events[0].run_id, a.run_id());
    // A second partition starts after the first page. Its local sequence may
    // equal an already-consumed sequence in another partition.
    store.enqueue(b.clone()).await.unwrap();
    let second = store
        .claim_run(
            b.run_id(),
            "portable-b-owner",
            LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .settle(b.run_id(), second.lease.epoch(), DispatchOutcome::Done, &[])
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    let second_page = store
        .scan_completions(Some(&first_page.next_cursor), 1)
        .await
        .unwrap();
    assert_eq!(second_page.events.len(), 1);
    assert_eq!(&second_page.events[0].run_id, b.run_id());
    let empty = store
        .scan_completions(Some(&second_page.next_cursor), 0)
        .await
        .unwrap();
    assert!(empty.events.is_empty());
    assert_eq!(empty.next_cursor, second_page.next_cursor);
    assert!(
        store
            .scan_events(Some(&second_page.next_cursor), 1, None)
            .await
            .is_err(),
        "cursor kind isolation"
    );
    let filtered = store.scan_events(None, 1, Some(a.run_id())).await.unwrap();
    assert!(
        store
            .scan_events(Some(&filtered.next_cursor), 1, Some(b.run_id()))
            .await
            .is_err(),
        "Run filter isolation"
    );
    let mut seen = std::collections::BTreeMap::<RunId, usize>::new();
    for _ in 0..16 {
        let page = store
            .scan_events(operations.as_ref(), 1, None)
            .await
            .unwrap();
        operations = Some(page.next_cursor);
        if page.events.is_empty() {
            break;
        }
        for event in page.events {
            *seen.entry(event.operation.run_id().clone()).or_default() += 1;
        }
    }
    assert_eq!(
        seen.get(a.run_id()),
        Some(&2),
        "claim and settle of first partition"
    );
    assert_eq!(
        seen.get(b.run_id()),
        Some(&2),
        "claim and settle of late partition"
    );
    assert_eq!(seen.len(), 2, "no loss, duplicate, or unrelated replay");
    assert_eq!(
        store.completion_for_run(a.run_id()).await.unwrap().unwrap(),
        first_page.events[0]
    );
}
