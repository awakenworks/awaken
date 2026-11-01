//! T04-C03 (准备后写入失败) and T17-C01/C02/C03 (准备前后崩溃 / 补偿失败 /
//! 并发重试): a Session-owned Repository participant is prepared in the
//! Resource Registry (the external prepare), then the Session root insert
//! fails, loses its response, or the process dies, and the service is
//! rebuilt over the same durable Session store and the same embedded
//! (file-backed) Resource Registry.
//!
//! Production path: `repository_participants.configure` (prepare) ->
//! `creation_commands.create_session` (root insert, in-stack compensation via
//! `abort_unadopted`).
//!
//! T04-C03 oracles (groups-1.json): O0 the prepared responsibility is found by
//! its original intent and compensated/observed; O1 an exact retry keeps the
//! original identity.
//!
//! | Row | Root insert | Replay query | Same-call effect | After rebuild + exact retry |
//! |---|---|---|---|---|
//! | W1 | fails before insert | absent | participant compensated (Deleted) | - |
//! | W2 | committed, response lost | finds root | create returns the root | Replayed root; participant Active, referenced |
//! | W3 | committed, response lost | outage | participant preserved (unknown) | Replayed root adopts the original participant |
//! | W4 | fails before insert | outage | participant preserved (unknown) | recovery finds it by its durable intent and compensates it |
//!
//! T17-C01 (CrashProcess, file SQLite): child killed before / after the
//! prepare. Before: no Registry effect, no root. After: the definition exists,
//! no root; the original caller's exact retry adopts the SAME definition.
//! Without that caller, recovery finds it by its durable creation intent once
//! the intent lease (`CREATION_INTENT_LEASE_MS`) has expired.
//!
//! T17-C02: the root insert fails and compensation's purge scheduling fails,
//! hangs, or loses its response. The command never reports completion, the
//! participant stays Active (pending, not falsely retired), and retrying the
//! same compensation with the original receipt completes idempotently. The
//! pending compensation is durable: a rebuilt service's recovery completes it.
//!
//! T17-C03: two independent services continue the same prepare identity
//! concurrently: one Registry object, one Applied + one Replayed receipt; only
//! the Applied (exact) owner may clean up; a conflicting payload is rejected.
//! Compensation claims the creation intent right before retiring; a create
//! that adopted the participant first wins, and the participant stays Active.
use super::*;
use crate::tests::run_admission::response_loss_store::ResponseLossStore;

type Store = Arc<dyn crate::SessionStore>;

const WORKSPACE: &str = "workspace";
const REMOTE: &str = "https://example.test/source.git";

fn repository_id(session_id: &str) -> String {
    format!("managed:{session_id}:repository:source")
}

fn repository_input(session_id: &str, remote_url: &str) -> SessionRepositoryResourceInput {
    SessionRepositoryResourceInput {
        id: repository_id(session_id),
        workspace_id: WORKSPACE.into(),
        name: "Source".into(),
        description: "Session source".into(),
        remote_url: remote_url.into(),
        credential_material: None,
        credential: None,
        mount_path: "/workspace/source".into(),
        initial_branch: Some("main".into()),
        initial_commit: None,
    }
}

fn repository_resources(session_id: &str) -> crate::session_resource::ResolvedSessionResources {
    let id = repository_id(session_id);
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![crate::session_resource::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::from("source"),
            source: crate::session_resource::ResolvedInputSource::Repository {
                repository_id: id.clone().into(),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: id.into(),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: REMOTE.into(),
                    credential_binding: None,
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential: None,
            },
            mount_path: "/workspace/source".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        Vec::new(),
    )
    .expect("valid Repository input")
}

fn command(session_id: &str, configured: ConfiguredSessionRepository) -> CreateSessionCommand {
    let mut command = creation_command(session_id);
    command.intent.control.resources = repository_resources(session_id);
    command.repository_configurations = vec![configured];
    command
}

/// Test-only purge scheduler: the Registry participant's retirement first
/// schedules a purge, so its faults are compensation faults.
#[derive(Default)]
struct PurgeScheduler {
    mode: Mutex<PurgeMode>,
    calls: Mutex<Vec<awaken_resource_contract::ResourceTarget>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PurgeMode {
    #[default]
    Healthy,
    Fail,
    Hang,
    LoseResponse,
    Gate,
}

impl PurgeScheduler {
    fn set(&self, mode: PurgeMode) {
        *self.mode.lock().unwrap() = mode;
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourcePurgeScheduler for PurgeScheduler {
    async fn schedule_purge(
        &self,
        target: awaken_resource_contract::ResourceTarget,
        _config_version: Option<u64>,
        _requested_at_unix_ms: u64,
        _not_before_unix_ms: u64,
    ) -> Result<
        awaken_resource_contract::PutResourcePurgeOutcome,
        awaken_resource_contract::ResourcePurgeError,
    > {
        let mode = *self.mode.lock().unwrap();
        let injected = || awaken_resource_contract::ResourcePurgeError::Indeterminate {
            detail: "injected compensation fault".into(),
        };
        match mode {
            PurgeMode::Fail => return Err(injected()),
            PurgeMode::Hang => std::future::pending::<()>().await,
            PurgeMode::Gate => {
                self.entered.notify_one();
                self.release.notified().await;
            }
            PurgeMode::Healthy | PurgeMode::LoseResponse => {}
        }
        let first = {
            let mut calls = self.calls.lock().unwrap();
            let first = !calls.contains(&target);
            calls.push(target);
            first
        };
        if mode == PurgeMode::LoseResponse {
            return Err(injected());
        }
        Ok(if first {
            awaken_resource_contract::PutResourcePurgeOutcome::Inserted
        } else {
            awaken_resource_contract::PutResourcePurgeOutcome::Existing
        })
    }
}

/// One rebuildable service: durable Session store handle + Registry handle.
struct Service {
    loss: Arc<ResponseLossStore>,
    registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    purge: Arc<PurgeScheduler>,
    app: SessionApplication,
}

async fn registry(root: &std::path::Path) -> Arc<dyn awaken_resource_contract::ResourceRegistry> {
    awaken_resource_persistence::open_embedded(
        root,
        awaken_time::system_clock(),
        Arc::new(RecordingMemoryInvalidator::default()),
    )
    .await
    .expect("embedded Resource authorities")
    .authorities()
    .resource_registry()
}

/// A wall clock past every creation intent lease written by a service built
/// on the system clock: the original caller's ownership has expired.
struct AfterIntentExpiry;

impl awaken_time::WallClock for AfterIntentExpiry {
    fn now(&self) -> awaken_time::UtcTime {
        use awaken_time::WallClockExt as _;
        awaken_time::UtcTime::from_epoch_ms(
            awaken_time::system_clock().now_ms()
                + crate::repository_participants::CREATION_INTENT_LEASE_MS
                + 1,
        )
    }
}

async fn service(store: Store, registry_root: &std::path::Path) -> Service {
    service_on(store, registry_root, awaken_time::system_clock()).await
}

/// The service a later process builds after the original caller is gone.
async fn rebuilt_after_lease(store: Store, registry_root: &std::path::Path) -> Service {
    service_on(store, registry_root, Arc::new(AfterIntentExpiry)).await
}

async fn service_on(
    store: Store,
    registry_root: &std::path::Path,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Service {
    let loss = Arc::new(ResponseLossStore::new(store));
    let registry = registry(registry_root).await;
    let purge = Arc::new(PurgeScheduler::default());
    let mut app = configured_application(
        Arc::new(RecordingCreateRuntime::default()),
        SessionPersistence::new(loss.clone(), clock),
        Arc::new(RecordingEnvironmentSource::default()),
        Default::default(),
        None,
        {
            let probe = Arc::new(crate::ExecutionObservationProbe::default());
            crate::SessionExecutionAdmission::new(probe.clone(), probe.clone(), probe)
        },
    );
    app.set_resource_registry(registry.clone());
    app.set_resource_purge_scheduler(purge.clone());
    Service {
        loss,
        registry,
        purge,
        app,
    }
}

impl Service {
    async fn prepare(&self, session_id: &str) -> ConfiguredSessionRepository {
        self.app
            .repository_participants
            .configure(repository_input(session_id, REMOTE))
            .await
            .expect("prepare the Session-owned Repository")
    }

    async fn state(&self, session_id: &str) -> Option<awaken_resource_contract::ResourceState> {
        self.registry
            .find_repository(WORKSPACE, &repository_id(session_id))
            .await
            .expect("read Registry participant")
            .map(|definition| definition.state)
    }

    async fn root(&self, session_id: &str) -> Option<PersistedSession> {
        match self
            .loss
            .get(&awaken_session_types::SessionId::from(session_id))
            .await
        {
            Ok(root) => Some(root),
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => None,
            Err(error) => panic!("root read: {error}"),
        }
    }
}

fn references_participant(root: &PersistedSession, session_id: &str) -> bool {
    root.resources
        .resource_references()
        .inputs()
        .iter()
        .any(|input| {
            matches!(
                &input.source,
                crate::session_resource::ResolvedInputSource::Repository { repository_id: id, .. }
                    if id.as_str() == repository_id(session_id)
            )
        })
}

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

/// T04-C03 rows W1-W3 over one durable Session store. `reopen` yields an
/// independent handle on the same database for the rebuilt service.
async fn prepared_participant_table<F, Fut>(prefix: &str, reopen: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Store>,
{
    let registry_dir = tempfile::tempdir().expect("T04 Registry directory");

    // W1: deterministic insert failure, absent replay -> in-stack
    // compensation retires the prepared participant.
    let id = unique(&format!("{prefix}-w1"));
    let original = service(reopen().await, registry_dir.path()).await;
    let configured = original.prepare(&id).await;
    assert_eq!(
        configured.registry_provenance,
        SessionParticipantProvenance::Applied,
        "W1 the prepare is this command's"
    );
    original.loss.create_unavailable_once();
    assert!(
        original
            .app
            .creation_commands
            .create_session(command(&id, configured))
            .await
            .is_err(),
        "W1 the root insert failure is reported"
    );
    let rebuilt = service(reopen().await, registry_dir.path()).await;
    assert_eq!(rebuilt.root(&id).await, None, "W1 no root");
    assert_eq!(
        rebuilt.state(&id).await,
        Some(awaken_resource_contract::ResourceState::Deleted),
        "W1/O0 the prepared participant was compensated"
    );

    // W2: committed, response lost, replay query finds the root.
    let id = unique(&format!("{prefix}-w2"));
    let original = service(reopen().await, registry_dir.path()).await;
    let configured = original.prepare(&id).await;
    original.loss.create_then_unavailable_once();
    let created = original
        .app
        .creation_commands
        .create_session(command(&id, configured))
        .await
        .expect("W2 the exact replay query recovers the committed root");
    drop(original);
    let rebuilt = service(reopen().await, registry_dir.path()).await;
    let replayed_prepare = rebuilt.prepare(&id).await;
    assert_eq!(
        replayed_prepare.registry_provenance,
        SessionParticipantProvenance::Replayed,
        "W2/O1 the retry finds the original Registry object"
    );
    assert_eq!(
        replayed_prepare.repository_id.as_str(),
        repository_id(&id),
        "W2/O1 same identity"
    );
    let replayed = rebuilt
        .app
        .creation_commands
        .create_session(command(&id, replayed_prepare))
        .await
        .expect("W2/O1 exact retry");
    assert_eq!(replayed.session_id, created.session_id, "W2/O1 same root");
    assert_eq!(
        replayed.revision, created.revision,
        "W2/O1 no second insert"
    );
    assert!(references_participant(&replayed, &id), "W2/O1 adopted");
    assert_eq!(
        rebuilt.state(&id).await,
        Some(awaken_resource_contract::ResourceState::Active),
        "W2 the adopted participant is kept"
    );

    // W3: committed, response lost, replay query unavailable -> the outcome
    // is unknown and the participant is preserved; the rebuilt service's
    // exact retry observes the committed root and adopts the SAME object.
    let id = unique(&format!("{prefix}-w3"));
    let original = service(reopen().await, registry_dir.path()).await;
    let configured = original.prepare(&id).await;
    original.loss.create_then_unavailable_once();
    original.loss.replay_create_unavailable_once();
    assert!(
        original
            .app
            .creation_commands
            .create_session(command(&id, configured))
            .await
            .is_err(),
        "W3 unknown outcome is not reported as success"
    );
    assert_eq!(
        original.state(&id).await,
        Some(awaken_resource_contract::ResourceState::Active),
        "W3/O0 an unknown outcome preserves (observes) the participant"
    );
    drop(original);
    let rebuilt = service(reopen().await, registry_dir.path()).await;
    let committed = rebuilt.root(&id).await.expect("W3 the root committed");
    let replayed_prepare = rebuilt.prepare(&id).await;
    let replayed = rebuilt
        .app
        .creation_commands
        .create_session(command(&id, replayed_prepare))
        .await
        .expect("W3/O1 exact retry");
    assert_eq!(replayed.revision, committed.revision, "W3/O1 original root");
    assert!(
        references_participant(&replayed, &id),
        "W3/O1 original participant"
    );
    assert_eq!(
        rebuilt.state(&id).await,
        Some(awaken_resource_contract::ResourceState::Active),
        "W3 kept"
    );
}

fn sqlite_reopen(
    path: String,
) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Store>>> {
    move || {
        let path = path.clone();
        Box::pin(async move {
            Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path)
                    .await
                    .expect("T04 SQLite handle"),
            ) as Store
        })
    }
}

#[tokio::test]
async fn sqlite_prepared_participant_survives_root_failure_and_rebuild() {
    let directory = tempfile::tempdir().expect("T04 SQLite directory");
    let path = directory
        .path()
        .join("t04-sessions.db")
        .to_str()
        .unwrap()
        .to_owned();
    prepared_participant_table("t04-sqlite", sqlite_reopen(path)).await;
}

#[tokio::test]
async fn postgres_prepared_participant_survives_root_failure_and_rebuild() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    prepared_participant_table("t04-postgres", || {
        let url = url.clone();
        async move {
            Arc::new(
                awaken_session_store::PostgresManagedSessionRepository::connect(&url)
                    .await
                    .expect("T04 PostgreSQL pool"),
            ) as Store
        }
    })
    .await;
}

#[test]
fn postgres_prepared_participant_survives_root_failure_and_rebuild_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::creation::prepared_participants::postgres_prepared_participant_survives_root_failure_and_rebuild",
    );
}

#[tokio::test]
async fn orphaned_prepare_is_found_by_recovery_after_rebuild() {
    // W4: insert never happened, replay query outage -> participant
    // preserved; the original caller then disappears.
    let directory = tempfile::tempdir().expect("W4 directory");
    let path = directory.path().join("w4.db").to_str().unwrap().to_owned();
    let reopen = sqlite_reopen(path);
    let registry_dir = tempfile::tempdir().expect("W4 Registry directory");
    let id = "t04-w4-orphan";
    let original = service(reopen().await, registry_dir.path()).await;
    let configured = original.prepare(id).await;
    original.loss.create_unavailable_once();
    original.loss.replay_create_unavailable_once();
    let _ = original
        .app
        .creation_commands
        .create_session(command(id, configured))
        .await;
    drop(original);
    let rebuilt = rebuilt_after_lease(reopen().await, registry_dir.path()).await;
    let app = Arc::new(rebuilt.app);
    let _ = app.recovery().reconcile_pending_session_state().await;
    assert_eq!(
        rebuilt
            .registry
            .find_repository(WORKSPACE, &repository_id(id))
            .await
            .unwrap()
            .map(|definition| definition.state),
        Some(awaken_resource_contract::ResourceState::Deleted),
        "O0 the prepared, never-adopted participant is found by its intent and compensated"
    );
}

const CRASH_MODE: &str = "AWAKEN_T17_CRASH_MODE";
const CRASH_DB: &str = "AWAKEN_T17_CRASH_DB";
const CRASH_REGISTRY: &str = "AWAKEN_T17_CRASH_REGISTRY";
const CRASH_MARKER: &str = "AWAKEN_T17_CRASH_MARKER";
const CRASH_SESSION: &str = "t17-crash";

/// Child body shared by the CrashProcess tests: prepare (or not), then stop
/// at the marker and wait to be killed before the root insert.
async fn crash_child() -> bool {
    let Ok(mode) = std::env::var(CRASH_MODE) else {
        return false;
    };
    let store: Store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(
            &std::env::var(CRASH_DB).unwrap(),
        )
        .await
        .unwrap(),
    );
    let child = service(
        store,
        std::path::Path::new(&std::env::var(CRASH_REGISTRY).unwrap()),
    )
    .await;
    if mode == "after" {
        child.prepare(CRASH_SESSION).await;
    }
    std::fs::write(std::env::var(CRASH_MARKER).unwrap(), mode.as_bytes()).unwrap();
    std::future::pending::<()>().await;
    true
}

async fn crash_at(test: &str, mode: &str, directory: &std::path::Path) {
    let status = awaken_reliability_testkit::CrashProcess::new(test, directory.join("marker"))
        .env(CRASH_MODE, mode)
        .env(CRASH_DB, directory.join("sessions.db"))
        .env(CRASH_REGISTRY, directory.join("registry"))
        .env(CRASH_MARKER, directory.join("marker"))
        .run()
        .expect("child reaches the crash boundary and is killed");
    assert!(!status.success(), "child was killed");
}

#[tokio::test]
async fn crash_before_or_after_prepare_keeps_identity_for_the_original_intent() {
    if crash_child().await {
        return;
    }
    const TEST: &str = "tests::creation::prepared_participants::crash_before_or_after_prepare_keeps_identity_for_the_original_intent";
    for mode in ["before", "after"] {
        let directory = tempfile::tempdir().expect("T17 crash directory");
        crash_at(TEST, mode, directory.path()).await;
        let reopen = sqlite_reopen(
            directory
                .path()
                .join("sessions.db")
                .to_str()
                .unwrap()
                .into(),
        );
        let rebuilt = service(reopen().await, &directory.path().join("registry")).await;
        assert_eq!(rebuilt.root(CRASH_SESSION).await, None, "{mode}: no root");
        if mode == "before" {
            assert_eq!(
                rebuilt.state(CRASH_SESSION).await,
                None,
                "before/O0 no effect before the prepare"
            );
            continue;
        }
        assert_eq!(
            rebuilt.state(CRASH_SESSION).await,
            Some(awaken_resource_contract::ResourceState::Active),
            "after: the prepare is durable without any Session root"
        );
        // The original caller retries its exact intent: the SAME object is
        // found and adopted; no second external object exists.
        let replayed = rebuilt.prepare(CRASH_SESSION).await;
        assert_eq!(
            replayed.registry_provenance,
            SessionParticipantProvenance::Replayed,
            "after/O1 the retry continues the original prepare"
        );
        let created = rebuilt
            .app
            .creation_commands
            .create_session(command(CRASH_SESSION, replayed))
            .await
            .expect("after: the retry creates the root");
        assert!(
            references_participant(&created, CRASH_SESSION),
            "after: adopted"
        );
        assert_eq!(
            rebuilt
                .registry
                .list_repositories(WORKSPACE)
                .await
                .unwrap()
                .iter()
                .filter(|definition| definition.id.as_str() == repository_id(CRASH_SESSION))
                .count(),
            1,
            "after: exactly one external object"
        );
    }
}

#[tokio::test]
async fn crash_after_prepare_is_recoverable_without_the_original_caller() {
    if crash_child().await {
        return;
    }
    // The child runs the shared crash test, which stops at the prepare
    // boundary; this test's own body only recovers.
    const TEST: &str = "tests::creation::prepared_participants::crash_before_or_after_prepare_keeps_identity_for_the_original_intent";
    let directory = tempfile::tempdir().expect("T17 gap directory");
    crash_at(TEST, "after", directory.path()).await;
    let reopen = sqlite_reopen(
        directory
            .path()
            .join("sessions.db")
            .to_str()
            .unwrap()
            .into(),
    );
    let rebuilt = rebuilt_after_lease(reopen().await, &directory.path().join("registry")).await;
    let registry = rebuilt.registry.clone();
    let app = Arc::new(rebuilt.app);
    let _ = app.recovery().reconcile_pending_session_state().await;
    assert_eq!(
        registry
            .find_repository(WORKSPACE, &repository_id(CRASH_SESSION))
            .await
            .unwrap()
            .map(|definition| definition.state),
        Some(awaken_resource_contract::ResourceState::Deleted),
        "O1/O2 the started prepare is found by its stable intent without a Session root"
    );
}

#[tokio::test]
async fn compensation_failure_hang_or_lost_response_never_reports_completion() {
    for mode in [PurgeMode::Fail, PurgeMode::Hang, PurgeMode::LoseResponse] {
        let directory = tempfile::tempdir().expect("T17-C02 directory");
        let reopen = sqlite_reopen(directory.path().join("c02.db").to_str().unwrap().into());
        let id = format!("t17-c02-{mode:?}").to_lowercase();
        let service = service(reopen().await, &directory.path().join("registry")).await;
        let configured = service.prepare(&id).await;
        service.purge.set(mode);
        service.loss.create_unavailable_once();
        let created = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            service
                .app
                .creation_commands
                .create_session(command(&id, configured.clone())),
        )
        .await;
        match (mode, created) {
            (PurgeMode::Hang, Err(_elapsed)) => {}
            (_, Ok(Err(error))) => {
                let rendered = format!("{error:?}");
                assert!(
                    !rendered.contains("x-access-token"),
                    "{mode:?}/O2 no material in the error"
                );
            }
            (_, other) => panic!("{mode:?}/O2 the command must not report completion: {other:?}"),
        }
        assert_eq!(service.root(&id).await, None, "{mode:?} no root");
        assert_eq!(
            service.state(&id).await,
            Some(awaken_resource_contract::ResourceState::Active),
            "{mode:?}/O0 the participant stays pending, never falsely retired"
        );
        // O1: the same compensation, retried with the original receipt
        // after the fault clears, completes idempotently.
        service.purge.set(PurgeMode::Healthy);
        assert!(
            service
                .app
                .repository_participants
                .abort_unadopted(std::slice::from_ref(&configured))
                .await,
            "{mode:?}/O1 the original compensation completes"
        );
        assert!(
            service
                .app
                .repository_participants
                .abort_unadopted(std::slice::from_ref(&configured))
                .await,
            "{mode:?}/O1 repeating it is idempotent"
        );
        assert_eq!(
            service.state(&id).await,
            Some(awaken_resource_contract::ResourceState::Deleted),
            "{mode:?}/O1 retired once"
        );
    }
}

#[tokio::test]
async fn failing_compensation_reports_incompleteness_to_its_caller() {
    // NC: best-effort cleanup that discards its error cannot sign off. The
    // compensation port itself reports `false` under the injected fault; a
    // caller that ignored it would wrongly treat the participant as retired.
    let directory = tempfile::tempdir().expect("T17-C02 NC directory");
    let reopen = sqlite_reopen(directory.path().join("nc.db").to_str().unwrap().into());
    let service = service(reopen().await, &directory.path().join("registry")).await;
    let configured = service.prepare("t17-c02-nc").await;
    service.purge.set(PurgeMode::Fail);
    let completed = service
        .app
        .repository_participants
        .abort_unadopted(std::slice::from_ref(&configured))
        .await;
    assert!(
        !completed,
        "NC the failed compensation is not reported complete"
    );
    assert_eq!(
        service.state("t17-c02-nc").await,
        Some(awaken_resource_contract::ResourceState::Active),
        "NC still pending"
    );
}

#[tokio::test]
async fn failed_compensation_is_retried_by_a_rebuilt_service() {
    let directory = tempfile::tempdir().expect("T17-C02 gap directory");
    let reopen = sqlite_reopen(directory.path().join("gap.db").to_str().unwrap().into());
    let id = "t17-c02-gap";
    let original = service(reopen().await, &directory.path().join("registry")).await;
    let configured = original.prepare(id).await;
    original.purge.set(PurgeMode::Fail);
    original.loss.create_unavailable_once();
    let _ = original
        .app
        .creation_commands
        .create_session(command(id, configured))
        .await;
    drop(original);
    let rebuilt = rebuilt_after_lease(reopen().await, &directory.path().join("registry")).await;
    let registry = rebuilt.registry.clone();
    let app = Arc::new(rebuilt.app);
    let _ = app.recovery().reconcile_pending_session_state().await;
    assert_eq!(
        registry
            .find_repository(WORKSPACE, &repository_id(id))
            .await
            .unwrap()
            .map(|definition| definition.state),
        Some(awaken_resource_contract::ResourceState::Deleted),
        "O0/O1 the pending compensation is durable and retried"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_continuations_share_one_external_object() {
    // T17-C03: two independently built services (separate Session store and
    // Registry handles over the same files) continue one prepare identity.
    let directory = tempfile::tempdir().expect("T17-C03 directory");
    let reopen = sqlite_reopen(directory.path().join("c03.db").to_str().unwrap().into());
    let registry_root = directory.path().join("registry");
    let original = service(reopen().await, &registry_root).await;
    let recoverer = service(reopen().await, &registry_root).await;
    let id = "t17-c03";
    let (left, right) = tokio::join!(original.prepare(id), recoverer.prepare(id));
    let mut provenances = [left.registry_provenance, right.registry_provenance];
    provenances.sort_by_key(|provenance| format!("{provenance:?}"));
    assert_eq!(
        provenances,
        [
            SessionParticipantProvenance::Applied,
            SessionParticipantProvenance::Replayed
        ],
        "O0 one call created the object, the other continued it"
    );
    assert_eq!(left.repository_id, right.repository_id, "O0 one identity");
    let definitions = original
        .registry
        .list_repositories(WORKSPACE)
        .await
        .unwrap()
        .into_iter()
        .filter(|definition| definition.id.as_str() == repository_id(id))
        .count();
    assert_eq!(definitions, 1, "O0 no second external object");

    // O2: a conflicting continuation is rejected and changes nothing.
    let before = original
        .registry
        .find_repository_config(
            WORKSPACE,
            &repository_id(id),
            awaken_resource_contract::ConfigVersion::INITIAL,
        )
        .await
        .unwrap();
    let conflict = recoverer
        .app
        .repository_participants
        .configure(repository_input(id, "https://example.test/other.git"))
        .await
        .expect_err("O2 conflicting payload");
    assert_eq!(
        conflict.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "O2"
    );
    assert_eq!(
        original
            .registry
            .find_repository_config(
                WORKSPACE,
                &repository_id(id),
                awaken_resource_contract::ConfigVersion::INITIAL,
            )
            .await
            .unwrap(),
        before,
        "O2 original config unchanged"
    );

    // O1: only the exact (Applied) owner may clean up; the Replayed holder's
    // compensation is a no-op.
    let (applied, replayed) = if left.registry_provenance == SessionParticipantProvenance::Applied {
        (left, right)
    } else {
        (right, left)
    };
    assert!(
        recoverer
            .app
            .repository_participants
            .abort_unadopted(std::slice::from_ref(&replayed))
            .await
    );
    assert_eq!(
        original.state(id).await,
        Some(awaken_resource_contract::ResourceState::Active),
        "O1 a continuation that did not create the object cannot retire it"
    );
    assert!(
        original
            .app
            .repository_participants
            .abort_unadopted(std::slice::from_ref(&applied))
            .await
    );
    assert_eq!(
        original.state(id).await,
        Some(awaken_resource_contract::ResourceState::Deleted),
        "O1 the exact owner retires it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compensation_cannot_retire_a_participant_adopted_concurrently() {
    let directory = tempfile::tempdir().expect("T17-C03 gap directory");
    let reopen = sqlite_reopen(directory.path().join("c03-gap.db").to_str().unwrap().into());
    let registry_root = directory.path().join("registry");
    let original = Arc::new(service(reopen().await, &registry_root).await);
    let recoverer = service(reopen().await, &registry_root).await;
    let id = "t17-c03-gap";
    let applied = original.prepare(id).await;
    // The original caller's compensation passes its "not adopted" read and
    // is held at the purge scheduling step.
    original.purge.set(PurgeMode::Gate);
    let compensation = {
        let original = original.clone();
        let applied = applied.clone();
        tokio::spawn(async move {
            original
                .app
                .repository_participants
                .abort_unadopted(std::slice::from_ref(&applied))
                .await
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        original.purge.entered.notified(),
    )
    .await
    .expect("compensation passed its adoption check");
    // Meanwhile the recoverer continues the same identity and commits a root
    // that adopts it.
    let replayed = recoverer.prepare(id).await;
    let adopted = recoverer
        .app
        .creation_commands
        .create_session(command(id, replayed))
        .await
        .expect("concurrent adopting create");
    assert!(references_participant(&adopted, id));
    original.purge.release.notify_one();
    let _ = compensation.await;
    assert_eq!(
        recoverer.state(id).await,
        Some(awaken_resource_contract::ResourceState::Active),
        "O1 only an unadopted participant may be retired"
    );
}
