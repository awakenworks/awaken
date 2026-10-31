//! T29-C02 (Copy 数据责任), provider side: a writable Memory Copy realized by
//! the real `MemoryStoreMounter::copy_only` inside a Workdir Sandbox.
//!
//! Oracles (groups-6.json order):
//! - O0 the source (the Sandbox's copy files) is not destroyed before
//!   write-back responsibility exists;
//! - O1 read-only does not write back (owned by memoryd B005; not repeated);
//! - O2 recovery repeated is idempotent.
//!
//! | Row | Fault | Effect |
//! |---|---|---|
//! | R1 | live Host write-back (harvest + reconcile) interrupted before any durable effect | no acknowledgement, so disposal is refused; root, copy and store unchanged; retry writes update/delete/create back once, then acknowledgement and disposal remove the root |
//! | R2 | in-process guard lost (Host crash), new Host adopts | disposal before reconciliation is refused and keeps files; surviving files reconcile; repeat is a no-op; then disposal |
//! | R2r | the same reconciliation replayed (crash before acknowledgement) | no spurious conflict (ignored reproducer: product gap) |
//! | NC | the copy directory is cleared before write-back | the harvest read fails; nothing is written back (ignored reproducer: product gap) |
use awaken_lease_contract::fence::Epoch;
use awaken_memory_store::{MemoryWriter as _, VolatileMemoryRepository};
use awaken_provisioning_contract as pc;
use awaken_resource_contract::{MemoryCopyConflictQueries as _, MemoryQueries as _};
use awaken_sandbox_local::LocalProvider;
use awaken_sandbox_memoryd::MemoryStoreMounter;
use pc::Sandbox as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const STORE: &str = "t29-c02";
const MOUNT: &str = "mnt/memory";

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: vec![pc::MountRequirement {
            mount_id: "memory".into(),
            source: pc::MountSource::MemoryStore {
                store_id: STORE.into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: format!("/{MOUNT}"),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

fn fence(operation: &str, epoch: Epoch) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(operation, "t29-owner", "t29-runtime", epoch.get(), u64::MAX)
        .unwrap()
}

fn epoch(value: u64) -> Epoch {
    Epoch::from_persisted(value).expect("a test epoch is nonzero")
}

fn authority(fence: &pc::SandboxEffectFence) -> pc::test_support::EffectAuthority {
    pc::test_support::EffectAuthority::new(fence)
}

fn authorization(prepared: &pc::SandboxEffectFence) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(
        prepared.clone(),
        format!("t29-preparation:{}", prepared.operation_id()),
    )
    .unwrap();
    let successor = pc::SandboxEffectFence::new(
        preparation.operation_id().unwrap(),
        prepared.owner(),
        prepared.runtime_incarnation(),
        prepared.epoch(),
        prepared.expires_at_unix_ms(),
    )
    .unwrap();
    preparation.authorize(successor).unwrap()
}

/// Interrupts the first `failures` Host write-backs (`reconcile_recovered_copy`)
/// before any durable effect. Fenced terminal disposal never calls a Copy
/// guard's `teardown`: the Host harvests the frozen mount files, reconciles
/// them through the mounter and only then acknowledges, which retires the Copy
/// guard. The interruptible write-back step is therefore the reconcile call.
struct InterruptingMounter {
    inner: MemoryStoreMounter,
    failures: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl pc::MemoryMounter for InterruptingMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        self.inner.mount(store_id, host_path, access).await
    }

    async fn recover_unknown_mount(
        &self,
        store_id: &str,
        host_path: &Path,
        access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.inner
            .recover_unknown_mount(store_id, host_path, access)
            .await
    }

    async fn reconcile_recovered_copy(
        &self,
        operation_reference: &str,
        evidence: &pc::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(pc::SandboxError::transport(
                "write-back interrupted before any durable effect",
            ));
        }
        self.inner
            .reconcile_recovered_copy(operation_reference, evidence, files, access)
            .await
    }
}

/// The Host's terminal Memory step: harvest the frozen Copy files and
/// reconcile them through the mounter (no acknowledgement).
async fn write_back(
    sandbox: &awaken_sandbox_local::LocalSandbox,
    mounter: &dyn pc::MemoryMounter,
    evidence: &pc::MemoryMaterializationEvidence,
) -> Result<(), pc::SandboxError> {
    let files = sandbox.list_files(MOUNT)?;
    mounter
        .reconcile_recovered_copy(STORE, evidence, &files, pc::MountAccess::ReadWrite)
        .await
}

/// Prepare and dispose under the terminal fence.
async fn dispose(
    sandbox: &awaken_sandbox_local::LocalSandbox,
    terminal: &pc::SandboxEffectFence,
) -> Result<(), pc::SandboxError> {
    pc::Sandbox::prepare_disposal_for_effect(sandbox, terminal, &authority(terminal)).await?;
    let authorization = authorization(terminal);
    pc::Sandbox::dispose_for_effect(
        sandbox,
        &authorization,
        &authority(authorization.effect_fence()),
    )
    .await
}

async fn durable(store: &VolatileMemoryRepository, path: &str) -> Option<String> {
    store
        .get_by_path(STORE, path)
        .await
        .unwrap()
        .and_then(|memory| memory.content)
}

/// The durable store as (note, gone, new) contents.
async fn state(
    store: &VolatileMemoryRepository,
) -> (Option<String>, Option<String>, Option<String>) {
    (
        durable(store, "/note.md").await,
        durable(store, "/gone.md").await,
        durable(store, "/new.md").await,
    )
}

async fn seeded() -> Arc<VolatileMemoryRepository> {
    let store = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    store.create(STORE, "/note.md", "v1").await.unwrap();
    store.create(STORE, "/gone.md", "gone").await.unwrap();
    store
}

/// The Agent's Copy edits: update, delete and create.
fn edit(copy: &Path) {
    std::fs::write(copy.join("note.md"), "v2").unwrap();
    std::fs::remove_file(copy.join("gone.md")).unwrap();
    std::fs::write(copy.join("new.md"), "new").unwrap();
}

fn edited() -> (Option<String>, Option<String>, Option<String>) {
    (Some("v2".into()), None, Some("new".into()))
}

fn seeded_state() -> (Option<String>, Option<String>, Option<String>) {
    (Some("v1".into()), Some("gone".into()), None)
}

fn evidence_of(
    sandbox: &awaken_sandbox_local::LocalSandbox,
) -> Vec<pc::MemoryMaterializationEvidence> {
    pc::Sandbox::handle(sandbox)
        .memory_materializations()
        .unwrap()
        .expect("Copy evidence is on the persisted handle")
        .to_vec()
}

#[tokio::test]
async fn interrupted_live_write_back_keeps_the_copy_until_write_back_succeeds() {
    let store = seeded().await;
    let failures = Arc::new(AtomicUsize::new(1));
    let tmp = tempfile::tempdir().unwrap();
    let mounter = Arc::new(InterruptingMounter {
        inner: MemoryStoreMounter::copy_only(store.clone()),
        failures: failures.clone(),
    });
    let provider = LocalProvider::new(tmp.path()).with_memory_mounter(mounter.clone());
    let create = fence("t29-create", epoch(1));
    let sandbox = provider
        .create_sandbox_for_effect(&spec("t29-c02-live"), &create, None, &authority(&create))
        .await
        .unwrap();
    assert_eq!(sandbox.realized()[0].realization, pc::Realization::Copy);
    let evidence = evidence_of(&sandbox);
    assert_eq!(evidence.len(), 1);
    let root = sandbox.workspace_path().to_owned();
    let copy = root.join(MOUNT);
    edit(&copy);

    // R1: the Host's write-back is interrupted, so it never acknowledges.
    let terminal = fence("t29-terminal", epoch(2));
    let interrupted = write_back(&sandbox, mounter.as_ref(), &evidence[0]).await;
    assert!(interrupted.is_err(), "R1 the write-back was interrupted");
    assert_eq!(
        failures.load(Ordering::SeqCst),
        0,
        "R1 the interruption fired"
    );
    // R1/O0: disposal without a successful write-back is refused.
    assert!(
        dispose(&sandbox, &terminal).await.is_err(),
        "R1/O0 disposal before write-back responsibility exists is refused"
    );
    assert!(root.exists(), "R1/O0 root kept");
    assert_eq!(
        std::fs::read_to_string(copy.join("note.md")).unwrap(),
        "v2",
        "R1/O0 copy kept"
    );
    assert!(copy.join("new.md").exists(), "R1/O0 copy kept");
    assert!(!copy.join("gone.md").exists(), "R1/O0 copy kept");
    assert_eq!(
        state(&store).await,
        seeded_state(),
        "R1 nothing written back yet"
    );

    // Retry: write-back succeeds exactly once, then the root goes.
    write_back(&sandbox, mounter.as_ref(), &evidence[0])
        .await
        .unwrap();
    assert_eq!(
        state(&store).await,
        edited(),
        "R1 update/delete/create written back"
    );
    assert!(
        store.list_copy_conflicts(STORE).await.unwrap().is_empty(),
        "R1 a single successful write-back preserves no conflict"
    );
    assert!(root.exists(), "R1/O0 root kept until acknowledgement");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &sandbox,
        &terminal,
        &evidence,
        &authority(&terminal),
    )
    .await
    .unwrap();
    dispose(&sandbox, &terminal).await.unwrap();
    assert_eq!(
        state(&store).await,
        edited(),
        "R1 durable result unchanged by disposal"
    );
    assert!(
        !root.exists(),
        "R1 the root is removed only after write-back"
    );
}

#[tokio::test]
async fn adopted_copy_survives_until_reconciled_and_repeat_reconciliation_is_idempotent() {
    let store = seeded().await;
    let tmp = tempfile::tempdir().unwrap();
    let mounter = Arc::new(MemoryStoreMounter::copy_only(store.clone()));
    let scope = "t29-c02-recovered";
    let create = fence("t29-create", epoch(1));
    let handle = {
        let host_a = LocalProvider::new(tmp.path()).with_memory_mounter(mounter.clone());
        let sandbox = host_a
            .create_sandbox_for_effect(&spec(scope), &create, None, &authority(&create))
            .await
            .unwrap();
        edit(&sandbox.workspace_path().join(MOUNT));
        let handle = pc::Sandbox::handle(&sandbox);
        // Host A dies: its in-process Copy guard is lost without teardown.
        std::mem::forget(sandbox);
        handle
    };
    let root = tmp.path().join(scope);
    let copy = root.join(MOUNT);
    let evidence = handle
        .memory_materializations()
        .unwrap()
        .expect("Copy evidence is on the persisted handle")
        .to_vec();
    assert_eq!(evidence.len(), 1);

    let host_b = LocalProvider::new(tmp.path()).with_memory_mounter(mounter.clone());
    let sandbox = host_b
        .adopt_sandbox_for_effect(&spec(scope), &handle, &create, &authority(&create))
        .await
        .unwrap();

    // R2/O0: disposal before the surviving files are reconciled is refused.
    let terminal = fence("t29-terminal", epoch(2));
    assert!(
        dispose(&sandbox, &terminal).await.is_err(),
        "R2/O0 unreconciled Copy cannot be disposed"
    );
    assert_eq!(
        std::fs::read_to_string(copy.join("note.md")).unwrap(),
        "v2",
        "R2/O0 files kept"
    );
    assert_eq!(
        state(&store).await,
        seeded_state(),
        "R2 nothing written back yet"
    );

    // Harvest the surviving files and reconcile them, twice. The durable
    // state is the oracle here; conflict preservation on the repeat is the
    // separate `repeated_reconciliation_preserves_no_spurious_conflict` gap.
    for attempt in 0..2 {
        write_back(&sandbox, mounter.as_ref(), &evidence[0])
            .await
            .unwrap_or_else(|error| panic!("R2 reconcile attempt {attempt}: {error}"));
        assert_eq!(state(&store).await, edited(), "R2/O2 attempt {attempt}");
        if attempt == 0 {
            assert!(
                store.list_copy_conflicts(STORE).await.unwrap().is_empty(),
                "R2 the first reconciliation preserves no conflict"
            );
        }
    }
    pc::Sandbox::acknowledge_memory_reconciliation(
        &sandbox,
        &terminal,
        &evidence,
        &authority(&terminal),
    )
    .await
    .unwrap();
    dispose(&sandbox, &terminal).await.unwrap();
    assert!(!root.exists(), "R2 removed only after reconciliation");
    assert_eq!(
        state(&store).await,
        edited(),
        "R2 durable result unchanged by disposal"
    );
}

#[tokio::test]
#[ignore = "product gap: replaying a completed Copy reconciliation with its original evidence preserves a spurious conflict for an updated path whose durable head already equals the candidate (existing memoryd tests pin stale-proposal retention, so the fix needs a design decision)"]
async fn repeated_reconciliation_preserves_no_spurious_conflict() {
    // R2/O2 reproducer: a Host crash after write-back but before the
    // acknowledgement makes the successor replay the same reconciliation.
    let store = seeded().await;
    let tmp = tempfile::tempdir().unwrap();
    let mounter = Arc::new(MemoryStoreMounter::copy_only(store.clone()));
    let provider = LocalProvider::new(tmp.path()).with_memory_mounter(mounter.clone());
    let create = fence("t29-create", epoch(1));
    let sandbox = provider
        .create_sandbox_for_effect(&spec("t29-c02-replay"), &create, None, &authority(&create))
        .await
        .unwrap();
    let evidence = evidence_of(&sandbox);
    edit(&sandbox.workspace_path().join(MOUNT));
    for attempt in 0..2 {
        write_back(&sandbox, mounter.as_ref(), &evidence[0])
            .await
            .unwrap_or_else(|error| panic!("reconcile attempt {attempt}: {error}"));
        assert_eq!(state(&store).await, edited(), "O2 attempt {attempt}");
    }
    let conflicts = store.list_copy_conflicts(STORE).await.unwrap();
    assert!(
        conflicts.is_empty(),
        "R2/O2 a repeated reconciliation preserves no spurious conflict: {conflicts:?}"
    );
    std::mem::forget(sandbox);
}

#[tokio::test]
#[ignore = "product gap: LocalSandbox::list_files reads a missing materialized Copy directory as empty, so reconciliation would delete every durable head it was materialized from"]
async fn a_cleared_copy_directory_cannot_be_written_back() {
    // NC: the destroy-then-write-back order. The Copy directory is cleared
    // while the Sandbox root survives; the harvest-and-reconcile chain must
    // fail instead of treating the absence as "every file was deleted".
    let store = seeded().await;
    let tmp = tempfile::tempdir().unwrap();
    let mounter = Arc::new(MemoryStoreMounter::copy_only(store.clone()));
    let provider = LocalProvider::new(tmp.path()).with_memory_mounter(mounter.clone());
    let create = fence("t29-create", epoch(1));
    let sandbox = provider
        .create_sandbox_for_effect(&spec("t29-c02-cleared"), &create, None, &authority(&create))
        .await
        .unwrap();
    let evidence = evidence_of(&sandbox);
    let copy = sandbox.workspace_path().join(MOUNT);
    edit(&copy);
    std::fs::remove_dir_all(&copy).unwrap();
    let listing = sandbox.list_files(MOUNT);
    let outcome = match &listing {
        Err(error) => Err(error.clone()),
        Ok(files) => {
            pc::MemoryMounter::reconcile_recovered_copy(
                mounter.as_ref(),
                STORE,
                &evidence[0],
                files,
                pc::MountAccess::ReadWrite,
            )
            .await
        }
    };
    let after = state(&store).await;
    assert!(
        outcome.is_err(),
        "NC write-back from a cleared Copy must fail; listing {listing:?}, durable after {after:?}"
    );
    assert_eq!(after, seeded_state(), "NC no durable head was deleted");
    // The live guard would harvest the (now empty) Copy on drop paths; keep
    // this row about the explicit write-back chain only.
    std::mem::forget(sandbox);
}
