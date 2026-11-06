//! T14-C03 (替换重放与崩溃): an owned child process commits a Resource
//! replacement's desired root, then dies while the Resource projection of that
//! root is still in flight. A rebuilt service over the same durable store
//! replays the original replacement command.
//!
//! Oracles (groups-3.json order):
//! - O0 the replay restores the original version and intent (the pinned
//!   desired revision and manifest of the dead attempt, not a new revision);
//! - O1 no different Resource is prepared (every forward application targets
//!   that one pinned generation);
//! - O2 still-protected physical responsibility is not released (the installed
//!   generation is never Released before the replacement is Active, and no
//!   rollback discards the pinned attempt).
//!
//! Negative control: substituting the current head for the original pinned
//! version must fail. It is executed: the same idempotency key presented with
//! a re-resolved manifest is refused and changes nothing.
//!
//! | Row | Cause | Effect |
//! |---|---|---|
//! | C1 | child: replace (rev 1 -> 2), killed inside the projection | desired rev 2 durable; rev-1 identity held (Active or Releasing, never Released) |
//! | R1 | rebuilt service: same command, same key | rev 2 installs; no rev 3; one generation applied |
//! | R2 | the same command again | replayed; no further application |
//! | NC | same key, current-head manifest | refused; root unchanged |
//!
//! Backends: file SQLite with a reopened handle, and PostgreSQL through an
//! independent pool (required when `AWAKEN_TEST_REQUIRE_POSTGRES=1`).
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use awaken_session_contract::ActivationState;

const CHILD_STORE: &str = "AWAKEN_T14_C03_STORE";
const CHILD_SESSION: &str = "AWAKEN_T14_C03_SESSION";
const CHILD_MARKER: &str = "AWAKEN_T14_C03_MARKER";
const KEY: &str = "t14-c03-replace";
const CHILD_TEST: &str = "tests::realization::replacement_crash::t14_c03_replacement_child";

fn file_input(binding: &str, file: &str, mount: &str) -> crate::session_resource::ResolvedInput {
    crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from(binding),
        source: crate::session_resource::ResolvedInputSource::File {
            file_id: awaken_resource_contract::FileId::from(file),
        },
        mount_path: mount.into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    }
}

fn installed() -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![file_input("kept", "kept-file-v1", "/workspace/kept")],
        Vec::new(),
    )
    .unwrap()
}

/// The pinned replacement the dead attempt committed.
fn pinned() -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![
            file_input("kept", "kept-file-v1", "/workspace/kept"),
            file_input("second", "second-file-v1", "/workspace/second"),
        ],
        Vec::new(),
    )
    .unwrap()
}

/// The same bindings re-resolved against a moved current head.
fn current_head() -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![
            file_input("kept", "kept-file-v1", "/workspace/kept"),
            file_input("second", "second-file-v2", "/workspace/second"),
        ],
        Vec::new(),
    )
    .unwrap()
}

/// The pinned identity of a manifest: binding, exact source version and
/// mount. Commit timestamps stamped by the store are not part of it.
fn pins(resources: &crate::session_resource::ResolvedSessionResources) -> Vec<String> {
    resources
        .inputs()
        .iter()
        .map(|input| {
            format!(
                "{}|{:?}|{}",
                input.binding_id.as_str(),
                input.source,
                input.mount_path
            )
        })
        .collect()
}

fn replace(
    resources: crate::session_resource::ResolvedSessionResources,
) -> ReplaceSessionResourceManifest {
    ReplaceSessionResourceManifest {
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(&resources, KEY)),
        resources,
        expected_resource_revision: Some(1),
        idempotency_key: Some(KEY.into()),
    }
}

fn states(session: &PersistedSession) -> Vec<(String, ActivationState)> {
    let mut states = session
        .resources
        .activations
        .iter()
        .map(|activation| (activation.activation_id.clone(), activation.state))
        .collect::<Vec<_>>();
    states.sort_by(|left, right| left.0.cmp(&right.0));
    states
}

type Store = Arc<dyn crate::SessionStore>;

#[derive(Clone)]
enum Backend {
    Sqlite(String),
    Postgres(String),
}

impl Backend {
    fn from_env(value: &str) -> Self {
        match value.split_once(':') {
            Some(("sqlite", path)) => Self::Sqlite(path.to_owned()),
            Some(("postgres", url)) => Self::Postgres(url.to_owned()),
            _ => panic!("unknown T14-C03 store {value}"),
        }
    }

    fn to_env(&self) -> String {
        match self {
            Self::Sqlite(path) => format!("sqlite:{path}"),
            Self::Postgres(url) => format!("postgres:{url}"),
        }
    }

    /// A NEW store object (independent handle or pool) on every call.
    async fn open(&self) -> Store {
        match self {
            Self::Sqlite(path) => Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(path)
                    .await
                    .expect("T14-C03 SQLite handle"),
            ),
            Self::Postgres(url) => Arc::new(
                awaken_session_store::PostgresManagedSessionRepository::connect(url)
                    .await
                    .expect("T14-C03 PostgreSQL pool"),
            ),
        }
    }
}

fn service(store: Store, runtime: Arc<EffectCountingRuntime>) -> SessionApplication {
    application_with_runtime(
        runtime,
        store,
        Arc::new(RecordingEnvironmentSource::default()),
    )
}

/// C1: the owned child. It only acts when its parent configured it.
#[tokio::test]
async fn t14_c03_replacement_child() {
    let (Ok(store), Ok(session), Ok(marker)) = (
        std::env::var(CHILD_STORE),
        std::env::var(CHILD_SESSION),
        std::env::var(CHILD_MARKER),
    ) else {
        return;
    };
    let runtime = Arc::new(EffectCountingRuntime::default());
    *runtime.hang_apply_marker.lock().unwrap() = Some(marker.into());
    let app = service(Backend::from_env(&store).open().await, runtime);
    let _ =
        Box::pin(app.resource_manifests.replace_session_resource_manifest(
            &SessionId::from(session.as_str()),
            replace(pinned()),
        ))
        .await;
    unreachable!("the child is killed inside the Resource projection");
}

async fn crash_inside_projection(backend: &Backend, session: &str, directory: &std::path::Path) {
    let marker = directory.join(format!("{session}.marker"));
    let status = awaken_reliability_testkit::CrashProcess::new(CHILD_TEST, &marker)
        .env(CHILD_STORE, backend.to_env())
        .env(CHILD_SESSION, session)
        .env(CHILD_MARKER, &marker)
        .timeout(std::time::Duration::from_secs(60))
        .run()
        .expect("C1 the child reaches the projection and is killed");
    assert!(!status.success(), "C1 the child was killed");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "2",
        "C1 projection of rev 2"
    );
}

/// The O0-O2 checker over the recovered root and the rebuilt Runtime's
/// forward applications. It never looks at a command result.
fn recovery_violations(
    session: &str,
    root: &PersistedSession,
    applies: &[(u64, u64, Vec<String>)],
    rollbacks: &[(u64, u64)],
) -> Vec<String> {
    let mut violations = Vec::new();
    let (revision, active) = root.resources.active_generation();
    if revision != 2 || pins(active) != pins(&pinned()) || root.resources.pending.is_some() {
        violations.push(format!(
            "O0 active generation {revision} is not the pinned rev 2"
        ));
    }
    if applies.iter().any(|(_, desired, _)| *desired != 2) {
        violations.push(format!(
            "O1 a different generation was prepared: {applies:?}"
        ));
    }
    if !rollbacks.is_empty() {
        violations.push(format!("O2 the projection was rolled back: {rollbacks:?}"));
    }
    let expected = [
        (format!("{session}:1:kept"), ActivationState::Released),
        (format!("{session}:2:kept"), ActivationState::Active),
        (format!("{session}:2:second"), ActivationState::Active),
    ];
    if states(root) != expected {
        violations.push(format!("O2 activations {:?} != {expected:?}", states(root)));
    }
    violations
}

async fn replacement_replay_table(backend: Backend, session: &str, directory: &std::path::Path) {
    let id = SessionId::from(session);
    let seed = backend.open().await;
    let mut original = persisted(session, false, "idle");
    original.resources.prepare(&id, installed()).unwrap();
    original.resources.commit().unwrap();
    create(seed.as_ref(), original).await;
    drop(seed);

    crash_inside_projection(&backend, session, directory).await;

    // After the crash: the desired root is durable and the installed
    // responsibility is still protected.
    let store = backend.open().await;
    let crashed = store.get(&id).await.unwrap();
    assert_eq!(crashed.resources.revision, 2, "C1 desired rev 2 committed");
    assert_eq!(
        crashed.resources.pending.as_ref().map(pins),
        Some(pins(&pinned())),
        "C1 pinned intent"
    );
    let installed_state = states(&crashed)
        .into_iter()
        .find(|(activation, _)| activation == &format!("{session}:1:kept"))
        .map(|(_, state)| state);
    assert!(
        matches!(
            installed_state,
            Some(ActivationState::Active | ActivationState::Releasing)
        ),
        "C1/O2 the installed generation is still held (not Released): {:?}",
        states(&crashed)
    );

    // NC: the same key with the current head instead of the pinned version.
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = service(store.clone(), runtime.clone());
    let substituted = Box::pin(
        app.resource_manifests
            .replace_session_resource_manifest(&id, replace(current_head())),
    )
    .await;
    assert!(
        substituted.is_err(),
        "NC a current-head substitution is refused: {substituted:?}"
    );
    assert_eq!(store.get(&id).await.unwrap(), crashed, "NC root unchanged");
    assert_eq!(runtime.counts().resource_applies, 0, "NC nothing prepared");

    // R1: the original command on the rebuilt service.
    let replayed = Box::pin(
        app.resource_manifests
            .replace_session_resource_manifest(&id, replace(pinned())),
    )
    .await
    .expect("R1 the original replacement is recoverable");
    assert!(replayed.session.resources.pending.is_none(), "R1 settled");
    let root = store.get(&id).await.unwrap();
    let applies = runtime.resource_applies.lock().unwrap().clone();
    let rollbacks = runtime.resource_rollbacks.lock().unwrap().clone();
    let violations = recovery_violations(session, &root, &applies, &rollbacks);
    assert!(violations.is_empty(), "R1/O0-O2: {violations:?}");
    assert_eq!(applies.len(), 1, "R1/O1 one forward application");

    // R2: replaying again adds no application and keeps the root.
    Box::pin(
        app.resource_manifests
            .replace_session_resource_manifest(&id, replace(pinned())),
    )
    .await
    .expect("R2 replay");
    assert_eq!(
        runtime.counts().resource_applies,
        1,
        "R2 no further application"
    );
    assert_eq!(
        store.get(&id).await.unwrap().resources,
        root.resources,
        "R2 root"
    );

    // NC (checker): a recovery that re-prepared the current head as rev 3, or
    // released the installed generation early, is reported.
    let mut moved = root.clone();
    moved.resources.revision = 3;
    let mut early = applies.clone();
    early.push((2, 3, vec!["kept".into(), "second".into()]));
    assert!(
        !recovery_violations(session, &root, &early, &[]).is_empty(),
        "NC re-prepare"
    );
    assert!(
        !recovery_violations(session, &root, &applies, &[(2, 1)]).is_empty(),
        "NC rollback"
    );
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

#[tokio::test]
async fn t14_c03_sqlite_replacement_replay_after_projection_crash() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("t14-c03.db")
        .to_str()
        .unwrap()
        .to_owned();
    Box::pin(replacement_replay_table(
        Backend::Sqlite(path),
        "t14-c03-sqlite",
        directory.path(),
    ))
    .await;
}

#[tokio::test]
async fn t14_c03_postgres_replacement_replay_after_projection_crash() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    Box::pin(replacement_replay_table(
        Backend::Postgres(url),
        &unique("t14-c03-pg"),
        directory.path(),
    ))
    .await;
}

#[test]
fn t14_c03_postgres_replacement_replay_after_projection_crash_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "tests::realization::replacement_crash::t14_c03_postgres_replacement_replay_after_projection_crash",
    );
}
