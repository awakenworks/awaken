//! Durable event sink wiring for canonical event capture.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::commit_coordinator::CanonicalEventStager;
use super::event::AgentEvent;
use super::event_sink::EventSink;
use super::event_store::{
    CanonicalEventDraft, CanonicalEventKind, EventScope, EventStoreError, EventVisibility,
    FidelityClass,
};
use super::lifecycle::TerminationReason;
use super::suspension::{ToolCallOutcome, ToolCallResumeMode};
use super::tool::ToolStatus;

mod compaction;
use compaction::CompactionObservation;

/// Runtime event durability mode used by [`DurableEventSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventDurability {
    /// Do not persist runtime events.
    Disabled,
    /// Persist committed, domain, and control events; skip streaming observations.
    Compacted,
    /// Persist all normalized runtime events, including streaming observations.
    FullFidelity,
}

impl RuntimeEventDurability {
    /// Return whether an event with `fidelity` should be appended.
    #[must_use]
    pub const fn should_persist(self, fidelity: FidelityClass) -> bool {
        match self {
            Self::Disabled => false,
            Self::Compacted => !matches!(fidelity, FidelityClass::ObservedRuntimeEvent),
            Self::FullFidelity => true,
        }
    }
}

/// A normalized canonical event draft with its durability class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedCanonicalEvent {
    pub fidelity: FidelityClass,
    pub draft: CanonicalEventDraft,
}

impl NormalizedCanonicalEvent {
    /// Build a normalized event after validating the draft.
    pub fn new(
        fidelity: FidelityClass,
        draft: CanonicalEventDraft,
    ) -> Result<Self, EventStoreError> {
        draft.validate()?;
        Ok(Self { fidelity, draft })
    }
}

/// Converts runtime [`AgentEvent`]s to protocol-neutral canonical event drafts.
pub trait AgentEventNormalizer: Send + Sync {
    /// Normalize one runtime event.
    ///
    /// Returning `None` means the event is intentionally not represented as a
    /// canonical fact. Errors indicate a durable capture failure.
    fn normalize(
        &self,
        event: &AgentEvent,
    ) -> Result<Option<NormalizedCanonicalEvent>, EventStoreError>;

    /// Normalize one runtime event into one or more canonical facts.
    ///
    /// The default preserves the original one-event contract. Normalizers may
    /// override this for coarse companion facts such as permission request
    /// events derived from the same runtime boundary.
    fn normalize_many(
        &self,
        event: &AgentEvent,
    ) -> Result<Vec<NormalizedCanonicalEvent>, EventStoreError> {
        Ok(self.normalize(event)?.into_iter().collect())
    }
}

/// Scope and metadata used when normalizing runtime events that do not carry
/// thread or run identifiers themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEventNormalizationContext {
    pub thread_id: String,
    pub run_id: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl AgentEventNormalizationContext {
    /// Create a normalization context for one thread/run pair.
    pub fn new(
        thread_id: impl Into<String>,
        run_id: impl Into<String>,
        origin: impl Into<String>,
    ) -> Result<Self, EventStoreError> {
        let context = Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            origin: origin.into(),
            correlation_id: None,
        };
        context.validate()?;
        Ok(context)
    }

    /// Attach a correlation id used for tracing and diagnostics.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }

    fn validate(&self) -> Result<(), EventStoreError> {
        reject_blank("thread_id", &self.thread_id)?;
        reject_blank("run_id", &self.run_id)?;
        reject_blank("origin", &self.origin)?;
        Ok(())
    }
}

/// Default normalizer for one runtime stream.
#[derive(Debug)]
pub struct ScopedAgentEventNormalizer {
    context: AgentEventNormalizationContext,
    started_runs: Mutex<BTreeSet<String>>,
    terminal_runs: Mutex<BTreeSet<String>>,
    compaction: Mutex<CompactionObservation>,
}

impl ScopedAgentEventNormalizer {
    /// Create a normalizer for one thread/run stream.
    #[must_use]
    pub fn new(context: AgentEventNormalizationContext) -> Self {
        Self {
            context,
            started_runs: Mutex::new(BTreeSet::new()),
            terminal_runs: Mutex::new(BTreeSet::new()),
            compaction: Mutex::new(CompactionObservation::default()),
        }
    }

    /// Create a normalizer for a stream that resumes an already-started run.
    #[must_use]
    pub fn new_resumed(context: AgentEventNormalizationContext) -> Self {
        let run_id = context.run_id.clone();
        let normalizer = Self::new(context);
        normalizer.started_runs.lock().insert(run_id);
        normalizer
    }

    fn scopes_for(&self, thread_id: &str, run_id: &str) -> Vec<EventScope> {
        vec![EventScope::thread(thread_id), EventScope::run(run_id)]
    }

    fn context_scopes(&self) -> Vec<EventScope> {
        self.scopes_for(&self.context.thread_id, &self.context.run_id)
    }

    fn build(
        &self,
        fidelity: FidelityClass,
        event_kind: &str,
        scopes: Vec<EventScope>,
        payload: Value,
    ) -> Result<NormalizedCanonicalEvent, EventStoreError> {
        let mut draft = CanonicalEventDraft::new(
            scopes,
            CanonicalEventKind::new(event_kind)?,
            payload,
            self.context.origin.clone(),
        )?;
        draft.visibility = EventVisibility::Public;
        draft.correlation_id = self.context.correlation_id.clone();
        NormalizedCanonicalEvent::new(fidelity, draft)
    }
}

impl AgentEventNormalizer for ScopedAgentEventNormalizer {
    fn normalize_many(
        &self,
        event: &AgentEvent,
    ) -> Result<Vec<NormalizedCanonicalEvent>, EventStoreError> {
        let mut events = self.normalize(event)?.into_iter().collect::<Vec<_>>();
        if let Some(permission_requested) = self.tool_permission_requested(event)? {
            events.push(permission_requested);
        }
        events.extend(self.context_compaction_events(event)?);
        Ok(events)
    }

    fn normalize(
        &self,
        event: &AgentEvent,
    ) -> Result<Option<NormalizedCanonicalEvent>, EventStoreError> {
        let (fidelity, kind, scopes) = match event {
            AgentEvent::RunStart {
                thread_id, run_id, ..
            } => {
                let kind = {
                    let mut started = self.started_runs.lock();
                    if started.insert(run_id.clone()) {
                        "RunStarted"
                    } else {
                        "RunResumed"
                    }
                };
                (
                    FidelityClass::DomainEvent,
                    kind,
                    self.scopes_for(thread_id, run_id),
                )
            }
            AgentEvent::RunFinish {
                thread_id,
                run_id,
                termination,
                ..
            } => {
                let already_terminal = {
                    let mut terminal = self.terminal_runs.lock();
                    !terminal.insert(run_id.clone())
                };
                if already_terminal {
                    return Ok(None);
                }
                (
                    FidelityClass::DomainEvent,
                    run_finish_kind(termination),
                    self.scopes_for(thread_id, run_id),
                )
            }
            AgentEvent::TextDelta { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "TextDeltaObserved",
                self.context_scopes(),
            ),
            AgentEvent::ReasoningDelta { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ReasoningDeltaObserved",
                self.context_scopes(),
            ),
            AgentEvent::ReasoningEncryptedValue { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ReasoningEncryptedValueObserved",
                self.context_scopes(),
            ),
            AgentEvent::ToolCallStart { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ToolCallStarted",
                self.context_scopes(),
            ),
            AgentEvent::ToolCallDelta { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ToolCallDeltaObserved",
                self.context_scopes(),
            ),
            AgentEvent::ToolCallReady { .. } => (
                FidelityClass::CommittedRuntimeEvent,
                "ToolCallReady",
                self.context_scopes(),
            ),
            AgentEvent::ToolCallDone {
                result, outcome, ..
            } => {
                // ADR-0034 D11: ToolCallSuspended/Rejected/TimedOut are
                // canonical lifecycle DomainEvents even though the source
                // AgentEvent::ToolCallDone is a CommittedRuntimeEvent.
                let (fidelity, kind) = if *outcome == ToolCallOutcome::Suspended {
                    (FidelityClass::DomainEvent, "ToolCallSuspended")
                } else if *outcome == ToolCallOutcome::Failed
                    && result.metadata.contains_key("rejected")
                {
                    (FidelityClass::DomainEvent, "ToolCallRejected")
                } else if *outcome == ToolCallOutcome::Failed
                    && result
                        .metadata
                        .get("timed_out")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    (FidelityClass::DomainEvent, "ToolCallTimedOut")
                } else {
                    (FidelityClass::CommittedRuntimeEvent, "ToolCallDone")
                };
                (fidelity, kind, self.context_scopes())
            }
            AgentEvent::ToolCallStreamDelta { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ToolCallStreamDeltaObserved",
                self.context_scopes(),
            ),
            AgentEvent::ToolCallResumed { .. } => (
                FidelityClass::ControlEvent,
                "ToolCallResumed",
                self.context_scopes(),
            ),
            AgentEvent::ToolCallCancel { .. } => (
                // ADR-0034 D11: ToolCallCancelled is a canonical lifecycle
                // DomainEvent.
                FidelityClass::DomainEvent,
                "ToolCallCancelled",
                self.context_scopes(),
            ),
            AgentEvent::StreamReset { .. } => (
                FidelityClass::CommittedRuntimeEvent,
                "StreamReset",
                self.context_scopes(),
            ),
            AgentEvent::StepStart { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "StepStarted",
                self.context_scopes(),
            ),
            AgentEvent::StepEnd => (
                FidelityClass::ObservedRuntimeEvent,
                "StepEnded",
                self.context_scopes(),
            ),
            AgentEvent::InferenceComplete { .. } => (
                FidelityClass::CommittedRuntimeEvent,
                "InferenceComplete",
                self.context_scopes(),
            ),
            AgentEvent::MessagesSnapshot { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "MessagesSnapshotObserved",
                self.context_scopes(),
            ),
            AgentEvent::ActivitySnapshot { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ActivitySnapshotObserved",
                self.context_scopes(),
            ),
            AgentEvent::ActivityDelta { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "ActivityDeltaObserved",
                self.context_scopes(),
            ),
            AgentEvent::StateSnapshot { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "StateSnapshotObserved",
                self.context_scopes(),
            ),
            AgentEvent::StateDelta { .. } => (
                FidelityClass::ObservedRuntimeEvent,
                "StateDeltaObserved",
                self.context_scopes(),
            ),
            AgentEvent::Error { .. } => (
                FidelityClass::CommittedRuntimeEvent,
                "ErrorRecorded",
                self.context_scopes(),
            ),
        };

        let payload = serde_json::to_value(event)
            .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
        self.build(fidelity, kind, scopes, payload).map(Some)
    }
}

impl ScopedAgentEventNormalizer {
    fn tool_permission_requested(
        &self,
        event: &AgentEvent,
    ) -> Result<Option<NormalizedCanonicalEvent>, EventStoreError> {
        let AgentEvent::ToolCallDone {
            result, outcome, ..
        } = event
        else {
            return Ok(None);
        };
        if *outcome != ToolCallOutcome::Suspended || result.status != ToolStatus::Pending {
            return Ok(None);
        }
        let Some(ticket) = result.suspension.as_ref() else {
            return Ok(None);
        };
        if ticket.resume_mode != ToolCallResumeMode::ReplayToolCall
            || ticket.suspension.id.trim().is_empty()
        {
            return Ok(None);
        }
        let payload = serde_json::to_value(event)
            .map_err(|error| EventStoreError::Serialization(error.to_string()))?;
        self.build(
            FidelityClass::DomainEvent,
            "ToolPermissionRequested",
            self.context_scopes(),
            payload,
        )
        .map(Some)
    }
}

/// EventSink wrapper that forwards live events and stages canonical drafts.
pub struct DurableEventSink {
    inner: Arc<dyn EventSink>,
    stager: Arc<dyn CanonicalEventStager>,
    normalizer: Arc<dyn AgentEventNormalizer>,
    mode: RuntimeEventDurability,
}

impl DurableEventSink {
    /// Create a durable sink wrapper.
    #[must_use]
    pub fn new(
        inner: Arc<dyn EventSink>,
        stager: Arc<dyn CanonicalEventStager>,
        normalizer: Arc<dyn AgentEventNormalizer>,
        mode: RuntimeEventDurability,
    ) -> Self {
        Self {
            inner,
            stager,
            normalizer,
            mode,
        }
    }
}

#[async_trait]
impl EventSink for DurableEventSink {
    async fn emit(&self, event: AgentEvent) {
        self.inner.emit(event.clone()).await;

        let normalized = match self.normalizer.normalize_many(&event) {
            Ok(normalized) => normalized,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "durable event sink normalizer failed; live event was forwarded without canonical staging"
                );
                return;
            }
        };

        for normalized in normalized {
            if !self.mode.should_persist(normalized.fidelity) {
                continue;
            }
            self.stager.stage(normalized.draft);
        }
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

fn run_finish_kind(termination: &TerminationReason) -> &'static str {
    match termination {
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => "RunFinished",
        TerminationReason::Suspended => "RunSuspended",
        TerminationReason::Cancelled => "RunCancelled",
        TerminationReason::Error(_) => "RunErrored",
        TerminationReason::Stopped(_) | TerminationReason::Blocked(_) => "RunTerminated",
    }
}

fn reject_blank(field: &str, value: &str) -> Result<(), EventStoreError> {
    if value.trim().is_empty() {
        return Err(EventStoreError::Validation(format!("{field} is required")));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
