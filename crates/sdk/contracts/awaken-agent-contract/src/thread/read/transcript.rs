//! Neutral, immutable views over a committed Thread transcript.
//!
//! This module owns no Memory, Compact, Goal, or provider vocabulary. Extensions
//! decide *which* ranges they need; the transcript contract only freezes a
//! committed prefix and returns validated half-open ranges from it.

use std::num::NonZeroUsize;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agent::message::Message;
use crate::agent::thread::Id as ThreadId;

/// The two platform-level transcript views. `ContextEffective` initially
/// coincides with `RawCommitted`; durable compaction later supplies its fold
/// without changing any consumer's range contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptView {
    RawCommitted,
    ContextEffective,
}

/// A half-open ordinal range in one frozen transcript view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TranscriptRange {
    pub start: u64,
    pub end: u64,
}

impl TranscriptRange {
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }
}

/// Durable identity of one committed transcript prefix.
///
/// `version` is deliberately distinct from `end_seq` even though the first
/// implementation advances both by message count. Storage backends may later
/// use a native per-thread message version without changing serialized intents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TranscriptSnapshotRef {
    pub thread_id: ThreadId,
    pub view: TranscriptView,
    pub version: u64,
    pub end_seq: u64,
}

/// A bounded read of one committed transcript. A continuation supplies the
/// first page's snapshot and the preceding page's exclusive end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPageRequest {
    pub thread_id: ThreadId,
    pub view: TranscriptView,
    pub snapshot: Option<TranscriptSnapshotRef>,
    pub start: u64,
    pub limit: NonZeroUsize,
}

impl TranscriptPageRequest {
    /// Resolve the bounds for stores whose transcript versions are append-only
    /// message counts, as used by the existing frozen transcript references.
    pub fn resolve_append_only(
        &self,
        available_end: u64,
    ) -> Result<(TranscriptSnapshotRef, TranscriptRange), TranscriptError> {
        let snapshot = match &self.snapshot {
            Some(snapshot) => {
                if snapshot.thread_id != self.thread_id
                    || snapshot.view != self.view
                    || snapshot.version != snapshot.end_seq
                {
                    return Err(TranscriptError::SnapshotMismatch);
                }
                if snapshot.end_seq > available_end {
                    return Err(TranscriptError::SnapshotUnavailable {
                        requested_end: snapshot.end_seq,
                        available_end,
                    });
                }
                snapshot.clone()
            }
            None => TranscriptSnapshotRef {
                thread_id: self.thread_id.clone(),
                view: self.view,
                version: available_end,
                end_seq: available_end,
            },
        };
        if self.start > snapshot.end_seq {
            return Err(TranscriptError::InvalidRange {
                start: self.start,
                end: self.start,
                snapshot_end: snapshot.end_seq,
            });
        }
        let limit =
            u64::try_from(self.limit.get()).map_err(|_| TranscriptError::SequenceOverflow)?;
        let end = self.start.saturating_add(limit).min(snapshot.end_seq);
        Ok((snapshot, TranscriptRange::new(self.start, end)))
    }

    pub fn page_from_append_only(
        &self,
        messages: Vec<Message>,
    ) -> Result<TranscriptPage, TranscriptError> {
        let len = u64::try_from(messages.len()).map_err(|_| TranscriptError::SequenceOverflow)?;
        let (snapshot, range) = self.resolve_append_only(len)?;
        let start = usize::try_from(range.start).map_err(|_| TranscriptError::SequenceOverflow)?;
        let end = usize::try_from(range.end).map_err(|_| TranscriptError::SequenceOverflow)?;
        let messages = messages.into_iter().skip(start).take(end - start).collect();
        Ok(TranscriptPage {
            snapshot,
            range,
            messages,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptPage {
    pub snapshot: TranscriptSnapshotRef,
    pub range: TranscriptRange,
    pub messages: Vec<Message>,
}

impl TranscriptPage {
    #[must_use]
    pub fn next_start(&self) -> Option<u64> {
        (self.range.end < self.snapshot.end_seq).then_some(self.range.end)
    }
}

/// An immutable in-process snapshot. Clones share the message allocation.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptSnapshot {
    reference: TranscriptSnapshotRef,
    messages: Arc<[Message]>,
}

impl TranscriptSnapshot {
    #[must_use]
    pub fn new(thread_id: ThreadId, view: TranscriptView, messages: Vec<Message>) -> Self {
        let end_seq = u64::try_from(messages.len()).unwrap_or(u64::MAX);
        Self {
            reference: TranscriptSnapshotRef {
                thread_id,
                view,
                version: end_seq,
                end_seq,
            },
            messages: messages.into(),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &TranscriptSnapshotRef {
        &self.reference
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn verify(&self, expected: &TranscriptSnapshotRef) -> Result<(), TranscriptError> {
        if &self.reference == expected {
            Ok(())
        } else {
            Err(TranscriptError::SnapshotMismatch)
        }
    }

    pub fn slice(&self, spec: &TranscriptSliceSpec) -> Result<TranscriptSlice, TranscriptError> {
        self.verify(&spec.snapshot)?;
        validate_ranges(&spec.ranges, self.reference.end_seq)?;

        let mut selected = Vec::new();
        for range in &spec.ranges {
            let start =
                usize::try_from(range.start).map_err(|_| TranscriptError::SequenceOverflow)?;
            let end = usize::try_from(range.end).map_err(|_| TranscriptError::SequenceOverflow)?;
            selected.extend_from_slice(&self.messages[start..end]);
        }
        Ok(TranscriptSlice {
            snapshot: self.reference.clone(),
            ranges: spec.ranges.clone(),
            messages: selected.into(),
        })
    }
}

/// A consumer-authored selection over a frozen snapshot. Ranges must be sorted,
/// non-overlapping, and within the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptSliceSpec {
    pub snapshot: TranscriptSnapshotRef,
    pub ranges: Vec<TranscriptRange>,
}

impl TranscriptSliceSpec {
    /// Validate only the durable range contract, without loading messages.
    pub fn validate(&self) -> Result<(), TranscriptError> {
        validate_ranges(&self.ranges, self.snapshot.end_seq)
    }
}

/// The immutable selected messages plus evidence tying them to their source.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptSlice {
    pub snapshot: TranscriptSnapshotRef,
    pub ranges: Vec<TranscriptRange>,
    pub messages: Arc<[Message]>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    #[error("transcript range [{start}, {end}) is invalid for end sequence {snapshot_end}")]
    InvalidRange {
        start: u64,
        end: u64,
        snapshot_end: u64,
    },
    #[error("transcript ranges must be sorted and non-overlapping")]
    OverlappingRanges,
    #[error("transcript sequence cannot be represented on this platform")]
    SequenceOverflow,
    #[error(
        "transcript snapshot ending at {requested_end} is unavailable; committed end is {available_end}"
    )]
    SnapshotUnavailable {
        requested_end: u64,
        available_end: u64,
    },
    #[error("transcript snapshot identity does not match committed truth")]
    SnapshotMismatch,
}

fn validate_ranges(ranges: &[TranscriptRange], snapshot_end: u64) -> Result<(), TranscriptError> {
    let mut previous_end = 0;
    for (index, range) in ranges.iter().enumerate() {
        if range.start > range.end || range.end > snapshot_end {
            return Err(TranscriptError::InvalidRange {
                start: range.start,
                end: range.end,
                snapshot_end,
            });
        }
        if index > 0 && range.start < previous_end {
            return Err(TranscriptError::OverlappingRanges);
        }
        previous_end = range.end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_pages_reuse_the_frozen_prefix_and_reject_invalid_bounds() {
        let mut request = TranscriptPageRequest {
            thread_id: ThreadId("thread".into()),
            view: TranscriptView::RawCommitted,
            snapshot: None,
            start: 1,
            limit: NonZeroUsize::MIN,
        };
        let (snapshot, range) = request.resolve_append_only(3).unwrap();
        assert_eq!(range, TranscriptRange::new(1, 2));
        assert_eq!(snapshot.end_seq, 3);
        request.snapshot = Some(snapshot.clone());
        request.start = range.end;
        assert_eq!(
            request.resolve_append_only(5).unwrap(),
            (snapshot, TranscriptRange::new(2, 3))
        );
        request.start = 3;
        assert_eq!(
            request.resolve_append_only(5).unwrap().1,
            TranscriptRange::new(3, 3)
        );
        request.start = 4;
        assert!(matches!(
            request.resolve_append_only(5),
            Err(TranscriptError::InvalidRange { .. })
        ));
        assert!(matches!(
            request.resolve_append_only(2),
            Err(TranscriptError::SnapshotUnavailable { .. })
        ));
        request.thread_id = ThreadId("other".into());
        assert_eq!(
            request.resolve_append_only(5),
            Err(TranscriptError::SnapshotMismatch)
        );
    }
    use crate::agent::message::{Id as MessageId, Role};

    fn messages() -> Vec<Message> {
        ["zero", "one", "two", "three"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| Message::text(MessageId(format!("m{index}")), Role::User, text))
            .collect()
    }

    #[test]
    fn disjoint_ranges_select_in_order_and_carry_source_evidence() {
        let snapshot = TranscriptSnapshot::new(
            ThreadId("thread".into()),
            TranscriptView::RawCommitted,
            messages(),
        );
        let spec = TranscriptSliceSpec {
            snapshot: snapshot.reference().clone(),
            ranges: vec![TranscriptRange::new(0, 1), TranscriptRange::new(2, 4)],
        };
        let slice = snapshot.slice(&spec).expect("valid slice");
        assert_eq!(
            slice
                .messages
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>(),
            ["zero", "two", "three"]
        );
        assert_eq!(slice.snapshot, spec.snapshot);
    }

    #[test]
    fn range_validation_rejects_overlap_reverse_and_past_end() {
        let snapshot = TranscriptSnapshot::new(
            ThreadId("thread".into()),
            TranscriptView::RawCommitted,
            messages(),
        );
        for ranges in [
            vec![TranscriptRange::new(0, 3), TranscriptRange::new(2, 4)],
            vec![TranscriptRange::new(3, 2)],
            vec![TranscriptRange::new(0, 5)],
        ] {
            let error = snapshot
                .slice(&TranscriptSliceSpec {
                    snapshot: snapshot.reference().clone(),
                    ranges,
                })
                .expect_err("invalid range");
            assert!(matches!(
                error,
                TranscriptError::OverlappingRanges | TranscriptError::InvalidRange { .. }
            ));
        }
    }

    #[test]
    fn thread_view_and_version_participate_in_snapshot_identity() {
        let raw = TranscriptSnapshot::new(
            ThreadId("thread".into()),
            TranscriptView::RawCommitted,
            messages(),
        );
        let effective = TranscriptSnapshot::new(
            ThreadId("thread".into()),
            TranscriptView::ContextEffective,
            messages(),
        );
        assert_ne!(raw.reference(), effective.reference());

        let other_thread = TranscriptSnapshot::new(
            ThreadId("other".into()),
            TranscriptView::RawCommitted,
            messages(),
        );
        assert_ne!(raw.reference(), other_thread.reference());

        let mut extended = messages();
        extended.push(Message::text(MessageId("m4".into()), Role::User, "four"));
        let extended = TranscriptSnapshot::new(
            ThreadId("thread".into()),
            TranscriptView::RawCommitted,
            extended,
        );
        assert_ne!(raw.reference(), extended.reference());
    }
}
