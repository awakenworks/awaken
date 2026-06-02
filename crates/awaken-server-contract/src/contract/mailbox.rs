//! Run dispatch data vocabulary and the mailbox store contract.
//!
//! Both the `RunDispatch*` data types and the `MailboxStore` persistence trait
//! (plus the `DispatchSignal*` durable-signal pair and the
//! `MailboxStore`->`LiveRunCommandSource` bridge) are server/store concerns and
//! live here. The runtime engine references none of them; it steers a live run
//! only through the narrower `live_control` port in runtime-contract.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_runtime_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_runtime_contract::contract::storage::StorageError;
use serde::{Deserialize, Serialize};

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

// ── RunDispatchStatus ───────────────────────────────────────────────

/// Six-state lifecycle for a dispatch attempt.
///
/// ```text
/// Queued ──claim──> Claimed ──ack──> Acked (terminal)
///   |                  |
///   |               nack(retry) ──> Queued (attempt_count++, available_at = retry_at)
///   |                  |
///   |               nack(permanent) ──> DeadLetter (terminal)
///   |
///   |── cancel ──> Cancelled (terminal)
///   └── interrupt(dispatch epoch bump) ──> Superseded (terminal)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunDispatchStatus {
    Queued,
    Claimed,
    Acked,
    Cancelled,
    Superseded,
    DeadLetter,
}

impl RunDispatchStatus {
    /// Returns `true` for terminal states that cannot transition further.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Acked | Self::Cancelled | Self::Superseded | Self::DeadLetter
        )
    }
}

// ── RunDispatchResult ────────────────────────────────────────────────

/// Durable runtime-result projection for the dispatch that consumed a run.
///
/// `RunRecord` remains the source of truth for business outcome. This compact
/// projection exists on the queue record so operators can inspect what happened
/// to a claimed dispatch without treating `Acked` as agent success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDispatchResult {
    /// Runtime run ID used by the execution engine.
    pub run_id: String,
    /// Dispatch attempt ID that links a queue claim to a runtime invocation.
    pub dispatch_instance_id: String,
    /// Durable runtime status reached by this run.
    pub status: RunStatus,
    /// Structured terminal reason, if the runtime reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<TerminationReason>,
    /// Final response text, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Runtime error text, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── RunDispatch ─────────────────────────────────────────────────────

/// A run dispatch persisted in the mailbox queue.
///
/// This record owns delivery/lease/retry state only. Business request,
/// message, and outcome semantics live on `RunRecord` and the thread message
/// log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDispatch {
    // ── identity ──
    /// UUID v7, globally unique.
    dispatch_id: String,
    /// Thread ID, routing anchor.
    thread_id: String,
    /// Canonical runtime run ID this dispatch activates.
    run_id: String,

    // ── queue semantics ──
    /// 0 = highest, 255 = lowest, default 128.
    priority: u8,
    /// Idempotent delivery key.
    dedupe_key: Option<String>,
    /// Thread dispatch epoch captured when this dispatch was created.
    dispatch_epoch: u64,

    // ── lifecycle ──
    /// Current status.
    status: RunDispatchStatus,
    /// Unix millis; future value = delayed delivery.
    available_at: u64,
    /// Number of claim attempts so far.
    attempt_count: u32,
    /// Maximum attempts before dead-lettering (default 5).
    max_attempts: u32,
    /// Last error message.
    last_error: Option<String>,

    // ── lease ──
    /// UUID set on claim.
    claim_token: Option<String>,
    /// Consumer identifier (process) that claimed this dispatch.
    claimed_by: Option<String>,
    /// Unix millis, extended by heartbeat.
    lease_until: Option<u64>,

    // ── runtime trace ──
    /// Dispatch attempt ID associated with the current/latest claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dispatch_instance_id: Option<String>,
    /// Runtime status associated with this dispatch's current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_status: Option<RunStatus>,
    /// Structured terminal reason for the current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    termination: Option<TerminationReason>,
    /// Final response text for the current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_response: Option<String>,
    /// Runtime error text for the current/latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_error: Option<String>,
    /// Unix millis when the runtime result was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_at: Option<u64>,

    // ── timestamps ──
    /// Unix millis when the dispatch was created.
    created_at: u64,
    /// Unix millis of the last update.
    updated_at: u64,
}

/// Explicit persisted representation used by store codecs.
///
/// This bag keeps deserialization and backend row decoding possible without
/// reopening `RunDispatch` to arbitrary field mutation. Convert it with
/// [`RunDispatch::from_persisted_parts`], which validates lifecycle
/// invariants before returning a dispatch.
#[derive(Debug, Clone)]
pub struct RunDispatchParts {
    pub dispatch_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub priority: u8,
    pub dedupe_key: Option<String>,
    pub dispatch_epoch: u64,
    pub status: RunDispatchStatus,
    pub available_at: u64,
    pub attempt_count: u32,
    pub max_attempts: u32,
    pub last_error: Option<String>,
    pub claim_token: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<u64>,
    pub dispatch_instance_id: Option<String>,
    pub run_status: Option<RunStatus>,
    pub termination: Option<TerminationReason>,
    pub run_response: Option<String>,
    pub run_error: Option<String>,
    pub completed_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl RunDispatch {
    /// Build a queued dispatch. The result is validateable for enqueue and is
    /// the only public constructor for new mailbox records.
    #[must_use]
    pub fn queued(
        dispatch_id: impl Into<String>,
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        created_at: u64,
    ) -> Self {
        Self {
            dispatch_id: dispatch_id.into(),
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            priority: 128,
            dedupe_key: None,
            dispatch_epoch: 0,
            status: RunDispatchStatus::Queued,
            available_at: created_at,
            attempt_count: 0,
            max_attempts: 5,
            last_error: None,
            claim_token: None,
            claimed_by: None,
            lease_until: None,
            dispatch_instance_id: None,
            run_status: None,
            termination: None,
            run_response: None,
            run_error: None,
            completed_at: None,
            created_at,
            updated_at: created_at,
        }
    }

    #[must_use]
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub fn with_dedupe_key(mut self, dedupe_key: impl Into<Option<String>>) -> Self {
        self.dedupe_key = dedupe_key.into();
        self
    }

    #[must_use]
    pub fn with_available_at(mut self, available_at: u64) -> Self {
        self.available_at = available_at;
        self
    }

    #[must_use]
    pub fn with_created_at(mut self, created_at: u64) -> Self {
        self.created_at = created_at;
        self
    }

    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    #[must_use]
    pub fn with_attempt_count(mut self, attempt_count: u32) -> Self {
        self.attempt_count = attempt_count;
        self
    }

    #[must_use]
    pub fn with_dispatch_epoch(mut self, dispatch_epoch: u64) -> Self {
        self.dispatch_epoch = dispatch_epoch;
        self
    }

    pub fn from_persisted_parts(parts: RunDispatchParts) -> Result<Self, StorageError> {
        let dispatch = Self {
            dispatch_id: parts.dispatch_id,
            thread_id: parts.thread_id,
            run_id: parts.run_id,
            priority: parts.priority,
            dedupe_key: parts.dedupe_key,
            dispatch_epoch: parts.dispatch_epoch,
            status: parts.status,
            available_at: parts.available_at,
            attempt_count: parts.attempt_count,
            max_attempts: parts.max_attempts,
            last_error: parts.last_error,
            claim_token: parts.claim_token,
            claimed_by: parts.claimed_by,
            lease_until: parts.lease_until,
            dispatch_instance_id: parts.dispatch_instance_id,
            run_status: parts.run_status,
            termination: parts.termination,
            run_response: parts.run_response,
            run_error: parts.run_error,
            completed_at: parts.completed_at,
            created_at: parts.created_at,
            updated_at: parts.updated_at,
        };
        dispatch.validate_for_persist()?;
        Ok(dispatch)
    }

    #[must_use]
    pub fn to_persisted_parts(&self) -> RunDispatchParts {
        RunDispatchParts {
            dispatch_id: self.dispatch_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            priority: self.priority,
            dedupe_key: self.dedupe_key.clone(),
            dispatch_epoch: self.dispatch_epoch,
            status: self.status,
            available_at: self.available_at,
            attempt_count: self.attempt_count,
            max_attempts: self.max_attempts,
            last_error: self.last_error.clone(),
            claim_token: self.claim_token.clone(),
            claimed_by: self.claimed_by.clone(),
            lease_until: self.lease_until,
            dispatch_instance_id: self.dispatch_instance_id.clone(),
            run_status: self.run_status,
            termination: self.termination.clone(),
            run_response: self.run_response.clone(),
            run_error: self.run_error.clone(),
            completed_at: self.completed_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    #[must_use]
    pub fn dispatch_id(&self) -> &String {
        &self.dispatch_id
    }

    #[must_use]
    pub fn thread_id(&self) -> &String {
        &self.thread_id
    }

    #[must_use]
    pub fn run_id(&self) -> &String {
        &self.run_id
    }

    #[must_use]
    pub fn priority(&self) -> u8 {
        self.priority
    }

    #[must_use]
    pub fn dedupe_key(&self) -> Option<&str> {
        self.dedupe_key.as_deref()
    }

    #[must_use]
    pub fn dispatch_epoch(&self) -> u64 {
        self.dispatch_epoch
    }

    #[must_use]
    pub fn status(&self) -> RunDispatchStatus {
        self.status
    }

    #[must_use]
    pub fn available_at(&self) -> u64 {
        self.available_at
    }

    #[must_use]
    pub fn attempt_count(&self) -> u32 {
        self.attempt_count
    }

    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    #[must_use]
    pub fn claim_token(&self) -> Option<&str> {
        self.claim_token.as_deref()
    }

    #[must_use]
    pub fn claimed_by(&self) -> Option<&str> {
        self.claimed_by.as_deref()
    }

    #[must_use]
    pub fn lease_until(&self) -> Option<u64> {
        self.lease_until
    }

    #[must_use]
    pub fn dispatch_instance_id(&self) -> Option<&str> {
        self.dispatch_instance_id.as_deref()
    }

    #[must_use]
    pub fn run_status(&self) -> Option<RunStatus> {
        self.run_status
    }

    #[must_use]
    pub fn termination(&self) -> Option<&TerminationReason> {
        self.termination.as_ref()
    }

    #[must_use]
    pub fn run_response(&self) -> Option<&str> {
        self.run_response.as_deref()
    }

    #[must_use]
    pub fn run_error(&self) -> Option<&str> {
        self.run_error.as_deref()
    }

    #[must_use]
    pub fn completed_at(&self) -> Option<u64> {
        self.completed_at
    }

    #[must_use]
    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    #[must_use]
    pub fn updated_at(&self) -> u64 {
        self.updated_at
    }

    /// Store enqueue normalization: bind the dispatch to the current thread
    /// epoch and clear all runtime/terminal state.
    pub fn prepare_for_enqueue(&mut self, dispatch_epoch: u64) {
        self.dispatch_epoch = dispatch_epoch;
        self.status = RunDispatchStatus::Queued;
        self.claim_token = None;
        self.claimed_by = None;
        self.lease_until = None;
        self.dispatch_instance_id = None;
        self.run_status = None;
        self.termination = None;
        self.run_response = None;
        self.run_error = None;
        self.completed_at = None;
    }

    /// Transition a queued dispatch to claimed.
    pub fn claim(
        &mut self,
        consumer_id: impl Into<String>,
        claim_token: impl Into<String>,
        lease_until: u64,
        now: u64,
    ) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Queued, "claim")?;
        self.status = RunDispatchStatus::Claimed;
        self.claim_token = Some(claim_token.into());
        self.claimed_by = Some(consumer_id.into());
        self.lease_until = Some(lease_until);
        self.updated_at = now;
        self.validate_for_persist()
    }

    pub fn extend_lease(&mut self, lease_until: u64, now: u64) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "lease extension")?;
        self.lease_until = Some(lease_until);
        self.updated_at = now;
        self.validate_for_persist()
    }

    pub fn record_dispatch_start(
        &mut self,
        dispatch_instance_id: impl Into<String>,
        now: u64,
    ) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "recording runtime start")?;
        self.dispatch_instance_id = Some(dispatch_instance_id.into());
        self.run_status = Some(RunStatus::Running);
        self.termination = None;
        self.run_response = None;
        self.run_error = None;
        self.completed_at = None;
        self.updated_at = now;
        self.validate_for_persist()
    }

    pub fn record_run_result(
        &mut self,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "recording runtime result")?;
        if result.run_id != self.run_id {
            return Err(StorageError::Validation(format!(
                "dispatch '{}' result run_id '{}' does not match '{}'",
                self.dispatch_id, result.run_id, self.run_id
            )));
        }
        self.dispatch_instance_id = Some(result.dispatch_instance_id.clone());
        self.run_status = Some(result.status);
        self.termination = result.termination.clone();
        self.run_response = result.response.clone();
        self.run_error = result.error.clone();
        self.completed_at = Some(now);
        self.updated_at = now;
        self.validate_for_persist()
    }

    pub fn mark_acked(&mut self, now: u64) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "ack")?;
        self.status = RunDispatchStatus::Acked;
        self.completed_at = Some(now);
        self.updated_at = now;
        self.clear_claim_fields();
        self.validate_for_persist()
    }

    pub fn mark_cancelled(&mut self, now: u64) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Queued, "cancel")?;
        self.status = RunDispatchStatus::Cancelled;
        self.completed_at = Some(now);
        self.updated_at = now;
        self.clear_claim_fields();
        self.validate_for_persist()
    }

    pub fn mark_superseded(&mut self, now: u64, reason: Option<&str>) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Queued, "supersede")?;
        self.status = RunDispatchStatus::Superseded;
        self.completed_at = Some(now);
        self.updated_at = now;
        if let Some(reason) = reason {
            self.last_error = Some(reason.to_string());
        }
        self.clear_claim_fields();
        self.clear_runtime_projection();
        self.validate_for_persist()
    }

    pub fn mark_superseded_at_epoch(
        &mut self,
        now: u64,
        epoch: u64,
        reason: Option<&str>,
    ) -> Result<(), StorageError> {
        self.dispatch_epoch = epoch;
        match self.status {
            RunDispatchStatus::Queued => self.mark_superseded(now, reason),
            RunDispatchStatus::Claimed => {
                self.status = RunDispatchStatus::Superseded;
                self.completed_at = Some(now);
                self.updated_at = now;
                if let Some(reason) = reason {
                    self.last_error = Some(reason.to_string());
                }
                self.clear_claim_fields();
                self.clear_runtime_projection();
                self.validate_for_persist()
            }
            _ => Err(StorageError::Validation(format!(
                "dispatch '{}' must be Queued or Claimed before epoch supersede",
                self.dispatch_id
            ))),
        }
    }

    pub fn mark_dead_letter(&mut self, now: u64, error: &str) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "dead letter")?;
        self.status = RunDispatchStatus::DeadLetter;
        self.last_error = Some(error.to_string());
        self.completed_at = Some(now);
        self.updated_at = now;
        self.clear_claim_fields();
        self.validate_for_persist()
    }

    pub fn mark_nack_result(
        &mut self,
        now: u64,
        retry_at: u64,
        error: &str,
    ) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "nack")?;
        self.attempt_count = self.attempt_count.saturating_add(1);
        self.last_error = Some(error.to_string());
        self.updated_at = now;
        self.clear_claim_fields();
        if self.attempt_count >= self.max_attempts {
            self.status = RunDispatchStatus::DeadLetter;
            self.completed_at = Some(now);
        } else {
            self.status = RunDispatchStatus::Queued;
            self.available_at = retry_at;
            self.completed_at = None;
            self.clear_runtime_projection();
        }
        self.validate_for_persist()
    }

    pub fn mark_expired_lease(
        &mut self,
        now: u64,
        max_attempts_error: &str,
    ) -> Result<(), StorageError> {
        self.require_status(RunDispatchStatus::Claimed, "lease expiration")?;
        self.attempt_count = self.attempt_count.saturating_add(1);
        self.available_at = now;
        self.updated_at = now;
        self.clear_claim_fields();
        // A lease expiry abandons the in-flight attempt without a terminal run
        // result, so the runtime projection (e.g. run_status=Running,
        // dispatch_instance_id) is stale on every outcome — retry and the
        // max-attempt dead-letter alike. Clear it unconditionally so a terminal
        // DeadLetter dispatch can never project the abandoned attempt as Running.
        self.clear_runtime_projection();
        if self.attempt_count >= self.max_attempts {
            self.status = RunDispatchStatus::DeadLetter;
            self.last_error = Some(max_attempts_error.to_string());
            self.completed_at = Some(now);
        } else {
            self.status = RunDispatchStatus::Queued;
            self.completed_at = None;
        }
        self.validate_for_persist()
    }

    pub fn remap_identity(
        &mut self,
        dispatch_id: impl Into<String>,
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        dedupe_key: Option<String>,
    ) {
        self.dispatch_id = dispatch_id.into();
        self.thread_id = thread_id.into();
        self.run_id = run_id.into();
        self.dedupe_key = dedupe_key;
    }

    fn clear_claim_fields(&mut self) {
        self.claim_token = None;
        self.claimed_by = None;
        self.lease_until = None;
    }

    fn clear_runtime_projection(&mut self) {
        self.dispatch_instance_id = None;
        self.run_status = None;
        self.termination = None;
        self.run_response = None;
        self.run_error = None;
    }

    fn require_status(
        &self,
        expected: RunDispatchStatus,
        transition: &str,
    ) -> Result<(), StorageError> {
        if self.status != expected {
            return Err(StorageError::Validation(format!(
                "dispatch '{}' must be {:?} before {transition}",
                self.dispatch_id, expected
            )));
        }
        Ok(())
    }

    /// Validate an externally supplied dispatch before queue admission.
    pub fn validate_for_enqueue(&self) -> Result<(), StorageError> {
        self.validate_identity_and_retry()?;
        if self.status != RunDispatchStatus::Queued {
            return Err(StorageError::Validation(format!(
                "enqueued dispatch '{}' must start as Queued",
                self.dispatch_id
            )));
        }
        self.validate_queued()
    }

    /// Validate persisted dispatch lifecycle invariants.
    pub fn validate_for_persist(&self) -> Result<(), StorageError> {
        self.validate_identity_and_retry()?;
        match self.status {
            RunDispatchStatus::Queued => self.validate_queued(),
            RunDispatchStatus::Claimed => {
                if self
                    .claim_token
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
                    || self
                        .claimed_by
                        .as_deref()
                        .is_none_or(|value| value.trim().is_empty())
                    || self.lease_until.is_none()
                {
                    return Err(StorageError::Validation(format!(
                        "Claimed dispatch '{}' must carry claim_token, claimed_by, and lease_until",
                        self.dispatch_id
                    )));
                }
                Ok(())
            }
            RunDispatchStatus::Acked
            | RunDispatchStatus::Cancelled
            | RunDispatchStatus::Superseded
            | RunDispatchStatus::DeadLetter => {
                if self.claim_token.is_some()
                    || self.claimed_by.is_some()
                    || self.lease_until.is_some()
                {
                    return Err(StorageError::Validation(format!(
                        "{:?} dispatch '{}' must not carry active lease fields",
                        self.status, self.dispatch_id
                    )));
                }
                if self.completed_at.is_none() {
                    return Err(StorageError::Validation(format!(
                        "{:?} dispatch '{}' must carry completed_at",
                        self.status, self.dispatch_id
                    )));
                }
                Ok(())
            }
        }
    }

    fn validate_identity_and_retry(&self) -> Result<(), StorageError> {
        require_non_empty("dispatch_id", &self.dispatch_id)?;
        require_non_empty("thread_id", &self.thread_id)?;
        require_non_empty("run_id", &self.run_id)?;
        if self.max_attempts == 0 {
            return Err(StorageError::Validation(format!(
                "dispatch '{}' max_attempts must be greater than zero",
                self.dispatch_id
            )));
        }
        if self.attempt_count > self.max_attempts {
            return Err(StorageError::Validation(format!(
                "dispatch '{}' attempt_count must not exceed max_attempts",
                self.dispatch_id
            )));
        }
        Ok(())
    }

    fn validate_queued(&self) -> Result<(), StorageError> {
        if self.claim_token.is_some() || self.claimed_by.is_some() || self.lease_until.is_some() {
            return Err(StorageError::Validation(format!(
                "Queued dispatch '{}' must not carry claim fields",
                self.dispatch_id
            )));
        }
        if self.completed_at.is_some() {
            return Err(StorageError::Validation(format!(
                "Queued dispatch '{}' must not carry completed_at",
                self.dispatch_id
            )));
        }
        if self.dispatch_instance_id.is_some()
            || self.run_status.is_some()
            || self.termination.is_some()
            || self.run_response.is_some()
            || self.run_error.is_some()
        {
            return Err(StorageError::Validation(format!(
                "Queued dispatch '{}' must not carry runtime result fields",
                self.dispatch_id
            )));
        }
        Ok(())
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(StorageError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

// ── MailboxInterrupt ────────────────────────────────────────────────

/// Result of a mailbox interrupt operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInterrupt {
    /// New thread dispatch epoch after bump.
    pub new_dispatch_epoch: u64,
    /// The dispatch that was Claimed (running) at interrupt time, if any.
    /// Caller should cancel the corresponding runtime run.
    pub active_dispatch: Option<RunDispatch>,
    /// Number of Queued dispatches superseded.
    pub superseded_count: usize,
}

/// Detailed result of a mailbox interrupt operation.
///
/// `MailboxInterrupt` intentionally keeps the 0.2 public struct shape so
/// downstream struct literals remain source-compatible. New code that needs the
/// exact superseded dispatch records should use this type via
/// [`MailboxStore::interrupt_detailed`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInterruptDetails {
    /// New thread dispatch epoch after bump.
    pub new_dispatch_epoch: u64,
    /// The dispatch that was Claimed (running) at interrupt time, if any.
    /// Caller should cancel the corresponding runtime run.
    pub active_dispatch: Option<RunDispatch>,
    /// Number of Queued dispatches superseded.
    pub superseded_count: usize,
    /// Queued dispatches that were atomically superseded by this interrupt.
    ///
    /// This is the authoritative set callers should use to reconcile terminal
    /// dispatch state back to the durable run lifecycle.
    #[serde(default)]
    pub superseded_dispatches: Vec<RunDispatch>,
}

impl MailboxInterruptDetails {
    #[must_use]
    pub fn into_summary(self) -> MailboxInterrupt {
        MailboxInterrupt {
            new_dispatch_epoch: self.new_dispatch_epoch,
            active_dispatch: self.active_dispatch,
            superseded_count: self.superseded_count,
        }
    }

    #[must_use]
    pub fn summary(&self) -> MailboxInterrupt {
        MailboxInterrupt {
            new_dispatch_epoch: self.new_dispatch_epoch,
            active_dispatch: self.active_dispatch.clone(),
            superseded_count: self.superseded_count,
        }
    }
}

impl From<MailboxInterrupt> for MailboxInterruptDetails {
    fn from(interrupt: MailboxInterrupt) -> Self {
        Self {
            new_dispatch_epoch: interrupt.new_dispatch_epoch,
            active_dispatch: interrupt.active_dispatch,
            superseded_count: interrupt.superseded_count,
            superseded_dispatches: Vec::new(),
        }
    }
}

impl From<MailboxInterruptDetails> for MailboxInterrupt {
    fn from(details: MailboxInterruptDetails) -> Self {
        details.into_summary()
    }
}

pub use awaken_runtime_contract::contract::live_control::{
    LiveCommandReceipt, LiveControlError, LiveDeliveryOutcome, LiveRunCommand, LiveRunCommandEntry,
    LiveRunCommandSource, LiveRunCommandStream, LiveRunTarget,
};

// ── DispatchSignal ─────────────────────────────────────────────────────────

/// Receipt for a durable dispatch delivery signal.
///
/// Implementations should ack only after the scheduler has attempted to claim
/// the indicated thread. Nack requests redelivery when the scheduler cannot
/// safely make a claim decision.
#[async_trait]
pub trait DispatchSignalReceipt: Send + Sync {
    fn redelivery_attempts(&self) -> Option<u64> {
        None
    }

    async fn ack(self: Box<Self>) -> Result<(), StorageError>;
    async fn nack(self: Box<Self>) -> Result<(), StorageError>;
    async fn nack_with_delay(self: Box<Self>, delay: Duration) -> Result<(), StorageError> {
        let _ = delay;
        self.nack().await
    }
}

/// One durable dispatch delivery signal pulled from a backend work queue.
pub struct DispatchSignalEntry {
    pub thread_id: String,
    pub dispatch_id: String,
    pub receipt: Box<dyn DispatchSignalReceipt>,
}

impl std::fmt::Debug for DispatchSignalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchSignalEntry")
            .field("thread_id", &self.thread_id)
            .field("dispatch_id", &self.dispatch_id)
            .finish_non_exhaustive()
    }
}

// ── MailboxStore trait ──────────────────────────────────────────────

/// Persistent mailbox queue with lease-based distributed claim.
///
/// Implementations must guarantee:
/// - enqueue is durable before returning
/// - claim is atomic (exactly one consumer wins)
/// - interrupt atomically bumps dispatch epoch + supersedes stale dispatches
/// - ack/nack/dead_letter validate claim_token (reject stale claims)
#[async_trait]
pub trait MailboxStore: Send + Sync {
    // ── write path ──

    /// Persist a dispatch. Sets dispatch epoch from current thread state
    /// (auto-creates state if first dispatch for this thread_id).
    /// Rejects if dedupe_key matches an existing non-terminal dispatch.
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError>;

    /// Atomically claim up to `limit` Queued dispatches for a thread
    /// where `available_at <= now`. Sets status=Claimed, claim_token,
    /// claimed_by, lease_until = now + lease_ms.
    /// Returns claimed dispatches ordered by (priority ASC, created_at ASC).
    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError>;

    /// Claim a specific dispatch by dispatch_id. Same semantics as `claim()`
    /// but targets a single known dispatch (used for inline streaming).
    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError>;

    /// Mark mailbox delivery as consumed and no longer retryable.
    ///
    /// This validates `claim_token` and only records dispatch consumption. Use
    /// `record_run_result` for the agent run outcome.
    async fn ack(&self, dispatch_id: &str, claim_token: &str, now: u64)
    -> Result<(), StorageError>;

    /// Record the runtime dispatch identity for a claimed dispatch.
    ///
    /// Implementations should validate the claim token and set
    /// `run_status=Running`, while clearing any prior terminal result fields.
    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Record the runtime result for a claimed dispatch.
    ///
    /// This is intentionally separate from `ack`: `Acked` means the mailbox
    /// delivery was consumed, while these fields describe the agent run outcome.
    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Return dispatch to queue for retry. Sets available_at = retry_at,
    /// increments attempt_count, records error.
    /// If attempt_count >= max_attempts, transitions to DeadLetter instead.
    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Permanently fail a dispatch. Terminal state.
    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Cancel a specific dispatch. Works on Queued dispatches only.
    /// For Claimed dispatches, caller must also cancel the runtime run.
    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError>;

    /// Extend an active lease. Returns false if dispatch not Claimed
    /// or claim_token mismatch (lease already expired and reclaimed).
    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError>;

    // ── interrupt ──

    /// Atomically: bump dispatch epoch, supersede stale Queued dispatches,
    /// return the Claimed dispatch (if any) so caller can cancel its runtime run.
    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError>;

    /// Detailed interrupt result including the exact queued dispatches that
    /// were superseded.
    ///
    /// The default delegates to the 0.2-compatible summary method. Stores that
    /// can return authoritative superseded records should override this method.
    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        self.interrupt(thread_id, now).await.map(Into::into)
    }

    /// Return the authoritative dispatch epoch for a thread.
    ///
    /// Implementations that do not persist epochs may keep the default `0`;
    /// production mailbox stores must override this so dispatch workers can
    /// reject claimed work that became stale after an interrupt.
    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        let _ = thread_id;
        Ok(0)
    }

    /// Terminalize a claimed dispatch as superseded.
    ///
    /// Used when an interrupt wins the race after a dispatch was claimed but
    /// before (or while) the runtime starts. Implementations must validate the
    /// claim token and clear lease/claim ownership. Returning `Ok(None)` means
    /// the dispatch is gone or no longer claimed.
    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let _ = (dispatch_id, claim_token, now, reason);
        Err(StorageError::Io(
            "supersede claimed dispatch is not supported by this mailbox store".into(),
        ))
    }

    // ── read path ──

    /// Load a single dispatch by ID.
    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError>;

    /// List dispatches for a thread, filtered by status.
    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError>;

    /// Count dispatches by status for low-cardinality operational gauges.
    ///
    /// Implementations that cannot provide an efficient count may return a
    /// storage error; callers must treat this as a metrics-only failure.
    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        let _ = status;
        Err(StorageError::Io(
            "count dispatches by status is not supported by this mailbox store".into(),
        ))
    }

    /// List terminal dispatches across all threads.
    ///
    /// Used by recovery/maintenance reconciliation to repair run lifecycle
    /// records after a process crashes between a mailbox terminal transition
    /// and the corresponding run-store checkpoint.
    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let _ = (limit, offset);
        Err(StorageError::Io(
            "list terminal dispatches is not supported by this mailbox store".into(),
        ))
    }

    // ── maintenance ──

    /// Reclaim dispatches whose lease_until < now (orphaned by crashed consumers).
    /// Resets to Queued with incremented attempt_count.
    /// Returns reclaimed dispatches for immediate execution.
    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError>;

    /// Purge terminal dispatches (Acked, Cancelled, Superseded, DeadLetter)
    /// older than `older_than` timestamp. Returns count purged.
    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError>;

    /// List distinct thread_ids that have at least one Queued dispatch.
    /// Used by recover() at startup.
    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError>;

    // ── dispatch signals (durable wakeups) ──

    /// Whether this store exposes durable dispatch delivery signals.
    fn supports_dispatch_signals(&self) -> bool {
        false
    }

    /// Pull durable dispatch delivery signals, if supported by the backend.
    ///
    /// The default is empty so non-work-queue stores continue relying on local
    /// submit, startup recovery, and sweep.
    async fn pull_dispatch_signals(
        &self,
        max: usize,
        expires: Duration,
    ) -> Result<Vec<DispatchSignalEntry>, StorageError> {
        let _ = (max, expires);
        Ok(Vec::new())
    }

    // ── live-channel (ephemeral steering) ──
    //
    // Separate from durable dispatch: these deliver best-effort control
    // commands to whichever node currently owns the run. Default impls are
    // no-ops so stores that don't support live delivery (test fakes) opt out.

    /// Deliver a `LiveRunCommand` to the run currently active for `thread_id`.
    /// Implementations report `Delivered` when at least one subscriber has
    /// observed the command, or `NoSubscriber` when delivery would be a
    /// silent drop (the caller then owns durable-fallback policy). The
    /// default implementation is `NoSubscriber` so stores that opt out of
    /// live delivery force every caller to fall back automatically.
    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        let _ = (thread_id, cmd);
        Ok(LiveDeliveryOutcome::NoSubscriber)
    }

    /// Deliver a live command to an exact run target.
    ///
    /// Backends with targeted live subjects should override this. The default
    /// preserves compatibility for stores that only support thread-level live
    /// routing.
    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.deliver_live(&target.thread_id, cmd).await
    }

    /// Subscribe to the live-command stream for `thread_id`. Called by the
    /// runtime on the owning node when a run is registered.
    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        let _ = thread_id;
        Ok(Box::pin(futures::stream::empty()))
    }

    /// Subscribe to the live-command stream for an exact run target.
    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.open_live_channel(&target.thread_id).await
    }
}

/// Adapter exposing any [`MailboxStore`] as a runtime [`LiveRunCommandSource`].
///
/// The runtime consumes live commands through `LiveRunCommandSource` (defined
/// in runtime-contract); the mailbox store is the durable source of those
/// commands. With `MailboxStore` now living in server-contract, a blanket
/// `impl<T: MailboxStore> LiveRunCommandSource for T` would violate the orphan
/// rule (foreign trait over a generic type), so this concrete wrapper provides
/// the bridge instead.
pub struct MailboxLiveControlSource(Arc<dyn MailboxStore>);

impl MailboxLiveControlSource {
    pub fn new(store: Arc<dyn MailboxStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl LiveRunCommandSource for MailboxLiveControlSource {
    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, LiveControlError> {
        MailboxStore::open_live_channel_for(self.0.as_ref(), target)
            .await
            .map_err(|error| LiveControlError::Subscribe(error.to_string()))
    }
}

#[derive(Clone)]
pub struct ScopedMailboxStore {
    inner: Arc<dyn MailboxStore>,
    scope_id: ScopeId,
}

impl ScopedMailboxStore {
    pub fn new(inner: Arc<dyn MailboxStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn MailboxStore {
        self.inner.as_ref()
    }

    fn scoped(&self, id: &str) -> String {
        scoped_key(&self.scope_id, id)
    }

    fn unscoped<'a>(&self, id: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, id)
    }

    fn encode_dispatch(&self, dispatch: &RunDispatch) -> RunDispatch {
        let mut dispatch = dispatch.clone();
        dispatch.dispatch_id = self.scoped(&dispatch.dispatch_id);
        dispatch.thread_id = self.scoped(&dispatch.thread_id);
        dispatch.run_id = self.scoped(&dispatch.run_id);
        dispatch.dedupe_key = dispatch.dedupe_key.as_deref().map(|key| self.scoped(key));
        dispatch
    }

    fn decode_dispatch(&self, mut dispatch: RunDispatch) -> Option<RunDispatch> {
        dispatch.dispatch_id = self.unscoped(&dispatch.dispatch_id)?.to_string();
        dispatch.thread_id = self.unscoped(&dispatch.thread_id)?.to_string();
        dispatch.run_id = self.unscoped(&dispatch.run_id)?.to_string();
        dispatch.dedupe_key = dispatch
            .dedupe_key
            .as_deref()
            .map(|key| self.unscoped(key).map(str::to_string))
            .unwrap_or(None);
        Some(dispatch)
    }

    fn encode_target(&self, target: &LiveRunTarget) -> LiveRunTarget {
        LiveRunTarget {
            thread_id: self.scoped(&target.thread_id),
            run_id: self.scoped(&target.run_id),
            dispatch_id: target.dispatch_id.as_deref().map(|id| self.scoped(id)),
        }
    }

    fn encode_result(&self, result: &RunDispatchResult) -> RunDispatchResult {
        let mut result = result.clone();
        result.run_id = self.scoped(&result.run_id);
        result
    }
}

#[async_trait]
impl MailboxStore for ScopedMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        self.inner.enqueue(&self.encode_dispatch(dispatch)).await
    }

    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .claim(&self.scoped(thread_id), consumer_id, lease_ms, now, limit)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .claim_dispatch(&self.scoped(dispatch_id), consumer_id, lease_ms, now)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn ack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .ack(&self.scoped(dispatch_id), claim_token, now)
            .await
    }

    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .record_dispatch_start(
                &self.scoped(dispatch_id),
                claim_token,
                dispatch_instance_id,
                now,
            )
            .await
    }

    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .record_run_result(
                &self.scoped(dispatch_id),
                claim_token,
                &self.encode_result(result),
                now,
            )
            .await
    }

    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .nack(&self.scoped(dispatch_id), claim_token, retry_at, error, now)
            .await
    }

    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .dead_letter(&self.scoped(dispatch_id), claim_token, error, now)
            .await
    }

    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .cancel(&self.scoped(dispatch_id), now)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        self.inner
            .extend_lease(&self.scoped(dispatch_id), claim_token, extension_ms, now)
            .await
    }

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        let interrupt = self.inner.interrupt(&self.scoped(thread_id), now).await?;
        Ok(MailboxInterrupt {
            new_dispatch_epoch: interrupt.new_dispatch_epoch,
            active_dispatch: interrupt
                .active_dispatch
                .and_then(|dispatch| self.decode_dispatch(dispatch)),
            superseded_count: interrupt.superseded_count,
        })
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        let details = self
            .inner
            .interrupt_detailed(&self.scoped(thread_id), now)
            .await?;
        let superseded_dispatches: Vec<_> = details
            .superseded_dispatches
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect();
        Ok(MailboxInterruptDetails {
            new_dispatch_epoch: details.new_dispatch_epoch,
            active_dispatch: details
                .active_dispatch
                .and_then(|dispatch| self.decode_dispatch(dispatch)),
            superseded_count: superseded_dispatches.len(),
            superseded_dispatches,
        })
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        self.inner
            .current_dispatch_epoch(&self.scoped(thread_id))
            .await
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .supersede_claimed(&self.scoped(dispatch_id), claim_token, now, reason)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .load_dispatch(&self.scoped(dispatch_id))
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .list_dispatches(&self.scoped(thread_id), status_filter, limit, offset)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        match status {
            RunDispatchStatus::Queued => {
                let mut total = 0;
                for thread_id in self.queued_thread_ids().await? {
                    total += self
                        .list_dispatches(
                            &thread_id,
                            Some(&[RunDispatchStatus::Queued]),
                            usize::MAX,
                            0,
                        )
                        .await?
                        .len();
                }
                Ok(total)
            }
            status if status.is_terminal() => Ok(self
                .list_terminal_dispatches(usize::MAX, 0)
                .await?
                .into_iter()
                .filter(|dispatch| dispatch.status == status)
                .count()),
            _ => Err(StorageError::Io(
                "scoped claimed dispatch count is not supported".into(),
            )),
        }
    }

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let all: Vec<_> = self
            .inner
            .list_terminal_dispatches(usize::MAX, 0)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect();
        Ok(all.into_iter().skip(offset).take(limit).collect())
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .reclaim_expired_leases(now, limit)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn purge_terminal(&self, _older_than: u64) -> Result<usize, StorageError> {
        Err(StorageError::Io(
            "scoped terminal dispatch purge is not supported".into(),
        ))
    }

    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        Ok(self
            .inner
            .queued_thread_ids()
            .await?
            .into_iter()
            .filter_map(|thread_id| self.unscoped(&thread_id).map(str::to_string))
            .collect())
    }

    fn supports_dispatch_signals(&self) -> bool {
        self.inner.supports_dispatch_signals()
    }

    async fn pull_dispatch_signals(
        &self,
        max: usize,
        expires: Duration,
    ) -> Result<Vec<DispatchSignalEntry>, StorageError> {
        Ok(self
            .inner
            .pull_dispatch_signals(max, expires)
            .await?
            .into_iter()
            .filter_map(|entry| {
                Some(DispatchSignalEntry {
                    thread_id: self.unscoped(&entry.thread_id)?.to_string(),
                    dispatch_id: self.unscoped(&entry.dispatch_id)?.to_string(),
                    receipt: entry.receipt,
                })
            })
            .collect())
    }

    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.inner.deliver_live(&self.scoped(thread_id), cmd).await
    }

    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.inner
            .deliver_live_to(&self.encode_target(target), cmd)
            .await
    }

    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.inner.open_live_channel(&self.scoped(thread_id)).await
    }

    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.inner
            .open_live_channel_for(&self.encode_target(target))
            .await
    }
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
