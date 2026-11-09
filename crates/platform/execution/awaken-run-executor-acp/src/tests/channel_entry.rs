//! Channel entry must satisfy the existing hosted model-access liveness contract.
use super::*;
use tokio::sync::Mutex as ObservationLock;

struct AdmissionSource {
    script: ScriptedSource,
    observed: Arc<ObservationLock<Vec<Option<RunState>>>>,
}

#[async_trait]
impl AgentChannelSource for AdmissionSource {
    fn check_tool_narrowing(
        &self,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> std::result::Result<(), OpenError> {
        self.script.check_tool_narrowing(narrowing)
    }

    async fn open(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
        door: ChannelOpenDoor,
    ) -> std::result::Result<AgentSession, OpenError> {
        self.observed.lock().await.push(
            context
                .reader
                .as_ref()
                .and_then(|reader| reader.run_state(&activation.run_id)),
        );
        self.script.open(activation, context, door).await
    }
}

#[tokio::test]
async fn fresh_acp_channel_observes_committed_running_before_model_authorization() {
    // Causes: C1 a fresh admitted Run has no journal record; C2 the same
    // authoritative Coordinator and reader are wired; C3 channel entry resolves
    // hosted model access, which requires a committed Running Run. Effects:
    // E1 Running is durable before channel entry; E2 final messages/outcome still
    // use that one Coordinator. Rule G1 C1+C2+C3 -> E1+E2. Before this regression,
    // channel entry observed None, so the real Gateway resolver rejected 404.
    // The scripted channel reuses the existing protocol/process fixture; it
    // records actual executor entry and cannot pre-seed the required Run fact.
    let (executor, committed, observed) = fixture();
    let activation = activation_without_session_home();
    let context = RuntimeRunContext::new()
        .with_commit(committed.clone())
        .with_reader(committed.clone());
    assert_eq!(committed.run_state(&activation.run_id), None, "C1");
    assert_eq!(
        executor.execute(activation.clone(), context).await.unwrap(),
        RunState::Ended(EndCause::NaturalEnd),
        "E2",
    );
    assert_eq!(
        *observed.lock().await,
        vec![Some(RunState::Running)],
        "G1: the real hosted model resolver must see committed Running before ACP channel entry",
    );
    let commits = committed.commits.lock();
    assert_eq!(commits.len(), 2, "one entry and one terminal boundary");
    assert!(
        commits[0].messages.is_empty(),
        "entry cannot duplicate accepted input"
    );
    assert_eq!(
        commits
            .iter()
            .flat_map(|c| &c.messages)
            .filter(|m| m.id == activation.input[0].id)
            .count(),
        1
    );
}

type EntryFixture = (
    AcpRunExecutor,
    Arc<RecordingCoordinator>,
    Arc<ObservationLock<Vec<Option<RunState>>>>,
);

fn fixture() -> EntryFixture {
    let committed = Arc::new(RecordingCoordinator::default());
    let observed = Arc::new(ObservationLock::new(Vec::new()));
    let executor = AcpRunExecutor::new(Arc::new(AdmissionSource {
        script: ScriptedSource {
            frames: vec![
                r#"{"type":"message","text":"done"}"#.into(),
                r#"{"type":"turn_end","reason":"natural_end"}"#.into(),
            ],
            open_error: None,
        },
        observed: observed.clone(),
    }));
    (executor, committed, observed)
}

#[path = "channel_entry_matrix.rs"]
mod matrix;
