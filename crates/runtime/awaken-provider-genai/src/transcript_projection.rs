//! Closed decision kernel for projecting neutral transcript turns onto `genai`.
//!
//! Provider payloads (text, JSON arguments, and binary content) remain opaque to
//! this module.  It owns only the finite structural decisions that must be
//! identical for every payload: part-category projection, incomplete
//! reasoning-only row omission, and reasoning placement on streamed and
//! non-streamed responses.

/// Closed neutral content categories relevant to provider transcript replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NeutralPartKind {
    Text,
    Image,
    Document,
    SearchResult,
    Redacted,
    ToolUse,
    ToolResult,
    Thinking,
}

/// Closed provider content categories emitted by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderPartKind {
    Text,
    Binary,
    ToolCall,
    ToolResponse,
    Reasoning,
    SignedThinking,
    Omitted,
}

impl ProviderPartKind {
    #[must_use]
    pub(crate) const fn is_reasoning_like(self) -> bool {
        matches!(self, Self::Reasoning | Self::SignedThinking)
    }

    #[must_use]
    #[cfg(kani)]
    pub(crate) const fn completes_row(self) -> bool {
        !matches!(self, Self::Reasoning | Self::SignedThinking | Self::Omitted)
    }
}

/// Closed provider dialect relevant to replaying neutral thinking blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptDialect {
    /// Anthropic requires the original thinking text/signature pair.
    Anthropic,
    /// Other adapters consume the normalized reasoning-content representation.
    Other,
}

/// Project one neutral category without inspecting or changing its payload.
#[must_use]
pub(crate) const fn project_part_kind(
    dialect: TranscriptDialect,
    kind: NeutralPartKind,
) -> ProviderPartKind {
    match (dialect, kind) {
        (_, NeutralPartKind::Text) => ProviderPartKind::Text,
        (_, NeutralPartKind::Image) => ProviderPartKind::Binary,
        (_, NeutralPartKind::Document) => ProviderPartKind::Binary,
        (_, NeutralPartKind::SearchResult) => ProviderPartKind::Text,
        (_, NeutralPartKind::Redacted) => ProviderPartKind::Omitted,
        (_, NeutralPartKind::ToolUse) => ProviderPartKind::ToolCall,
        (_, NeutralPartKind::ToolResult) => ProviderPartKind::ToolResponse,
        (TranscriptDialect::Anthropic, NeutralPartKind::Thinking) => {
            ProviderPartKind::SignedThinking
        }
        (TranscriptDialect::Other, NeutralPartKind::Thinking) => ProviderPartKind::Reasoning,
    }
}

/// Payload-preserving thinking projection. Generic payloads deliberately make
/// text and signature opaque: this kernel selects only their wire envelope and
/// moves both values without inspecting, joining, or reordering them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThinkingProjection<T, S> {
    Signed { thinking: T, signature: Option<S> },
    Reasoning(T),
}

#[must_use]
pub(crate) fn project_thinking<T, S>(
    dialect: TranscriptDialect,
    thinking: T,
    signature: Option<S>,
) -> ThinkingProjection<T, S> {
    match dialect {
        TranscriptDialect::Anthropic => ThinkingProjection::Signed {
            thinking,
            signature,
        },
        TranscriptDialect::Other => ThinkingProjection::Reasoning(thinking),
    }
}

/// Incremental classification of one transcript row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayRowState {
    /// No content part has been observed.
    Empty,
    /// Every observed part is reasoning, so the row is not a complete turn.
    ReasoningOnly,
    /// At least one public or tool part makes the complete row replayable.
    Replay,
}

impl ReplayRowState {
    /// Absorb one projected part. `Replay` is deliberately absorbing: attached
    /// reasoning can never cause a complete tool/public turn to be dropped.
    #[must_use]
    pub(crate) const fn absorb(self, part: ProviderPartKind) -> Self {
        match (self, part) {
            (state, ProviderPartKind::Omitted) => state,
            (Self::Replay, _) => Self::Replay,
            (Self::Empty | Self::ReasoningOnly, part) if part.is_reasoning_like() => {
                Self::ReasoningOnly
            }
            (Self::Empty | Self::ReasoningOnly, _) => Self::Replay,
        }
    }

    #[must_use]
    pub(crate) const fn should_replay(self) -> bool {
        matches!(self, Self::Replay)
    }
}

/// Transport path that captured provider-normalized reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseTransport {
    Streaming,
    NonStreaming,
}

/// Structural action for provider-normalized reasoning carried outside the
/// normal response content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningFoldAction {
    Keep,
    Prepend,
}

/// Decide whether external reasoning must be prepended to the committed turn.
///
/// `has_reasoning` means the external payload is non-blank.
/// `has_ordered_thinking` records whether ordered content already contains any
/// Thinking block. Ordered content is authoritative even when its text differs
/// from the compatibility scalar. Both transport paths share this table.
#[must_use]
pub(crate) const fn decide_reasoning_fold(
    transport: ResponseTransport,
    has_reasoning: bool,
    has_ordered_thinking: bool,
) -> ReasoningFoldAction {
    match transport {
        ResponseTransport::Streaming | ResponseTransport::NonStreaming => {
            if has_reasoning && !has_ordered_thinking {
                ReasoningFoldAction::Prepend
            } else {
                ReasoningFoldAction::Keep
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_reasoning_keeps_the_complete_tool_turn_replayable() {
        let state = ReplayRowState::Empty
            .absorb(project_part_kind(
                TranscriptDialect::Anthropic,
                NeutralPartKind::Thinking,
            ))
            .absorb(project_part_kind(
                TranscriptDialect::Anthropic,
                NeutralPartKind::ToolUse,
            ));
        assert_eq!(state, ReplayRowState::Replay);
    }

    #[test]
    fn reasoning_fold_shape_is_identical_for_both_transports() {
        for has_reasoning in [false, true] {
            for already_present in [false, true] {
                assert_eq!(
                    decide_reasoning_fold(
                        ResponseTransport::Streaming,
                        has_reasoning,
                        already_present,
                    ),
                    decide_reasoning_fold(
                        ResponseTransport::NonStreaming,
                        has_reasoning,
                        already_present,
                    )
                );
            }
        }
    }

    #[test]
    fn managed_blocks_reuse_the_canonical_row_projection() {
        // Causes: C1 document; C2 search result; C3 redacted placeholder.
        // Effects: E1/E2 keep the row replayable through existing binary/text
        // categories; E3 contributes no provider token and cannot fabricate a
        // complete turn. Rules M1=C1=>E1, M2=C2=>E2, M3=C3=>E3.
        assert_eq!(
            ReplayRowState::Empty.absorb(project_part_kind(
                TranscriptDialect::Anthropic,
                NeutralPartKind::Document,
            )),
            ReplayRowState::Replay
        );
        assert_eq!(
            ReplayRowState::Empty.absorb(project_part_kind(
                TranscriptDialect::Other,
                NeutralPartKind::SearchResult,
            )),
            ReplayRowState::Replay
        );
        assert_eq!(
            ReplayRowState::Empty.absorb(project_part_kind(
                TranscriptDialect::Other,
                NeutralPartKind::Redacted,
            )),
            ReplayRowState::Empty
        );
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn symbolic_neutral_part(tag: u8) -> NeutralPartKind {
        match tag {
            0 => NeutralPartKind::Text,
            1 => NeutralPartKind::Image,
            2 => NeutralPartKind::ToolUse,
            3 => NeutralPartKind::ToolResult,
            _ => NeutralPartKind::Thinking,
        }
    }

    fn symbolic_row_state(tag: u8) -> ReplayRowState {
        match tag {
            0 => ReplayRowState::Empty,
            1 => ReplayRowState::ReasoningOnly,
            _ => ReplayRowState::Replay,
        }
    }

    #[kani::proof]
    fn reasoning_replay_projection_preserves_reasoning_and_complete_turns() {
        let tag: u8 = kani::any();
        kani::assume(tag <= 4);
        let neutral = symbolic_neutral_part(tag);
        let dialect = if kani::any() {
            TranscriptDialect::Anthropic
        } else {
            TranscriptDialect::Other
        };
        let projected = project_part_kind(dialect, neutral);

        assert_eq!(
            matches!(neutral, NeutralPartKind::Thinking),
            projected.is_reasoning_like()
        );
        if matches!(neutral, NeutralPartKind::Thinking) {
            assert_eq!(
                matches!(projected, ProviderPartKind::SignedThinking),
                dialect == TranscriptDialect::Anthropic
            );
        }
        assert_eq!(
            matches!(neutral, NeutralPartKind::ToolUse),
            matches!(projected, ProviderPartKind::ToolCall)
        );

        let prior_tag: u8 = kani::any();
        kani::assume(prior_tag <= 4);
        let prior = project_part_kind(dialect, symbolic_neutral_part(prior_tag));
        let state = ReplayRowState::Empty.absorb(prior).absorb(projected);
        let has_complete_part = prior.completes_row() || projected.completes_row();
        assert_eq!(state.should_replay(), has_complete_part);

        let state_tag: u8 = kani::any();
        kani::assume(state_tag <= 2);
        let prior_state = symbolic_row_state(state_tag);
        let next_state = prior_state.absorb(projected);
        if prior_state == ReplayRowState::Replay {
            assert_eq!(next_state, ReplayRowState::Replay);
        }
        if projected.is_reasoning_like() {
            assert_eq!(next_state.should_replay(), prior_state.should_replay());
        } else {
            assert_eq!(next_state, ReplayRowState::Replay);
        }

        let reasoning_then_tool = ReplayRowState::Empty
            .absorb(project_part_kind(dialect, NeutralPartKind::Thinking))
            .absorb(project_part_kind(dialect, NeutralPartKind::ToolUse));
        assert_eq!(reasoning_then_tool, ReplayRowState::Replay);
        assert_eq!(
            ReplayRowState::Empty.absorb(project_part_kind(dialect, NeutralPartKind::Thinking)),
            ReplayRowState::ReasoningOnly
        );
        assert!(
            !ReplayRowState::Empty
                .absorb(project_part_kind(dialect, NeutralPartKind::Thinking))
                .should_replay()
        );

        let text_identity: u8 = kani::any();
        let signature_identity: u8 = kani::any();
        let signature = if kani::any() {
            Some(signature_identity)
        } else {
            None
        };
        match project_thinking(dialect, text_identity, signature) {
            ThinkingProjection::Signed {
                thinking,
                signature: projected_signature,
            } => {
                assert_eq!(dialect, TranscriptDialect::Anthropic);
                assert_eq!(thinking, text_identity);
                assert_eq!(projected_signature, signature);
            }
            ThinkingProjection::Reasoning(thinking) => {
                assert_eq!(dialect, TranscriptDialect::Other);
                assert_eq!(thinking, text_identity);
            }
        }

        let second_text_identity: u8 = kani::any();
        let second_signature = if kani::any() {
            Some(kani::any::<u8>())
        } else {
            None
        };
        let first = project_thinking(dialect, text_identity, signature);
        let second = project_thinking(dialect, second_text_identity, second_signature);
        match (first, second) {
            (
                ThinkingProjection::Signed {
                    thinking: first_text,
                    signature: first_signature,
                },
                ThinkingProjection::Signed {
                    thinking: second_text,
                    signature: projected_second_signature,
                },
            ) => {
                assert_eq!(first_text, text_identity);
                assert_eq!(first_signature, signature);
                assert_eq!(second_text, second_text_identity);
                assert_eq!(projected_second_signature, second_signature);
            }
            (
                ThinkingProjection::Reasoning(first_text),
                ThinkingProjection::Reasoning(second_text),
            ) => {
                assert_eq!(first_text, text_identity);
                assert_eq!(second_text, second_text_identity);
            }
            _ => unreachable!("one dialect cannot mix thinking envelopes"),
        }
    }

    #[kani::proof]
    fn reasoning_fold_prepends_exactly_once_and_is_transport_independent() {
        let has_reasoning: bool = kani::any();
        let has_ordered_thinking: bool = kani::any();
        let streaming = decide_reasoning_fold(
            ResponseTransport::Streaming,
            has_reasoning,
            has_ordered_thinking,
        );
        let non_streaming = decide_reasoning_fold(
            ResponseTransport::NonStreaming,
            has_reasoning,
            has_ordered_thinking,
        );

        assert_eq!(streaming, non_streaming);
        assert_eq!(
            matches!(streaming, ReasoningFoldAction::Prepend),
            has_reasoning && !has_ordered_thinking
        );
        if has_ordered_thinking || !has_reasoning {
            assert_eq!(streaming, ReasoningFoldAction::Keep);
        }
    }
}
