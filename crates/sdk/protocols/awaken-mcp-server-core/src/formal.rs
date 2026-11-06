// @formal entrypoint mcp.at_most_one_final_response
// @formal entrypoint mcp.cancellation_has_no_success_result
// @formal entrypoint mcp.final_stops_progress
// @formal entrypoint mcp.notification_has_no_response
// @formal entrypoint mcp.rejected_request_skips_host
//! Pure refinement model for the MCP request lifecycle.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Active,
    Final,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    StartRequest { supported: bool, valid: bool },
    StartNotification,
    Progress,
    Complete,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
struct ProtocolState {
    phase: Phase,
    notification: bool,
    admitted: bool,
    cancelled: bool,
    host_calls: u8,
    responses: u8,
    results: u8,
    progress: u8,
    progress_at_final: u8,
}

impl ProtocolState {
    const fn initial() -> Self {
        Self {
            phase: Phase::Idle,
            notification: false,
            admitted: false,
            cancelled: false,
            host_calls: 0,
            responses: 0,
            results: 0,
            progress: 0,
            progress_at_final: 0,
        }
    }

    fn apply(mut self, action: Action) -> Self {
        match (self.phase, action) {
            (Phase::Idle, Action::StartRequest { supported, valid }) => {
                self.admitted = supported && valid;
                if self.admitted {
                    self.phase = Phase::Active;
                    self.host_calls = 1;
                } else {
                    self.finish_response(false);
                }
            }
            (Phase::Idle, Action::StartNotification) => {
                self.notification = true;
                self.phase = Phase::Final;
                self.progress_at_final = self.progress;
            }
            (Phase::Active, Action::Progress) if !self.cancelled => {
                self.progress = self.progress.saturating_add(1);
            }
            (Phase::Active, Action::Complete) => self.finish_response(true),
            (Phase::Active, Action::Cancel) => {
                self.cancelled = true;
                self.finish_response(false);
            }
            // Final is absorbing; malformed ordering cannot reopen the request.
            _ => {}
        }
        self
    }

    fn finish_response(&mut self, success: bool) {
        self.phase = Phase::Final;
        self.responses = self.responses.saturating_add(1);
        self.results = self.results.saturating_add(u8::from(success));
        self.progress_at_final = self.progress;
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.responses <= 1
            && (!self.notification || self.responses == 0)
            && (self.phase != Phase::Final || self.progress == self.progress_at_final)
            && (self.admitted || self.host_calls == 0)
            && (!self.cancelled || self.results == 0)
            && (self.phase != Phase::Idle || self.host_calls == 0)
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_ACTION: [Action; 8] = [
        Action::StartRequest {
            supported: false,
            valid: false,
        },
        Action::StartRequest {
            supported: false,
            valid: true,
        },
        Action::StartRequest {
            supported: true,
            valid: false,
        },
        Action::StartRequest {
            supported: true,
            valid: true,
        },
        Action::StartNotification,
        Action::Progress,
        Action::Complete,
        Action::Cancel,
    ];

    const fn listed_action(action: Action) -> usize {
        match action {
            Action::StartRequest {
                supported: false,
                valid: false,
            } => 0,
            Action::StartRequest {
                supported: false,
                valid: true,
            } => 1,
            Action::StartRequest {
                supported: true,
                valid: false,
            } => 2,
            Action::StartRequest {
                supported: true,
                valid: true,
            } => 3,
            Action::StartNotification => 4,
            Action::Progress => 5,
            Action::Complete => 6,
            Action::Cancel => 7,
        }
    }

    #[test]
    fn every_action_is_listed_once() {
        for (index, action) in EVERY_ACTION.into_iter().enumerate() {
            assert_eq!(listed_action(action), index);
        }
    }

    /// Every field of the state, destructured without `..` so a new field
    /// cannot be left out of the identity two traces are merged on.
    fn state_key(state: &ProtocolState) -> (u8, bool, bool, bool, u8, u8, u8, u8, u8) {
        let ProtocolState {
            phase,
            notification,
            admitted,
            cancelled,
            host_calls,
            responses,
            results,
            progress,
            progress_at_final,
        } = *state;
        let phase = match phase {
            Phase::Idle => 0,
            Phase::Active => 1,
            Phase::Final => 2,
        };
        (
            phase,
            notification,
            admitted,
            cancelled,
            host_calls,
            responses,
            results,
            progress,
            progress_at_final,
        )
    }

    /// The end state of every one of the 8^8 traces of eight actions from the
    /// initial state. `apply` is a
    /// deterministic function of (state, action), so traces that reach equal
    /// states have equal continuations: expanding each distinct state of a
    /// step by every action reaches exactly the states of every trace.
    fn every_trace_end_state() -> Vec<ProtocolState> {
        let mut frontier = vec![ProtocolState::initial()];
        for _ in 0..8 {
            let mut next: Vec<ProtocolState> = Vec::new();
            for state in &frontier {
                for action in EVERY_ACTION {
                    let successor = state.apply(action);
                    let key = state_key(&successor);
                    if !next.iter().any(|seen| state_key(seen) == key) {
                        next.push(successor);
                    }
                }
            }
            frontier = next;
        }
        frontier
    }

    /// @exhaustive Action(8)^8 = 16777216 traces, every end state enumerated
    /// @obligation mcp.at_most_one_final_response
    #[test]
    fn one_request_has_at_most_one_final_response() {
        for state in every_trace_end_state() {
            assert!(state.responses <= 1);
        }
    }

    /// @exhaustive Action(8)^8 = 16777216 traces, every end state enumerated
    /// @obligation mcp.notification_has_no_response
    #[test]
    fn notifications_never_have_a_jsonrpc_response() {
        for state in every_trace_end_state() {
            assert!(!state.notification || state.responses == 0);
        }
    }

    /// @exhaustive Action(8)^8 = 16777216 traces, every end state enumerated
    /// @obligation mcp.final_stops_progress
    #[test]
    fn final_response_is_progress_absorbing() {
        for state in every_trace_end_state() {
            assert!(state.phase != Phase::Final || state.progress == state.progress_at_final);
        }
    }

    /// @exhaustive Action(8)^8 = 16777216 traces, every end state enumerated
    /// @obligation mcp.rejected_request_skips_host
    #[test]
    fn rejected_requests_never_enter_the_host() {
        for state in every_trace_end_state() {
            assert!(state.admitted || state.host_calls == 0);
        }
    }

    /// @exhaustive Action(8)^8 = 16777216 traces, every end state enumerated
    /// @obligation mcp.cancellation_has_no_success_result
    #[test]
    fn cancellation_never_produces_a_success_result() {
        for state in every_trace_end_state() {
            assert!(!state.cancelled || state.results == 0);
        }
    }
}

#[test]
fn representative_traces_preserve_all_invariants() {
    let traces = [
        [
            Action::StartRequest {
                supported: true,
                valid: true,
            },
            Action::Progress,
            Action::Complete,
            Action::Progress,
        ],
        [
            Action::StartRequest {
                supported: false,
                valid: true,
            },
            Action::Complete,
            Action::Progress,
            Action::Cancel,
        ],
        [
            Action::StartNotification,
            Action::Complete,
            Action::Progress,
            Action::Cancel,
        ],
        [
            Action::StartRequest {
                supported: true,
                valid: true,
            },
            Action::Progress,
            Action::Cancel,
            Action::Complete,
        ],
    ];
    for trace in traces {
        let state = trace
            .into_iter()
            .fold(ProtocolState::initial(), ProtocolState::apply);
        assert!(state.invariants(), "invalid trace ended at {state:?}");
    }
}
