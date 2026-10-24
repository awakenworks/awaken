//! T18-C01 (归档竞争): archive races a new Run admission, a Resource manifest
//! replacement and the activity recoverer at a barrier, through two
//! independently opened Session stores (two file SQLite handles; two
//! PostgreSQL pools).
//!
//! Oracles (groups-3.json order):
//! - O0 a terminal Session is never revived;
//! - O1 only a legal root-CAS order wins;
//! - O2 a still-protected Resource reference is not reclaimed.
//!
//! The barrier is the production root CAS itself: client A's command is held
//! at its CAS with the revision it read, client B commits the competing
//! command through its own store, then A is released and must re-decide.
//!
//! | Row | Held at CAS (A) | Committed first (B) | Legal outcome |
//! |---|---|---|---|
//! | R1 | archive | Run admission | admission keeps its epoch; archive is refused (not idle); root live; reference blocks purge |
//! | R2 | Run activity admission | archive | archive terminal; admission refused; zero activation/model; recoverer cannot admit; root stays terminal |
//! | R3 | manifest replacement | archive | archive terminal; replacement refused; active manifest unchanged |
//!
//! Negative control: calling archive and then admission sequentially must
//! not count as a race. The race witness (A's held candidate carries the
//! revision B superseded) holds in R1-R3 and is absent in the sequential NC.
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use awaken_resource_contract::ResourcePurgeGuard as _;
use awaken_session_execution_contract::{
    AdmitSessionRun, SessionAgentCoordination as _, SessionRunActivityAdmission,
    SessionRunActivityAdmissionMode,
};

type Store = Arc<dyn awaken_session_repository_contract::SessionStore>;

const MEMORY: &str = "t18-memory";

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

fn memory_resources() -> crate::session_resource::ResolvedSessionResources {
    let memory_store_id = awaken_resource_contract::MemoryStoreId::from(MEMORY);
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![crate::session_resource::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::from("t18-memory"),
            source: crate::session_resource::ResolvedInputSource::MemoryStore {
                memory_store_id: memory_store_id.clone(),
                config: awaken_resource_contract::MemoryStoreConfigVersion {
                    memory_store_id,
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    retention_policy: Default::default(),
                },
            },
            mount_path: "/memory/t18".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        Vec::new(),
    )
    .expect("valid T18 MemoryStore binding")
}

fn fresh_root(id: &str) -> PersistedSession {
    let legacy = persisted(id, false, "idle");
    let mut session = PersistedSession::frozen_with_budget(
        id,
        legacy.frozen_baseline().unwrap().clone(),
        legacy.resources,
        legacy.mcp,
        legacy.title,
        legacy.metadata,
        legacy.tools,
        legacy.budget,
    );
    session.execution = legacy.execution;
    session.resources =
        awaken_session_state_contract::SessionResourceState::from_active(memory_resources());
    session
}

fn admit_command(session: &str) -> AdmitSessionRun {
    let session_id = SessionId::from(session);
    AdmitSessionRun {
        session_id: session_id.clone(),
        agent_id: "agent".into(),
        operation_id: "t18-operation".into(),
        run_id: awaken_session_execution_contract::session_run_id(&session_id, "t18-operation"),
        messages: vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id("t18-input".into()),
            awaken_agent_contract::agent::message::Role::User,
            "work racing archive",
        )],
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
    }
}

fn replace_manifest(key: &str) -> ReplaceSessionResourceManifest {
    let resources = crate::session_resource::ResolvedSessionResources::default();
    ReplaceSessionResourceManifest {
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(&resources, key)),
        resources,
        expected_resource_revision: None,
        idempotency_key: Some(key.into()),
    }
}

struct Client {
    faulting: Arc<FaultingSessionRepository>,
    runtime: Arc<EffectCountingRuntime>,
    app: Arc<SessionApplication>,
}

fn client(store: Store) -> Client {
    let faulting = Arc::new(FaultingSessionRepository::new(store));
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = Arc::new(application_with_runtime(
        runtime.clone(),
        faulting.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    Client {
        faulting,
        runtime,
        app,
    }
}

async fn blockers(store: &Store) -> Vec<awaken_resource_contract::ResourceReference> {
    SessionResourcePurgeGuard::new(store.clone(), Arc::new(UnusedFileCatalog))
        .blockers(
            &awaken_resource_contract::ResourceTarget::new(
                "workspace",
                awaken_resource_contract::ResourceKind::MemoryStore,
                MEMORY,
            ),
            None,
            0,
        )
        .await
        .expect("O2 complete reference observation")
}

/// O2 checker: a root that still references the MemoryStore must be reported
/// as a purge blocker.
async fn assert_protected_reference(store: &Store, session: &PersistedSession, row: &str) {
    let references_memory = session
        .resources
        .resource_references()
        .inputs()
        .iter()
        .any(|input| {
            matches!(
                &input.source,
                crate::session_resource::ResolvedInputSource::MemoryStore { memory_store_id, .. }
                    if memory_store_id.as_str() == MEMORY
            )
        });
    let blocked = blockers(store)
        .await
        .iter()
        .any(|reference| reference.reference_id == session.session_id.as_str());
    if references_memory {
        assert!(blocked, "{row}/O2 a referenced MemoryStore blocks purge");
    }
}

/// Hold `operation` of client A at its CAS, run `competitor` through B,
/// release A, and return (revision after B's commit, A's held revision,
/// A's result, B's result).
async fn race<T, F, C, CF>(
    a: &Client,
    store_b: &Store,
    id: &str,
    operation: &str,
    held: F,
    competitor: C,
) -> (
    awaken_session_types::SessionRevision,
    awaken_session_types::SessionRevision,
    T,
    CF::Output,
)
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
    C: FnOnce() -> CF,
    CF: std::future::Future,
{
    let pause = a.faulting.pause_next_mutation(operation);
    let held = tokio::spawn(Box::pin(held));
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered.notified())
        .await
        .expect("A reaches its root CAS");
    let held_revision = pause
        .candidate
        .lock()
        .unwrap()
        .as_ref()
        .expect("held candidate")
        .expected_revision;
    let competitor = Box::pin(competitor()).await;
    let superseding = store_b.get(&SessionId::from(id)).await.unwrap().revision;
    pause.release.notify_one();
    let held = tokio::time::timeout(std::time::Duration::from_secs(10), held)
        .await
        .expect("A re-decides after the competing commit")
        .expect("A task");
    (superseding, held_revision, held, competitor)
}

async fn archive_race_table(prefix: &str, store_a: Store, store_b: Store) {
    assert!(
        !std::ptr::addr_eq(Arc::as_ptr(&store_a), Arc::as_ptr(&store_b)),
        "independently opened stores"
    );

    // R1: archive held at its CAS; a new Run admission commits first.
    let id = unique(&format!("{prefix}-r1"));
    create(store_a.as_ref(), fresh_root(&id)).await;
    let a = client(store_a.clone());
    let b = client(store_b.clone());
    let archive = {
        let app = a.app.clone();
        let id = id.clone();
        async move { app.termination_commands.archive_session(&id).await }
    };
    let (superseding, held, archived, admitted) =
        race(&a, &store_b, &id, "session:archive:", archive, || {
            b.app.run_admissions.admit_session_run(admit_command(&id))
        })
        .await;
    assert!(
        held < superseding,
        "R1 race witness: archive holds revision {held:?} superseded by {superseding:?}"
    );
    let admitted = admitted.expect("R1/O1 the admission committed first");
    assert!(
        archived.is_err(),
        "R1/O1 archive of a busy Session is refused"
    );
    let root = store_a.get(&SessionId::from(id.as_str())).await.unwrap();
    assert!(!root.is_terminal(), "R1/O1 root stays live");
    assert_eq!(
        root.active_activity_epochs,
        BTreeSet::from([admitted_epoch(&admitted)]),
        "R1/O1 the admission keeps its epoch"
    );
    assert_protected_reference(&store_b, &root, "R1").await;

    // R2: the Run activity admission is held at its CAS; archive commits
    // first. The terminal root must never be revived.
    let id = unique(&format!("{prefix}-r2"));
    create(store_a.as_ref(), fresh_root(&id)).await;
    let a = client(store_a.clone());
    let b = client(store_b.clone());
    let command = admit_command(&id);
    let run_id = command.run_id.clone();
    let admission = {
        let app = a.app.clone();
        async move { app.run_admissions.admit_session_run(command).await }
    };
    let (superseding, held, admission, archived) = race(
        &a,
        &store_b,
        &id,
        "managed:session-activity:",
        admission,
        || b.app.termination_commands.archive_session(&id),
    )
    .await;
    assert!(
        held < superseding,
        "R2 race witness: admission holds revision {held:?} superseded by {superseding:?}"
    );
    assert!(
        archived.is_ok(),
        "R2/O1 archive committed first: {archived:?}"
    );
    assert!(admission.is_err(), "R2/O0 the held admission is refused");
    let terminal = store_a.get(&SessionId::from(id.as_str())).await.unwrap();
    assert!(terminal.is_terminal(), "R2/O0 root is terminal");
    assert!(
        terminal.active_activity_epochs.is_empty(),
        "R2/O0 no epoch opened after archive"
    );
    assert_eq!(a.runtime.counts().activations, 0, "R2/O0 no activation");
    assert_eq!(a.runtime.counts().model_calls, 0, "R2/O0 no model call");
    // The recoverer finds the reservation of the refused Run and must not
    // admit it into the terminal root.
    let recovered = Box::pin(b.app.coordination.admit_session_run_activity(
        &SessionId::from(id.as_str()),
        "agent",
        &run_id,
        SessionRunActivityAdmissionMode::RecoverOrAdmit,
    ))
    .await;
    assert!(
        !matches!(recovered, Ok(SessionRunActivityAdmission::Admitted { .. })),
        "R2/O0 the recoverer cannot revive the terminal root: {recovered:?}"
    );
    let after_recovery = store_b.get(&SessionId::from(id.as_str())).await.unwrap();
    assert!(after_recovery.is_terminal(), "R2/O0 still terminal");
    assert!(after_recovery.active_activity_epochs.is_empty(), "R2/O0");
    assert_protected_reference(&store_b, &after_recovery, "R2").await;

    // R3: a manifest replacement is held at its CAS; archive commits first.
    let id = unique(&format!("{prefix}-r3"));
    create(store_a.as_ref(), fresh_root(&id)).await;
    let a = client(store_a.clone());
    let b = client(store_b.clone());
    let before = store_a.get(&SessionId::from(id.as_str())).await.unwrap();
    let replacement = {
        let app = a.app.clone();
        let id = SessionId::from(id.as_str());
        async move {
            app.resource_manifests
                .replace_session_resource_manifest(&id, replace_manifest("t18-replace"))
                .await
        }
    };
    let (superseding, held, replaced, archived) = race(
        &a,
        &store_b,
        &id,
        "managed:resource-manifest-command:",
        replacement,
        || b.app.termination_commands.archive_session(&id),
    )
    .await;
    assert!(
        held < superseding,
        "R3 race witness: replacement holds revision {held:?} superseded by {superseding:?}"
    );
    assert!(
        archived.is_ok(),
        "R3/O1 archive committed first: {archived:?}"
    );
    assert!(
        replaced.is_err(),
        "R3/O0 replacement of a terminal root is refused"
    );
    let terminal = store_a.get(&SessionId::from(id.as_str())).await.unwrap();
    assert!(terminal.is_terminal(), "R3/O0 root is terminal");
    assert_eq!(
        terminal.resources.active_generation(),
        before.resources.active_generation(),
        "R3/O0 active manifest unchanged"
    );
    assert!(
        terminal.resources.pending.is_none(),
        "R3/O0 no pending manifest"
    );
    assert_protected_reference(&store_b, &terminal, "R3").await;
}

fn admitted_epoch(admitted: &awaken_session_execution_contract::AdmittedSessionRun) -> u64 {
    match admitted {
        awaken_session_execution_contract::AdmittedSessionRun::Reserved(delivery)
        | awaken_session_execution_contract::AdmittedSessionRun::AlreadyReserved(delivery) => {
            delivery.session_activity_epoch
        }
        other => panic!("admission without a delivery: {other:?}"),
    }
}

/// The race composes three full application commands per row; run it on
/// threads whose stacks fit those futures (the default 1-2 MiB overflowed).
fn on_large_stacks<F: std::future::Future<Output = ()>>(test: impl FnOnce() -> F + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("T18 runtime")
                .block_on(test());
        })
        .expect("T18 thread")
        .join()
        .expect("T18 race table");
}

#[test]
fn sqlite_archive_races_admission_replacement_and_recovery_through_root_cas() {
    on_large_stacks(sqlite_archive_race);
}

async fn sqlite_archive_race() {
    let directory = tempfile::tempdir().expect("T18 SQLite directory");
    let path = directory.path().join("t18-archive.db");
    let path = path.to_str().unwrap();
    let store_a: Store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(path)
            .await
            .expect("T18 left SQLite client"),
    );
    let store_b: Store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(path)
            .await
            .expect("T18 right SQLite client"),
    );
    Box::pin(archive_race_table("t18-sqlite", store_a, store_b)).await;
}

#[test]
fn postgres_archive_races_admission_replacement_and_recovery_through_root_cas() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    on_large_stacks(move || postgres_archive_race(url));
}

async fn postgres_archive_race(url: String) {
    let store_a: Store = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("T18 left PostgreSQL pool"),
    );
    let store_b: Store = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("T18 right PostgreSQL pool"),
    );
    Box::pin(archive_race_table("t18-postgres", store_a, store_b)).await;
}

#[test]
fn postgres_archive_races_admission_replacement_and_recovery_through_root_cas_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::authority::archive_race::postgres_archive_races_admission_replacement_and_recovery_through_root_cas",
    );
}

#[tokio::test]
async fn sequential_archive_then_admission_has_no_race_witness() {
    // NC: archive completes first, then admission starts. Admission is held
    // at its CAS but read the post-archive revision, so the witness used by
    // R1-R3 is absent: a sequential test never exercises the competing CAS.
    let directory = tempfile::tempdir().expect("NC SQLite directory");
    let path = directory.path().join("t18-sequential.db");
    let store: Store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(path.to_str().unwrap())
            .await
            .unwrap(),
    );
    let id = "t18-sequential";
    create(store.as_ref(), fresh_root(id)).await;
    let a = client(store.clone());
    a.app
        .termination_commands
        .archive_session(id)
        .await
        .expect("NC archive first");
    let archived = store.get(&SessionId::from(id)).await.unwrap().revision;
    let pause = a.faulting.pause_next_mutation("managed:session-activity:");
    let admission = {
        let app = a.app.clone();
        tokio::spawn(async move {
            app.run_admissions
                .admit_session_run(admit_command(id))
                .await
        })
    };
    let held = tokio::time::timeout(std::time::Duration::from_secs(2), pause.entered.notified())
        .await
        .ok()
        .and_then(|()| {
            pause
                .candidate
                .lock()
                .unwrap()
                .as_ref()
                .map(|candidate| candidate.expected_revision)
        });
    pause.release.notify_one();
    assert!(
        admission.await.unwrap().is_err(),
        "NC the sequential admission is refused"
    );
    assert!(
        held.is_none_or(|held| held == archived),
        "NC no stale-revision candidate: the sequential order is not a race"
    );
}
