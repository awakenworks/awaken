use serde::{Deserialize, Serialize};

use super::{Message, gen_message_id};

/// Runtime boundary at which a pending message may be consumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryBoundary {
    /// Interrupt the currently active run and consume immediately.
    Interrupt,
    /// Consume at the next loop step boundary.
    NextStep,
    /// Consume when the current run reaches its natural end.
    OnNaturalEnd,
    /// Consume by starting or resuming a queued run.
    #[default]
    NewRun,
}

impl DeliveryBoundary {
    /// Return true when a pending message targeting `self` is eligible at
    /// `current` after applying the ADR-0042 fallback cascade.
    #[must_use]
    pub fn eligible_at(self, current: Self) -> bool {
        self.precedence() <= current.precedence()
    }

    fn precedence(self) -> u8 {
        match self {
            Self::Interrupt => 0,
            Self::NextStep => 1,
            Self::OnNaturalEnd => 2,
            Self::NewRun => 3,
        }
    }
}

/// Number of eligible pending messages one freeze consumes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryGranularity {
    /// Consume one pending message per freeze.
    One,
    /// Consume all eligible pending messages per freeze.
    #[default]
    Batch,
}

/// Delivery policy attached to a pending message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryMode {
    #[serde(default)]
    pub boundary: DeliveryBoundary,
    #[serde(default)]
    pub granularity: DeliveryGranularity,
    #[serde(default, skip_serializing_if = "is_false")]
    pub barrier: bool,
}

impl DeliveryMode {
    /// Foreground interruption semantics.
    #[must_use]
    pub fn interrupt(granularity: DeliveryGranularity) -> Self {
        Self {
            boundary: DeliveryBoundary::Interrupt,
            granularity,
            barrier: false,
        }
    }

    /// Live steering semantics.
    #[must_use]
    pub fn next_step(granularity: DeliveryGranularity) -> Self {
        Self {
            boundary: DeliveryBoundary::NextStep,
            granularity,
            barrier: false,
        }
    }

    /// Continue the same run after natural completion.
    #[must_use]
    pub fn on_natural_end(granularity: DeliveryGranularity) -> Self {
        Self {
            boundary: DeliveryBoundary::OnNaturalEnd,
            granularity,
            barrier: false,
        }
    }

    /// Queue a new run.
    #[must_use]
    pub fn new_run(granularity: DeliveryGranularity) -> Self {
        Self {
            boundary: DeliveryBoundary::NewRun,
            granularity,
            barrier: false,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Delivered-but-unconsumed message staged for a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingMessageRecord {
    /// Stable pending identifier. Defaults to the message id when available.
    pub pending_id: String,
    /// Thread that owns the pending entry.
    pub thread_id: String,
    /// Mutable 1-based ordering position within the pending queue.
    pub position: u64,
    /// Message payload that will be appended to committed history on freeze.
    pub message: Message,
    /// Delivery policy used by freeze to select this entry.
    #[serde(default)]
    pub delivery_mode: DeliveryMode,
    /// Unix timestamp (seconds) when the message was delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    /// Unix timestamp (seconds) when the pending entry was last edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

impl PendingMessageRecord {
    /// Build a pending record from a delivered message.
    pub fn from_message(
        thread_id: impl Into<String>,
        position: u64,
        mut message: Message,
        delivery_mode: DeliveryMode,
    ) -> Self {
        let pending_id = message.id.clone().unwrap_or_else(gen_message_id);
        if message.id.is_none() {
            message.id = Some(pending_id.clone());
        }
        Self {
            pending_id,
            thread_id: thread_id.into(),
            position,
            message,
            delivery_mode,
            created_at: None,
            updated_at: None,
        }
    }
}

/// Select pending entries that a freeze should consume, returning their
/// indexes in ascending pending order.
#[must_use]
pub fn select_pending_for_freeze(
    pending: &[PendingMessageRecord],
    boundary: DeliveryBoundary,
) -> Vec<usize> {
    let mut selected = Vec::new();
    for (index, entry) in pending.iter().enumerate() {
        if !entry.delivery_mode.boundary.eligible_at(boundary) {
            break;
        }
        if !selected.is_empty() && entry.delivery_mode.granularity == DeliveryGranularity::One {
            break;
        }
        selected.push(index);
        if entry.delivery_mode.barrier
            || entry.delivery_mode.granularity == DeliveryGranularity::One
        {
            break;
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(
        id: &str,
        position: u64,
        boundary: DeliveryBoundary,
        granularity: DeliveryGranularity,
    ) -> PendingMessageRecord {
        PendingMessageRecord::from_message(
            "thread-1",
            position,
            Message::user(id).with_id(id.to_string()),
            DeliveryMode {
                boundary,
                granularity,
                barrier: false,
            },
        )
    }

    fn pending_with_barrier(
        id: &str,
        position: u64,
        boundary: DeliveryBoundary,
        granularity: DeliveryGranularity,
    ) -> PendingMessageRecord {
        let mut record = pending(id, position, boundary, granularity);
        record.delivery_mode.barrier = true;
        record
    }

    #[test]
    fn delivery_boundary_fallback_cascades_forward() {
        assert!(DeliveryBoundary::Interrupt.eligible_at(DeliveryBoundary::Interrupt));
        assert!(DeliveryBoundary::Interrupt.eligible_at(DeliveryBoundary::NextStep));
        assert!(DeliveryBoundary::NextStep.eligible_at(DeliveryBoundary::OnNaturalEnd));
        assert!(DeliveryBoundary::OnNaturalEnd.eligible_at(DeliveryBoundary::NewRun));
        assert!(DeliveryBoundary::NewRun.eligible_at(DeliveryBoundary::NewRun));
        assert!(!DeliveryBoundary::NewRun.eligible_at(DeliveryBoundary::OnNaturalEnd));
        assert!(!DeliveryBoundary::OnNaturalEnd.eligible_at(DeliveryBoundary::NextStep));
    }

    #[test]
    fn pending_record_uses_message_id_as_pending_id() {
        let record = PendingMessageRecord::from_message(
            "thread-1",
            1,
            Message::user("hello").with_id("msg-1".to_string()),
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        );
        assert_eq!(record.pending_id, "msg-1");
        assert_eq!(record.message.id.as_deref(), Some("msg-1"));
        assert_eq!(record.thread_id, "thread-1");
        assert_eq!(record.position, 1);
        assert_eq!(record.delivery_mode.boundary, DeliveryBoundary::NewRun);
    }

    #[test]
    fn pending_record_assigns_generated_id_to_message() {
        let record = PendingMessageRecord::from_message(
            "thread-1",
            1,
            Message::user("hello"),
            DeliveryMode::new_run(DeliveryGranularity::Batch),
        );
        assert_eq!(
            record.message.id.as_deref(),
            Some(record.pending_id.as_str())
        );
    }

    #[test]
    fn freeze_selection_takes_one_when_first_eligible_is_one() {
        let pending = vec![
            pending("a", 1, DeliveryBoundary::NewRun, DeliveryGranularity::One),
            pending("b", 2, DeliveryBoundary::NewRun, DeliveryGranularity::Batch),
        ];
        assert_eq!(
            select_pending_for_freeze(&pending, DeliveryBoundary::NewRun),
            vec![0]
        );
    }

    #[test]
    fn freeze_selection_batches_all_eligible_at_boundary() {
        let pending = vec![
            pending(
                "a",
                1,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
            pending(
                "b",
                2,
                DeliveryBoundary::OnNaturalEnd,
                DeliveryGranularity::Batch,
            ),
            pending("c", 3, DeliveryBoundary::NewRun, DeliveryGranularity::Batch),
        ];
        assert_eq!(
            select_pending_for_freeze(&pending, DeliveryBoundary::OnNaturalEnd),
            vec![0, 1]
        );
    }

    #[test]
    fn freeze_selection_barrier_does_not_flush_ineligible_prior_pending() {
        let pending = vec![
            pending("a", 1, DeliveryBoundary::NewRun, DeliveryGranularity::Batch),
            pending_with_barrier(
                "b",
                2,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
            pending(
                "c",
                3,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
        ];
        assert_eq!(
            select_pending_for_freeze(&pending, DeliveryBoundary::NextStep),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn freeze_selection_barrier_stops_batch_before_later_messages() {
        let pending = vec![
            pending(
                "a",
                1,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
            pending_with_barrier(
                "barrier",
                2,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
            pending(
                "c",
                3,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
        ];

        assert_eq!(
            select_pending_for_freeze(&pending, DeliveryBoundary::NextStep),
            vec![0, 1]
        );
    }

    #[test]
    fn freeze_selection_batch_stops_before_later_one_message() {
        let pending = vec![
            pending(
                "batch-1",
                1,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
            pending(
                "one",
                2,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::One,
            ),
            pending(
                "batch-2",
                3,
                DeliveryBoundary::NextStep,
                DeliveryGranularity::Batch,
            ),
        ];

        assert_eq!(
            select_pending_for_freeze(&pending, DeliveryBoundary::NextStep),
            vec![0]
        );
    }
}
