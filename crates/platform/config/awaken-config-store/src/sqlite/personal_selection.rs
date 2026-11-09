// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This native preference contract preserves the exact owner tuple,
// @formal reason: generation and immutable original receipt in the existing configuration
// @formal reason: store. It neither authorizes Agent use nor changes definitions. Shared
// @formal reason: SQLite/PostgreSQL conformance covers its transaction boundary; this
// @formal reason: classification is not a formal-proof claim.
//! Native preference transaction; this adapter cannot grant Agent use.
use super::{SqliteConfigStore, reject};
use crate::personal_selection as codec;
use awaken_agent_config::personal_selection::{
    AgentSelectionDecision, AgentSelectionError, AgentSelectionIntent, AgentSelectionOwner,
    AgentSelectionReceipt, AgentSelectionState, PersonalAgentSelectionRepository,
    decide_agent_selection,
};
use awaken_agent_config::{ConfigStoreError, ScopeId};
use sqlx::Row;

#[async_trait::async_trait]
impl PersonalAgentSelectionRepository for SqliteConfigStore {
    async fn personal_agent_selection(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
    ) -> Result<Option<AgentSelectionState>, ConfigStoreError> {
        codec::target(scope, agent)?;
        let row = sqlx::query("SELECT generation, selected FROM config_personal_agent_selection WHERE scope_id = ?1 AND issuer = ?2 AND account_id = ?3 AND agent_id = ?4")
        .bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(agent).fetch_optional(&self.pool).await.map_err(reject)?;
        row.map(|row| {
            codec::state(
                row.try_get("generation").map_err(reject)?,
                row.try_get("selected").map_err(reject)?,
            )
        })
        .transpose()
    }

    async fn personal_agent_selection_outcome(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
        key: &str,
    ) -> Result<Option<AgentSelectionReceipt>, ConfigStoreError> {
        codec::target(scope, agent)?;
        let record: Option<String> = sqlx::query_scalar("SELECT record FROM config_personal_agent_selection_receipt WHERE scope_id = ?1 AND issuer = ?2 AND account_id = ?3 AND agent_id = ?4 AND request_key = ?5")
        .bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(agent).bind(key).fetch_optional(&self.pool).await.map_err(reject)?;
        record
            .map(|record| codec::receipt(&record, key))
            .transpose()
    }

    async fn set_personal_agent_selection(
        &self,
        scope: &ScopeId,
        owner: &AgentSelectionOwner,
        agent: &str,
        intent: &AgentSelectionIntent,
    ) -> Result<Result<AgentSelectionReceipt, AgentSelectionError>, ConfigStoreError> {
        codec::target(scope, agent)?;
        if let Err(error) = intent.validate() {
            return Ok(Err(error));
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(reject)?;
        let prior: Option<String> = sqlx::query_scalar("SELECT record FROM config_personal_agent_selection_receipt WHERE scope_id = ?1 AND issuer = ?2 AND account_id = ?3 AND agent_id = ?4 AND request_key = ?5")
        .bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(agent).bind(&intent.request_key).fetch_optional(&mut *tx).await.map_err(reject)?;
        let prior = prior
            .map(|record| codec::receipt(&record, &intent.request_key))
            .transpose()?;
        let row = sqlx::query("SELECT generation, selected FROM config_personal_agent_selection WHERE scope_id = ?1 AND issuer = ?2 AND account_id = ?3 AND agent_id = ?4")
        .bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(agent).fetch_optional(&mut *tx).await.map_err(reject)?;
        let current = row
            .map(|row| {
                codec::state(
                    row.try_get("generation").map_err(reject)?,
                    row.try_get("selected").map_err(reject)?,
                )
            })
            .transpose()?;
        let decision = match decide_agent_selection(current, prior.as_ref(), intent) {
            Ok(decision) => decision,
            Err(error) => return Ok(Err(error)),
        };
        let receipt = match decision {
            AgentSelectionDecision::Replay(receipt) => receipt,
            AgentSelectionDecision::Apply { state, receipt } => {
                let generation = i64::try_from(state.generation).map_err(reject)?;
                let expected = i64::try_from(intent.expected_generation).map_err(reject)?;
                let changed = sqlx::query("INSERT INTO config_personal_agent_selection (scope_id, issuer, account_id, agent_id, generation, selected) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (scope_id, issuer, account_id, agent_id) DO UPDATE SET generation = excluded.generation, selected = excluded.selected WHERE config_personal_agent_selection.generation = ?7")
                .bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(agent).bind(generation).bind(i64::from(state.selected)).bind(expected)
                .execute(&mut *tx).await.map_err(reject)?.rows_affected();
                if changed != 1 {
                    return Err(ConfigStoreError(
                        "personal Agent selection lost its CAS writer".into(),
                    ));
                }
                sqlx::query("INSERT INTO config_personal_agent_selection_receipt (scope_id, issuer, account_id, agent_id, request_key, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
                .bind(&scope.0).bind(owner.issuer()).bind(owner.account()).bind(agent).bind(&intent.request_key).bind(serde_json::to_string(&receipt).map_err(reject)?)
                .execute(&mut *tx).await.map_err(reject)?;
                receipt
            }
        };
        tx.commit().await.map_err(reject)?;
        Ok(Ok(receipt))
    }
}
