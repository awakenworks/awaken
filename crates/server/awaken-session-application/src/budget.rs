//! Session-root list-cost admission and cumulative usage settlement.

use awaken_session_contract::{
    ManagedBudgetUsageCursor, ManagedLifecycleFact, ManagedModelUsageCursor, PersistedSession,
    SessionBudgetState, SessionUsage,
};

use super::{SessionApplication, SessionMutationError, mutation::repository_failure};

#[derive(Debug, Clone)]
pub struct BudgetSettlementOutcome {
    pub session: PersistedSession,
    pub reached_now: bool,
}

impl SessionApplication {
    pub async fn reconcile_managed_budget_usage(
        &self,
        session_id: &str,
        usage: SessionUsage,
    ) -> Result<BudgetSettlementOutcome, SessionMutationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await?;
            let mut session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)?;
            if matches!(session.budget, SessionBudgetState::Absent) {
                return Ok(BudgetSettlementOutcome {
                    session,
                    reached_now: false,
                });
            }
            let fallback_model = session
                .frozen_baseline()
                .map(|baseline| baseline.execution_model_ref.clone())
                .ok_or_else(|| {
                    SessionMutationError::Unavailable(
                        "Session baseline is not frozen while settling budget".into(),
                    )
                })?;
            let was_admissible = session.budget.can_admit_model_request();
            let mut by_model = usage
                .by_model
                .iter()
                .map(|(model, usage)| {
                    (
                        model.clone(),
                        ManagedModelUsageCursor {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_read_tokens: usage.cache_read_tokens,
                            cache_creation_tokens: usage.cache_creation_tokens,
                        },
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            if by_model.is_empty()
                && (usage.input_tokens != 0
                    || usage.output_tokens != 0
                    || usage.cache_read_tokens != 0
                    || usage.cache_creation_tokens != 0)
            {
                by_model.insert(
                    fallback_model,
                    ManagedModelUsageCursor {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                    },
                );
            }
            let active_seconds = session
                .budget
                .usage_cursor()
                .map_or(usage.active_seconds, |cursor| {
                    cursor.active_seconds.max(usage.active_seconds)
                });
            session
                .budget
                .reconcile_cumulative_usage(ManagedBudgetUsageCursor {
                    by_model,
                    active_seconds,
                    web_fetch_requests: usage.web_fetch_requests,
                    web_search_requests: usage.web_search_requests,
                })
                .map_err(|error| SessionMutationError::Unavailable(error.to_string()))?;
            let reached_now = was_admissible && !session.budget.can_admit_model_request();
            let lifecycle_facts = if reached_now {
                if let SessionBudgetState::Active {
                    reached_event_emitted,
                    ..
                } = &mut session.budget
                {
                    *reached_event_emitted = true;
                }
                vec![ManagedLifecycleFact {
                    id: format!("budget-reached:{session_id}:{}", session.revision.0),
                    object_id: session_id.to_owned(),
                    workspace_id: Some(owner_scope.clone()),
                    event_type: "session.budget_reached".into(),
                    timestamp: i64::try_from(super::activity::now_unix_ms() / 1_000)
                        .unwrap_or(i64::MAX),
                    runtime_interval: None,
                }]
            } else {
                Vec::new()
            };
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "reconcile-managed-budget-usage",
                    lifecycle_facts,
                )
                .await
            {
                Ok(session) => {
                    if reached_now {
                        self.notify_lifecycle_fact();
                    }
                    return Ok(BudgetSettlementOutcome {
                        session,
                        reached_now,
                    });
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
                Err(error) => return Err(error),
            }
        }
        Err(SessionMutationError::Conflict)
    }
}
