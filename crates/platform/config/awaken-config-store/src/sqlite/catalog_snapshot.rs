// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: One native readonly transaction supplies the bounded Config
// @formal reason: snapshot and scope epoch; no row or observation grants permission.
use super::{SqliteConfigStore, reject};
use crate::catalog_snapshot as codec;
use awaken_agent_config::catalog_snapshot::{
    AgentCatalogEntry, AgentCatalogPreference, AgentCatalogSnapshot, AgentCatalogSnapshotRepository,
};
use awaken_agent_config::personal_selection::AgentSelectionOwner;
use awaken_agent_config::{ConfigStoreError, ScopeId};
use sqlx::Row;

#[async_trait::async_trait]
impl AgentCatalogSnapshotRepository for SqliteConfigStore {
    async fn agent_catalog_epoch(&self, scope: &ScopeId) -> Result<Option<u64>, ConfigStoreError> {
        codec::bound(scope, 1)?;
        let value: Option<i64> =
            sqlx::query_scalar("SELECT epoch FROM config_agent_catalog_epoch WHERE scope_id=?1")
                .bind(&scope.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(reject)?;
        let observed = codec::epoch(value)?;
        if observed.is_none() {
            let data: i64 = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM config_agent WHERE scope_id=?1 UNION ALL SELECT 1 FROM config_publication WHERE scope_id=?1 AND COALESCE(json_extract(record, '$.execution_workspace'), scope_id)=?1 UNION ALL SELECT 1 FROM config_agent_revision WHERE scope_id=?1 UNION ALL SELECT 1 FROM config_personal_agent_selection WHERE scope_id=?1)").bind(&scope.0).fetch_one(&self.pool).await.map_err(reject)?;
            if data != 0 {
                return Err(ConfigStoreError(
                    "catalog epoch is missing for stored state".into(),
                ));
            }
        }
        Ok(observed)
    }

    async fn agent_catalog_snapshot(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        maximum_agents: usize,
    ) -> Result<AgentCatalogSnapshot, ConfigStoreError> {
        let bound = codec::bound(scope, maximum_agents)?;
        let mut tx = self.pool.begin().await.map_err(reject)?;
        let observed: Option<i64> =
            sqlx::query_scalar("SELECT epoch FROM config_agent_catalog_epoch WHERE scope_id=?1")
                .bind(&scope.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(reject)?;
        let epoch = codec::epoch(observed)?;
        if epoch.is_none() {
            let data: i64 = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM config_agent WHERE scope_id=?1 UNION ALL SELECT 1 FROM config_publication WHERE scope_id=?1 AND COALESCE(json_extract(record, '$.execution_workspace'), scope_id)=?1 UNION ALL SELECT 1 FROM config_agent_revision WHERE scope_id=?1 UNION ALL SELECT 1 FROM config_personal_agent_selection WHERE scope_id=?1)").bind(&scope.0).fetch_one(&mut *tx).await.map_err(reject)?;
            if data != 0 {
                return Err(ConfigStoreError(
                    "catalog epoch is missing for stored state".into(),
                ));
            }
        }
        let rows = sqlx::query("SELECT a.id, a.data AS data, a.generation, CAST(strftime('%s', first.created_at) AS INTEGER) * 1000 AS created, CAST(strftime('%s', current.created_at) AS INTEGER) * 1000 AS updated FROM config_agent a LEFT JOIN config_agent_revision first ON first.scope_id=a.scope_id AND first.id=a.id AND first.generation=1 LEFT JOIN config_agent_revision current ON current.scope_id=a.scope_id AND current.id=a.id AND current.generation=a.generation WHERE a.scope_id=?1 ORDER BY a.id LIMIT ?2").bind(&scope.0).bind(bound).fetch_all(&mut *tx).await.map_err(reject)?;
        let mut entries = std::collections::BTreeMap::new();
        for row in rows {
            let id: String = row.try_get("id").map_err(reject)?;
            let head = codec::revision(
                &id,
                &row.try_get::<String, _>("data").map_err(reject)?,
                row.try_get("generation").map_err(reject)?,
                row.try_get("created").map_err(reject)?,
                row.try_get("updated").map_err(reject)?,
            )?;
            entries.insert(
                id,
                AgentCatalogEntry {
                    head,
                    published: None,
                    publication: None,
                },
            );
        }
        if entries.len() > maximum_agents {
            return Err(ConfigStoreError(
                "catalog read exceeds its native bound".into(),
            ));
        }
        let rows = sqlx::query("SELECT catalog_agent_id, record, sequence, published_at, order_verified FROM (SELECT p.agent_id AS catalog_agent_id, p.record AS record, p.rowid AS sequence, CAST(strftime('%s', p.created_at) AS INTEGER) * 1000 AS published_at, 1 AS order_verified, ROW_NUMBER() OVER (PARTITION BY p.agent_id ORDER BY COALESCE(json_extract(p.record, '$.source_revision'), json_extract(p.record, '$.source_generation'), 0) DESC, p.rowid ASC) AS rank FROM config_publication p JOIN config_agent a ON a.scope_id=p.scope_id AND a.id=p.agent_id WHERE p.scope_id=?1 AND p.state='published' AND COALESCE(json_extract(p.record, '$.execution_workspace'), p.scope_id)=?1 AND COALESCE(json_extract(p.record, '$.source_revision'), json_extract(p.record, '$.source_generation'), 0)>0) candidates WHERE rank=1 LIMIT ?2").bind(&scope.0).bind(bound).fetch_all(&mut *tx).await.map_err(reject)?;
        for row in rows {
            let record: String = row.try_get("record").map_err(reject)?;
            let publication = crate::codec::decode_publication(&record, scope).map_err(reject)?;
            let indexed_agent: String = row.try_get("catalog_agent_id").map_err(reject)?;
            if publication.agent_id != indexed_agent {
                return Err(ConfigStoreError(
                    "catalog publication index and record disagree".into(),
                ));
            }
            let id = publication.agent_id.clone();
            let entry = entries.get_mut(&id).ok_or_else(|| {
                ConfigStoreError("catalog publication has no current Agent".into())
            })?;
            let generation = i64::try_from(publication.source_revision).map_err(reject)?;
            let version = sqlx::query("SELECT r.data AS data, r.generation, CAST(strftime('%s', first.created_at) AS INTEGER) * 1000 AS created, CAST(strftime('%s', r.created_at) AS INTEGER) * 1000 AS updated FROM config_agent_revision r JOIN config_agent_revision first ON first.scope_id=r.scope_id AND first.id=r.id AND first.generation=1 WHERE r.scope_id=?1 AND r.id=?2 AND r.generation=?3").bind(&scope.0).bind(&id).bind(generation).fetch_optional(&mut *tx).await.map_err(reject)?.ok_or_else(|| ConfigStoreError("catalog publication source is missing".into()))?;
            entry.published = Some(codec::revision(
                &id,
                &version.try_get::<String, _>("data").map_err(reject)?,
                version.try_get("generation").map_err(reject)?,
                version.try_get("created").map_err(reject)?,
                version.try_get("updated").map_err(reject)?,
            )?);
            entry.publication = Some(crate::codec::attach_publication_row(
                publication,
                row.try_get("sequence").map_err(reject)?,
                row.try_get("published_at").map_err(reject)?,
                true,
            )?);
        }
        let rows = sqlx::query("SELECT p.agent_id, p.generation, p.selected FROM config_personal_agent_selection p WHERE p.scope_id=?1 AND p.issuer=?2 AND p.account_id=?3 ORDER BY p.agent_id LIMIT ?4").bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(bound).fetch_all(&mut *tx).await.map_err(reject)?;
        let mut preferences = Vec::with_capacity(rows.len());
        for row in rows {
            preferences.push(AgentCatalogPreference {
                agent_id: row.try_get("agent_id").map_err(reject)?,
                state: crate::personal_selection::state(
                    row.try_get("generation").map_err(reject)?,
                    row.try_get("selected").map_err(reject)?,
                )?,
            });
        }
        let snapshot = AgentCatalogSnapshot {
            epoch,
            entries: entries.into_values().collect(),
            preferences,
        };
        codec::validate(&snapshot, maximum_agents)?;
        tx.commit().await.map_err(reject)?;
        Ok(snapshot)
    }
}
