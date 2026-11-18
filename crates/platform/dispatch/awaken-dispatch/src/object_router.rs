// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Routes Dispatch operations to the owning tenant and Session authority.
//! Internal Session routing over the selected object engine.
//! Only per-Session commands publish truth. Discovery never authorizes a claim.
use super::ObjectDispatchPartition;
use crate::memory::ResumeResult;
use awaken_agent_contract::{
    agent::{run::Id as RunId, thread::Id as ThreadId},
    credential::CandidateFingerprint,
};
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_credential_contract::*;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_dispatch_contract::*;
use awaken_object_storage::{SessionObject, SessionObjectStore};
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_worker_contract::*;
use std::sync::Arc;

mod discovery;
use discovery::{Discovery, SearchKind};

pub struct ObjectDispatchStore {
    objects: Arc<dyn SessionObjectStore>,
    default_scope: String,
    claim_index: tokio::sync::OnceCell<()>,
    discovery: Discovery,
    clock: Arc<dyn awaken_time::WallClock>,
}
impl ObjectDispatchStore {
    pub fn new(
        objects: Arc<dyn SessionObjectStore>,
        default_scope: String,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            objects,
            default_scope,
            claim_index: tokio::sync::OnceCell::new(),
            discovery: Discovery::default(),
            clock,
        }
    }
    async fn ensure_claim_index(&self) -> Result<(), DispatchError> {
        self.claim_index
            .get_or_try_init(|| async {
                if !self
                    .objects
                    .claim_index_initialized()
                    .await
                    .map_err(DispatchError::unavailable)?
                {
                    for object in self
                        .objects
                        .list()
                        .await
                        .map_err(DispatchError::unavailable)?
                    {
                        self.partition(&object).backfill_claim_candidate().await?;
                    }
                    self.objects
                        .finish_claim_index_backfill()
                        .await
                        .map_err(DispatchError::unavailable)?;
                }
                Ok::<_, DispatchError>(())
            })
            .await
            .map(|_| ())
    }

    fn partition(&self, object: &SessionObject) -> ObjectDispatchPartition {
        ObjectDispatchPartition::new(
            self.objects.clone(),
            object.owner_scope.clone(),
            object.session_id.clone(),
            self.clock.clone(),
        )
    }
    async fn for_session(&self, session: &str) -> Result<ObjectDispatchPartition, DispatchError> {
        Ok(
            match self
                .objects
                .load(session)
                .await
                .map_err(DispatchError::unavailable)?
            {
                Some(object) => self.partition(&object),
                None => ObjectDispatchPartition::new(
                    self.objects.clone(),
                    self.default_scope.clone(),
                    session.into(),
                    self.clock.clone(),
                ),
            },
        )
    }
    async fn for_identity(
        &self,
        id: &str,
        fallback: &str,
    ) -> Result<ObjectDispatchPartition, DispatchError> {
        match self
            .objects
            .locate_record(id)
            .await
            .map_err(DispatchError::unavailable)?
        {
            Some(object) => Ok(self.partition(&object)),
            None => self.for_session(fallback).await,
        }
    }
    async fn for_request(
        &self,
        request: &RunDispatch,
    ) -> Result<ObjectDispatchPartition, DispatchError> {
        let stated = request
            .execution_scope
            .as_ref()
            .map(|scope| scope.0.0.as_str());
        if let Some(object) = self
            .objects
            .locate_record(&format!("run:{}", request.run_id().0))
            .await
            .map_err(DispatchError::unavailable)?
        {
            if stated.is_some_and(|scope| scope != object.owner_scope)
                || object.session_id != request.session_thread_id().0
            {
                return Err(DispatchError::Rejected(
                    "Run identity belongs to another Session or scope".into(),
                ));
            }
            return Ok(self.partition(&object));
        }
        // Legacy envelopes may omit scope; an existing Session identity supplies
        // its original owner. A caller-supplied scope never rebinds that owner.
        if let Some(object) = self
            .objects
            .load(&request.session_thread_id().0)
            .await
            .map_err(DispatchError::unavailable)?
        {
            if stated.is_some_and(|scope| scope != object.owner_scope) {
                return Err(DispatchError::Rejected(
                    "Session owner scope mismatch".into(),
                ));
            }
            return Ok(self.partition(&object));
        }
        Ok(ObjectDispatchPartition::new(
            self.objects.clone(),
            stated.unwrap_or(&self.default_scope).into(),
            request.session_thread_id().0.clone(),
            self.clock.clone(),
        ))
    }
    async fn for_input(
        &self,
        input: &PendingInput,
    ) -> Result<ObjectDispatchPartition, DispatchError> {
        if let Some(object) = self
            .objects
            .locate_record(&format!("input:{}", input.message_id))
            .await
            .map_err(DispatchError::unavailable)?
        {
            return Ok(self.partition(&object));
        }
        self.for_identity(&format!("thread:{}", input.thread_id.0), &input.thread_id.0)
            .await
    }
    async fn partitions(&self) -> Result<Vec<ObjectDispatchPartition>, DispatchError> {
        let mut objects = self
            .objects
            .list()
            .await
            .map_err(DispatchError::unavailable)?;
        objects
            .sort_by(|a, b| (&a.owner_scope, &a.session_id).cmp(&(&b.owner_scope, &b.session_id)));
        Ok(objects
            .iter()
            .map(|object| self.partition(object))
            .collect())
    }
    async fn single_partition(&self) -> Result<ObjectDispatchPartition, DispatchError> {
        let mut partitions = self.partitions().await?;
        match partitions.len() {
            0 => self.for_session("_empty").await,
            1 => Ok(partitions.remove(0)),
            _ => Err(DispatchError::Rejected(
                "a numeric cursor names one Session partition; use the partition-aware scan API"
                    .into(),
            )),
        }
    }
}
#[async_trait::async_trait]
impl DispatchEnqueue for ObjectDispatchStore {
    async fn enqueue_with(
        &self,
        request: RunDispatch,
        options: SubmitOptions,
    ) -> Result<(), DispatchError> {
        let partition = self.for_request(&request).await?;
        partition.enqueue_with(request, options).await
    }
}

#[async_trait::async_trait]
impl DispatchClaims for ObjectDispatchStore {
    async fn resolve_claimed_session_run_reservation(
        &self,
        claim: &RunClaim,
        resolution: SessionRunReservationResolution,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition
            .resolve_claimed_session_run_reservation(claim, resolution)
            .await
    }
    async fn claim_new_run(
        &self,
        request: RunDispatch,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self.for_request(&request).await?;
        partition
            .claim_new_run(request, owner, lease_ms, capabilities)
            .await
    }
    async fn claim_new_run_compatible(
        &self,
        request: RunDispatch,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self.for_request(&request).await?;
        partition
            .claim_new_run_compatible(request, worker, lease_ms)
            .await
    }
    async fn deliver_and_claim(
        &self,
        input: PendingInput,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self.for_input(&input).await?;
        partition
            .deliver_and_claim(input, owner, lease_ms, capabilities)
            .await
    }
    async fn deliver_and_claim_compatible(
        &self,
        input: PendingInput,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self.for_input(&input).await?;
        partition
            .deliver_and_claim_compatible(input, worker, lease_ms)
            .await
    }
    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let discovery = self.discovery_in(owner, SearchKind::Local, None).await?;
        let Some(selected) = discovery.claim(owner, lease_ms, capabilities).await? else {
            return Ok(None);
        };
        let partition = self
            .for_identity(
                &format!("run:{}", selected.request.run_id().0),
                &selected.request.run_id().0,
            )
            .await?;
        partition.claim(owner, lease_ms, capabilities).await
    }
    async fn claim_compatible(
        &self,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let discovery = self
            .discovery_in(&worker.identity.lease_owner(), SearchKind::Compatible, None)
            .await?;
        let Some(selected) = discovery.claim_compatible(worker, lease_ms).await? else {
            return Ok(None);
        };
        let partition = self
            .for_identity(
                &format!("run:{}", selected.request.run_id().0),
                &selected.request.run_id().0,
            )
            .await?;
        partition.claim_compatible(worker, lease_ms).await
    }
    async fn claim_run(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition
            .claim_run(run_id, owner, lease_ms, capabilities)
            .await
    }
    async fn claim_retry_exhausted(
        &self,
        owner: &str,
        lease_ms: u64,
        max_attempts: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let discovery = self
            .discovery_in(owner, SearchKind::RetryExhausted, None)
            .await?;
        let Some(selected) = discovery
            .claim_retry_exhausted(owner, lease_ms, max_attempts)
            .await?
        else {
            return Ok(None);
        };
        let partition = self
            .for_identity(
                &format!("run:{}", selected.request.run_id().0),
                &selected.request.run_id().0,
            )
            .await?;
        partition
            .claim_retry_exhausted(owner, lease_ms, max_attempts)
            .await
    }
    async fn claim_run_compatible(
        &self,
        run_id: &RunId,
        worker: &WorkerSnapshot,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition
            .claim_run_compatible(run_id, worker, lease_ms)
            .await
    }
}

#[async_trait::async_trait]
impl DispatchExecution for ObjectDispatchStore {
    async fn close_attempt(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.close_attempt(claim).await
    }
    async fn renew_lease(
        &self,
        claim: &RunClaim,
        lease_ms: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.renew_lease(claim, lease_ms).await
    }
    async fn begin_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.begin_attempt(claim, _binding).await
    }
    async fn handoff_attempt(
        &self,
        claim: &RunClaim,
        _binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.handoff_attempt(claim, _binding).await
    }
    async fn finish_attempt(
        &self,
        claim: &RunClaim,
        cause: Option<awaken_dispatch_contract::AttemptReturnCause>,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.finish_attempt(claim, cause).await
    }
    async fn relinquish_claim(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.relinquish_claim(claim).await
    }
    async fn defer_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.defer_claim(claim, retry_after_ms).await
    }
    async fn retry_claim(
        &self,
        claim: &RunClaim,
        retry_after_ms: u64,
        max_attempts: u64,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition
            .retry_claim(claim, retry_after_ms, max_attempts)
            .await
    }
    async fn record_credential_realization(
        &self,
        claim: &RunClaim,
        receipt: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition
            .record_credential_realization(claim, receipt)
            .await
    }
    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        sandbox_ref: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.bind_sandbox(claim, sandbox_ref).await
    }
}

#[async_trait::async_trait]
impl DispatchClaimOwnership for ObjectDispatchStore {
    async fn claim_is_current(&self, claim: &RunClaim) -> Result<bool, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.claim_is_current(claim).await
    }
}

#[async_trait::async_trait]
impl DispatchSettlement for ObjectDispatchStore {
    async fn settle(
        &self,
        run_id: &RunId,
        generation: u64,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<SettleOutcome, DispatchError> {
        let Ok(epoch) = awaken_lease_contract::fence::Epoch::try_from(generation) else {
            return Ok(SettleOutcome::Fenced);
        };
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition
            .settle(run_id, epoch.get(), outcome, consumed)
            .await
    }
}

#[async_trait::async_trait]
impl InboxRead for ObjectDispatchStore {
    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let partition = self
            .for_identity(&format!("thread:{}", thread_id.0), &thread_id.0)
            .await?;
        partition.list(thread_id).await
    }
}

#[async_trait::async_trait]
impl DispatchAdministration for ObjectDispatchStore {
    async fn cancel(&self, run_id: &RunId) -> Result<Option<ThreadId>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.cancel(run_id).await
    }
    async fn requeue(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.requeue(run_id).await
    }
}

#[async_trait::async_trait]
impl DispatchReads for ObjectDispatchStore {
    async fn dead_letters(&self) -> Result<Vec<RunId>, DispatchError> {
        let mut result = Vec::new();
        for partition in self.partitions().await? {
            result.extend(partition.dead_letters().await?);
        }
        Ok(result)
    }
    async fn superseded(&self) -> Result<Vec<RunId>, DispatchError> {
        let mut result = Vec::new();
        for partition in self.partitions().await? {
            result.extend(partition.superseded().await?);
        }
        Ok(result)
    }
    async fn list_dispatches(&self) -> Result<Vec<DispatchSummary>, DispatchError> {
        let mut result = Vec::new();
        for partition in self.partitions().await? {
            result.extend(partition.list_dispatches().await?);
        }
        Ok(result)
    }
}

#[async_trait::async_trait]
impl DispatchMaintenance for ObjectDispatchStore {
    async fn quarantine_retry_exhausted(
        &self,
        max_attempts: u64,
        expired_before_ms: u64,
    ) -> Result<usize, DispatchError> {
        let mut count = 0usize;
        for partition in self.partitions().await? {
            count = count
                .checked_add(
                    partition
                        .quarantine_retry_exhausted(max_attempts, expired_before_ms)
                        .await?,
                )
                .ok_or_else(|| DispatchError::unavailable("aggregate count overflow"))?;
        }
        Ok(count)
    }
    async fn purge_dead_letters(&self) -> Result<usize, DispatchError> {
        let mut count = 0usize;
        for partition in self.partitions().await? {
            count = count
                .checked_add(partition.purge_dead_letters().await?)
                .ok_or_else(|| DispatchError::unavailable("aggregate count overflow"))?;
        }
        Ok(count)
    }
    async fn purge_dead_letters_before(&self, cutoff_ms: u64) -> Result<usize, DispatchError> {
        let mut count = 0usize;
        for partition in self.partitions().await? {
            count = count
                .checked_add(partition.purge_dead_letters_before(cutoff_ms).await?)
                .ok_or_else(|| DispatchError::unavailable("aggregate count overflow"))?;
        }
        Ok(count)
    }
}

#[async_trait::async_trait]
impl DispatchAttemptControl for ObjectDispatchStore {
    async fn finish_attempt_closure(
        &self,
        record: &awaken_dispatch_contract::AttemptClosureRecord,
    ) -> Result<SettleOutcome, DispatchError> {
        let partition = self
            .for_identity(
                &format!("run:{}", record.attempt().claim.run_id.0),
                &record.attempt().claim.run_id.0,
            )
            .await?;
        partition.finish_attempt_closure(record).await
    }
    async fn admit_attempt_guarded(
        &self,
        claim: &RunClaim,
        binding: Option<&awaken_dispatch_contract::dispatch::AttemptExecutionBinding>,
        guard: awaken_worker_contract::WorkerIncarnationGuard,
        handoff: bool,
    ) -> Result<awaken_dispatch_contract::AttemptAdmission, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition
            .admit_attempt_guarded(claim, binding, guard, handoff)
            .await
    }
}

#[async_trait::async_trait]
impl DispatchControlClaims for ObjectDispatchStore {
    async fn search_claim_request(
        &self,
        request: &ClaimRequest,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
        search: ClaimSearch,
    ) -> Result<ClaimRequestOutcome, DispatchError> {
        use crate::claim_request::ClaimReceiptRequest;
        use awaken_time::WallClockExt as _;

        let receipt = ClaimReceiptRequest::new(request)?;
        if !receipt.live_at(self.clock.now_ms())? {
            return Ok(ClaimRequestOutcome::Empty);
        }
        let expiry = u64::try_from(receipt.expires_at_ms).map_err(DispatchError::unavailable)?;
        let identity = format!(
            "claim-request:{}:{}",
            request.request_id,
            requester.identity.lease_owner()
        );
        let existing = self
            .objects
            .claim_request_partition(&identity, expiry, None)
            .await
            .map_err(DispatchError::unavailable)?;
        let shard = match search {
            ClaimSearch::Replay => {
                let Some(object) = existing else {
                    return Ok(ClaimRequestOutcome::Empty);
                };
                let state = super::memory::MemoryDispatchStore::from_snapshot(
                    object.payload.as_deref(),
                    self.clock.clone(),
                )?;
                return Ok(state
                    .requested_claim(&identity, request)?
                    .flatten()
                    .map_or(ClaimRequestOutcome::Empty, |claim| {
                        ClaimRequestOutcome::Claimed(Box::new(claim))
                    }));
            }
            ClaimSearch::Any => None,
            ClaimSearch::Shard(shard) => Some(shard),
        };
        let object = if let Some(existing) = existing {
            existing
        } else {
            let run = match &request.run_id {
                Some(run) => Some(run.clone()),
                None => self
                    .discovery_in(
                        &requester.identity.lease_owner(),
                        SearchKind::Requested,
                        shard,
                    )
                    .await?
                    .claim_placed(requester, workers.clone(), policy.clone(), lease_ms)
                    .await?
                    .map(|candidate| candidate.request.run_id().clone()),
            };
            let proposed = if let Some(run) = run {
                self.objects
                    .locate_record(&format!("run:{}", run.0))
                    .await
                    .map_err(DispatchError::unavailable)?
            } else {
                None
            };
            // Discovery can observe an empty queue after the concurrent
            // original request has already claimed it. Read the reserved
            // route again even when this discovery offers no candidate.
            let Some(bound) = self
                .objects
                .claim_request_partition(&identity, expiry, proposed.as_ref())
                .await
                .map_err(DispatchError::unavailable)?
            else {
                return Ok(ClaimRequestOutcome::Empty);
            };
            bound
        };
        if let Some(run) = &request.run_id
            && let Some(target) = self
                .objects
                .locate_record(&format!("run:{}", run.0))
                .await
                .map_err(DispatchError::unavailable)?
            && (target.session_id != object.session_id || target.owner_scope != object.owner_scope)
        {
            return Err(DispatchError::Conflict(
                "claim request already routes to different work".into(),
            ));
        }
        Ok(self
            .partition(&object)
            .claim_requested(identity, receipt, requester, workers, policy, lease_ms)
            .await?
            .map_or(ClaimRequestOutcome::Empty, |claim| {
                ClaimRequestOutcome::Claimed(Box::new(claim))
            }))
    }

    async fn claimable_shards(&self) -> Result<Option<Vec<u8>>, DispatchError> {
        use awaken_time::WallClockExt as _;
        self.ensure_claim_index().await?;
        self.objects
            .claimable_shards(self.clock.now_ms())
            .await
            .map_err(DispatchError::unavailable)
    }

    async fn dispatch_shard_for_run(&self, run: &RunId) -> Result<Option<u8>, DispatchError> {
        Ok(self
            .objects
            .locate_record(&format!("run:{}", run.0))
            .await
            .map_err(DispatchError::unavailable)?
            .map(|object| dispatch_shard(&object.owner_scope, &object.session_id)))
    }

    async fn claim_placed(
        &self,
        requester: &WorkerSnapshot,
        workers: Vec<WorkerSnapshot>,
        policy: Arc<dyn PlacementPolicy>,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let discovery = self
            .discovery_in(&requester.identity.lease_owner(), SearchKind::Placed, None)
            .await?;
        let Some(selected) = discovery
            .claim_placed(requester, workers.clone(), policy.clone(), lease_ms)
            .await?
        else {
            return Ok(None);
        };
        let partition = self
            .for_identity(
                &format!("run:{}", selected.request.run_id().0),
                &selected.request.run_id().0,
            )
            .await?;
        partition
            .claim_placed(requester, workers, policy, lease_ms)
            .await
    }
    async fn claim_for_terminal_recovery(
        &self,
        run_id: &RunId,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition
            .claim_for_terminal_recovery(run_id, owner, lease_ms)
            .await
    }
}

#[async_trait::async_trait]
impl DispatchAttemptRecordReads for ObjectDispatchStore {
    async fn attempt_admissions_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        session_thread: &ThreadId,
        member_threads: &[ThreadId],
    ) -> Result<DispatchPage, DispatchError> {
        let partition = self.for_session(&session_thread.0).await?;
        partition
            .attempt_admissions_after(cursor, limit, session_thread, member_threads)
            .await
    }
    async fn attempt_closure_records_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<awaken_dispatch_contract::AttemptClosurePage, DispatchError> {
        let partition = if let Some(run) = run {
            self.for_identity(&format!("run:{}", run.0), &run.0).await?
        } else {
            self.single_partition().await?
        };
        partition
            .attempt_closure_records_after(cursor, limit, run)
            .await
    }
}

#[async_trait::async_trait]
impl DispatchControlReads for ObjectDispatchStore {
    async fn physical_slot_occupied_guarded(
        &self,
        guard: &mut awaken_worker_contract::WorkerIncarnationGuard,
    ) -> Result<bool, DispatchError> {
        // Every physical admission requires the same original Registry
        // exclusion. Keep it held while checking all Session partitions.
        for partition in self.partitions().await? {
            if partition.physical_slot_occupied_guarded(guard).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn current_credential_binding(
        &self,
        claim: &RunClaim,
        candidate: &CandidateFingerprint,
    ) -> Result<Option<AttemptCredentialBinding>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.current_credential_binding(claim, candidate).await
    }
    async fn load_physical_attempt(
        &self,
        run_id: &RunId,
    ) -> Result<Option<awaken_dispatch_contract::dispatch::PhysicalAttempt>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.load_physical_attempt(run_id).await
    }
    async fn completion_for_run(
        &self,
        run: &RunId,
    ) -> Result<Option<DispatchCompletion>, DispatchError> {
        self.for_identity(&format!("run:{}", run.0), &run.0)
            .await?
            .completion_for_run(run)
            .await
    }
    async fn scan_completions(
        &self,
        cursor: Option<&DispatchScanCursor>,
        limit: usize,
    ) -> Result<DispatchScanPage<DispatchCompletion>, DispatchError> {
        self.scan_completion_partitions(cursor, limit).await
    }
    async fn completion_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<DispatchCompletion>, DispatchError> {
        let partition = self.single_partition().await?;
        partition
            .completion_events_after(after_sequence, limit)
            .await
    }
    async fn dispatch_depths(&self) -> Result<Option<DispatchDepths>, DispatchError> {
        let mut total = DispatchDepths {
            runnable: 0,
            leased: 0,
        };
        self.ensure_claim_index().await?;
        let mut seen = std::collections::HashSet::new();
        let mut after = None;
        loop {
            let page = self
                .objects
                .scan_runnable_sessions(
                    u64::MAX,
                    None,
                    after.as_deref(),
                    discovery::SESSIONS_PER_SEARCH,
                )
                .await
                .map_err(DispatchError::unavailable)?;
            for object in page.sessions {
                if !seen.insert(object.session_id.clone()) {
                    continue;
                }
                let partition = self.partition(&object);
                let Some(depths) = partition.dispatch_depths().await? else {
                    return Ok(None);
                };
                total.runnable = total.runnable.saturating_add(depths.runnable);
                total.leased = total.leased.saturating_add(depths.leased);
            }
            let Some(next) = page.resume_after else {
                break;
            };
            after = Some(next);
        }
        Ok(Some(total))
    }
    async fn scan_dispatch_ids(
        &self,
        after: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunId>, DispatchError> {
        let mut result = Vec::new();
        for partition in self.partitions().await? {
            result.extend(partition.scan_dispatch_ids(after, limit).await?);
        }
        result.sort();
        result.dedup();
        result.truncate(limit);
        Ok(result)
    }
    async fn dispatch_summary(
        &self,
        run_id: &RunId,
    ) -> Result<Option<DispatchSummary>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.dispatch_summary(run_id).await
    }
}

#[async_trait::async_trait]
impl Inbox for ObjectDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let partition = self.for_input(&input).await?;
        partition.append(input).await
    }
    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("input:{message_id}"), message_id)
            .await?;
        partition.retract(message_id, expected_revision).await
    }
    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let partition = self
            .for_identity(&format!("input:{message_id}"), message_id)
            .await?;
        partition.edit(message_id, expected_revision, result).await
    }
}

#[async_trait::async_trait]
impl Outbox for ObjectDispatchStore {
    async fn stage(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let partition = self.for_input(&input).await?;
        partition.stage(input).await
    }
    async fn relay(&self) -> Result<usize, DispatchError> {
        let mut count = 0usize;
        for partition in self.partitions().await? {
            count = count
                .checked_add(partition.relay().await?)
                .ok_or_else(|| DispatchError::unavailable("aggregate count overflow"))?;
        }
        Ok(count)
    }
}

#[async_trait::async_trait]
impl SessionContinuationOutbox for ObjectDispatchStore {
    async fn stage_session_resume(
        &self,
        input: PendingInput,
        thread: &ThreadId,
        prior: Option<u64>,
        epoch: Option<u64>,
    ) -> Result<bool, DispatchError> {
        let partition = self.for_input(&input).await?;
        partition
            .stage_session_resume(input, thread, prior, epoch)
            .await
    }
    async fn relay_and_enqueue(
        &self,
        input: PendingInput,
        request: RunDispatch,
        admission: ContinuationAdmission,
    ) -> Result<(), DispatchError> {
        let partition = self.for_request(&request).await?;
        partition.relay_and_enqueue(input, request, admission).await
    }
}

#[async_trait::async_trait]
impl DispatchOperationalFeed for ObjectDispatchStore {
    async fn scan_events(
        &self,
        cursor: Option<&DispatchScanCursor>,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchScanPage<DispatchOperationalEvent>, DispatchError> {
        self.scan_operation_partitions(cursor, limit, run).await
    }
    async fn events_after(
        &self,
        cursor: DispatchCursor,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchPage, DispatchError> {
        let partition = if let Some(run) = run {
            self.for_identity(&format!("run:{}", run.0), &run.0).await?
        } else {
            self.single_partition().await?
        };
        partition.events_after(cursor, limit, run).await
    }
}

#[async_trait::async_trait]
impl SessionRunAdmissionReads for ObjectDispatchStore {
    async fn observe_session_run_admission(
        &self,
        run_id: &RunId,
    ) -> Result<awaken_dispatch_contract::SessionRunAdmissionObservation, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.observe_session_run_admission(run_id).await
    }
}

#[async_trait::async_trait]
impl SessionRunReservations for ObjectDispatchStore {
    async fn adopt_pending_session_run(
        &self,
        expected_original: RunDispatch,
        command_fingerprint: awaken_session_types::SessionRunCommandFingerprint,
        owner: &str,
        lease_ms: u64,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let partition = self.for_request(&expected_original).await?;
        partition
            .adopt_pending_session_run(expected_original, command_fingerprint, owner, lease_ms)
            .await
    }
    async fn reserve_session_run(
        &self,
        request: RunDispatch,
        reservation_ttl_ms: u64,
    ) -> Result<SessionRunReservationOutcome, DispatchError> {
        let partition = self.for_request(&request).await?;
        partition
            .reserve_session_run(request, reservation_ttl_ms)
            .await
    }
    async fn activate_session_run_reservation(
        &self,
        run_id: &RunId,
        session_thread_id: &ThreadId,
        activity_version: u64,
    ) -> Result<SessionRunReservationActivation, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition
            .activate_session_run_reservation(run_id, session_thread_id, activity_version)
            .await
    }
    async fn reject_session_run_reservation(&self, run_id: &RunId) -> Result<bool, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.reject_session_run_reservation(run_id).await
    }
    async fn enqueue_session_child(
        &self,
        request: RunDispatch,
        admission: SessionChildAdmission,
    ) -> Result<(), DispatchError> {
        let partition = self.for_request(&request).await?;
        partition.enqueue_session_child(request, admission).await
    }
}

#[async_trait::async_trait]
impl DispatchCommitAuthority for ObjectDispatchStore {
    async fn worker_owns_run(
        &self,
        identity: &WorkerIdentity,
        run_id: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", run_id.0), &run_id.0)
            .await?;
        partition.worker_owns_run(identity, run_id).await
    }
    async fn lock_commit_epoch(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        Err(DispatchError::Rejected(
            "object storage requires conditional publication, not a held row lock".into(),
        ))
    }
    async fn acquire_commit_authority(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<ClaimAuthority>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition.acquire_commit_authority(claim).await
    }
}

#[async_trait::async_trait]
impl SessionReservationAuthority for ObjectDispatchStore {
    async fn lock_session_run_reservation_epoch(
        &self,
        _claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        Err(DispatchError::Rejected(
            "object storage requires conditional publication, not a held row lock".into(),
        ))
    }
    async fn acquire_session_run_reservation_authority(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<ClaimAuthority>, DispatchError> {
        let partition = self
            .for_identity(&format!("run:{}", claim.run_id.0), &claim.run_id.0)
            .await?;
        partition
            .acquire_session_run_reservation_authority(claim)
            .await
    }
}

mod scan;
