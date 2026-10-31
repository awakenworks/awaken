//! T06-C02: revocation after a long wait in the Worker File content read.
//!
//! Production entrypoint: the real `worker_file_content_router` handler over
//! real HTTP, called by the production `HttpFileContentSource` client. The
//! collaborators are wrapped, not replaced: `GatedDispatch` delegates to the
//! real `MemoryDispatchStore` after a barrier at the commit-epoch lock wait,
//! and `GatedSource` delegates to the real `ApplicationFileContentSource`
//! after a barrier inside the physical read.
//!
//! Decision table (the event is confirmed by a barrier, never by sleep):
//! | rule | window                    | revocation                 | expected                           |
//! | P1   | inside physical read      | settle the dispatch claim  | settle linearizes after the read;  |
//! |      |                           |                            | next read 409, zero source entries |
//! | P2   | commit-epoch lock wait    | settle the dispatch claim  | 409, zero source entries           |
//! | G1   | commit-epoch lock wait    | claim lease expires        | 409, zero source entries           |
//! | G2   | inside physical read      | claim lease expires        | no bytes released, fenced read     |
//! Negative control (executed): an in-test pipeline that checks authority
//! before the long wait and then reads must be detected by the same oracle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use awaken_dispatch::{
    CommitEpochGuard, DispatchClaims as _, DispatchCommitAuthority, DispatchEnqueue as _,
    DispatchError, DispatchSettlement as _, MemoryDispatchStore, RunClaim, RunDispatch,
    WorkerIdentity,
};
use awaken_dispatch_testkit::worker_http as support;
use awaken_file_store::FileStore as _;
use awaken_resource_application::ApplicationFileContentSource;
use awaken_resource_contract::FileCatalog as _;
use awaken_resource_contract::{
    FileContentSource, FileContentSourceError, FileReadPurpose, ResolvedFileContent,
};
use awaken_resource_worker_http::HttpFileContentSource;
use awaken_resource_worker_http::{WorkerFileContentService, worker_file_content_router};
use awaken_session_realization_contract::ResourceOperationFence;
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

const WORKSPACE: &str = "workspace-file";
const FILE: &str = "file-gated";
const BYTES: &[u8] = b"gated-file-bytes";
/// Bound on every barrier wait, so a stalled producer fails naming its barrier.
const BARRIER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Barrier shared by both wrappers.
struct Gate {
    armed: AtomicBool,
    entries: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            armed: AtomicBool::new(false),
            entries: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl Gate {
    async fn pass(&self) {
        self.entries.fetch_add(1, Ordering::SeqCst);
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.acquire().await.expect("gate open").forget();
        }
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::SeqCst)
    }
}

struct GatedDispatch {
    inner: Arc<MemoryDispatchStore>,
    gate: Gate,
}

#[async_trait::async_trait]
impl DispatchCommitAuthority for GatedDispatch {
    async fn worker_owns_run(
        &self,
        identity: &WorkerIdentity,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<RunClaim>, DispatchError> {
        self.inner.worker_owns_run(identity, run_id).await
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        self.gate.pass().await;
        self.inner.lock_commit_epoch(claim).await
    }
}

struct GatedSource {
    inner: Arc<dyn FileContentSource<ResourceOperationFence>>,
    gate: Gate,
    bytes_released: AtomicUsize,
    fenced_reads: AtomicUsize,
}

#[async_trait::async_trait]
impl FileContentSource<ResourceOperationFence> for GatedSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        purpose: &FileReadPurpose,
        fence: Option<&ResourceOperationFence>,
    ) -> Result<Option<ResolvedFileContent>, FileContentSourceError> {
        if fence.is_some() {
            self.fenced_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.gate.pass().await;
        let resolved = self
            .inner
            .read(workspace_id, file_id, purpose, fence)
            .await?;
        if let Some(content) = &resolved {
            self.bytes_released
                .fetch_add(content.bytes.len(), Ordering::SeqCst);
        }
        Ok(resolved)
    }
}

struct Fixture {
    clock_start: u64,
    clock: Arc<awaken_lease_testkit::ManualClock>,
    dispatch: Arc<MemoryDispatchStore>,
    gated_dispatch: Arc<GatedDispatch>,
    source: Arc<GatedSource>,
    claim: RunClaim,
    client: HttpFileContentSource,
}

fn resources() -> awaken_session_configuration_contract::resource::ResolvedSessionResources {
    awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
        vec![
            awaken_session_configuration_contract::resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                source:
                    awaken_session_configuration_contract::resource::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from(FILE),
                    },
                mount_path: "/workspace/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            },
        ],
        Vec::new(),
    )
    .unwrap()
}

async fn build_fixture() -> Fixture {
    let clock_start = support::unix_now_ms();
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(clock_start));
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = store.put(BYTES).await.unwrap();
    store
        .create_file(awaken_resource_contract::FileRecord {
            id: FILE.into(),
            workspace_id: WORKSPACE.into(),
            blob_id: digest,
            filename: "input.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: BYTES.len() as u64,
            created_at: "2026-07-30T00:00:00Z".into(),
            expires_at: None,
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-file").await;
    let dispatch = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let request = RunDispatch::new(support::activation(FILE))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from(WORKSPACE),
        ))
        .with_session_resources(awaken_dispatch::SessionResourceEnvelope::new(
            WORKSPACE,
            serde_json::to_string(&resources()).unwrap(),
        ));
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(&identity.lease_owner(), 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    let lifecycle = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .unwrap(),
    );
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        store.clone(),
        store,
        lifecycle,
        clock.clone(),
    ));
    let source = Arc::new(GatedSource {
        inner: Arc::new(ApplicationFileContentSource::new(application)),
        gate: Gate::default(),
        bytes_released: AtomicUsize::new(0),
        fenced_reads: AtomicUsize::new(0),
    });
    let gated_dispatch = Arc::new(GatedDispatch {
        inner: dispatch.clone(),
        gate: Gate::default(),
    });
    let service = Arc::new(WorkerFileContentService::new(
        source.clone(),
        gated_dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory,
        clock.clone(),
    ));
    let address = support::serve(worker_file_content_router(service)).await;
    let client = HttpFileContentSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );
    Fixture {
        clock_start,
        clock,
        dispatch,
        gated_dispatch,
        source,
        claim,
        client,
    }
}

type ReadOutcome = Result<Option<ResolvedFileContent>, String>;

async fn read(fixture: &Fixture) -> ReadOutcome {
    fixture
        .client
        .read(
            WORKSPACE,
            FILE,
            &FileReadPurpose::SessionResource,
            Some(&ResourceOperationFence::Run(fixture.claim.clone())),
        )
        .await
        .map_err(|error| error.to_string())
}

async fn settle(dispatch: &MemoryDispatchStore, claim: &RunClaim) {
    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_dispatch::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
}

/// T06-C02 oracle for a read whose authority was revoked before the bytes
/// could be released: a 403/409 refusal and no File bytes delivered.
fn check_denied_after_revocation(outcome: &ReadOutcome) -> Result<(), String> {
    match outcome {
        Ok(Some(content)) => Err(format!(
            "{} File byte(s) released after revocation",
            content.bytes.len()
        )),
        Ok(None) => Err("revoked read reported a missing File instead of a refusal".into()),
        Err(error) if error.contains("403") || error.contains("409") => Ok(()),
        Err(error) => Err(format!("revoked read failed for another reason: {error}")),
    }
}

async fn yield_many() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn t06_c02_settlement_during_physical_read_linearizes_after_the_read() {
    // P1: the dispatch commit epoch is the linearization point. A settle
    // attempted while the physical read is held at the barrier cannot
    // complete until the read's response is done; afterwards the claim is
    // revoked and the next read is refused without touching the source.
    let fixture = build_fixture().await;
    fixture.source.gate.armed.store(true, Ordering::SeqCst);
    // The block hands back the still-pending settle task on purpose: it may
    // only be awaited after the read has returned.
    #[allow(clippy::async_yields_async)]
    let (outcome, pending_settle) = tokio::join!(read(&fixture), async {
        tokio::time::timeout(BARRIER_WAIT, fixture.source.gate.entered.notified())
            .await
            .expect("the physical File read never reached its source barrier");
        let settle_task = tokio::spawn({
            let dispatch = fixture.dispatch.clone();
            let claim = fixture.claim.clone();
            async move { settle(&dispatch, &claim).await }
        });
        yield_many().await;
        assert!(
            !settle_task.is_finished(),
            "P1: revocation must wait for the commit-epoch guard held by the read"
        );
        fixture.source.gate.release.add_permits(1);
        settle_task
    });
    let content = outcome
        .expect("P1 read linearized before revocation")
        .expect("P1 File");
    assert_eq!(content.bytes, BYTES, "P1 exact bytes");
    tokio::time::timeout(BARRIER_WAIT, pending_settle)
        .await
        .expect("P1 settle never completed after the read released its commit-epoch guard")
        .unwrap();

    let entries = fixture.source.gate.entries();
    let released = fixture.source.bytes_released.load(Ordering::SeqCst);
    let after = read(&fixture).await;
    check_denied_after_revocation(&after)
        .unwrap_or_else(|violation| panic!("P1 after settle: {violation}"));
    assert_eq!(
        fixture.source.gate.entries(),
        entries,
        "P1: no cached-success re-read"
    );
    assert_eq!(
        fixture.source.bytes_released.load(Ordering::SeqCst),
        released,
        "P1: no new bytes after revocation"
    );
}

#[tokio::test]
async fn t06_c02_settlement_during_lock_wait_is_rechecked_and_premature_check_is_detected() {
    // P2: settle while the handler waits at the commit-epoch lock.
    let fixture = build_fixture().await;
    fixture
        .gated_dispatch
        .gate
        .armed
        .store(true, Ordering::SeqCst);
    let (outcome, ()) = tokio::join!(read(&fixture), async {
        tokio::time::timeout(BARRIER_WAIT, fixture.gated_dispatch.gate.entered.notified())
            .await
            .expect("the File read never reached the commit-epoch lock barrier");
        settle(&fixture.dispatch, &fixture.claim).await;
        fixture.gated_dispatch.gate.release.add_permits(1);
    });
    check_denied_after_revocation(&outcome).unwrap_or_else(|violation| panic!("P2: {violation}"));
    assert!(
        outcome.as_ref().unwrap_err().contains("409"),
        "P2: {outcome:?}"
    );
    assert_eq!(fixture.source.gate.entries(), 0, "P2: zero physical reads");
    assert_eq!(
        fixture.source.bytes_released.load(Ordering::SeqCst),
        0,
        "P2"
    );

    // Negative control, executed: the same authority check moved before the
    // long wait (check, release the guard, wait, then read) must be caught
    // by the same oracle when the revocation lands during that wait.
    let fixture = build_fixture().await;
    let premature = async {
        let guard = fixture
            .dispatch
            .lock_commit_epoch(&fixture.claim)
            .await
            .unwrap()
            .expect("NC: claim current at the premature check");
        assert!(
            guard.is_live_at(fixture.clock_start),
            "NC premature check passes"
        );
        drop(guard);
        fixture.source.gate.armed.store(true, Ordering::SeqCst);
        fixture
            .source
            .read(WORKSPACE, FILE, &FileReadPurpose::SessionResource, None)
            .await
            .map_err(|error| error.to_string())
    };
    let (mutant, ()) = tokio::join!(premature, async {
        tokio::time::timeout(BARRIER_WAIT, fixture.source.gate.entered.notified())
            .await
            .expect("the physical File read never reached its source barrier");
        settle(&fixture.dispatch, &fixture.claim).await;
        fixture.source.gate.release.add_permits(1);
    });
    let violation = check_denied_after_revocation(&mutant)
        .expect_err("NC: a check placed before the wait must be detected");
    assert!(
        violation.contains("released after revocation"),
        "NC: {violation}"
    );
}

#[tokio::test]
async fn t06_c02_lease_expiry_during_lock_wait_is_rechecked() {
    // G1: the lease expires while the handler waits at the commit-epoch lock.
    let fixture = build_fixture().await;
    fixture
        .gated_dispatch
        .gate
        .armed
        .store(true, Ordering::SeqCst);
    let (outcome, ()) = tokio::join!(read(&fixture), async {
        tokio::time::timeout(BARRIER_WAIT, fixture.gated_dispatch.gate.entered.notified())
            .await
            .expect("the File read never reached the commit-epoch lock barrier");
        fixture.clock.set(fixture.clock_start + 60_000);
        fixture.gated_dispatch.gate.release.add_permits(1);
    });
    check_denied_after_revocation(&outcome).unwrap_or_else(|violation| panic!("G1: {violation}"));
    assert_eq!(fixture.source.gate.entries(), 0, "G1: zero physical reads");
}

#[tokio::test]
async fn t06_c02_lease_expiry_during_physical_read_releases_no_bytes() {
    // G2: the lease expires while the physical read is held at the barrier.
    let fixture = build_fixture().await;
    fixture.source.gate.armed.store(true, Ordering::SeqCst);
    let (outcome, ()) = tokio::join!(read(&fixture), async {
        tokio::time::timeout(BARRIER_WAIT, fixture.source.gate.entered.notified())
            .await
            .expect("the physical File read never reached its source barrier");
        fixture.clock.set(fixture.clock_start + 60_000);
        fixture.source.gate.release.add_permits(1);
    });
    assert_eq!(
        fixture.source.fenced_reads.load(Ordering::SeqCst),
        1,
        "G2: the physical read must carry the current operation fence"
    );
    check_denied_after_revocation(&outcome).unwrap_or_else(|violation| panic!("G2: {violation}"));
}
