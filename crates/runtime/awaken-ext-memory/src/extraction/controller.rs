// @formal entrypoint memory_extraction.unknown_execution_never_reenters_retry
//! Memory-owned application controller for durable extraction intents.
//!
//! The parent module retains the aggregate and its ports; this module advances
//! those existing contracts without introducing a second lifecycle authority.

use super::*;
use awaken_time::WallClockExt as _;

impl Default for MemoryExtractionPolicy {
    fn default() -> Self {
        Self {
            lease_ms: 3_000,
            heartbeat_ms: 1_000,
            max_attempts: 5,
            retry_base_ms: 25,
        }
    }
}

impl MemoryExtractionController {
    #[must_use]
    pub fn new(
        repository: std::sync::Arc<dyn MemoryExtractionRepository>,
        owner: impl Into<String>,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            repository,
            owner: owner.into(),
            policy: MemoryExtractionPolicy::default(),
            clock,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: MemoryExtractionPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// CAS-create an intent. Redelivery is accepted only for the same immutable
    /// request; a reused stable identity with different content fails closed.
    pub async fn enqueue(
        &self,
        intent: MemoryExtractionIntent,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        if let Some(existing) = self.repository.get_extraction(&intent.intent_id).await? {
            return if existing.same_request(&intent) {
                Ok(PutMemoryExtractionOutcome::Existing)
            } else {
                Err(MemoryExtractionError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        self.repository.put_extraction_if_absent(intent).await
    }

    /// Freeze execution facts on an existing, still-pristine intent through its
    /// original revision fence. This selects no claim and performs no external
    /// effect. Already selected or progressed work retains its exact choice;
    /// a competing first claim/selection wins once, without relocation retries.
    pub async fn prepare_execution(
        &self,
        intent_id: &str,
        execution: MemoryExtractionExecution,
    ) -> Result<MemoryExtractionIntent, MemoryExtractionError> {
        let current = self
            .repository
            .get_extraction(intent_id)
            .await?
            .ok_or_else(|| MemoryExtractionError::NotFound(intent_id.into()))?;
        let Some(selected) = current.clone().selected_execution(execution)? else {
            return Ok(current);
        };
        if selected == current {
            return Ok(current);
        }
        match self
            .repository
            .compare_and_swap_extraction(current.revision, selected.clone())
            .await
        {
            Ok(()) => Ok(selected),
            Err(MemoryExtractionError::RevisionConflict(_)) => self
                .repository
                .get_extraction(intent_id)
                .await?
                .ok_or_else(|| MemoryExtractionError::NotFound(intent_id.into())),
            Err(error) => Err(error),
        }
    }

    /// Create the stable terminal-Run intent over only the transcript suffix not
    /// already owned by an earlier durable intent.
    pub async fn enqueue_terminal(
        &self,
        request: MemoryTerminalExtractionRequest,
    ) -> Result<PutMemoryExtractionOutcome, MemoryExtractionError> {
        let logical_thread_id = request.committed_transcript.reference().thread_id.0.clone();
        if logical_thread_id.trim().is_empty() {
            return Err(MemoryExtractionError::Invalid(
                "terminal transcript snapshot requires a logical Thread".into(),
            ));
        }
        let idempotency_key = format!("{}:{}", logical_thread_id, request.terminal_run_id);
        let intent_id = format!("memory-extraction:{idempotency_key}");
        if let Some(existing) = self.repository.get_extraction(&intent_id).await? {
            // This terminal's first intent already freezes execution facts.
            // Redelivery after deployment never relocates an existing Run or
            // rewrites unknown historical attribution from ambient defaults.
            let same_binding = existing.workspace_id == request.workspace_id
                && existing.session_id == request.session_id
                && existing.logical_thread_id() == logical_thread_id
                && existing.terminal_commit_id == request.terminal_run_id
                && existing.memory_store_id == request.memory_store_id
                && existing.memory_config_version == request.memory_config_version
                && existing.extractor == request.extractor;
            return if same_binding {
                Ok(PutMemoryExtractionOutcome::Existing)
            } else {
                Err(MemoryExtractionError::IdempotencyConflict(idempotency_key))
            };
        }
        let start = self
            .repository
            .extraction_cursor(&logical_thread_id)
            .await?;
        if start > request.committed_transcript.messages().len() {
            return Err(MemoryExtractionError::Invalid(format!(
                "extraction cursor {start} exceeds committed transcript length {}",
                request.committed_transcript.messages().len()
            )));
        }
        let end = request.committed_transcript.messages().len();
        let start_seq = u64::try_from(start)
            .map_err(|_| MemoryExtractionError::Invalid("transcript cursor overflow".into()))?;
        let end_seq = u64::try_from(end)
            .map_err(|_| MemoryExtractionError::Invalid("transcript cursor overflow".into()))?;
        let mut ranges = Vec::new();
        let mut open = None;
        let mut transcript = Vec::new();
        for (offset, message) in request.committed_transcript.messages()[start..]
            .iter()
            .enumerate()
        {
            let sequence = start_seq
                + u64::try_from(offset).map_err(|_| {
                    MemoryExtractionError::Invalid("transcript sequence overflow".into())
                })?;
            if message.id.0.starts_with(crate::RECALL_MESSAGE_ID_PREFIX) {
                if let Some(range_start) = open.take() {
                    ranges.push(TranscriptRange::new(range_start, sequence));
                }
            } else {
                open.get_or_insert(sequence);
                transcript.push(message.clone());
            }
        }
        if let Some(range_start) = open {
            ranges.push(TranscriptRange::new(range_start, end_seq));
        }
        let intent = MemoryExtractionIntent::new_snapshot(
            intent_id,
            idempotency_key,
            request.workspace_id,
            request.session_id,
            request.terminal_run_id,
            request.memory_store_id,
            request.memory_config_version,
            request.committed_transcript.reference().clone(),
            ranges,
            transcript,
            request.extractor,
        )?
        .with_execution(request.execution)?;
        self.repository.put_extraction_if_absent(intent).await
    }

    /// Find the oldest recoverable intent accepted by one frozen binding.
    ///
    /// The repository port is intentionally global because Resource-reference
    /// rebuilding also consumes it. Use its established exhaustive-scan contract
    /// so one busy physical Session cannot permanently hide another binding
    /// behind the first scheduling page.
    async fn next_recoverable(
        &self,
        driver: &dyn MemoryExtractionDriver,
    ) -> Result<Option<MemoryExtractionIntent>, MemoryExtractionError> {
        self.repository
            .recoverable_extractions(usize::MAX)
            .await
            .map(|candidates| candidates.into_iter().find(|intent| driver.accepts(intent)))
    }

    /// Preflight recovery through the same exhaustive selector used by the
    /// background driver. Repository failures remain visible to the caller so
    /// Session realization can fail closed and retry instead of caching an
    /// apparently healthy resident context.
    pub async fn has_recoverable(
        &self,
        driver: &dyn MemoryExtractionDriver,
    ) -> Result<bool, MemoryExtractionError> {
        self.next_recoverable(driver)
            .await
            .map(|intent| intent.is_some())
    }

    /// Drive every recoverable intent accepted by one frozen binding.
    pub async fn drive_recoverable(&self, driver: &dyn MemoryExtractionDriver) {
        loop {
            let mut intent = match self.next_recoverable(driver).await {
                Ok(Some(intent)) => intent,
                Ok(None) => return,
                Err(_) => {
                    // This is the existing Memory recovery controller's wake
                    // loop. A transient repository read must not terminate the
                    // sole driver and strand its durable intent; foreground
                    // preflight exposes the same error through `has_recoverable`.
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.policy.retry_base_ms.max(1),
                    ))
                    .await;
                    continue;
                }
            };
            let now = self.clock.now_ms();
            let expected_revision = intent.revision;
            let generation = match intent.claim(&self.owner, now, self.policy.lease_ms) {
                Ok(generation) => generation,
                Err(MemoryExtractionError::LeaseHeld {
                    lease_expires_at_unix_ms,
                }) => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        lease_expires_at_unix_ms
                            .saturating_sub(now)
                            .saturating_add(1),
                    ))
                    .await;
                    continue;
                }
                Err(_) => return,
            };
            if self
                .repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .is_err()
            {
                continue;
            }

            let result = self.advance_claimed(driver, &mut intent, generation).await;
            if let Err(failure) = result {
                let Ok(Some(current)) = self.repository.get_extraction(&intent.intent_id).await
                else {
                    return;
                };
                if current.revision != intent.revision
                    || current.claim_owner.as_deref() != Some(self.owner.as_str())
                    || current.claim_generation != generation
                {
                    continue;
                }
                let now = self.clock.now_ms();
                let expected_revision = intent.revision;
                let action = failure.into_action(intent.attempts, self.policy.max_attempts);
                let retryable = matches!(&action, AdvanceFailureAction::Retry(_));
                let transition = match action {
                    AdvanceFailureAction::ExecutionIndeterminate(error) => {
                        intent.mark_execution_indeterminate(&self.owner, generation, now, error)
                    }
                    AdvanceFailureAction::Terminal(error) => {
                        intent.terminal_fail(&self.owner, generation, now, error)
                    }
                    AdvanceFailureAction::Retry(error) => {
                        intent.retry(&self.owner, generation, now, error)
                    }
                };
                if transition.is_ok() {
                    let _ = self
                        .repository
                        .compare_and_swap_extraction(expected_revision, intent.clone())
                        .await;
                }
                if retryable && intent.attempts < self.policy.max_attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        self.policy.retry_base_ms * u64::from(intent.attempts.max(1)),
                    ))
                    .await;
                    continue;
                }
            }
        }
    }

    /// Renew from committed claim state, not from the controller's pre-I/O
    /// snapshot. Credential materialization may record its receipt while the
    /// extractor is running; overwriting that newer revision would either lose
    /// the receipt or turn every heartbeat into a false extraction retry.
    pub(super) async fn renew_current_claim(
        &self,
        intent: &mut MemoryExtractionIntent,
        generation: u64,
    ) -> Result<(), MemoryExtractionError> {
        for _ in 0..8 {
            let mut current = self
                .repository
                .get_extraction(&intent.intent_id)
                .await?
                .ok_or_else(|| MemoryExtractionError::NotFound(intent.intent_id.clone()))?;
            let expected_revision = current.revision;
            current.renew_claim(
                &self.owner,
                generation,
                self.clock.now_ms(),
                self.policy.lease_ms,
            )?;
            match self
                .repository
                .compare_and_swap_extraction(expected_revision, current.clone())
                .await
            {
                Ok(()) => {
                    *intent = current;
                    return Ok(());
                }
                Err(MemoryExtractionError::RevisionConflict(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(MemoryExtractionError::RevisionConflict(
            intent.intent_id.clone(),
        ))
    }

    async fn advance_claimed(
        &self,
        driver: &dyn MemoryExtractionDriver,
        intent: &mut MemoryExtractionIntent,
        generation: u64,
    ) -> Result<(), AdvanceFailure> {
        driver
            .validate_binding(intent)
            .await
            .map_err(AdvanceFailure::binding)?;
        if intent.status == MemoryExtractionStatus::Claimed {
            let extraction_input = intent.clone();
            let extraction = driver.extract(&extraction_input);
            tokio::pin!(extraction);
            let mutations = loop {
                tokio::select! {
                    result = &mut extraction => break match result {
                        MemoryExtractionOutcome::Mutations(mutations) => mutations,
                        MemoryExtractionOutcome::Retryable { reason } => {
                            return Err(AdvanceFailure::Retryable(reason));
                        }
                        MemoryExtractionOutcome::ExecutionIndeterminate {
                            auxiliary_run_id,
                            reason,
                        } => {
                            if auxiliary_run_id != intent.auxiliary_run_id() {
                                return Err(AdvanceFailure::Permanent(format!(
                                    "Memory extractor returned unknown execution for foreign auxiliary Run `{auxiliary_run_id}`"
                                )));
                            }
                            return Err(AdvanceFailure::ExecutionIndeterminate(reason));
                        }
                        MemoryExtractionOutcome::PermanentFailure { reason } => {
                            return Err(AdvanceFailure::Permanent(reason));
                        }
                    },
                    () = tokio::time::sleep(std::time::Duration::from_millis(self.policy.heartbeat_ms)) => {
                        self.renew_current_claim(intent, generation)
                            .await
                            .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
                    }
                }
            };
            // A driver may persist claim-fenced auxiliary facts (currently the
            // common credential-realization receipt) while extraction is in
            // flight. Reload the same claim before the lifecycle transition so
            // its CAS revision is authoritative instead of being overwritten by
            // the controller's pre-I/O snapshot.
            let current = self
                .repository
                .get_extraction(&intent.intent_id)
                .await
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?
                .ok_or_else(|| {
                    AdvanceFailure::Permanent(
                        "Memory extraction disappeared during execution".into(),
                    )
                })?;
            current
                .require_claim(&self.owner, generation, self.clock.now_ms())
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
            *intent = current;
            let expected_revision = intent.revision;
            intent
                .mark_extracted(&self.owner, generation, self.clock.now_ms(), mutations)
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
        }
        if intent.status == MemoryExtractionStatus::Extracted {
            driver
                .validate_binding(intent)
                .await
                .map_err(AdvanceFailure::binding)?;
            let mut receipts = Vec::with_capacity(intent.mutations.len());
            for mutation in &intent.mutations {
                receipts.push(
                    driver
                        .apply(intent, mutation)
                        .await
                        .map_err(AdvanceFailure::Retryable)?,
                );
            }
            let expected_revision = intent.revision;
            let now = self.clock.now_ms();
            intent
                .mark_stored(
                    &self.owner,
                    generation,
                    now,
                    MemoryExtractionReceipt {
                        stored_at_unix_ms: now,
                        mutations: receipts,
                    },
                )
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
        }
        if intent.status == MemoryExtractionStatus::Stored {
            let expected_revision = intent.revision;
            intent
                .complete(&self.owner, generation, self.clock.now_ms())
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| AdvanceFailure::Retryable(error.to_string()))?;
        }
        Ok(())
    }
}

enum AdvanceFailure {
    Retryable(String),
    ExecutionIndeterminate(String),
    Permanent(String),
}

#[derive(Debug, PartialEq, Eq)]
enum AdvanceFailureAction {
    Retry(String),
    ExecutionIndeterminate(String),
    Terminal(String),
}

impl AdvanceFailure {
    /// Binding validation reports a typed observation, never a message to parse.
    /// Completed proposals remain on the same intent when observation must retry.
    fn binding(error: MemoryExtractionError) -> Self {
        match error {
            MemoryExtractionError::Unavailable(reason) | MemoryExtractionError::Storage(reason) => {
                Self::Retryable(reason)
            }
            MemoryExtractionError::Invalid(reason) => Self::Permanent(reason),
            error @ (MemoryExtractionError::NotFound(_)
            | MemoryExtractionError::IdempotencyConflict(_)
            | MemoryExtractionError::RevisionConflict(_)
            | MemoryExtractionError::LeaseHeld { .. }
            | MemoryExtractionError::StaleClaim
            | MemoryExtractionError::InvalidTransition { .. }) => {
                Self::Permanent(error.to_string())
            }
        }
    }

    /// Sole controller decision for an observed extraction failure. Unknown
    /// execution has its own terminal action and can never enter retry-budget
    /// handling, regardless of the current attempt counters.
    fn into_action(self, attempts: u32, max_attempts: u32) -> AdvanceFailureAction {
        match self {
            Self::ExecutionIndeterminate(error) => {
                AdvanceFailureAction::ExecutionIndeterminate(error)
            }
            Self::Permanent(error) => AdvanceFailureAction::Terminal(error),
            Self::Retryable(error) if attempts >= max_attempts => {
                AdvanceFailureAction::Terminal(error)
            }
            Self::Retryable(error) => AdvanceFailureAction::Retry(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AdvanceFailure, AdvanceFailureAction};
    use crate::{MemoryExtractionError, MemoryExtractionStatus};

    #[test]
    fn failure_action_table_never_retries_indeterminate_execution() {
        // Causes: C1 failure={retryable, indeterminate, permanent}; C2 attempts
        // are below/at/above the configured maximum. Effects: E1 retry occurs
        // only for retryable+below; E2 indeterminate always selects its distinct
        // attention terminal; E3 permanent or exhausted retryable selects the
        // ordinary terminal. Rules F1=C1(retryable)+C2(below)->E1;
        // F2=C1(retryable)+!C2(below)->E3; F3=C1(indeterminate)+any C2->E2;
        // F4=C1(permanent)+any C2->E3. Boundary and extreme counters cover the
        // complete comparison partition while the Kani harness exhausts u32.
        // B1 typed dependency/storage observation => F1/F2; B2 every other
        // closed binding error => F4, never an execution-indeterminate action.
        for (attempts, max_attempts) in [
            (0, 0),
            (0, 1),
            (1, 1),
            (2, 1),
            (u32::MAX - 1, u32::MAX),
            (u32::MAX, u32::MAX),
        ] {
            assert_eq!(
                AdvanceFailure::Retryable("retry".into()).into_action(attempts, max_attempts),
                if attempts < max_attempts {
                    AdvanceFailureAction::Retry("retry".into())
                } else {
                    AdvanceFailureAction::Terminal("retry".into())
                },
                "F1/F2 attempts={attempts} max={max_attempts}"
            );
            assert_eq!(
                AdvanceFailure::ExecutionIndeterminate("unknown".into())
                    .into_action(attempts, max_attempts),
                AdvanceFailureAction::ExecutionIndeterminate("unknown".into()),
                "F3 attempts={attempts} max={max_attempts}"
            );
            assert_eq!(
                AdvanceFailure::Permanent("permanent".into()).into_action(attempts, max_attempts),
                AdvanceFailureAction::Terminal("permanent".into()),
                "F4 attempts={attempts} max={max_attempts}"
            );
            for (error, retryable) in [
                (MemoryExtractionError::Unavailable("same".into()), true),
                (MemoryExtractionError::Storage("same".into()), true),
                (MemoryExtractionError::Invalid("same".into()), false),
                (MemoryExtractionError::NotFound("same".into()), false),
                (
                    MemoryExtractionError::IdempotencyConflict("same".into()),
                    false,
                ),
                (
                    MemoryExtractionError::RevisionConflict("same".into()),
                    false,
                ),
                (
                    MemoryExtractionError::LeaseHeld {
                        lease_expires_at_unix_ms: u64::MAX,
                    },
                    false,
                ),
                (MemoryExtractionError::StaleClaim, false),
                (
                    MemoryExtractionError::InvalidTransition {
                        from: MemoryExtractionStatus::Pending,
                        to: MemoryExtractionStatus::Completed,
                    },
                    false,
                ),
            ] {
                let action = AdvanceFailure::binding(error).into_action(attempts, max_attempts);
                assert_eq!(
                    matches!(action, AdvanceFailureAction::Retry(_)),
                    retryable && attempts < max_attempts,
                    "B1/B2"
                );
                assert!(
                    !matches!(action, AdvanceFailureAction::ExecutionIndeterminate(_)),
                    "B1/B2"
                );
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_status(value: u8) -> MemoryExtractionStatus {
        match value % 7 {
            0 => MemoryExtractionStatus::Pending,
            1 => MemoryExtractionStatus::Claimed,
            2 => MemoryExtractionStatus::Extracted,
            3 => MemoryExtractionStatus::Stored,
            4 => MemoryExtractionStatus::Completed,
            5 => MemoryExtractionStatus::ExecutionIndeterminate,
            _ => MemoryExtractionStatus::TerminalFailed,
        }
    }

    /// @obligation memory_extraction.unknown_execution_never_reenters_retry
    #[kani::proof]
    fn memory_extraction_unknown_is_terminal_and_never_retryable() {
        // Causes: C1 every closed failure class; C2 every u32 attempts/max pair;
        // C3 every closed persisted status. Effects: E1 only in-budget Retryable
        // selects Retry; E2 indeterminate always selects its distinct terminal;
        // E3 recoverable and terminal scheduling sets are exact complements;
        // E4 the four SQL-bound names equal exactly the recoverable states.
        // No assumptions exclude counter values or closed enum variants.
        let failure_class: u8 = kani::any();
        let attempts: u32 = kani::any();
        let max_attempts: u32 = kani::any();
        let action = match failure_class % 3 {
            0 => AdvanceFailure::Retryable(String::new()),
            1 => AdvanceFailure::ExecutionIndeterminate(String::new()),
            _ => AdvanceFailure::Permanent(String::new()),
        }
        .into_action(attempts, max_attempts);
        match failure_class % 3 {
            0 if attempts < max_attempts => {
                assert!(matches!(action, AdvanceFailureAction::Retry(_)));
            }
            1 => {
                assert!(matches!(
                    action,
                    AdvanceFailureAction::ExecutionIndeterminate(_)
                ));
            }
            _ => assert!(matches!(action, AdvanceFailureAction::Terminal(_))),
        }

        // Exhaust the typed binding boundary before the same failure policy.
        // Only failed dependency observations enter the existing retry budget;
        // no binding observation can manufacture unknown execution or proposals.
        let binding_class: u8 = kani::any();
        let binding_error = match binding_class % 9 {
            0 => MemoryExtractionError::Unavailable(String::new()),
            1 => MemoryExtractionError::Storage(String::new()),
            2 => MemoryExtractionError::Invalid(String::new()),
            3 => MemoryExtractionError::NotFound(String::new()),
            4 => MemoryExtractionError::IdempotencyConflict(String::new()),
            5 => MemoryExtractionError::RevisionConflict(String::new()),
            6 => MemoryExtractionError::LeaseHeld {
                lease_expires_at_unix_ms: kani::any(),
            },
            7 => MemoryExtractionError::StaleClaim,
            _ => MemoryExtractionError::InvalidTransition {
                from: symbolic_status(kani::any()),
                to: symbolic_status(kani::any()),
            },
        };
        let action = AdvanceFailure::binding(binding_error).into_action(attempts, max_attempts);
        assert_eq!(
            matches!(action, AdvanceFailureAction::Retry(_)),
            binding_class % 9 < 2 && attempts < max_attempts
        );
        assert!(!matches!(
            action,
            AdvanceFailureAction::ExecutionIndeterminate(_)
        ));

        let status = symbolic_status(kani::any());
        assert_eq!(status.is_terminal(), !status.is_recoverable());
        let recoverable_names = MemoryExtractionStatus::recoverable_storage_names();
        assert_eq!(
            recoverable_names.contains(&status.storage_name()),
            status.is_recoverable()
        );
        assert!(!MemoryExtractionStatus::ExecutionIndeterminate.is_recoverable());
        assert!(MemoryExtractionStatus::ExecutionIndeterminate.is_terminal());
    }
}
