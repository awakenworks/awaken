// @formal entrypoint store_history.s3_publication_refines_committed_history
use crate::CommittedProjection;
use crate::{
    Claim, Error, HeadView, Result, RunPhase, SessionKey,
    model::{CommitDescriptor, Head, Record, RunSlot, ThreadHead, next},
    objects::{Inner, corrupt, digest},
};
use awaken_agent_contract::{
    agent::run::RunState,
    thread::commit::operation::{CommitOperation, CommitReceipt},
};
use awaken_dispatch_contract::RunDispatch;
use awaken_time::WallClockExt;
use awaken_worker_contract::RunClaim;
use object_store::UpdateVersion;
use std::sync::Arc;

// Control/journal commits do not consume an interrupted response. Conversation
// advancement or a terminal Run retires it at the same publication point.
fn retires_stream_checkpoint(
    messages: &[awaken_agent_contract::agent::message::Message],
    state: &RunState,
) -> bool {
    !messages.is_empty() || matches!(state, RunState::Ended(_))
}

#[cfg(test)]
mod checkpoint_retirement_tests {
    use super::*;

    /// @exhaustive conversation_advanced(2) x run_ended(2) = 4
    /// @obligation checkpoint.crash_preserves_durable_truth
    /// @obligation store_history.s3_publication_refines_committed_history
    #[test]
    fn control_commits_retain_checkpoints_until_conversation_advances_or_run_ends() {
        use awaken_agent_contract::agent::message::{Id, Message, Role};
        let mut visited = 0;
        for has_messages in [false, true] {
            for is_terminal in [false, true] {
                visited += 1;
                let messages = if has_messages {
                    vec![Message::text(
                        Id("proof".into()),
                        Role::Assistant,
                        "partial",
                    )]
                } else {
                    Vec::new()
                };
                let state = if is_terminal {
                    RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd)
                } else {
                    RunState::Running
                };
                // A control commit (no new messages, Run still live) keeps the
                // stream checkpoint; a conversation advance or a Run end
                // retires it.
                assert_eq!(
                    retires_stream_checkpoint(&messages, &state),
                    has_messages || is_terminal,
                    "messages={has_messages} terminal={is_terminal}",
                );
            }
        }
        assert_eq!(visited, 4, "the whole domain is enumerated");
    }
}

/// A scope-bound application port. Authentication resolves the key before opening it.
#[derive(Clone)]
pub struct SessionStorage {
    pub(crate) inner: Arc<Inner>,
    pub(crate) key: SessionKey,
}
impl SessionStorage {
    pub fn streams(&self) -> crate::SessionStreams {
        crate::SessionStreams {
            session: self.clone(),
        }
    }

    pub fn key(&self) -> &SessionKey {
        &self.key
    }

    pub async fn create(&self) -> Result<()> {
        let mut head = Head::new(self.key.clone());
        match self.inner.publish(&mut head, None).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.inner.load_head(&self.key).await?;
                Ok(())
            }
            Err(Error::OutcomeUnknown(error)) => match self.inner.load_head(&self.key).await {
                Ok(_) => Ok(()),
                _ => Err(Error::OutcomeUnknown(error)),
            },
            Err(error) => Err(error),
        }
    }
    pub async fn head(&self) -> Result<HeadView> {
        Ok(HeadView::from(&self.inner.load_head(&self.key).await?.0))
    }

    /// Accept a frozen activation. Retrying a Run id compares the entire request.
    pub async fn accept(&self, request: RunDispatch) -> Result<()> {
        let run = request.run_id().0.clone();
        let thread = request.thread_id().0.clone();
        if run.is_empty() || thread.is_empty() {
            return Err(Error::Invalid("blank Run or Thread".into()));
        }
        let partition = request
            .session_thread_id
            .as_ref()
            .unwrap_or(request.thread_id());
        if partition.0 != self.key.session {
            return Err(Error::Invalid("dispatch Session mismatch".into()));
        }
        request
            .validate_worker_codec_requirements()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if matches!(
            request.admission_shape(),
            awaken_dispatch_contract::DispatchAdmissionShape::SessionRootAwaitingActivity
                | awaken_dispatch_contract::DispatchAdmissionShape::InvalidZeroActivityEpoch
        ) {
            return Err(Error::Invalid(
                "Session activity admission must finish before enqueue".into(),
            ));
        }
        let identity = digest(&serde_json::to_vec(&request).map_err(corrupt)?);
        let mut staged: Option<(u64, crate::ObjectRef)> = None;
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            if head.archived {
                return Err(Error::Conflict("Session archived".into()));
            }
            if let Some(slot) = head.runs.get(&run) {
                return if slot.identity == identity {
                    Ok(())
                } else {
                    Err(Error::Conflict("Run id payload mismatch".into()))
                };
            }
            if head.runs.len() >= self.inner.limits.max_active_runs {
                return Err(Error::Capacity(
                    "Run admission limit; retain tombstones until indexed".into(),
                ));
            }
            if !head.threads.contains_key(&thread) {
                if head.threads.len() >= self.inner.limits.max_threads {
                    return Err(Error::Capacity("Thread limit".into()));
                }
                head.threads.insert(thread.clone(), ThreadHead::empty());
            }
            if head.threads[&thread].archived {
                return Err(Error::Conflict("Thread archived".into()));
            }
            let activation = match &staged {
                Some((epoch, reference)) if *epoch == head.gc_generation => reference.clone(),
                _ => {
                    let reference = self
                        .inner
                        .put_json(
                            &self.key,
                            head.gc_generation,
                            "segments",
                            &Record::RunAccepted(Box::new(request.clone())),
                        )
                        .await?;
                    staged = Some((head.gc_generation, reference.clone()));
                    reference
                }
            };
            head.runs.insert(
                run.clone(),
                RunSlot {
                    thread: thread.clone(),
                    activation,
                    identity: identity.clone(),
                    phase: RunPhase::Pending,
                    claim: None,
                    last_epoch: None,
                    cancel_requested: false,
                    next_ordinal: 0,
                    physical_attempt: None,
                },
            );
            head.order.push(run.clone());
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    pub async fn activation(&self, run: &str) -> Result<RunDispatch> {
        let (head, _) = self.inner.load_head(&self.key).await?;
        let row = head.runs.get(run).ok_or(Error::NotFound)?;
        match self.inner.read_json(&self.key, &row.activation).await? {
            Record::RunAccepted(request) => Ok(*request),
            _ => Err(Error::Corrupt("activation record kind".into())),
        }
    }

    /// Time permits a takeover; the successful head CAS changes write authority.
    pub async fn claim(&self, run: &str, owner: &str, lease_ms: u64) -> Result<Claim> {
        if owner.is_empty() || lease_ms == 0 {
            return Err(Error::Invalid(
                "owner and positive lease duration required".into(),
            ));
        }
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            if head.archived {
                return Err(Error::Conflict("Session archived".into()));
            }
            let now = self.inner.clock.now_ms();
            let target = head.runs.get(run).ok_or(Error::NotFound)?;
            if head.runs.iter().any(|(id, r)| {
                id != run
                    && r.thread == target.thread
                    && (r.physical_attempt.is_some()
                        || r.claim.as_ref().is_some_and(|c| c.expires_at_ms > now))
            }) {
                return Err(Error::Conflict("Thread has an occupied attempt".into()));
            }
            let target_thread = target.thread.clone();
            for (id, peer) in &mut head.runs {
                if id != run && peer.thread == target_thread && peer.claim.is_some() {
                    peer.claim = None;
                    peer.phase = RunPhase::Pending;
                }
            }
            let row = head.runs.get_mut(run).ok_or(Error::NotFound)?;
            if matches!(
                row.phase,
                RunPhase::Done | RunPhase::Cancelled | RunPhase::Awaiting
            ) {
                return Err(Error::Conflict("Run is not claimable".into()));
            }
            if let Some(existing) = &row.claim
                && existing.expires_at_ms > now
            {
                if existing.owner == owner {
                    return Ok(existing.clone());
                }
                return Err(Error::Conflict("Run already leased".into()));
            }
            let claim = Claim {
                run_id: run.into(),
                owner: owner.into(),
                epoch: awaken_lease_contract::fence::Epoch::successor(row.last_epoch)
                    .map_err(|error| Error::Corrupt(error.to_string()))?,
                expires_at_ms: now
                    .checked_add(lease_ms)
                    .ok_or_else(|| Error::Invalid("lease deadline overflow".into()))?,
            };
            row.last_epoch = Some(claim.epoch);
            row.claim = Some(claim.clone());
            row.phase = RunPhase::Running;
            // physical_attempt deliberately survives takeover.
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(claim);
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    pub async fn renew(&self, claim: &RunClaim, lease_ms: u64) -> Result<()> {
        if lease_ms == 0 {
            return Err(Error::Invalid("positive lease required".into()));
        }
        self.mutate_claim(claim, |row, now| {
            let deadline = now
                .checked_add(lease_ms)
                .ok_or_else(|| Error::Invalid("lease deadline overflow".into()))?;
            let current = row.claim.as_mut().ok_or(Error::Fenced)?;
            current.expires_at_ms = current.expires_at_ms.max(deadline);
            Ok(())
        })
        .await
    }
    pub async fn begin_attempt(&self, claim: &RunClaim) -> Result<()> {
        self.mutate_claim(claim, |row, _| {
            if row.cancel_requested {
                return Err(Error::Fenced);
            }
            match &row.physical_attempt {
                Some(active) if active != claim => Err(Error::Conflict(
                    "predecessor has not acknowledged quiescence".into(),
                )),
                _ => {
                    row.physical_attempt = Some(claim.clone());
                    Ok(())
                }
            }
        })
        .await
    }
    /// An old owner may acknowledge only its exact physical slot after takeover.
    pub async fn end_attempt(&self, claim: &RunClaim) -> Result<()> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let row = head.runs.get_mut(&claim.run_id.0).ok_or(Error::NotFound)?;
            if row.physical_attempt.as_ref() != Some(claim) {
                return Err(Error::Fenced);
            }
            row.physical_attempt = None;
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    /// Give back an unoccupied claim without inventing physical quiescence.
    pub async fn relinquish(&self, claim: &RunClaim) -> Result<()> {
        self.mutate_claim(claim, |row, _| {
            if row.physical_attempt.is_some() {
                return Err(Error::Conflict("physical attempt remains occupied".into()));
            }
            row.claim = None;
            row.phase = RunPhase::Pending;
            Ok(())
        })
        .await
    }
    pub async fn cancel(&self, run: &str) -> Result<()> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let row = head.runs.get_mut(run).ok_or(Error::NotFound)?;
            if row.cancel_requested || matches!(row.phase, RunPhase::Done | RunPhase::Cancelled) {
                return Ok(());
            }
            row.cancel_requested = true;
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    pub(crate) async fn mutate_claim(
        &self,
        claim: &RunClaim,
        change: impl Fn(&mut RunSlot, u64) -> Result<()>,
    ) -> Result<()> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            self.require_claim(&head, claim)?;
            change(
                head.runs.get_mut(&claim.run_id.0).ok_or(Error::Fenced)?,
                self.inner.clock.now_ms(),
            )?;
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    pub(crate) fn require_claim(&self, head: &Head, claim: &RunClaim) -> Result<()> {
        let row = head.runs.get(&claim.run_id.0).ok_or(Error::Fenced)?;
        if row.phase != RunPhase::Running
            || row.claim.as_ref().map(Claim::run_claim).as_ref() != Some(claim)
        {
            return Err(Error::Fenced);
        }
        Ok(())
    }

    pub(crate) async fn descriptors(&self, head: &Head) -> Result<Vec<CommitDescriptor>> {
        let mut records = Vec::new();
        for index in &head.indexes {
            let chunk: Vec<CommitDescriptor> =
                self.inner.read_json(&self.key, &index.object).await?;
            if chunk.first().map(|r| r.receipt.commit_sequence) != Some(index.first)
                || chunk.last().map(|r| r.receipt.commit_sequence) != Some(index.last)
            {
                return Err(Error::Corrupt("commit index bounds".into()));
            }
            records.extend(chunk);
        }
        records.extend(head.recent.clone());
        let mut previous = String::new();
        for (index, record) in records.iter().enumerate() {
            if record.receipt.commit_sequence != index as u64 + 1
                || record.previous_hash != previous
                || descriptor_hash(record)? != record.commit_hash
            {
                return Err(Error::Corrupt("non-contiguous commit chain".into()));
            }
            previous = record.commit_hash.clone();
        }
        if records.len() as u64 != head.commit_sequence || previous != head.chain_hash {
            return Err(Error::Corrupt("head commit chain mismatch".into()));
        }
        Ok(records)
    }

    pub(crate) async fn replay(&self, head: &Head) -> Result<CommittedProjection> {
        let descriptors = self.descriptors(head).await?;
        self.prefetch(
            &descriptors
                .iter()
                .map(|r| r.body.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
        let mut replay = CommittedProjection::default();
        for descriptor in descriptors {
            let record = self.inner.read_json(&self.key, &descriptor.body).await?;
            let Record::StepCommitted(operation) = record else {
                return Err(Error::Corrupt("commit record kind".into()));
            };
            if operation.commit.thread_id.0 != descriptor.thread {
                return Err(Error::Corrupt("commit Thread mismatch".into()));
            }
            let receipt = replay
                .apply(*operation, descriptor.at_ms, false)
                .map_err(|e| Error::Corrupt(e.to_string()))?;
            if receipt != descriptor.receipt {
                return Err(Error::Corrupt("replay receipt mismatch".into()));
            }
        }
        Ok(replay)
    }

    /// Body, receipt, Thread CAS, claim, checkpoint retirement and publication are one CAS.
    pub async fn commit(
        &self,
        claim: &RunClaim,
        operation: CommitOperation,
    ) -> Result<CommitReceipt> {
        self.commit_with_blobs(claim, operation, &[]).await
    }
    pub async fn commit_with_blobs(
        &self,
        claim: &RunClaim,
        operation: CommitOperation,
        artifacts: &[crate::BlobRef],
    ) -> Result<CommitReceipt> {
        self.commit_authorized(claim, operation, artifacts, None)
            .await
    }
    /// Publish against the canonical Dispatch component in this same Session
    /// head. No mirror claim or second queue is involved.
    pub async fn commit_conditionally(
        &self,
        claim: &RunClaim,
        operation: CommitOperation,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<CommitReceipt> {
        awaken_agent_contract::thread::commit::operation::require_commit_operation_payload_hash(
            &operation,
        )
        .map_err(|error| Error::Identity(error.to_string()))?;
        self.commit_authorized(claim, operation, &[], Some(condition))
            .await
    }
    pub(crate) async fn commit_authorized(
        &self,
        claim: &RunClaim,
        operation: CommitOperation,
        artifacts: &[crate::BlobRef],
        condition: Option<&awaken_session_types::SessionPublicationCondition>,
    ) -> Result<CommitReceipt> {
        if operation.operation_id.run_id != claim.run_id {
            return Err(Error::Invalid("claim Run mismatch".into()));
        }
        let mut staged: Option<(u64, crate::ObjectRef)> = None;
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let allow_cancelled = matches!(
                &operation.commit.run_state(),
                RunState::Ended(awaken_agent_contract::agent::run::EndCause::Cancelled)
            );
            let (thread_id, cancel_requested) = if let Some(condition) = condition {
                self.inner
                    .validate_claim_condition(&head, claim, condition, allow_cancelled)
                    .await?;
                let request: RunDispatch = serde_json::from_value(
                    self.component_record_at(&head, "dispatch", &format!("run:{}", claim.run_id.0))
                        .await?
                        .ok_or_else(|| Error::Corrupt("accepted Dispatch record absent".into()))?,
                )
                .map_err(corrupt)?;
                if request.run_id() != &claim.run_id
                    || request.session_thread_id().0 != self.key.session
                {
                    return Err(Error::Fenced);
                }
                (request.thread_id().0.clone(), false)
            } else {
                self.require_claim(&head, claim)?;
                let row = head.runs.get(&claim.run_id.0).ok_or(Error::Fenced)?;
                (row.thread.clone(), row.cancel_requested)
            };
            if thread_id != operation.commit.thread_id.0 {
                return Err(Error::Invalid("claim Thread mismatch".into()));
            }
            if !head.threads.contains_key(&thread_id) {
                if head.threads.len() >= self.inner.limits.max_threads {
                    return Err(Error::Capacity("Thread limit".into()));
                }
                head.threads.insert(thread_id.clone(), ThreadHead::empty());
            }
            let mut kernel = self.replay(&head).await?;
            if let Some(receipt) = kernel.inspect_operation(&operation)? {
                if !artifacts.is_empty() {
                    let stored = self
                        .descriptors(&head)
                        .await?
                        .into_iter()
                        .find(|r| r.receipt.operation_id == operation.operation_id)
                        .ok_or_else(|| Error::Corrupt("receipt descriptor absent".into()))?;
                    if stored.artifacts != artifacts {
                        return Err(Error::Conflict("operation artifacts differ".into()));
                    }
                }
                return Ok(receipt);
            }
            for artifact in artifacts {
                self.streams().read_blob(artifact).await?;
                if artifact.object.generation != head.gc_generation {
                    let retained = head.blobs.values().any(|b| b == artifact)
                        || self
                            .descriptors(&head)
                            .await?
                            .iter()
                            .any(|d| d.artifacts.contains(artifact));
                    if !retained {
                        return Err(Error::Conflict("upload epoch sealed; upload again".into()));
                    }
                }
            }
            if cancel_requested
                && !matches!(
                    operation.commit.run_state(),
                    RunState::Ended(awaken_agent_contract::agent::run::EndCause::Cancelled)
                )
            {
                return Err(Error::Fenced);
            }
            let thread = head.threads.get(&thread_id).ok_or(Error::NotFound)?;
            if head.archived || thread.archived {
                return Err(Error::Conflict("Session or Thread archived".into()));
            }
            let start = thread.message_end;
            let (sequence, thread_version, end) = next_commit_position(
                head.commit_sequence,
                thread.version,
                start,
                operation.commit.messages.len() as u64,
            )?;
            let at_ms = self.inner.clock.now_ms();
            let receipt = kernel.apply(operation.clone(), at_ms, true)?;
            if receipt.duplicate
                || receipt.commit_sequence != sequence
                || receipt.thread_version != thread_version
            {
                return Err(Error::Corrupt(
                    "commit projection disagrees with head".into(),
                ));
            }
            let body = match &staged {
                Some((epoch, reference)) if *epoch == head.gc_generation => reference.clone(),
                _ => {
                    let reference = self
                        .inner
                        .put_json(
                            &self.key,
                            head.gc_generation,
                            "segments",
                            &Record::StepCommitted(Box::new(operation.clone())),
                        )
                        .await?;
                    staged = Some((head.gc_generation, reference.clone()));
                    reference
                }
            };
            let mut descriptor = CommitDescriptor {
                thread: operation.commit.thread_id.0.clone(),
                receipt: receipt.clone(),
                at_ms,
                message_start: start,
                message_end: end,
                previous_hash: head.chain_hash.clone(),
                commit_hash: String::new(),
                body,
                artifacts: artifacts.to_vec(),
            };
            descriptor.commit_hash = descriptor_hash(&descriptor)?;
            head.chain_hash = descriptor.commit_hash.clone();
            head.commit_sequence = sequence;
            head.recent.push(descriptor);
            let thread = head
                .threads
                .get_mut(&operation.commit.thread_id.0)
                .ok_or(Error::NotFound)?;
            thread.version = thread_version;
            thread.message_end = end;
            if condition.is_none() {
                head.runs
                    .get_mut(&claim.run_id.0)
                    .ok_or(Error::Fenced)?
                    .next_ordinal = next(operation.operation_id.ordinal)?;
            }
            if retires_stream_checkpoint(&operation.commit.messages, &operation.commit.run_state())
            {
                head.checkpoints.remove(&claim.run_id.0);
            }
            self.streams()
                .archive_step(&mut head, &claim.run_id.0, operation.operation_id.ordinal)
                .await?;
            // Completed streams remain replayable without occupying the hot head.
            self.fold_recent(&mut head).await?;
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(receipt);
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    pub async fn settle(&self, claim: &RunClaim) -> Result<()> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            self.require_claim(&head, claim)?;
            let kernel = self.replay(&head).await?;
            use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
            let run = kernel
                .run(&claim.run_id)
                .ok_or_else(|| Error::Conflict("Run has no committed state".into()))?;
            let row = head.runs.get_mut(&claim.run_id.0).ok_or(Error::Fenced)?;
            if row.physical_attempt.is_some() {
                return Err(Error::Conflict("physical attempt remains occupied".into()));
            }
            row.phase = match run.state {
                RunState::Running => {
                    return Err(Error::Conflict("Run has not reached a settlement".into()));
                }
                RunState::Awaiting => RunPhase::Awaiting,
                RunState::Ended(_) => {
                    if row.cancel_requested {
                        RunPhase::Cancelled
                    } else {
                        RunPhase::Done
                    }
                }
            };
            row.claim = None;
            if self.inner.publish(&mut head, Some(version)).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    pub(crate) async fn canonical_inflight(
        &self,
        head: &Head,
        claim: &RunClaim,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<(String, u64)> {
        use awaken_agent_contract::agent::thread::Id as ThreadId;
        use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;
        self.inner
            .validate_claim_condition(head, claim, condition, false)
            .await?;
        let request: RunDispatch = serde_json::from_value(
            self.component_record_at(head, "dispatch", &format!("run:{}", claim.run_id.0))
                .await?
                .ok_or_else(|| Error::Corrupt("accepted Dispatch record absent".into()))?,
        )
        .map_err(corrupt)?;
        if request.run_id() != &claim.run_id || request.session_thread_id().0 != self.key.session {
            return Err(Error::Fenced);
        }
        let snapshot = self
            .replay(head)
            .await?
            .recovery_snapshot(&ThreadId(request.thread_id().0.clone()), &claim.run_id)
            .await
            .map_err(|e| Error::Corrupt(e.to_string()))?;
        Ok((request.thread_id().0.clone(), snapshot.next_commit_ordinal))
    }
    pub(crate) async fn fold_recent(&self, head: &mut Head) -> Result<()> {
        if head.recent.len() <= self.inner.limits.recent_commits {
            return Ok(());
        }
        // One bounded immutable index. Hierarchical folding is a maintenance concern.
        let records = std::mem::take(&mut head.recent);
        let mut threads = std::collections::BTreeMap::new();
        for record in &records {
            threads
                .entry(record.thread.clone())
                .and_modify(|range: &mut (u64, u64)| range.1 = record.message_end)
                .or_insert((record.message_start, record.message_end));
        }
        let first = records
            .first()
            .ok_or_else(|| Error::Corrupt("empty index".into()))?
            .receipt
            .commit_sequence;
        let last = records
            .last()
            .ok_or_else(|| Error::Corrupt("empty index".into()))?
            .receipt
            .commit_sequence;
        let object = self
            .inner
            .put_json(&self.key, head.gc_generation, "indexes", &records)
            .await?;
        head.indexes.push(crate::model::IndexRef {
            object,
            first,
            last,
            threads,
        });
        Ok(())
    }

    pub(crate) async fn publish_head(
        &self,
        head: &mut Head,
        version: UpdateVersion,
    ) -> Result<bool> {
        self.inner.publish(head, Some(version)).await
    }

    pub async fn execution_view(&self) -> Result<CommittedProjection> {
        let (head, _) = self.inner.load_head(&self.key).await?;
        self.replay(&head).await
    }
}
pub(crate) fn next_commit_position(
    sequence: u64,
    thread_version: u64,
    message_end: u64,
    message_count: u64,
) -> Result<(u64, u64, u64)> {
    let end = message_end
        .checked_add(message_count)
        .ok_or_else(|| Error::Capacity("message sequence exhausted".into()))?;
    Ok((next(sequence)?, next(thread_version)?, end))
}

#[cfg(kani)]
/// @obligation store_history.s3_publication_refines_committed_history
#[kani::proof]
fn publication_advances_the_exact_nonwrapping_head_position() {
    let sequence: u64 = kani::any();
    let thread_version: u64 = kani::any();
    let message_end: u64 = kani::any();
    let count: u64 = kani::any();

    let new_end = u128::from(message_end) + u128::from(count);
    let can_publish =
        sequence < u64::MAX && thread_version < u64::MAX && new_end <= u128::from(u64::MAX);
    let position = next_commit_position(sequence, thread_version, message_end, count);
    assert_eq!(position.is_ok(), can_publish);
    if let Ok((commit_sequence, version, end)) = position {
        assert_eq!(commit_sequence - 1, sequence);
        assert_eq!(version - 1, thread_version);
        assert_eq!(u128::from(end), new_end);
    }
}

fn descriptor_hash(record: &CommitDescriptor) -> Result<String> {
    // Physical location is excluded so compaction can relocate identical frames.
    Ok(digest(
        &serde_json::to_vec(&(
            "awaken.session-commit.v1",
            &record.thread,
            &record.receipt,
            record.at_ms,
            record.message_start,
            record.message_end,
            &record.previous_hash,
            record
                .artifacts
                .iter()
                .map(|b| (&b.object.sha256, &b.media_type, b.size))
                .collect::<Vec<_>>(),
        ))
        .map_err(corrupt)?,
    ))
}
