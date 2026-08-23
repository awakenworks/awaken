//! Memory-owned application controller for durable extraction intents.
//!
//! The parent module retains the aggregate and its ports; this module advances
//! those existing contracts without introducing a second lifecycle authority.

use super::*;

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
    ) -> Self {
        Self {
            repository,
            owner: owner.into(),
            policy: MemoryExtractionPolicy::default(),
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
        )?;
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
            let now = unix_ms();
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
            if let Err((error, terminal)) = result {
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
                let now = unix_ms();
                let expected_revision = intent.revision;
                let transition = if terminal || intent.attempts >= self.policy.max_attempts {
                    intent.terminal_fail(&self.owner, generation, now, error)
                } else {
                    intent.retry(&self.owner, generation, now, error)
                };
                if transition.is_ok() {
                    let _ = self
                        .repository
                        .compare_and_swap_extraction(expected_revision, intent.clone())
                        .await;
                }
                if !terminal && intent.attempts < self.policy.max_attempts {
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
            current.renew_claim(&self.owner, generation, unix_ms(), self.policy.lease_ms)?;
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
    ) -> Result<(), (String, bool)> {
        driver
            .validate_binding(intent)
            .await
            .map_err(|error| (error, true))?;
        if intent.status == MemoryExtractionStatus::Claimed {
            let extraction_input = intent.clone();
            let extraction = driver.extract(&extraction_input);
            tokio::pin!(extraction);
            let mutations = loop {
                tokio::select! {
                    result = &mut extraction => break result.map_err(|error| (error, false))?,
                    () = tokio::time::sleep(std::time::Duration::from_millis(self.policy.heartbeat_ms)) => {
                        self.renew_current_claim(intent, generation)
                            .await
                            .map_err(|error| (error.to_string(), false))?;
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
                .map_err(|error| (error.to_string(), false))?
                .ok_or_else(|| {
                    (
                        "Memory extraction disappeared during execution".into(),
                        true,
                    )
                })?;
            current
                .require_claim(&self.owner, generation, unix_ms())
                .map_err(|error| (error.to_string(), false))?;
            *intent = current;
            let expected_revision = intent.revision;
            intent
                .mark_extracted(&self.owner, generation, unix_ms(), mutations)
                .map_err(|error| (error.to_string(), false))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        if intent.status == MemoryExtractionStatus::Extracted {
            driver
                .validate_binding(intent)
                .await
                .map_err(|error| (error, true))?;
            let mut receipts = Vec::with_capacity(intent.mutations.len());
            for mutation in &intent.mutations {
                receipts.push(
                    driver
                        .apply(intent, mutation)
                        .await
                        .map_err(|error| (error, false))?,
                );
            }
            let expected_revision = intent.revision;
            intent
                .mark_stored(
                    &self.owner,
                    generation,
                    unix_ms(),
                    MemoryExtractionReceipt {
                        stored_at_unix_ms: unix_ms(),
                        mutations: receipts,
                    },
                )
                .map_err(|error| (error.to_string(), false))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        if intent.status == MemoryExtractionStatus::Stored {
            let expected_revision = intent.revision;
            intent
                .complete(&self.owner, generation, unix_ms())
                .map_err(|error| (error.to_string(), false))?;
            self.repository
                .compare_and_swap_extraction(expected_revision, intent.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
        }
        Ok(())
    }
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
