// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The process-selected shared PG handle composes existing store primitives in
// @formal reason: one transaction. Claim/physical/Work decisions stay in their existing kernels.
//! Process-selected conditional Work release. No pool or durable state is created.
use awaken_dispatch::{
    ConditionalSessionWorkRelease, DispatchCommitAuthority, DispatchError, PhysicalAttempt,
    RunClaim,
};
use awaken_session_application_contract::{SessionWorkGrant, SessionWorkOwnership};
use std::sync::Arc;

/// Whether this exact source borrows the connection held by the Dispatch fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimConnectionUse {
    Borrowed,
    Independent,
}

/// Explicit source affinity selected by the store assembly. Independent means
/// neither source borrows the held Dispatch connection; partial sharing stays
/// explicit and never queries another database's tables on that connection.
#[derive(Clone, Default)]
pub enum SessionWorkReleasePersistence {
    #[default]
    Independent,
    Postgres {
        pool: sqlx::PgPool,
        roots: ClaimConnectionUse,
        work: ClaimConnectionUse,
    },
}

pub fn conditional_session_work_release(
    persistence: SessionWorkReleasePersistence,
    dispatch: Arc<dyn DispatchCommitAuthority>,
    work: Arc<awaken_session_application::SessionWorkAssignments>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Arc<dyn ConditionalSessionWorkRelease> {
    match persistence {
        SessionWorkReleasePersistence::Independent => Arc::new(
            awaken_dispatch::ClaimGuardedSessionWorkRelease::new(dispatch, work),
        ),
        SessionWorkReleasePersistence::Postgres {
            pool,
            roots,
            work: work_source,
        } => Arc::new(PostgresSessionWorkRelease {
            pool,
            clock,
            views: work.persistence_views(),
            roots,
            work: work_source,
        }),
    }
}

struct PostgresSessionWorkRelease {
    pool: sqlx::PgPool,
    clock: Arc<dyn awaken_time::WallClock>,
    views: awaken_session_application::SessionWorkPersistenceViews,
    roots: ClaimConnectionUse,
    work: ClaimConnectionUse,
}
impl PostgresSessionWorkRelease {
    async fn environment(
        &self,
        connection: &mut sqlx::PgConnection,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Option<(String, bool)>, DispatchError> {
        let root = match self.roots {
            ClaimConnectionUse::Borrowed => {
                awaken_session_store::postgres_session_root_on(connection, session_id).await
            }
            ClaimConnectionUse::Independent => self.views.roots.get(session_id).await,
        };
        match root {
            Ok(root) => Ok(awaken_session_application::session_work_environment(&root)),
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => Ok(None),
            Err(error) => Err(DispatchError::unavailable(error)),
        }
    }
}

#[async_trait::async_trait]
impl ConditionalSessionWorkRelease for PostgresSessionWorkRelease {
    async fn release(
        &self,
        claim: &RunClaim,
        observed: &PhysicalAttempt,
        model_only: bool,
    ) -> Result<bool, DispatchError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(DispatchError::unavailable)?;
        let result = async {
            let Some(current) =
                awaken_dispatch::lock_running_claim_for_commit(&mut tx, claim).await?
            else {
                return Ok(false);
            };
            if !awaken_dispatch::session_work_release_is_authorized(
                &current, claim, observed, model_only,
            ) {
                return Ok(false);
            }
            let session_id = awaken_session_types::SessionId::from(
                current.request.session_thread_id().0.clone(),
            );
            let Some((environment, terminal)) = self.environment(&mut tx, &session_id).await?
            else {
                return Ok(true);
            };
            let grant = match self.work {
                ClaimConnectionUse::Borrowed => awaken_work_store::renew_postgres_session_work_on(
                    &mut tx,
                    self.clock.as_ref(),
                    &environment,
                    &session_id,
                    &claim.owner,
                )
                .await
                .map_err(DispatchError::unavailable)?,
                ClaimConnectionUse::Independent => self
                    .views
                    .leases
                    .renew_session(&environment, &session_id, &claim.owner)
                    .await
                    .map_err(DispatchError::unavailable)?,
            };
            // This narrow operation is the existing RealizationRenewal, not
            // claimed-Run revival. The original kernel remains authoritative.
            if awaken_session_application_contract::session_work_revival_is_authorized(
                awaken_session_application_contract::SessionWorkAcquisition::RealizationRenewal,
                terminal,
                grant.is_some(),
            ) {
                return Err(DispatchError::Rejected(
                    "conditional release cannot revive stopped Session Work".into(),
                ));
            }
            let ownership = grant
                .map(SessionWorkGrant::from_transfer)
                .map(SessionWorkOwnership::Leased)
                .unwrap_or(SessionWorkOwnership::Unowned);
            if !ownership.permits_effect_for(&claim.owner) {
                return Ok(false);
            }
            let SessionWorkOwnership::Leased(grant) = ownership else {
                return Ok(false);
            };
            // Preserve the original release-side fresh topology observation.
            let Some((fresh_environment, _)) = self.environment(&mut tx, &session_id).await? else {
                return Ok(true);
            };
            if grant.environment_id() != fresh_environment {
                return Ok(false);
            }
            match self.work {
                ClaimConnectionUse::Borrowed => {
                    awaken_work_store::release_postgres_session_work_on(
                        &mut tx,
                        self.clock.as_ref(),
                        &grant.to_transfer(),
                    )
                    .await
                    .map_err(DispatchError::unavailable)
                }
                ClaimConnectionUse::Independent => self
                    .views
                    .leases
                    .release_session(&grant.to_transfer())
                    .await
                    .map_err(DispatchError::unavailable),
            }
        }
        .await;
        match result {
            Ok(true) => {
                tx.commit().await.map_err(DispatchError::unavailable)?;
                Ok(true)
            }
            Ok(false) => {
                tx.rollback().await.map_err(DispatchError::unavailable)?;
                Ok(false)
            }
            Err(error) => {
                tx.rollback().await.map_err(DispatchError::unavailable)?;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests;
