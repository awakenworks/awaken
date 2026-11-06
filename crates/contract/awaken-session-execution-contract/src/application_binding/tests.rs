// @formal surface exclude=test_only
// @formal owner crates/contract/awaken-session-execution-contract/src/application_binding.rs
// @formal reason: cfg(test) only; observes real Arc/Weak ownership and complete role acquisition without effect fakes.
use super::*;
use crate::{
    PreparedSessionRunActivity, SessionAgentBoundaryCommand, SessionAgentMessageCommand,
    SessionAgentMessageReceipt, SessionAgentRosterEntry, SessionRunActivityAdmission,
    SessionRunActivityAdmissionMode,
};
use awaken_agent_contract::agent::{message::Message, run::Id as RunId, thread::Id as ThreadId};
use awaken_runtime_contract::runs::RunError;

use awaken_session_contract::SessionThreadToolReplyCommand;
use awaken_session_types::SessionId;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::SessionApplicationBindingError as E;

struct Port(Arc<AtomicUsize>);
impl Drop for Port {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl SessionAgentCoordination for Port {
    // Ownership/acquisition tests never invoke admission. Requiring these
    // callbacks preserves the complete-role boundary without granting activity.
    async fn prepare_prepared_session_run_activity(
        &self,
        _: &SessionId,
        _: &str,
        _: &RunId,
        _: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, RunError> {
        panic!("unused prepared coordination projection role was invoked")
    }
    async fn admit_prepared_session_run_activity(
        &self,
        _: &SessionId,
        _: &RunId,
        _: SessionRunActivityAdmissionMode,
        _: PreparedSessionRunActivity,
    ) -> Result<SessionRunActivityAdmission, RunError> {
        panic!("unused prepared coordination activity role was invoked")
    }
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
    ) -> Result<RunId, RunError> {
        panic!("unused background submission role was invoked")
    }
    async fn submit_task_attention(
        &self,
        _: awaken_session_application_contract::TaskAttention,
    ) -> Result<awaken_session_application_contract::TaskAttentionAck, RunError> {
        panic!("unused Task attention role was invoked")
    }

    // This binding fixture owns lifetimes only; no receipt authority is installed.
    async fn task_attention_receipt(
        &self,
        _: awaken_session_application_contract::TaskAttention,
    ) -> Result<
        Option<awaken_session_application_contract::TaskAttentionAck>,
        awaken_runtime_contract::runs::RunError,
    > {
        panic!("unused Task attention receipt role was invoked")
    }
}

fn address<T: ?Sized>(value: &Arc<T>) -> *const () {
    Arc::as_ptr(value).cast::<()>()
}

#[test]
fn binding_refuses_unbound_use_and_a_second_bind_and_keeps_mode_after_drop() {
    // The background role also requires an explicit receipt reader. The external
    // compiler control omits only Port's reader: the former trait default fills
    // that omission; the required contract rejects it with E0046. This unused
    // fixture never gains receipt authority from its submit or lifetime records.
    // Decision table, one row per binding state and cause:
    // | Row | state                         | cause            | effect                                   |
    // |-----|-------------------------------|------------------|------------------------------------------|
    // | B1  | unbound                       | any use          | `Unbound`; nothing bound                 |
    // | B2  | unbound                       | bind Local       | bound; roles are the exact inputs        |
    // | B3  | bound Local                   | second bind      | `AlreadyBound`; first roles unchanged,   |
    // |     |                               |                  | rejected roles not retained              |
    // | B4  | bound Local, roles dropped    | any use          | `RequiredApplicationUnavailable`; mode   |
    // |     |                               |                  | stays Local (no authority switch)        |
    // | B5  | bound Local, roles dropped    | bind again       | `AlreadyBound`; never rebinds            |
    let drops = Arc::new(AtomicUsize::new(0));
    let binding = SessionApplicationBinding::default();
    let shared = binding.clone();

    // B1
    assert_eq!(binding.mode(), Err(E::Unbound), "B1 mode");
    assert_eq!(
        binding.agent_coordination().err(),
        Some(E::Unbound),
        "B1 coordination"
    );
    assert_eq!(
        binding.background_runs().err(),
        Some(E::Unbound),
        "B1 background"
    );
    assert_eq!(binding.acquire().err(), Some(E::Unbound), "B1 acquire");

    // B2
    let coordination: Arc<dyn SessionAgentCoordination> = Arc::new(Port(drops.clone()));
    let background: Arc<dyn SessionRunBackgroundApplication> = Arc::new(Port(drops.clone()));
    let coordination_address = address(&coordination);
    let background_address = address(&background);
    let lifetime = binding
        .bind(&SessionApplications::Local {
            coordination: coordination.clone(),
            background_runs: background.clone(),
        })
        .expect("B2 the first bind succeeds");
    assert_eq!(
        shared.mode(),
        Ok(SessionApplicationMode::Local),
        "B2 shared"
    );
    assert_eq!(
        address(&shared.agent_coordination().expect("B2 coordination")),
        coordination_address
    );
    assert_eq!(
        address(&shared.background_runs().expect("B2 background")),
        background_address
    );

    // B3
    let rejected_coordination: Arc<dyn SessionAgentCoordination> = Arc::new(Port(drops.clone()));
    let rejected_background: Arc<dyn SessionRunBackgroundApplication> =
        Arc::new(Port(drops.clone()));
    let rejected = SessionApplications::Local {
        coordination: rejected_coordination.clone(),
        background_runs: rejected_background.clone(),
    };
    assert_eq!(shared.bind(&rejected).err(), Some(E::AlreadyBound), "B3");
    assert_eq!(
        shared.bind(&SessionApplications::Standalone).err(),
        Some(E::AlreadyBound),
        "B3 another mode"
    );
    drop(rejected);
    assert_eq!(
        Arc::strong_count(&rejected_coordination),
        1,
        "B3 not retained"
    );
    assert_eq!(
        Arc::strong_count(&rejected_background),
        1,
        "B3 not retained"
    );
    drop((rejected_coordination, rejected_background));
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "B3 rejected roles released"
    );
    assert_eq!(
        address(&binding.agent_coordination().expect("B3 first roles")),
        coordination_address
    );

    // B4
    drop(lifetime);
    drop(coordination);
    drop(background);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        4,
        "B4 the binding owns no role"
    );
    assert_eq!(
        binding.agent_coordination().err(),
        Some(E::RequiredApplicationUnavailable("agent coordination")),
        "B4 coordination"
    );
    assert_eq!(
        binding.background_runs().err(),
        Some(E::RequiredApplicationUnavailable(
            "background Run submission"
        )),
        "B4 background"
    );
    assert!(
        matches!(binding.acquire(), Err(E::RequiredApplicationUnavailable(_))),
        "B4 acquire"
    );
    assert_eq!(binding.mode(), Ok(SessionApplicationMode::Local), "B4 mode");

    // B5
    let replacement: Arc<dyn SessionAgentCoordination> = Arc::new(Port(drops.clone()));
    let replacement_background: Arc<dyn SessionRunBackgroundApplication> =
        Arc::new(Port(drops.clone()));
    assert_eq!(
        binding
            .bind(&SessionApplications::Local {
                coordination: replacement,
                background_runs: replacement_background,
            })
            .err(),
        Some(E::AlreadyBound),
        "B5"
    );
    assert!(binding.agent_coordination().is_err(), "B5 never rebinds");
}

#[test]
fn complete_selection_retains_sole_arcs_and_releases_every_clone() {
    // C1 each input is the sole strong Arc; C2 the original owner is dropped;
    // C3 an acquired/clone lifetime remains; C4 the final owner is released.
    // E1 both exact ports live through C1-C3; E2 C4 drops both exactly once;
    // E3 the binding keeps neither alive and cannot change mode.
    let drops = Arc::new(AtomicUsize::new(0));
    let coordination = Arc::new(Port(drops.clone()));
    let background = Arc::new(Port(drops.clone()));
    let coordination_weak = Arc::downgrade(&coordination);
    let background_weak = Arc::downgrade(&background);
    let coordination_address = Arc::as_ptr(&coordination) as *const ();
    let background_address = Arc::as_ptr(&background) as *const ();
    let binding = SessionApplicationBinding::default();
    let selection = SessionApplications::Local {
        coordination,
        background_runs: background,
    };
    let owner = binding.bind(&selection).expect("unbound binding binds");
    drop(selection);
    assert_eq!(binding.mode(), Ok(SessionApplicationMode::Local));
    assert_eq!(
        address(&binding.agent_coordination().unwrap()),
        coordination_address
    );
    assert_eq!(
        address(&binding.background_runs().unwrap()),
        background_address
    );
    let clone = owner.clone();
    drop(owner);
    let acquired = binding.acquire().expect("both roles still owned");
    drop(clone);
    assert!(coordination_weak.upgrade().is_some());
    assert!(background_weak.upgrade().is_some());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(acquired);
    assert!(coordination_weak.upgrade().is_none());
    assert!(background_weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(binding.acquire().is_err());
    assert_eq!(
        binding.mode(),
        Ok(SessionApplicationMode::Local),
        "expiry never changes selected authority"
    );
}

#[test]
fn incomplete_or_expired_local_pair_never_acquires_one_role() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum State {
        Expired,
        Alive,
    }
    // Each role independently has an expired owner or a live owner. A Local
    // binding without one of its roles is unrepresentable: `bind` takes both
    // (see the `SessionApplications` compile_fail example). All three
    // incomplete rows refuse and retain neither temporary acquisition. The
    // complete row is owned by the positive test.
    for coordination_state in [State::Expired, State::Alive] {
        for background_state in [State::Expired, State::Alive] {
            if coordination_state == State::Alive && background_state == State::Alive {
                continue;
            }
            let drops = Arc::new(AtomicUsize::new(0));
            let mut coordination: Option<Arc<dyn SessionAgentCoordination>> =
                Some(Arc::new(Port(drops.clone())));
            let mut background: Option<Arc<dyn SessionRunBackgroundApplication>> =
                Some(Arc::new(Port(drops.clone())));
            let binding = SessionApplicationBinding::default();
            drop(
                binding
                    .bind(&SessionApplications::Local {
                        coordination: coordination.clone().unwrap(),
                        background_runs: background.clone().unwrap(),
                    })
                    .expect("unbound binding binds"),
            );
            if coordination_state == State::Expired {
                drop(coordination.take());
            }
            if background_state == State::Expired {
                drop(background.take());
            }
            let coordination_count = coordination.as_ref().map(Arc::strong_count);
            let background_count = background.as_ref().map(Arc::strong_count);
            assert!(
                binding.acquire().is_err(),
                "{coordination_state:?}/{background_state:?}"
            );
            assert_eq!(
                coordination.as_ref().map(Arc::strong_count),
                coordination_count
            );
            assert_eq!(background.as_ref().map(Arc::strong_count), background_count);
            assert_eq!(binding.mode(), Ok(SessionApplicationMode::Local));
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
        let binding = SessionApplicationBinding::default();
        let owner = binding.bind(&selection).expect("unbound binding binds");
        assert_eq!(binding.mode(), Ok(expected));
        assert_eq!(
            binding.agent_coordination().err(),
            Some(E::NoLocalApplication)
        );
        assert_eq!(binding.background_runs().err(), Some(E::NoLocalApplication));
        assert!(binding.acquire().is_ok());
        drop(owner);
        assert!(
            binding.acquire().is_ok(),
            "no hidden local application dependency"
        );
    }
}
