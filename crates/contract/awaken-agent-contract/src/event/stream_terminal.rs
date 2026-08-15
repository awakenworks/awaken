//! Finite decision kernel for closing a live protocol stream.
//!
//! Protocol adapters own their wire vocabularies, but they share these safety
//! rules: a terminal frame requires an observed start, is emitted at most once,
//! and an inexact live/committed reconciliation can only close as failure.

/// Protocol-neutral terminal category requested by committed runtime truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTerminalKind {
    Awaiting,
    Finished,
    Failed,
}

/// Complete result of the terminal projection decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTerminalDecision {
    /// The stream has not emitted its start boundary, so no terminal frame is legal.
    RejectUnstarted,
    /// A terminal frame was already emitted; the terminal state is absorbing.
    AlreadyEmitted,
    /// Emit exactly this terminal category and mark the stream terminal.
    Emit(StreamTerminalKind),
}

/// Decide whether and how a protocol stream may emit its terminal projection.
///
/// The inputs are a finite, allocation-free contract suitable for exhaustive
/// model checking. `reconciliation_exact` is the adapter's evidence that every
/// live tool prefix agrees with the later committed step. Missing evidence never
/// produces an awaiting/success terminal: it is downgraded to
/// [`StreamTerminalKind::Failed`].
#[must_use]
pub const fn decide_stream_terminal(
    started: bool,
    terminal_emitted: bool,
    reconciliation_exact: bool,
    requested: StreamTerminalKind,
) -> StreamTerminalDecision {
    if terminal_emitted {
        StreamTerminalDecision::AlreadyEmitted
    } else if !started {
        StreamTerminalDecision::RejectUnstarted
    } else if !reconciliation_exact {
        StreamTerminalDecision::Emit(StreamTerminalKind::Failed)
    } else {
        StreamTerminalDecision::Emit(requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_decision_table_is_total_and_fail_closed() {
        let kinds = [
            StreamTerminalKind::Awaiting,
            StreamTerminalKind::Finished,
            StreamTerminalKind::Failed,
        ];
        for started in [false, true] {
            for terminal_emitted in [false, true] {
                for exact in [false, true] {
                    for requested in kinds {
                        let decision =
                            decide_stream_terminal(started, terminal_emitted, exact, requested);
                        if terminal_emitted {
                            assert_eq!(decision, StreamTerminalDecision::AlreadyEmitted);
                        } else if !started {
                            assert_eq!(decision, StreamTerminalDecision::RejectUnstarted);
                        } else if !exact {
                            assert_eq!(
                                decision,
                                StreamTerminalDecision::Emit(StreamTerminalKind::Failed)
                            );
                        } else {
                            assert_eq!(decision, StreamTerminalDecision::Emit(requested));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn stream_terminal_projection_is_exact_fail_closed_and_absorbing() {
        let started: bool = kani::any();
        let terminal_emitted: bool = kani::any();
        let reconciliation_exact: bool = kani::any();
        let tag: u8 = kani::any();
        kani::assume(tag <= 2);
        let requested = match tag {
            0 => StreamTerminalKind::Awaiting,
            1 => StreamTerminalKind::Finished,
            _ => StreamTerminalKind::Failed,
        };

        let decision =
            decide_stream_terminal(started, terminal_emitted, reconciliation_exact, requested);

        if !started {
            assert!(!matches!(
                decision,
                StreamTerminalDecision::Emit(StreamTerminalKind::Awaiting)
                    | StreamTerminalDecision::Emit(StreamTerminalKind::Finished)
            ));
        }
        if terminal_emitted {
            assert_eq!(decision, StreamTerminalDecision::AlreadyEmitted);
        }
        if started && !terminal_emitted && !reconciliation_exact {
            assert_eq!(
                decision,
                StreamTerminalDecision::Emit(StreamTerminalKind::Failed)
            );
        }
        if started && !terminal_emitted && reconciliation_exact {
            assert_eq!(decision, StreamTerminalDecision::Emit(requested));
        }
    }
}
