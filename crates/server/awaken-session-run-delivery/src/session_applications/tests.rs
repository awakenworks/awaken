// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-run-delivery/src/session_applications.rs
// @formal reason: cfg(test) only; observes real Arc/Weak ownership and complete capability acquisition without effect fakes.
use super::*;
use awaken_agent_contract::agent::{message::Message, run::Id as RunId, thread::Id as ThreadId};
use awaken_session_application_contract::RunApplicationError;
use awaken_session_execution_contract::*;
use awaken_session_types::{RunError, SessionId};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Port(Arc<AtomicUsize>);
impl Drop for Port {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl SessionAgentCoordination for Port {
    async fn admit_session_run_activity(
        &self,
        _: &SessionId,
        _: &str,
        _: &RunId,
        _: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        panic!("unused coordination activity role was invoked")
    }
    async fn admit_session_model_request(
        &self,
        _: &SessionId,
        _: &ThreadId,
        _: &RunId,
    ) -> Result<bool, RunError> {
        panic!("unused model admission role was invoked")
    }
    async fn list_session_agents(
        &self,
        _: &SessionId,
    ) -> Result<Vec<SessionAgentRosterEntry>, RunError> {
        panic!("unused coordination roster role was invoked")
    }
    async fn send_session_agent_message(
        &self,
        _: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        panic!("unused coordination send role was invoked")
    }
    async fn settle_session_agent_boundary(
        &self,
        _: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        panic!("unused coordination settle role was invoked")
    }
    async fn finalize_session_agent_boundary(
        &self,
        _: SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        panic!("unused coordination finalize role was invoked")
    }
    async fn interrupt_session_thread(&self, _: &SessionId, _: &ThreadId) -> Result<(), RunError> {
        panic!("unused coordination interrupt role was invoked")
    }
    async fn reply_session_thread_tool(
        &self,
        _: SessionThreadToolReplyCommand,
    ) -> Result<(), RunError> {
        panic!("unused coordination reply role was invoked")
    }
}
#[async_trait::async_trait]
impl SessionRunBackgroundApplication for Port {
    async fn submit_session_run_background(
        &self,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
        _: Option<String>,
    ) -> Result<RunId, RunApplicationError> {
        panic!("unused background submission role was invoked")
    }
}
#[test]
fn complete_selection_retains_sole_arcs_and_releases_every_clone() {
    // C1 each input is the sole strong Arc; C2 the original owner is dropped;
    // C3 an acquired/clone lifetime remains; C4 the final owner is released.
    // E1 both exact ports live through C1-C3; E2 C4 drops both exactly once;
    // E3 weak Host observations keep neither alive and cannot change mode.
    let drops = Arc::new(AtomicUsize::new(0));
    let coordination = Arc::new(Port(drops.clone()));
    let background = Arc::new(Port(drops.clone()));
    let coordination_weak = Arc::downgrade(&coordination);
    let background_weak = Arc::downgrade(&background);
    let coordination_address = Arc::as_ptr(&coordination) as *const ();
    let background_address = Arc::as_ptr(&background) as *const ();
    let (connections, owner) = SessionApplications::Local {
        coordination,
        background_runs: background,
    }
    .connect();
    assert_eq!(connections.mode(), SessionApplicationMode::Local);
    assert_eq!(
        Arc::as_ptr(&connections.agent_coordination().unwrap().upgrade().unwrap()) as *const (),
        coordination_address
    );
    assert_eq!(
        Arc::as_ptr(
            &connections
                .background_run_submission()
                .unwrap()
                .upgrade()
                .unwrap()
        ) as *const (),
        background_address
    );
    let clone = owner.clone();
    drop(owner);
    let acquired = connections.acquire().expect("both roles still owned");
    drop(clone);
    assert!(coordination_weak.upgrade().is_some());
    assert!(background_weak.upgrade().is_some());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(acquired);
    assert!(coordination_weak.upgrade().is_none());
    assert!(background_weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(connections.acquire().is_none());
    assert_eq!(
        connections.mode(),
        SessionApplicationMode::Local,
        "expiry never changes selected authority"
    );
}

#[test]
fn incomplete_or_expired_local_pair_never_acquires_one_role() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum State {
        Missing,
        Expired,
        Alive,
    }
    // Each role independently has no connection, an expired weak reference,
    // or a live owner. All eight incomplete rows refuse and retain neither
    // temporary acquisition. The complete row is owned by the positive test.
    for coordination_state in [State::Missing, State::Expired, State::Alive] {
        for background_state in [State::Missing, State::Expired, State::Alive] {
            if coordination_state == State::Alive && background_state == State::Alive {
                continue;
            }
            let drops = Arc::new(AtomicUsize::new(0));
            let mut coordination: Option<Arc<dyn SessionAgentCoordination>> =
                Some(Arc::new(Port(drops.clone())));
            let mut background: Option<Arc<dyn SessionRunBackgroundApplication>> =
                Some(Arc::new(Port(drops.clone())));
            let mut connections = SessionApplicationConnections {
                mode: SessionApplicationMode::Local,
                coordination: coordination.as_ref().map(Arc::downgrade),
                background_runs: background.as_ref().map(Arc::downgrade),
            };
            match coordination_state {
                State::Missing => connections.coordination = None,
                State::Expired => drop(coordination.take()),
                State::Alive => {}
            }
            match background_state {
                State::Missing => connections.background_runs = None,
                State::Expired => drop(background.take()),
                State::Alive => {}
            }
            let coordination_count = coordination.as_ref().map(Arc::strong_count);
            let background_count = background.as_ref().map(Arc::strong_count);
            assert!(
                connections.acquire().is_none(),
                "{coordination_state:?}/{background_state:?}"
            );
            assert_eq!(
                coordination.as_ref().map(Arc::strong_count),
                coordination_count
            );
            assert_eq!(background.as_ref().map(Arc::strong_count), background_count);
            assert_eq!(connections.mode(), SessionApplicationMode::Local);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                usize::from(coordination_state == State::Expired)
                    + usize::from(background_state == State::Expired)
            );
            drop(coordination);
            drop(background);
            assert_eq!(drops.load(Ordering::SeqCst), 2);
        }
    }
}

#[test]
fn nonlocal_modes_explicitly_have_no_local_callbacks() {
    for (selection, expected) in [
        (
            SessionApplications::Standalone,
            SessionApplicationMode::Standalone,
        ),
        (
            SessionApplications::ClaimedWorker,
            SessionApplicationMode::ClaimedWorker,
        ),
    ] {
        let (connections, owner) = selection.connect();
        assert_eq!(connections.mode(), expected);
        assert!(connections.agent_coordination().is_none());
        assert!(connections.background_run_submission().is_none());
        assert!(connections.acquire().is_some());
        drop(owner);
        assert!(
            connections.acquire().is_some(),
            "no hidden local application dependency"
        );
    }
}
