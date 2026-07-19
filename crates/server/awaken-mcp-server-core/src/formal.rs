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

    fn invariants(&self) -> bool {
        self.responses <= 1
            && (!self.notification || self.responses == 0)
            && (self.phase != Phase::Final || self.progress == self.progress_at_final)
            && (self.admitted || self.host_calls == 0)
            && (!self.cancelled || self.results == 0)
            && (self.phase != Phase::Idle || self.host_calls == 0)
    }
}

#[cfg(kani)]
fn arbitrary_action() -> Action {
    match kani::any::<u8>() % 5 {
        0 => Action::StartRequest {
            supported: kani::any(),
            valid: kani::any(),
        },
        1 => Action::StartNotification,
        2 => Action::Progress,
        3 => Action::Complete,
        _ => Action::Cancel,
    }
}

#[cfg(kani)]
fn arbitrary_trace() -> ProtocolState {
    let mut state = ProtocolState::initial();
    for _ in 0..8 {
        state = state.apply(arbitrary_action());
    }
    state
}

#[cfg(kani)]
#[kani::proof]
fn one_request_has_at_most_one_final_response() {
    assert!(arbitrary_trace().responses <= 1);
}

#[cfg(kani)]
#[kani::proof]
fn notifications_never_have_a_jsonrpc_response() {
    let state = arbitrary_trace();
    assert!(!state.notification || state.responses == 0);
}

#[cfg(kani)]
#[kani::proof]
fn final_response_is_progress_absorbing() {
    let state = arbitrary_trace();
    assert!(state.phase != Phase::Final || state.progress == state.progress_at_final);
}

#[cfg(kani)]
#[kani::proof]
fn rejected_requests_never_enter_the_host() {
    let state = arbitrary_trace();
    assert!(state.admitted || state.host_calls == 0);
}

#[cfg(kani)]
#[kani::proof]
fn cancellation_never_produces_a_success_result() {
    let state = arbitrary_trace();
    assert!(!state.cancelled || state.results == 0);
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
