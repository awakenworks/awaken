//! T20-C03: concurrent update/archive by independent clients, a failure at each
//! production write step, and restart.
//!
//! Causes: C1 two independent clients (two file-SQLite handles / two PG pools)
//! race N updates (same expected revision) and one archive; C2 a SQL trigger
//! aborts exactly one write step {head UPDATE, revision INSERT, intent INSERT}
//! of `update` or `archive`; C3 the store is reopened.
//!
//! | Rule | Cause | Effect |
//! |---|---|---|
//! | U1 | C1 | revisions form one consecutive legal order r0..=rN; at most one CAS winner per base; archive is the last revision |
//! | U2 | C1 then more writes | terminal: update -> None, archive -> same item, revision frozen |
//! | U3 | C2 | Backend error; head, revision history and intents byte-identical (full rollback) |
//! | U4 | C2 then trigger removed | retry commits exactly once (one new revision + one intent) |
//! | U5 | C3 | U1-U4 history intact after reopen |

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_environment_contract::{
    EnvItem, EnvUpdate, EnvironmentConfig, EnvironmentDefinitionQueries,
    EnvironmentDefinitionStore, EnvironmentRegistrationIntent, EnvironmentRegistrationIntentFilter,
    EnvironmentRegistrationOutbox, EnvironmentRevision, EnvironmentStoreError,
};

trait Registry:
    EnvironmentDefinitionQueries + EnvironmentDefinitionStore + EnvironmentRegistrationOutbox
{
}
impl<T> Registry for T where
    T: EnvironmentDefinitionQueries + EnvironmentDefinitionStore + EnvironmentRegistrationOutbox
{
}

#[derive(Debug, PartialEq)]
struct Durable {
    head: Option<EnvItem>,
    history: Vec<EnvItem>,
    intents: Vec<EnvironmentRegistrationIntent>,
}

async fn durable<R: Registry + ?Sized>(registry: &R, id: &str) -> Durable {
    let head = registry.get(id).await.unwrap();
    let mut history = Vec::new();
    let mut revision = 1;
    while let Some(item) = registry
        .get_revision(id, EnvironmentRevision(revision))
        .await
        .unwrap()
    {
        history.push(item);
        revision += 1;
    }
    let intents = registry
        .registration_intents(EnvironmentRegistrationIntentFilter::All)
        .await
        .unwrap()
        .into_iter()
        .filter(|intent| intent.environment_id == id)
        .collect();
    Durable {
        head,
        history,
        intents,
    }
}

fn rename(name: &str) -> EnvUpdate {
    EnvUpdate {
        name: Some(name.into()),
        ..Default::default()
    }
}

async fn create<R: Registry + ?Sized>(registry: &R, name: &str) -> EnvItem {
    registry
        .create(
            name.into(),
            "t20".into(),
            BTreeMap::new(),
            EnvironmentConfig::SelfHosted,
        )
        .await
        .unwrap()
}

/// U1 legality of one durable history.
fn legal_history_violations(state: &Durable) -> Vec<String> {
    let mut violations = Vec::new();
    for (index, item) in state.history.iter().enumerate() {
        if item.revision != EnvironmentRevision(index as u64 + 1) {
            violations.push(format!("revision gap at {index}: {:?}", item.revision));
        }
        let last = index + 1 == state.history.len();
        if item.archived_at.is_some() && !last {
            violations.push(format!("revision after archive at {index}"));
        }
    }
    if state.head.as_ref() != state.history.last() {
        violations.push("head differs from last revision".into());
    }
    if state.intents.len() != state.history.len() {
        violations.push(format!(
            "{} intents for {} revisions",
            state.intents.len(),
            state.history.len()
        ));
    }
    violations
}

/// U1+U2 against two independent clients.
async fn concurrent_update_archive_conformance<R1, R2>(a: Arc<R1>, b: Arc<R2>)
where
    R1: Registry + 'static,
    R2: Registry + 'static,
{
    let item = create(a.as_ref(), "race").await;
    let base = item.revision;
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..6 {
        let (a, b, id) = (a.clone(), b.clone(), item.id.clone());
        tasks.spawn(async move {
            let name = format!("winner-{n}");
            let result = if n % 2 == 0 {
                a.update(&id, base, rename(&name)).await
            } else {
                b.update(&id, base, rename(&name)).await
            };
            ("update", result)
        });
    }
    {
        let (b, id) = (b.clone(), item.id.clone());
        tasks.spawn(async move { ("archive", b.archive(&id).await) });
    }
    let mut update_winners = 0;
    let mut archived = 0;
    while let Some(joined) = tasks.join_next().await {
        match joined.unwrap() {
            ("update", Ok(Some(updated))) => {
                assert_eq!(updated.revision.0, base.0 + 1, "U1 single CAS step");
                update_winners += 1;
            }
            ("update", Ok(None)) | ("update", Err(EnvironmentStoreError::RevisionConflict)) => {}
            ("archive", Ok(Some(done))) => {
                assert!(done.archived_at.is_some());
                archived += 1;
            }
            (kind, other) => panic!("U1 illegal {kind} outcome: {other:?}"),
        }
    }
    assert!(update_winners <= 1, "U1 at most one winner per base");
    assert_eq!(archived, 1, "U1 archive commits");
    let state = durable(a.as_ref(), &item.id).await;
    assert_eq!(legal_history_violations(&state), Vec::<String>::new(), "U1");
    assert_eq!(
        state.history.len(),
        1 + update_winners + 1,
        "U1 create + winners + archive"
    );
    assert_eq!(
        durable(b.as_ref(), &item.id).await,
        state,
        "U1 both clients agree"
    );

    // U2 terminal immutability.
    let terminal = state.head.clone().unwrap();
    assert_eq!(
        a.update(&item.id, terminal.revision, rename("late"))
            .await
            .unwrap(),
        None,
        "U2 update after archive"
    );
    assert_eq!(
        b.archive(&item.id).await.unwrap(),
        Some(terminal.clone()),
        "U2 archive replay"
    );
    assert_eq!(durable(a.as_ref(), &item.id).await, state, "U2 frozen");
}

/// U3+U4 for one aborted write step. `arm`/`disarm` install/remove the trigger.
async fn faulted_step_conformance<R, Arm, Disarm, FA, FD>(
    registry: &R,
    step: &str,
    operation: &str,
    arm: Arm,
    disarm: Disarm,
) where
    R: Registry + ?Sized,
    Arm: FnOnce() -> FA,
    Disarm: FnOnce() -> FD,
    FA: std::future::Future<Output = ()>,
    FD: std::future::Future<Output = ()>,
{
    let item = create(registry, &format!("fault-{step}-{operation}")).await;
    let before = durable(registry, &item.id).await;
    arm().await;
    let failed = match operation {
        "update" => registry
            .update(&item.id, item.revision, rename("faulted"))
            .await
            .map(|_| ()),
        _ => registry.archive(&item.id).await.map(|_| ()),
    };
    assert!(
        matches!(failed, Err(EnvironmentStoreError::Backend(_))),
        "U3 {step}/{operation} classified: {failed:?}"
    );
    assert_eq!(
        durable(registry, &item.id).await,
        before,
        "U3 {step}/{operation} full rollback"
    );
    disarm().await;
    let retried = match operation {
        "update" => registry
            .update(&item.id, item.revision, rename("faulted"))
            .await
            .unwrap()
            .unwrap(),
        _ => registry.archive(&item.id).await.unwrap().unwrap(),
    };
    assert_eq!(
        retried.revision.0,
        item.revision.0 + 1,
        "U4 {step}/{operation}"
    );
    let after = durable(registry, &item.id).await;
    assert_eq!(
        after.history.len(),
        before.history.len() + 1,
        "U4 exactly once"
    );
    assert_eq!(legal_history_violations(&after), Vec::<String>::new(), "U4");
}

const STEPS: [(&str, &str, &str); 3] = [
    ("head", "env_registry_env", "UPDATE"),
    ("revision", "env_registry_revision", "INSERT"),
    ("intent", "env_registry_registration_intent", "INSERT"),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file_update_archive_race_faults_and_reopen_keep_one_legal_history() {
    use awaken_env_store::SqliteEnvRegistry;
    use sqlx::Connection as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env.db");
    let path = path.to_str().unwrap().to_string();
    let a = Arc::new(
        SqliteEnvRegistry::open(&path, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let b = Arc::new(
        SqliteEnvRegistry::open(&path, awaken_time::system_clock())
            .await
            .unwrap(),
    );
    concurrent_update_archive_conformance(a.clone(), b.clone()).await;

    for (step, table, event) in STEPS {
        for operation in ["update", "archive"] {
            let url = format!("sqlite://{path}");
            let arm_url = url.clone();
            faulted_step_conformance(
                a.as_ref(),
                step,
                operation,
                || async move {
                    let mut conn = sqlx::SqliteConnection::connect(&arm_url).await.unwrap();
                    sqlx::query(&format!(
                        "CREATE TRIGGER t20_fault BEFORE {event} ON {table} \
                         BEGIN SELECT RAISE(ABORT, 'injected t20 fault'); END"
                    ))
                    .execute(&mut conn)
                    .await
                    .unwrap();
                    conn.close().await.unwrap();
                },
                || async move {
                    let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
                    sqlx::query("DROP TRIGGER t20_fault")
                        .execute(&mut conn)
                        .await
                        .unwrap();
                    conn.close().await.unwrap();
                },
            )
            .await;
        }
    }
    let all = a.list_all().await.unwrap();
    let mut before = Vec::new();
    for item in &all {
        before.push(durable(a.as_ref(), &item.id).await);
    }
    drop(a);
    drop(b);
    let reopened = SqliteEnvRegistry::open(&path, awaken_time::system_clock())
        .await
        .unwrap();
    for (item, expected) in all.iter().zip(&before) {
        let state = durable(&reopened, &item.id).await;
        assert_eq!(&state, expected, "U5 history intact after reopen");
        assert_eq!(legal_history_violations(&state), Vec::<String>::new(), "U5");
    }
}

mod postgres {
    use super::*;
    use sqlx::Executor;
    use sqlx::postgres::{PgConnectOptions, PgPool};

    async fn connect(url: &str, schema: &str) -> PgPool {
        let options = url
            .parse::<PgConnectOptions>()
            .unwrap()
            .options([("search_path", schema)]);
        awaken_sql_storage::postgres_pool_options()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect to the test schema")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_update_archive_race_faults_and_reopen_keep_one_legal_history() {
        use awaken_env_store::PostgresEnvRegistry;
        let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let admin = awaken_sql_storage::postgres_pool_options()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("configured Environment PostgreSQL fixture must connect");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("t_env_t20_{}_{nonce:x}", std::process::id());
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        admin.close().await;
        let pool_a = connect(&url, &schema).await;
        let pool_b = connect(&url, &schema).await;
        let a = Arc::new(
            PostgresEnvRegistry::with_pool(pool_a.clone(), awaken_time::system_clock())
                .await
                .unwrap(),
        );
        let b = Arc::new(
            PostgresEnvRegistry::with_existing_pool(pool_b.clone(), awaken_time::system_clock())
                .await
                .unwrap(),
        );
        concurrent_update_archive_conformance(a.clone(), b.clone()).await;
        pool_a
            .execute(
                "CREATE FUNCTION t20_fault() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN RAISE EXCEPTION 'injected t20 fault'; END $$",
            )
            .await
            .unwrap();
        for (step, table, event) in STEPS {
            for operation in ["update", "archive"] {
                let (arm_pool, disarm_pool) = (pool_a.clone(), pool_a.clone());
                faulted_step_conformance(
                    b.as_ref(),
                    step,
                    operation,
                    || async move {
                        arm_pool
                            .execute(
                                format!(
                                    "CREATE TRIGGER t20_fault BEFORE {event} ON {table} \
                                     FOR EACH ROW EXECUTE FUNCTION t20_fault()"
                                )
                                .as_str(),
                            )
                            .await
                            .unwrap();
                    },
                    || async move {
                        disarm_pool
                            .execute(format!("DROP TRIGGER t20_fault ON {table}").as_str())
                            .await
                            .unwrap();
                    },
                )
                .await;
            }
        }
        let all = a.list_all().await.unwrap();
        let mut before = Vec::new();
        for item in &all {
            before.push(durable(a.as_ref(), &item.id).await);
        }
        drop(a);
        drop(b);
        pool_a.close().await;
        pool_b.close().await;
        let reopened = PostgresEnvRegistry::with_existing_pool(
            connect(&url, &schema).await,
            awaken_time::system_clock(),
        )
        .await
        .unwrap();
        for (item, expected) in all.iter().zip(&before) {
            let state = durable(&reopened, &item.id).await;
            assert_eq!(&state, expected, "U5 history intact after reopen");
            assert_eq!(legal_history_violations(&state), Vec::<String>::new(), "U5");
        }
    }

    #[test]
    fn postgres_update_archive_race_faults_and_reopen_keep_one_legal_history_fixture_admission() {
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(
            "postgres::postgres_update_archive_race_faults_and_reopen_keep_one_legal_history",
        );
    }
}
