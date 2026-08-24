use super::test_support::{RehydrateFake, create_session_fixture, ephemeral_session_repo};
use super::*;
use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::ToolPermissionDecision;
use std::collections::{BTreeMap, BTreeSet};

fn ephemeral_resource_registry() -> awaken_resource_application::RegistryApplication {
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Registry"),
    );
    awaken_resource_application::RegistryApplication::new(storage)
}

/// A runtime that records every `end_session` thread it is asked to tear down,
/// so a test can prove the terminal edges (delete/archive) reach the host's
/// sandbox disposal rather than leaking it. Every driving method is unused.
#[derive(Clone, Default)]
struct EndSessionRecorder {
    ended: Arc<std::sync::Mutex<Vec<String>>>,
    interrupted: Arc<std::sync::Mutex<Vec<String>>>,
    prepared: Arc<std::sync::Mutex<Vec<String>>>,
    delegated: Arc<std::sync::Mutex<Vec<awaken_session_contract::DelegatedRun>>>,
    block_quiesce: Arc<std::sync::atomic::AtomicBool>,
    quiesce_entered: Arc<tokio::sync::Notify>,
    quiesce_release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl SessionRuntime for EndSessionRecorder {
    async fn quiesce_terminal_delegations(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, RunError> {
        if self.block_quiesce.load(std::sync::atomic::Ordering::SeqCst) {
            self.quiesce_entered.notify_one();
            self.quiesce_release.notified().await;
        }
        Ok(awaken_session_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated.lock().unwrap().clone(),
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: 0,
        })
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn prepare_session(&self, thread: &str, _init: SessionInit) -> Result<(), RunError> {
        self.prepared.lock().unwrap().push(thread.to_string());
        Ok(())
    }
    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        unreachable!()
    }
    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // Terminal fake rule T0: these tests never commit a Run, so the one
        // consistency owner truthfully returns no snapshot. The production
        // default remains fail-closed and no split messages/ticket fallback is
        // reintroduced merely to make archive/delete tests executable.
        Ok(None)
    }
    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        self.ended.lock().unwrap().push(command.thread_id.clone());
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
    }
    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupted.lock().unwrap().push(thread.to_string());
        Ok(())
    }
    async fn delegated_runs(
        &self,
        _thread: &str,
    ) -> Result<Vec<awaken_session_contract::DelegatedRun>, RunError> {
        Ok(self.delegated.lock().unwrap().clone())
    }
    fn model(&self) -> String {
        "host-default-model".to_string()
    }
}

/// Cause/effect graph: C1 deployment disables the local pool; C2 the Session's
/// Environment definition is otherwise local. C1 freezes the selected Runtime placement fact,
/// which causes E1 one dispatch-only Runtime projection, E2 no local
/// realization lease, and E3 no resident environment binding. Without C1,
/// the canonical local phase driver owns realization (covered by the
/// existing create/realization tests).
///
/// | Rule | No local pool | Environment | Projection | Local lease/binding |
/// |---|---|---|---|---|
/// | P1 | yes | local | once | none |
/// | P2 | no | local | local phase driver | local |
mod binding_recovery;
mod external_identity;
mod terminal;

pub(in crate::state) use binding_recovery::sample_inputs;
pub(in crate::state) use external_identity::bare_create_params;
pub(in crate::state) use terminal::sample_persisted;
