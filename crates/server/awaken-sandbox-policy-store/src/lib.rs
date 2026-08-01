//! Durable SandboxExecutionPolicy versions.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract::{
    SandboxExecutionPolicy, SandboxExecutionPolicyError, SandboxExecutionPolicyRef,
    SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
};
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sqlx::{PgPool, Row};

const NS: &str = "sandbox_execution_policy";

/// The single portable schema authority shared by the SQLite and Postgres adapters.
fn sandbox_policy_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.sandbox_execution_policy",
        vec![
            Migration::new(
                1,
                "immutable sandbox execution policy versions",
                "CREATE TABLE {prefix}_version (policy_id TEXT NOT NULL, version BIGINT NOT NULL, policy_json TEXT NOT NULL, PRIMARY KEY(policy_id, version))",
            )?,
            Migration::new(
                2,
                "current sandbox execution policy version",
                "CREATE TABLE {prefix}_current (policy_id TEXT PRIMARY KEY, version BIGINT NOT NULL)",
            )?,
            Migration::new(
                3,
                "exact environment sandbox execution policy binding",
                "CREATE TABLE {prefix}_environment (environment_id TEXT PRIMARY KEY, policy_id TEXT NOT NULL, version BIGINT NOT NULL)",
            )?,
        ],
    )
}

fn validate(policy: &SandboxExecutionPolicy) -> Result<(), SandboxExecutionPolicyError> {
    if policy.config.network.is_some() {
        return Err(SandboxExecutionPolicyError::Invalid(
            "network belongs to the Environment contract".into(),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct InMemoryState {
    policies: BTreeMap<String, BTreeMap<u64, SandboxExecutionPolicy>>,
    current: BTreeMap<String, u64>,
}

#[derive(Default)]
pub struct InMemorySandboxExecutionPolicyStore(Mutex<InMemoryState>);

#[async_trait]
impl SandboxExecutionPolicyStore for InMemorySandboxExecutionPolicyStore {
    async fn create(
        &self,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        validate(&policy)?;
        if policy.version != SandboxExecutionPolicyVersion::INITIAL {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let mut state = self.0.lock().unwrap();
        if state.current.contains_key(&policy.id.0) {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        state.current.insert(policy.id.0.clone(), policy.version.0);
        state
            .policies
            .entry(policy.id.0.clone())
            .or_default()
            .insert(policy.version.0, policy);
        Ok(())
    }

    async fn publish(
        &self,
        expected_current: SandboxExecutionPolicyVersion,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        validate(&policy)?;
        let mut state = self.0.lock().unwrap();
        if state.current.get(&policy.id.0).copied() != Some(expected_current.0)
            || policy.version.0 != expected_current.0.saturating_add(1)
        {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        state.current.insert(policy.id.0.clone(), policy.version.0);
        state
            .policies
            .entry(policy.id.0.clone())
            .or_default()
            .insert(policy.version.0, policy);
        Ok(())
    }

    async fn get_exact(
        &self,
        reference: &SandboxExecutionPolicyRef,
    ) -> Result<SandboxExecutionPolicy, SandboxExecutionPolicyError> {
        self.0
            .lock()
            .unwrap()
            .policies
            .get(&reference.id.0)
            .and_then(|versions| versions.get(&reference.version.0))
            .cloned()
            .ok_or(SandboxExecutionPolicyError::NotFound)
    }
}

pub struct SqliteSandboxExecutionPolicyStore {
    conn: Arc<Mutex<Connection>>,
}

pub struct PostgresSandboxExecutionPolicyStore {
    pool: PgPool,
}

impl PostgresSandboxExecutionPolicyStore {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| error.to_string())?;
        Self::with_pool(pool).await
    }

    /// Wrap a pool and apply the canonical sandbox-policy migration bundle.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = sandbox_policy_bundle().map_err(|error| error.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| error.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self { pool })
    }

    /// Connect to an already-provisioned schema, verifying its scoped ledger
    /// without executing startup DDL.
    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|error| error.to_string())?;
        Self::with_existing_pool(pool).await
    }

    /// Wrap an existing pool after verifying its scoped migration ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = sandbox_policy_bundle().map_err(|error| error.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| error.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl SandboxExecutionPolicyStore for PostgresSandboxExecutionPolicyStore {
    async fn create(
        &self,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        validate(&policy)?;
        if policy.version != SandboxExecutionPolicyVersion::INITIAL {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let mut tx = self.pool.begin().await.map_err(store_failed)?;
        let json = serde_json::to_string(&policy).map_err(store_failed)?;
        sqlx::query("INSERT INTO sandbox_execution_policy_version(policy_id,version,policy_json) VALUES($1,$2,$3)")
            .bind(&policy.id.0)
            .bind(as_i64(policy.version.0)?)
            .bind(json)
            .execute(&mut *tx)
            .await
            .map_err(|_| SandboxExecutionPolicyError::VersionConflict)?;
        sqlx::query(
            "INSERT INTO sandbox_execution_policy_current(policy_id,version) VALUES($1,$2)",
        )
        .bind(&policy.id.0)
        .bind(as_i64(policy.version.0)?)
        .execute(&mut *tx)
        .await
        .map_err(|_| SandboxExecutionPolicyError::VersionConflict)?;
        tx.commit().await.map_err(store_failed)
    }

    async fn publish(
        &self,
        expected_current: SandboxExecutionPolicyVersion,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        validate(&policy)?;
        if policy.version.0 != expected_current.0.saturating_add(1) {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let mut tx = self.pool.begin().await.map_err(store_failed)?;
        let changed = sqlx::query("UPDATE sandbox_execution_policy_current SET version=$1 WHERE policy_id=$2 AND version=$3")
            .bind(as_i64(policy.version.0)?)
            .bind(&policy.id.0)
            .bind(as_i64(expected_current.0)?)
            .execute(&mut *tx)
            .await
            .map_err(store_failed)?
            .rows_affected();
        if changed != 1 {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let json = serde_json::to_string(&policy).map_err(store_failed)?;
        sqlx::query("INSERT INTO sandbox_execution_policy_version(policy_id,version,policy_json) VALUES($1,$2,$3)")
            .bind(&policy.id.0)
            .bind(as_i64(policy.version.0)?)
            .bind(json)
            .execute(&mut *tx)
            .await
            .map_err(|_| SandboxExecutionPolicyError::VersionConflict)?;
        tx.commit().await.map_err(store_failed)
    }

    async fn get_exact(
        &self,
        reference: &SandboxExecutionPolicyRef,
    ) -> Result<SandboxExecutionPolicy, SandboxExecutionPolicyError> {
        let row = sqlx::query("SELECT policy_json FROM sandbox_execution_policy_version WHERE policy_id=$1 AND version=$2")
            .bind(&reference.id.0)
            .bind(as_i64(reference.version.0)?)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_failed)?
            .ok_or(SandboxExecutionPolicyError::NotFound)?;
        serde_json::from_str(row.get::<String, _>(0).as_str()).map_err(store_failed)
    }
}

fn store_failed(error: impl std::fmt::Display) -> SandboxExecutionPolicyError {
    SandboxExecutionPolicyError::StoreFailed(error.to_string())
}

fn as_i64(value: u64) -> Result<i64, SandboxExecutionPolicyError> {
    i64::try_from(value).map_err(store_failed)
}

impl SqliteSandboxExecutionPolicyStore {
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|error| error.to_string())?;
        let bundle = sandbox_policy_bundle().map_err(|error| error.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|error| error.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn exact(
        conn: &Connection,
        reference: &SandboxExecutionPolicyRef,
    ) -> Result<SandboxExecutionPolicy, SandboxExecutionPolicyError> {
        let json: Option<String> = conn
            .query_row(
                "SELECT policy_json FROM sandbox_execution_policy_version WHERE policy_id=?1 AND version=?2",
                params![reference.id.0, reference.version.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))?;
        serde_json::from_str(&json.ok_or(SandboxExecutionPolicyError::NotFound)?)
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))
    }
}

#[async_trait]
impl SandboxExecutionPolicyStore for SqliteSandboxExecutionPolicyStore {
    async fn create(
        &self,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        validate(&policy)?;
        if policy.version != SandboxExecutionPolicyVersion::INITIAL {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))?;
        let json = serde_json::to_string(&policy)
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))?;
        tx.execute(
            "INSERT INTO sandbox_execution_policy_version(policy_id,version,policy_json) VALUES(?1,?2,?3)",
            params![policy.id.0, policy.version.0, json],
        )
        .and_then(|_| tx.execute("INSERT INTO sandbox_execution_policy_current(policy_id,version) VALUES(?1,?2)", params![policy.id.0, policy.version.0]))
        .map_err(|_| SandboxExecutionPolicyError::VersionConflict)?;
        tx.commit()
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))
    }

    async fn publish(
        &self,
        expected_current: SandboxExecutionPolicyVersion,
        policy: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        validate(&policy)?;
        if policy.version.0 != expected_current.0.saturating_add(1) {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))?;
        let changed = tx
            .execute(
                "UPDATE sandbox_execution_policy_current SET version=?1 WHERE policy_id=?2 AND version=?3",
                params![policy.version.0, policy.id.0, expected_current.0],
            )
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))?;
        if changed != 1 {
            return Err(SandboxExecutionPolicyError::VersionConflict);
        }
        let json = serde_json::to_string(&policy)
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))?;
        tx.execute(
            "INSERT INTO sandbox_execution_policy_version(policy_id,version,policy_json) VALUES(?1,?2,?3)",
            params![policy.id.0, policy.version.0, json],
        )
        .map_err(|_| SandboxExecutionPolicyError::VersionConflict)?;
        tx.commit()
            .map_err(|error| SandboxExecutionPolicyError::StoreFailed(error.to_string()))
    }

    async fn get_exact(
        &self,
        reference: &SandboxExecutionPolicyRef,
    ) -> Result<SandboxExecutionPolicy, SandboxExecutionPolicyError> {
        Self::exact(&self.conn.lock().unwrap(), reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{IsolationClass, SandboxExecutionPolicyId, SandboxOverride};

    fn policy(id: &str, version: u64, isolation: IsolationClass) -> SandboxExecutionPolicy {
        SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId(id.to_string()),
            version: SandboxExecutionPolicyVersion(version),
            config: SandboxOverride {
                isolation: Some(isolation),
                ..Default::default()
            },
            provisioning: Default::default(),
            disabled: false,
        }
    }

    async fn exact_version_decision_table(store: &dyn SandboxExecutionPolicyStore) {
        // Causal graph:
        // create v1 -> current v1; publish with current fence -> immutable v2
        // exact v1 remains readable after v2 exists; stale publish and missing
        // exact reads fail closed.
        //
        // | Rule | target exists | expected current | effect |
        // | P1   | v1            | -                | create |
        // | P2   | v2            | v1               | publish |
        // | P3   | v1            | -                | exact v1 readable |
        // | P4   | v3            | stale v1         | conflict |
        // | P5   | missing       | -                | not found |
        let v1 = policy("strict", 1, IsolationClass::Namespace);
        store.create(v1.clone()).await.expect("P1");
        let v2 = policy("strict", 2, IsolationClass::Container);
        store
            .publish(SandboxExecutionPolicyVersion(1), v2.clone())
            .await
            .expect("P2");
        let v1_ref = SandboxExecutionPolicyRef {
            id: v1.id.clone(),
            version: v1.version,
        };
        assert_eq!(store.get_exact(&v1_ref).await.unwrap(), v1);
        assert!(matches!(
            store
                .publish(
                    SandboxExecutionPolicyVersion(1),
                    policy("strict", 3, IsolationClass::Container)
                )
                .await,
            Err(SandboxExecutionPolicyError::VersionConflict)
        ));
        assert!(matches!(
            store
                .get_exact(&SandboxExecutionPolicyRef {
                    id: SandboxExecutionPolicyId("missing".into()),
                    version: SandboxExecutionPolicyVersion(1),
                })
                .await,
            Err(SandboxExecutionPolicyError::NotFound)
        ));
    }

    #[tokio::test]
    async fn in_memory_exact_version_rules() {
        exact_version_decision_table(&InMemorySandboxExecutionPolicyStore::default()).await;
    }

    #[tokio::test]
    async fn sqlite_exact_version_rules_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.db");
        let path = path.to_str().unwrap();
        let first = SqliteSandboxExecutionPolicyStore::open(path).unwrap();
        exact_version_decision_table(&first).await;
        drop(first);
        let restarted = SqliteSandboxExecutionPolicyStore::open(path).unwrap();
        let v1 = SandboxExecutionPolicyRef {
            id: SandboxExecutionPolicyId("strict".into()),
            version: SandboxExecutionPolicyVersion(1),
        };
        assert_eq!(
            restarted.get_exact(&v1).await.unwrap().config.isolation,
            Some(IsolationClass::Namespace)
        );
    }

    #[test]
    fn sqlite_schema_has_one_scoped_migration_authority() {
        // Causal graph:
        // open -> run canonical bundle -> ledger + three published tables -> serve
        // reopen -> ledger verifies checksums -> no duplicate schema path
        // runtime authority moved -> historical V3 table remains inert
        //
        // Decision table:
        // | first open | ledger current | expected effect                  |
        // | yes        | no             | apply exact published V1-V3      |
        // | no         | yes            | apply zero pending migrations    |
        // | no         | checksum drift | fail closed                      |
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy-schema.db");
        let path = path.to_str().unwrap();
        let first = SqliteSandboxExecutionPolicyStore::open(path).unwrap();
        let applied: i64 = first
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sandbox_execution_policy_schema_migrations WHERE bundle_id = 'awaken.sandbox_execution_policy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, 3);
        let historical_binding_table: Option<String> = first
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'sandbox_execution_policy_environment'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            historical_binding_table.as_deref(),
            Some("sandbox_execution_policy_environment")
        );
        drop(first);
        let reopened = SqliteSandboxExecutionPolicyStore::open(path).unwrap();
        let applied_after_reopen: i64 = reopened
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sandbox_execution_policy_schema_migrations WHERE bundle_id = 'awaken.sandbox_execution_policy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_after_reopen, 3);
        reopened
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sandbox_execution_policy_schema_migrations SET checksum = 'drifted' WHERE bundle_id = 'awaken.sandbox_execution_policy' AND version = 1",
                [],
            )
            .unwrap();
        drop(reopened);
        assert!(
            SqliteSandboxExecutionPolicyStore::open(path).is_err(),
            "checksum drift fails closed instead of being rewritten"
        );
    }

    #[tokio::test]
    async fn postgres_verify_is_fail_closed_and_read_only() {
        use sqlx::Executor;
        use sqlx::postgres::PgPoolOptions;

        let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
            return;
        };
        let admin = PgPool::connect(&url).await.unwrap();
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_sandbox_policy CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_sandbox_policy")
            .await
            .unwrap();
        admin.close().await;
        let pool = PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    connection
                        .execute("SET search_path = t_sandbox_policy")
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();

        // Decision table:
        // | ledger state | operation | DDL allowed | result  |
        // | absent       | verify    | no          | failure |
        // | absent       | migrate   | yes         | success |
        // | current      | verify    | no          | success |
        assert!(
            PostgresSandboxExecutionPolicyStore::with_existing_pool(pool.clone())
                .await
                .is_err()
        );
        let ledger_after_verify: Option<String> = sqlx::query_scalar(
            "SELECT to_regclass('sandbox_execution_policy_schema_migrations')::text",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            ledger_after_verify, None,
            "verify must not create its ledger"
        );

        PostgresSandboxExecutionPolicyStore::with_pool(pool.clone())
            .await
            .unwrap();
        PostgresSandboxExecutionPolicyStore::with_existing_pool(pool)
            .await
            .unwrap();
    }
}
