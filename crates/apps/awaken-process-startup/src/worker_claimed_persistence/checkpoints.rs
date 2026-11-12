// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The already-selected server port uses the existing claim and checkpoint/recovery kernels.
//! Existing claimed checkpoint/recovery verbs on one coherent transaction.
use super::{
    ClaimPurpose, LoadedRunRecovery, PostgresWorkerClaims, RunClaim, SessionWorkAuthorityReceipt,
    ThreadPrefixRef, internal, unavailable,
};
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_dispatch::SettleOutcome;
use awaken_dispatch_contract::ClaimedCheckpointTransport;
use awaken_runtime_contract::runs::RunError;

#[async_trait::async_trait]
impl ClaimedCheckpointTransport<RunError> for PostgresWorkerClaims {
    async fn load_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<StreamCheckpoint>, RunError> {
        let mut transaction = self.transaction(ClaimPurpose::Recovery).await?;
        if awaken_dispatch::lock_running_claim_for_commit(&mut transaction, claim)
            .await
            .map_err(internal)?
            .is_none()
        {
            transaction.rollback().await.map_err(internal)?;
            return Ok(None);
        }
        let observed = awaken_dispatch::PostgresStreamCheckpointStore::get_on(
            &mut transaction,
            &claim.run_id.0,
        )
        .await;
        transaction.rollback().await.map_err(internal)?;
        match observed {
            Observed::Present(checkpoint) => Ok(Some(checkpoint)),
            Observed::Absent => Ok(None),
            Observed::Unknown(cause) => Err(internal(cause)),
        }
    }

    async fn put_stream_checkpoint(
        &self,
        claim: &RunClaim,
        checkpoint: StreamCheckpoint,
    ) -> Result<SettleOutcome, RunError> {
        if checkpoint.run_id != claim.run_id.0 {
            return Err(RunError::bad_request(
                "checkpoint run id does not match claim",
            ));
        }
        let mut transaction = self.transaction(ClaimPurpose::Recovery).await?;
        if awaken_dispatch::lock_running_claim_for_commit(&mut transaction, claim)
            .await
            .map_err(internal)?
            .is_none()
        {
            transaction.rollback().await.map_err(internal)?;
            return Ok(SettleOutcome::Fenced);
        }
        awaken_dispatch::PostgresStreamCheckpointStore::put_on(&mut transaction, checkpoint)
            .await
            .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(SettleOutcome::Applied)
    }

    async fn delete_stream_checkpoint(&self, claim: &RunClaim) -> Result<SettleOutcome, RunError> {
        let mut transaction = self.transaction(ClaimPurpose::Recovery).await?;
        if awaken_dispatch::lock_running_claim_for_commit(&mut transaction, claim)
            .await
            .map_err(internal)?
            .is_none()
        {
            transaction.rollback().await.map_err(internal)?;
            return Ok(SettleOutcome::Fenced);
        }
        awaken_dispatch::PostgresStreamCheckpointStore::delete_on(
            &mut transaction,
            &claim.run_id.0,
        )
        .await
        .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(SettleOutcome::Applied)
    }

    async fn load_recovery(
        &self,
        claim: &RunClaim,
        work: Option<&SessionWorkAuthorityReceipt>,
        cached: Option<ThreadPrefixRef>,
    ) -> Result<LoadedRunRecovery, RunError> {
        let mut transaction = self.transaction(ClaimPurpose::Recovery).await?;
        let current = self
            .observe(&mut transaction, claim, ClaimPurpose::Recovery)
            .await?;
        match (current.request.requires_session_work_authority(), work) {
            (false, None) => {}
            (false, Some(_)) => {
                return Err(RunError::bad_request(
                    "ordinary recovery cannot present Session Work authority",
                ));
            }
            (true, None) => {
                return Err(RunError::bad_request(
                    "recovery requires exact Session Work authority",
                ));
            }
            (true, Some(receipt)) => {
                self.work(&mut transaction, &current.request, receipt, &claim.owner)
                    .await?
            }
        }
        let recovery = self
            .recovery_on(&mut transaction, &current.request, cached)
            .await?;
        transaction.commit().await.map_err(|error| {
            if work.is_some() {
                unavailable(error)
            } else {
                internal(error)
            }
        })?;
        Ok(recovery)
    }

    async fn load_recovery_snapshot(
        &self,
        claim: &RunClaim,
        work: Option<&SessionWorkAuthorityReceipt>,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, RunError> {
        match self.load_recovery(claim, work, None).await? {
            LoadedRunRecovery::Full { snapshot, .. } => Ok(*snapshot),
            LoadedRunRecovery::Reuse { .. } => Err(RunError::internal(
                "recovery attempted to reuse a prefix that was not supplied",
            )),
        }
    }
}
