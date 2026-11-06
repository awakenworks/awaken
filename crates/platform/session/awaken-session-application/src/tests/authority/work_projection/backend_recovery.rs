//! T41-C03 (恢复扫描完整可操作), real backends: the Work-dispatch recovery
//! scan over 257 durable Sessions (two pages) through independently opened
//! stores — two file SQLite handles plus a reopen; two PostgreSQL pools plus a
//! fresh pool.
//!
//! Oracles (groups JSON order):
//! - O0 an incomplete scan never authorizes purge or retirement;
//! - O1 diagnostics keep the failing domain and the original identity;
//! - O2 after the fault is repaired the same identities continue, without a
//!   manual database edit.
//!
//! Negative control: "testing only the first page, or re-running under a new
//! identity each time" must fail. It is executed twice: (a) a verdict that
//! accepts the scan prefix the page-2 fault leaves behind (failure read as
//! "no more rows") yields no blocker, while the complete scan yields the
//! page-two blocker; (b) a ledger whose retry minted a new Work identity is
//! rejected by the same identity checker that accepts the production ledger.
//!
//! | Row | Store | Fault | Expected |
//! |---|---|---|---|
//! | B1 | handle A | recovery scan fails on page 2 | zero dispatch, one `<repository>` failure, purge guard errs |
//! | B2 | handle B | one Session's queue write fails | 256 settled, failure keeps that Session and `env-worker` |
//! | B3 | reopened handle | none | the failed Session settles with the same (environment, Session); 257 identities, one each |
//! | B4 | reopened handle | none | the purge guard names the page-two blocker |
use super::*;
use awaken_resource_contract::ResourcePurgeGuard as _;
use awaken_session_application_contract::SessionWorkerAssignment;
use std::collections::BTreeMap;

type Store = Arc<dyn crate::SessionStore>;

const ACTIVE_ROWS: usize = 256;

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// An idempotent queue model: one Work identity per (environment, Session),
/// every call kept, and a scripted per-Session outage.
#[derive(Default)]
struct RecoveryQueue {
    calls: Mutex<Vec<(String, String, String)>>,
    identities: Mutex<BTreeMap<(String, String), String>>,
    failing: Mutex<BTreeSet<String>>,
}

#[async_trait::async_trait]
impl SessionWorkerAssignment for RecoveryQueue {
    async fn enqueue_session(
        &self,
        environment_id: &str,
        session_id: &SessionId,
    ) -> Result<String, RunError> {
        if self.failing.lock().unwrap().contains(session_id.as_str()) {
            return Err(RunError::unavailable(format!(
                "scripted queue outage for {session_id}"
            )));
        }
        let identity = {
            let mut identities = self.identities.lock().unwrap();
            let next = format!("work-{}", identities.len() + 1);
            identities
                .entry((environment_id.to_string(), session_id.to_string()))
                .or_insert(next)
                .clone()
        };
        self.calls.lock().unwrap().push((
            environment_id.to_string(),
            session_id.to_string(),
            identity.clone(),
        ));
        Ok(identity)
    }

    async fn wake_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<String, RunError> {
        panic!("recovery projection must not revive a Session")
    }

    async fn retire_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<(), RunError> {
        panic!("recovery projection must not retire a Session")
    }
}

impl RecoveryQueue {
    /// This table's calls only (a shared PostgreSQL database holds others).
    fn calls_for(&self, prefix: &str) -> Vec<(String, String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session, _)| session.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// O2 checker: each Session keeps exactly one Work identity across runs.
fn identity_violations(calls: &[(String, String, String)]) -> Vec<String> {
    let mut first = BTreeMap::new();
    let mut violations = Vec::new();
    for (environment, session, identity) in calls {
        let original = first
            .entry((environment.clone(), session.clone()))
            .or_insert_with(|| identity.clone());
        if original != identity {
            violations.push(format!("{session} re-ran under {identity}"));
        }
    }
    violations
}

fn blocker_root(id: &str, memory: &str) -> PersistedSession {
    let memory_store_id = awaken_resource_contract::MemoryStoreId::from(memory);
    let mut blocker = persisted(id, true, "idle");
    blocker.resources = awaken_session_contract::SessionResourceState::from_active(
        crate::session_resource::ResolvedSessionResources::try_new(
            vec![crate::session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::from("page-two-memory"),
                source: crate::session_resource::ResolvedInputSource::MemoryStore {
                    memory_store_id: memory_store_id.clone(),
                    config: awaken_resource_contract::MemoryStoreConfigVersion {
                        memory_store_id,
                        version: awaken_resource_contract::ConfigVersion::INITIAL,
                        retention_policy: Default::default(),
                    },
                },
                mount_path: "/memory/page-two".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .expect("valid page-two MemoryStore binding"),
    );
    blocker
}

async fn guard_blockers(
    store: Store,
    memory: &str,
) -> Result<
    Vec<awaken_resource_contract::ResourceReference>,
    awaken_resource_contract::ResourcePurgeError,
> {
    SessionResourcePurgeGuard::new(store, Arc::new(UnusedFileCatalog))
        .blockers(
            &awaken_resource_contract::ResourceTarget::new(
                "workspace",
                awaken_resource_contract::ResourceKind::MemoryStore,
                memory,
            ),
            None,
            0,
        )
        .await
}

/// `reopen` opens one more independent handle on the same database.
async fn backend_recovery_table<R, RF>(prefix: &str, store_a: Store, store_b: Store, reopen: R)
where
    R: FnOnce() -> RF,
    RF: std::future::Future<Output = Store>,
{
    assert!(
        !std::ptr::addr_eq(Arc::as_ptr(&store_a), Arc::as_ptr(&store_b)),
        "independently opened stores"
    );
    let prefix = unique(prefix);
    let memory = format!("{prefix}-memory");
    let ids = (0..ACTIVE_ROWS)
        .map(|index| format!("{prefix}-a-{index:03}"))
        .chain(std::iter::once(format!("{prefix}-z-blocker")))
        .collect::<Vec<_>>();
    for id in &ids[..ACTIVE_ROWS] {
        create(store_a.as_ref(), persisted(id, true, "idle")).await;
    }
    let blocker = ids[ACTIVE_ROWS].clone();
    create(store_a.as_ref(), blocker_root(&blocker, &memory)).await;
    let failed = ids[100].clone();
    let queue = RecoveryQueue::default();

    // B1/O0/O1: a page-two outage is one repository-domain failure with zero
    // partial effects, and grants zero purge authority.
    let faulting = Arc::new(FaultingSessionRepository::new(store_a.clone()));
    faulting.fail_recovery_scan_on_call(2);
    let incomplete = reconcile_work_dispatches(faulting.as_ref(), &queue, 0).await;
    assert_eq!(
        incomplete.settled, 0,
        "B1/O0 no settlement from a partial scan"
    );
    assert!(
        queue.calls.lock().unwrap().is_empty(),
        "B1/O0 no partial dispatch"
    );
    assert_eq!(incomplete.failures.len(), 1, "B1/O1 one failure");
    assert_eq!(
        incomplete.failures[0].session_id, "<repository>",
        "B1/O1 the failing domain is the repository scan"
    );
    let faulted_guard = Arc::new(FaultingSessionRepository::new(store_a.clone()));
    faulted_guard.fail_recovery_scan_on_call(2);
    let partial = guard_blockers(faulted_guard, &memory).await;
    assert!(
        partial.is_err(),
        "B1/O0 an incomplete scan grants no purge authority"
    );

    // B2/O1: the full scan on handle B; one Session's queue write fails.
    queue.failing.lock().unwrap().insert(failed.clone());
    // An unarmed decorator only gives the handle a concrete repository type.
    let handle_b = FaultingSessionRepository::new(store_b.clone());
    let first = reconcile_work_dispatches(&handle_b, &queue, 0).await;
    let ours = first
        .failures
        .iter()
        .filter(|failure| failure.session_id.starts_with(&prefix))
        .collect::<Vec<_>>();
    assert_eq!(
        ours.len(),
        1,
        "B2/O1 one Session failure: {:?}",
        first.failures
    );
    assert_eq!(
        (ours[0].session_id.as_str(), ours[0].environment_id.as_str()),
        (failed.as_str(), "env-worker"),
        "B2/O1 the failure keeps the original Session and Environment"
    );
    assert!(
        ours[0].message.contains("scripted queue outage"),
        "B2/O1 the retry diagnostic is kept"
    );
    let dispatched_once = queue
        .calls_for(&prefix)
        .into_iter()
        .map(|(_, session, _)| session)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        dispatched_once.len(),
        ACTIVE_ROWS,
        "B2 every other row settled"
    );
    assert!(
        !dispatched_once.contains(&failed),
        "B2 the failed row is not settled"
    );

    // B3/O2: repair the fault and re-run through a reopened handle.
    queue.failing.lock().unwrap().clear();
    let store_c = reopen().await;
    let handle_c = FaultingSessionRepository::new(store_c.clone());
    let repaired = reconcile_work_dispatches(&handle_c, &queue, 0).await;
    assert!(
        repaired
            .failures
            .iter()
            .all(|failure| !failure.session_id.starts_with(&prefix)),
        "B3/O2 no remaining failure: {:?}",
        repaired.failures
    );
    let calls = queue.calls_for(&prefix);
    let settled = calls
        .iter()
        .map(|(_, session, _)| session.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        settled,
        ids.iter().cloned().collect::<BTreeSet<_>>(),
        "B3/O2 the dispatched set is exactly the 257 durable Sessions"
    );
    assert!(
        calls
            .iter()
            .any(|(environment, session, _)| session == &failed && environment == "env-worker"),
        "B3/O2 the failed Session continued under its original identity"
    );
    assert_eq!(
        identity_violations(&calls),
        Vec::<String>::new(),
        "B3/O2 no settled row received a second Work identity"
    );
    let identities = queue
        .identities
        .lock()
        .unwrap()
        .keys()
        .filter(|(_, session)| session.starts_with(&prefix))
        .count();
    assert_eq!(
        identities,
        ACTIVE_ROWS + 1,
        "B3/O2 one Work per durable Session"
    );

    // NC (b): a retry that re-ran under a new identity.
    let mut minted = calls.clone();
    let (environment, session, _) = minted[0].clone();
    minted.push((environment, session, "work-minted-by-retry".into()));
    assert!(
        !identity_violations(&minted).is_empty(),
        "NC a new identity per run is detected"
    );

    // B4/O0: only the complete scan names the page-two blocker.
    let complete = guard_blockers(store_c.clone(), &memory)
        .await
        .expect("B4 complete scan");
    assert_eq!(
        complete,
        vec![awaken_resource_contract::ResourceReference {
            kind: awaken_resource_contract::ResourceReferenceKind::SessionBinding,
            reference_id: blocker.clone(),
        }],
        "B4/O0 the page-two blocker protects its MemoryStore"
    );
    // NC (a): a verdict that accepts the faulted scan prefix as complete.
    let first_page_only = partial.unwrap_or_default();
    assert_ne!(
        first_page_only, complete,
        "NC a first-page-only verdict would authorize the purge"
    );
}

#[tokio::test]
async fn sqlite_recovery_scan_repairs_through_independent_handles_and_reopen() {
    let directory = tempfile::tempdir().expect("T41 SQLite directory");
    let path = directory
        .path()
        .join("t41-recovery.db")
        .to_str()
        .unwrap()
        .to_string();
    let open = |path: String| async move {
        Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .await
                .expect("T41 SQLite handle"),
        ) as Store
    };
    let store_a = open(path.clone()).await;
    let store_b = open(path.clone()).await;
    Box::pin(backend_recovery_table(
        "t41-sqlite",
        store_a,
        store_b,
        || open(path.clone()),
    ))
    .await;
}

#[tokio::test]
async fn postgres_recovery_scan_repairs_through_independent_pools() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let connect = |url: String| async move {
        Arc::new(
            awaken_session_store::PostgresManagedSessionRepository::connect(&url)
                .await
                .expect("T41 PostgreSQL pool"),
        ) as Store
    };
    let store_a = connect(url.clone()).await;
    let store_b = connect(url.clone()).await;
    Box::pin(backend_recovery_table(
        "t41-postgres",
        store_a,
        store_b,
        || connect(url.clone()),
    ))
    .await;
}

#[test]
fn postgres_recovery_scan_repairs_through_independent_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::authority::work_projection::backend_recovery::postgres_recovery_scan_repairs_through_independent_pools",
    );
}
