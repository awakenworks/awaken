use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
    extract_meta_revision,
};
use awaken_contract::contract::storage::StorageError;
use sqlx::postgres::PgListener;

use super::PostgresStore;

struct PostgresConfigChangeSubscriber {
    listener: PgListener,
}

#[async_trait]
impl ConfigChangeSubscriber for PostgresConfigChangeSubscriber {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError> {
        let notification = self
            .listener
            .recv()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        serde_json::from_str(notification.payload())
            .map_err(|error| StorageError::Serialization(error.to_string()))
    }
}

// ── ConfigStore ─────────────────────────────────────────────────────

#[async_trait]
impl ConfigStore for PostgresStore {
    async fn get(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT data FROM {} WHERE namespace = $1 AND id = $2",
            self.configs_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(row.map(|(value,)| value))
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>, StorageError> {
        self.ensure_schema().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let offset = offset.min(i64::MAX as usize) as i64;
        let sql = format!(
            "SELECT id, data FROM {} WHERE namespace = $1 ORDER BY id ASC LIMIT $2 OFFSET $3",
            self.configs_table
        );
        sqlx::query_as(&sql)
            .bind(namespace)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))
    }

    async fn put(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let sql = format!(
            "INSERT INTO {} (namespace, id, data) VALUES ($1, $2, $3)
             ON CONFLICT (namespace, id) DO UPDATE SET data = $3, updated_at = now()",
            self.configs_table
        );
        sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let payload = serde_json::to_string(&ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        })
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.config_notify_channel)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(())
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let sql = format!(
            "INSERT INTO {} (namespace, id, data) VALUES ($1, $2, $3)",
            self.configs_table
        );
        let result = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .bind(value)
            .execute(&mut *tx)
            .await;
        if let Err(error) = result {
            if error
                .as_database_error()
                .and_then(|db_error| db_error.code())
                .as_deref()
                == Some("23505")
            {
                return Err(StorageError::AlreadyExists(format!("{namespace}/{id}")));
            }
            return Err(StorageError::Io(error.to_string()));
        }

        let payload = serde_json::to_string(&ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        })
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.config_notify_channel)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        tx.commit()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(())
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        let sql = format!(
            "DELETE FROM {} WHERE namespace = $1 AND id = $2",
            self.configs_table
        );
        let result = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;

        if result.rows_affected() > 0 {
            let payload = serde_json::to_string(&ConfigChangeEvent {
                namespace: namespace.to_string(),
                id: id.to_string(),
                kind: ConfigChangeKind::Delete,
            })
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&self.config_notify_channel)
                .bind(payload)
                .execute(&mut *tx)
                .await
                .map_err(|error| StorageError::Io(error.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(())
    }

    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &serde_json::Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        // Lock the row (or its absence) so that concurrent writers cannot race
        // between the read and the upsert within this transaction.
        let select_sql = format!(
            "SELECT data FROM {} WHERE namespace = $1 AND id = $2 FOR UPDATE",
            self.configs_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&select_sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let actual = row
            .as_ref()
            .map(|(v,)| v)
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }

        let upsert_sql = format!(
            "INSERT INTO {} (namespace, id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (namespace, id) DO UPDATE SET data = EXCLUDED.data, updated_at = now()",
            self.configs_table
        );
        sqlx::query(&upsert_sql)
            .bind(namespace)
            .bind(id)
            .bind(value)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        // Fire NOTIFY inside the same transaction, matching put semantics.
        let payload = serde_json::to_string(&ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        })
        .map_err(|e| StorageError::Serialization(e.to_string()))?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.config_notify_channel)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))")
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let select_sql = format!(
            "SELECT data FROM {} WHERE namespace = $1 AND id = $2 FOR UPDATE",
            self.configs_table
        );
        let row: Option<(serde_json::Value,)> = sqlx::query_as(&select_sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let actual = row
            .as_ref()
            .map(|(v,)| v)
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }

        let delete_sql = format!(
            "DELETE FROM {} WHERE namespace = $1 AND id = $2",
            self.configs_table
        );
        let result = sqlx::query(&delete_sql)
            .bind(namespace)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        if result.rows_affected() > 0 {
            let payload = serde_json::to_string(&ConfigChangeEvent {
                namespace: namespace.to_string(),
                id: id.to_string(),
                kind: ConfigChangeKind::Delete,
            })
            .map_err(|e| StorageError::Serialization(e.to_string()))?;
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&self.config_notify_channel)
                .bind(payload)
                .execute(&mut *tx)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }
}

// ── ConfigChangeNotifier ────────────────────────────────────────────

#[async_trait]
impl ConfigChangeNotifier for PostgresStore {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        self.ensure_schema().await?;
        let mut listener = PgListener::connect_with(&self.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        listener
            .listen(&self.config_notify_channel)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
        Ok(Box::new(PostgresConfigChangeSubscriber { listener }))
    }
}
