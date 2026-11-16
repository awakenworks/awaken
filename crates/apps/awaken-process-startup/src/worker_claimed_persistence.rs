// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Store assembly selects exact pool affinity once and composes original claim, Work, Root and recovery kernels.
//! The process-selected implementation of existing claimed Worker operations.
//! Cold application preparation never owns the Dispatch connection. Each final
//! Root mutation and every returned result re-enters this exact claim boundary.
use std::sync::Arc;

use awaken_agent_contract::thread::read::recovery::{LoadedRunRecovery, ThreadPrefixRef};
use awaken_agent_contract::{RunId, ThreadId};
use awaken_deployment_config::StoreKind;
use awaken_dispatch::{CommitEpochObservation, RunClaim, RunDispatch};
use awaken_dispatch_erasure::RuntimeAuthority;
use awaken_dispatch_http::WorkerClaimPersistence;
use awaken_runtime_contract::runs::RunError;
use awaken_session_application::{
    SessionAdmissionFailure, SessionApplication, SessionMutationError,
};
use awaken_session_application_contract::{
    SessionWorkAuthorityReceipt, SessionWorkGrant, SessionWorkOwnership,
};
use awaken_session_contract::SessionRepositoryError;
use awaken_session_types::SessionId;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{ClaimConnectionUse, SessionWorkReleasePersistence};

mod checkpoints;
mod claimed_preparation;
mod gate;
mod sessions;
#[cfg(test)]
pub(crate) mod test_cut;

/// Reuse the store affinity already selected for conditional Work operations.
/// PostgreSQL dispatch selects Postgres even when Root/Work are independent;
/// independent denotes a Dispatch backend that holds no PostgreSQL connection.
pub fn worker_claimed_persistence(
    persistence: SessionWorkReleasePersistence,
    committed_store: StoreKind,
    runtime: Arc<dyn RuntimeAuthority>,
    application: Arc<SessionApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> WorkerClaimPersistence {
    select_worker_claimed_persistence(
        persistence,
        committed_store,
        runtime,
        application,
        clock,
        #[cfg(test)]
        None,
    )
}

pub(crate) fn select_worker_claimed_persistence(
    persistence: SessionWorkReleasePersistence,
    committed_store: StoreKind,
    runtime: Arc<dyn RuntimeAuthority>,
    application: Arc<SessionApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
    #[cfg(test)] cut: Option<Arc<test_cut::FinalAdmissionTestCut>>,
) -> WorkerClaimPersistence {
    match persistence {
        SessionWorkReleasePersistence::Independent => WorkerClaimPersistence::Independent,
        SessionWorkReleasePersistence::Postgres { pool, roots, work } => {
            let selected = Arc::new(PostgresWorkerClaims {
                pool,
                roots,
                work,
                recovery_borrowed: committed_store == StoreKind::Postgres,
                runtime,
                views: application.work_assignments().persistence_views(),
                application,
                clock,
                #[cfg(test)]
                cut,
            });
            WorkerClaimPersistence::Postgres {
                checkpoints: selected.clone(),
                sessions: selected,
            }
        }
    }
}

#[derive(Clone)]
struct PostgresWorkerClaims {
    pool: PgPool,
    roots: ClaimConnectionUse,
    work: ClaimConnectionUse,
    recovery_borrowed: bool,
    runtime: Arc<dyn RuntimeAuthority>,
    application: Arc<SessionApplication>,
    views: awaken_session_application::SessionWorkPersistenceViews,
    clock: Arc<dyn awaken_time::WallClock>,
    #[cfg(test)]
    cut: Option<Arc<test_cut::FinalAdmissionTestCut>>,
}

#[derive(Clone, Copy)]
enum ClaimPurpose {
    Recovery,
    Resume,
    ResumeBegin,
    Model,
    Activity(awaken_session_execution_contract::SessionRunActivityAdmissionMode),
}
impl ClaimPurpose {
    fn needs_live(self) -> bool {
        matches!(
            self,
            Self::Resume | Self::ResumeBegin | Self::Model | Self::Activity(_)
        )
    }
}

#[derive(Clone)]
enum WorkAdmission {
    NotRequiredByOperation,
    Asserted(SessionWorkAuthorityReceipt),
}

#[derive(Clone)]
struct CommittedReadObservation {
    thread: ThreadId,
    run: RunId,
    prefix: ThreadPrefixRef,
}

struct RootGate {
    selected: Arc<PostgresWorkerClaims>,
    claim: RunClaim,
    original: RunDispatch,
    purpose: ClaimPurpose,
    work: WorkAdmission,
    committed: Option<CommittedReadObservation>,
}

fn unavailable(error: impl std::fmt::Display) -> RunError {
    RunError::unavailable(error.to_string())
}
fn mutation(error: SessionRepositoryError) -> SessionMutationError {
    error.into()
}
fn internal(error: impl std::fmt::Display) -> RunError {
    RunError::internal(error.to_string())
}

impl PostgresWorkerClaims {
    async fn transaction(
        &self,
        purpose: ClaimPurpose,
    ) -> Result<Transaction<'_, Postgres>, RunError> {
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        // Resume is the original claim/Work/Root writer: Read Committed lets
        // distinct child claims serialize renewal of their shared parent Work.
        // Recovery and Model/Activity prefix witnesses retain one Repeatable
        // Read snapshot, established before the first claim FOR UPDATE.
        if !matches!(purpose, ClaimPurpose::Resume | ClaimPurpose::ResumeBegin) {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
        }
        Ok(transaction)
    }

    async fn observe(
        &self,
        connection: &mut PgConnection,
        claim: &RunClaim,
        purpose: ClaimPurpose,
    ) -> Result<CommitEpochObservation, SessionAdmissionFailure> {
        let observation = match purpose {
            ClaimPurpose::Activity(_) => {
                awaken_dispatch::lock_session_run_reservation_for_admission(connection, claim).await
            }
            _ => awaken_dispatch::lock_running_claim_for_commit(connection, claim).await,
        }
        .map_err(internal)?
        .ok_or_else(|| match purpose {
            // Begin follows successful input/Work admission. Losing that exact
            // authority during cold preparation is not an invalid initial input.
            ClaimPurpose::ResumeBegin => SessionAdmissionFailure::Control(
                awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership,
            ),
            ClaimPurpose::Recovery => RunError::conflict("run claim is stale").into(),
            _ => RunError::bad_request("Session claim is stale").into(),
        })?;
        if purpose.needs_live()
            && !awaken_time::Expiry::from_epoch_ms(observation.expires_ms)
                .is_live_at(self.clock.now())
        {
            return Err(if matches!(purpose, ClaimPurpose::ResumeBegin) {
                SessionAdmissionFailure::Control(
                    awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership,
                )
            } else {
                RunError::bad_request("Session claim lease has expired").into()
            });
        }
        Ok(observation)
    }

    async fn root(
        &self,
        connection: &mut PgConnection,
        session: &SessionId,
    ) -> Result<awaken_session_contract::PersistedSession, SessionRepositoryError> {
        match self.roots {
            ClaimConnectionUse::Borrowed => {
                awaken_session_store::postgres_session_root_on(connection, session).await
            }
            ClaimConnectionUse::Independent => self.views.roots.get(session).await,
        }
    }

    async fn work(
        &self,
        connection: &mut PgConnection,
        dispatch: &RunDispatch,
        asserted: &SessionWorkAuthorityReceipt,
        owner: &str,
    ) -> Result<(), RunError> {
        let session = SessionId::from(dispatch.session_thread_id().0.clone());
        if asserted.session_id() != session || asserted.worker_owner() != owner {
            return Err(RunError::bad_request(
                "Session Work authority identity does not match the claim",
            ));
        }
        let root = match self.root(connection, &session).await {
            Ok(root) => Some(root),
            Err(SessionRepositoryError::NotFound) => None,
            Err(error) => return Err(unavailable(error)),
        };
        let observed = if let Some((environment, _)) = root
            .as_ref()
            .and_then(awaken_session_application::session_work_environment)
        {
            let lease = asserted
                .lease_for_renewal()
                .filter(|lease| lease.environment_id() == environment)
                .ok_or_else(|| RunError::bad_request("Session Work authority is stale"))?;
            let transfer = match self.work {
                ClaimConnectionUse::Borrowed => {
                    awaken_work_store::renew_exact_postgres_session_work_on(
                        connection,
                        self.clock.as_ref(),
                        &lease,
                    )
                    .await
                    .map_err(unavailable)?
                }
                ClaimConnectionUse::Independent => {
                    self.views.leases.renew_exact_session(&lease).await?
                }
            };
            transfer
                .map(SessionWorkGrant::from_transfer)
                .map(SessionWorkOwnership::Leased)
                .unwrap_or(SessionWorkOwnership::Unowned)
        } else {
            SessionWorkOwnership::NotRequired
        };
        if !asserted.matches_observation(&observed) {
            return Err(RunError::bad_request("Session Work authority is stale"));
        }
        Ok(())
    }

    async fn recovery_on(
        &self,
        connection: &mut PgConnection,
        dispatch: &RunDispatch,
        cached: Option<ThreadPrefixRef>,
    ) -> Result<LoadedRunRecovery, RunError> {
        if self.recovery_borrowed {
            awaken_store_postgres::load_postgres_recovery_on(
                connection,
                dispatch.thread_id(),
                dispatch.run_id(),
                cached,
            )
            .await
            .map_err(internal)
        } else {
            self.runtime
                .open_commit(&dispatch.session_thread_id().0)
                .await
                .map_err(internal)?
                .load_recovery(
                    dispatch.session_thread_id(),
                    dispatch.thread_id(),
                    dispatch.run_id(),
                    cached,
                )
                .await
                .map_err(internal)
        }
    }

    async fn validate_activity(
        &self,
        gate: &RootGate,
        session: &SessionId,
        agent: &str,
        run: &RunId,
        mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<(), RunError> {
        let mut transaction = self.transaction(gate.purpose).await?;
        let current = self
            .observe(&mut transaction, &gate.claim, gate.purpose)
            .await?;
        if !current.request.same_original_dispatch(&gate.original) {
            return Err(RunError::bad_request(
                "prepared reservation changed during preparation",
            ));
        }
        awaken_dispatch_http::require_worker_run_activity(
            &current,
            &gate.claim,
            session,
            agent,
            run,
            mode,
            awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
        )?;
        transaction.rollback().await.map_err(internal)
    }

    async fn freeze_committed_read(&self, gate: &mut RootGate) -> Result<(), RunError> {
        let mut transaction = self.transaction(gate.purpose).await?;
        gate.check(&mut transaction).await?;
        match self
            .recovery_on(&mut transaction, &gate.original, None)
            .await?
        {
            LoadedRunRecovery::Full {
                prefix: Some(prefix),
                ..
            } => {
                gate.committed = Some(CommittedReadObservation {
                    thread: gate.original.thread_id().clone(),
                    run: gate.original.run_id().clone(),
                    prefix,
                });
            }
            _ => {
                return Err(RunError::internal(
                    "committed admission read did not issue its original prefix",
                ));
            }
        }
        transaction.rollback().await.map_err(internal)
    }

    async fn freeze_prepared_activity(&self, gate: &mut RootGate) -> Result<u64, RunError> {
        let mut transaction = self.transaction(gate.purpose).await?;
        gate.check(&mut transaction).await?;
        let cursor = match self
            .recovery_on(&mut transaction, &gate.original, None)
            .await?
        {
            LoadedRunRecovery::Full {
                prefix: Some(prefix),
                snapshot,
            } => {
                let cursor = awaken_dispatch_contract::prepared_session_run_admission_cursor(
                    &gate.original,
                    &snapshot,
                )
                .map_err(RunError::internal)?;
                gate.committed = Some(CommittedReadObservation {
                    thread: gate.original.thread_id().clone(),
                    run: gate.original.run_id().clone(),
                    prefix,
                });
                cursor
            }
            _ => {
                return Err(RunError::internal(
                    "prepared admission read did not issue its original prefix",
                ));
            }
        };
        transaction.rollback().await.map_err(internal)?;
        Ok(cursor)
    }

    async fn context(
        &self,
        claim: &RunClaim,
        purpose: ClaimPurpose,
        work: WorkAdmission,
    ) -> Result<RootGate, RunError> {
        let mut transaction = self.transaction(purpose).await?;
        let observation = self.observe(&mut transaction, claim, purpose).await?;
        if let WorkAdmission::Asserted(receipt) = &work {
            self.work(
                &mut transaction,
                &observation.request,
                receipt,
                &claim.owner,
            )
            .await?;
        }
        let original = observation.request;
        transaction.commit().await.map_err(unavailable)?;
        Ok(RootGate {
            selected: Arc::new(self.clone()),
            claim: claim.clone(),
            original,
            purpose,
            work,
            committed: None,
        })
    }
}
