//! PostgreSQL adapter for the Resource Registry aggregate repository.

use awaken_resource_contract::{
    AggregateRevision, InsertOutcome, MemoryStoreAggregate, RegistryRepositoryError,
    ReplaceOutcome, RepositoryAggregate, ResourceRegistryRepository, Stored,
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;
use sqlx::types::Json;

use crate::postgres::PostgresResourceStore;
use crate::schema::REGISTRY_NS;

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

fn indeterminate(error: impl ToString) -> RegistryRepositoryError {
    RegistryRepositoryError::Indeterminate {
        detail: error.to_string(),
    }
}

fn corrupt(error: impl ToString) -> RegistryRepositoryError {
    RegistryRepositoryError::CorruptData(error.to_string())
}

impl PostgresResourceStore {
    async fn registry_record<T>(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<Stored<T>>, RegistryRepositoryError>
    where
        T: DeserializeOwned,
    {
        let sql =
            format!("SELECT revision, data FROM {REGISTRY_NS}_entry WHERE kind = $1 AND id = $2");
        sqlx::query(&sql)
            .bind(kind)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(indeterminate)?
            .map(|row| {
                let revision: i64 = row.try_get("revision").map_err(corrupt)?;
                let revision = u64::try_from(revision)
                    .map_err(corrupt)
                    .and_then(AggregateRevision::new)?;
                let Json(aggregate): Json<T> = row.try_get("data").map_err(corrupt)?;
                Ok(Stored {
                    revision,
                    aggregate,
                })
            })
            .transpose()
    }

    async fn insert_registry_record<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        aggregate: &T,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        let sql = format!(
            "INSERT INTO {REGISTRY_NS}_entry (kind, id, revision, data) VALUES ($1, $2, 1, $3)"
        );
        let data = serde_json::to_value(aggregate).map_err(corrupt)?;
        match sqlx::query(&sql)
            .bind(kind)
            .bind(id)
            .bind(Json(data))
            .execute(&self.pool)
            .await
        {
            Ok(_) => Ok(InsertOutcome::Inserted),
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
                Ok(InsertOutcome::AlreadyRegistered)
            }
            Err(error) => Err(indeterminate(error)),
        }
    }

    async fn replace_registry_record<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        expected_revision: AggregateRevision,
        aggregate: &T,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError> {
        let next = expected_revision.checked_next().ok_or_else(|| {
            RegistryRepositoryError::CorruptData(format!(
                "resource `{id}` exhausted aggregate revisions"
            ))
        })?;
        let sql = format!(
            "UPDATE {REGISTRY_NS}_entry SET revision = $4, data = $5 \
             WHERE kind = $1 AND id = $2 AND revision = $3"
        );
        let data = serde_json::to_value(aggregate).map_err(corrupt)?;
        let changed = sqlx::query(&sql)
            .bind(kind)
            .bind(id)
            .bind(expected_revision.get() as i64)
            .bind(next.get() as i64)
            .bind(Json(data))
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected())
            .map_err(indeterminate)?;
        Ok(if changed == 1 {
            ReplaceOutcome::Replaced { revision: next }
        } else {
            ReplaceOutcome::ConcurrentModification
        })
    }

    async fn list_registry_records<T: DeserializeOwned>(
        &self,
        kind: &'static str,
        validate: impl Fn(&T, &str) -> Result<(), RegistryRepositoryError>,
    ) -> Result<Vec<Stored<T>>, RegistryRepositoryError> {
        let sql = format!(
            "SELECT id, revision, data FROM {REGISTRY_NS}_entry WHERE kind = $1 ORDER BY id"
        );
        sqlx::query(&sql)
            .bind(kind)
            .fetch_all(&self.pool)
            .await
            .map_err(indeterminate)?
            .into_iter()
            .map(|row| {
                let id: String = row.try_get("id").map_err(corrupt)?;
                let revision: i64 = row.try_get("revision").map_err(corrupt)?;
                let revision = u64::try_from(revision)
                    .map_err(corrupt)
                    .and_then(AggregateRevision::new)?;
                let Json(aggregate): Json<T> = row.try_get("data").map_err(corrupt)?;
                validate(&aggregate, &id)?;
                Ok(Stored {
                    revision,
                    aggregate,
                })
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl ResourceRegistryRepository for PostgresResourceStore {
    async fn load_memory_store(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        let stored = self
            .registry_record::<MemoryStoreAggregate>(MEMORY, id)
            .await?;
        if let Some(stored) = &stored {
            if stored.aggregate.definition().id.as_str() != id {
                return Err(corrupt(format!(
                    "row id `{id}` does not match MemoryStore aggregate id `{}`",
                    stored.aggregate.definition().id
                )));
            }
            stored.aggregate.validate_integrity().map_err(corrupt)?;
        }
        Ok(stored)
    }

    async fn insert_memory_store(
        &self,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        self.insert_registry_record(MEMORY, aggregate.definition().id.as_str(), aggregate)
            .await
    }

    async fn replace_memory_store(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError> {
        self.replace_registry_record(
            MEMORY,
            aggregate.definition().id.as_str(),
            expected_revision,
            aggregate,
        )
        .await
    }

    async fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        Ok(self
            .list_registry_records(MEMORY, |aggregate: &MemoryStoreAggregate, id| {
                if aggregate.definition().id.as_str() != id {
                    return Err(corrupt(format!(
                        "row id `{id}` does not match aggregate identity"
                    )));
                }
                aggregate.validate_integrity().map_err(corrupt)
            })
            .await?
            .into_iter()
            .filter(|stored| stored.aggregate.definition().workspace_id == workspace_id)
            .collect())
    }

    async fn list_repositories(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<RepositoryAggregate>>, RegistryRepositoryError> {
        Ok(self
            .list_registry_records(REPOSITORY, |aggregate: &RepositoryAggregate, id| {
                if aggregate.definition().id.as_str() != id {
                    return Err(corrupt(format!(
                        "row id `{id}` does not match aggregate identity"
                    )));
                }
                aggregate.validate_integrity().map_err(corrupt)
            })
            .await?
            .into_iter()
            .filter(|stored| stored.aggregate.definition().workspace_id == workspace_id)
            .collect())
    }

    async fn load_repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, RegistryRepositoryError> {
        let stored = self
            .registry_record::<RepositoryAggregate>(REPOSITORY, id)
            .await?;
        if let Some(stored) = &stored {
            if stored.aggregate.definition().id.as_str() != id {
                return Err(corrupt(format!(
                    "row id `{id}` does not match Repository aggregate id `{}`",
                    stored.aggregate.definition().id
                )));
            }
            stored.aggregate.validate_integrity().map_err(corrupt)?;
        }
        Ok(stored)
    }

    async fn insert_repository(
        &self,
        aggregate: &RepositoryAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        self.insert_registry_record(REPOSITORY, aggregate.definition().id.as_str(), aggregate)
            .await
    }

    async fn replace_repository(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &RepositoryAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError> {
        self.replace_registry_record(
            REPOSITORY,
            aggregate.definition().id.as_str(),
            expected_revision,
            aggregate,
        )
        .await
    }
}
