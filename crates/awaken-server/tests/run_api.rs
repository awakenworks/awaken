use awaken_contract::contract::storage::RunQuery;

#[test]
fn run_query_defaults() {
    let q = RunQuery::default();
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, 50);
    assert!(q.thread_id.is_none());
    assert!(q.status.is_none());
}
