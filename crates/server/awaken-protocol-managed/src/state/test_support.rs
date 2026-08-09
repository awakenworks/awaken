//! Shared Managed-state test fixtures.
//!
//! These fixtures serve rehydration, deployment-session, and activity tests.
//! Keeping the single fake Runtime here avoids parallel fake implementations in
//! those behavior modules while leaving each test beside the behavior it covers.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::{ManagedSessionRepository, PersistedSession, SessionInit};
use awaken_session_store::SqliteManagedSessionRepository;

use super::{
    DelegatedRun, OutcomeReport, RunError, SessionRuntime, StepOutcome, ToolPermissionDecision,
};

pub(super) fn ephemeral_session_repo() -> SqliteManagedSessionRepository {
    SqliteManagedSessionRepository::open_in_memory()
        .expect("open ephemeral managed Session repository")
}

pub(super) async fn create_session_fixture(
    repo: &dyn ManagedSessionRepository,
    owner: &str,
    mut session: PersistedSession,
) {
    session.revision = awaken_session_contract::SessionRevision(0);
    let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    repo.create(
        owner,
        session.clone(),
        awaken_session_contract::IdempotencyRecord {
            key: format!("test:create:{}:{payload_hash}", session.session_id),
            payload_hash,
        },
        Vec::new(),
    )
    .await
    .expect("create Session fixture");
}

pub(super) type RestoredRuntime = (
    String,
    Option<String>,
    usize,
    awaken_session_contract::SessionNetworkPolicy,
    serde_json::Value,
);

/// A runtime that reports a non-empty committed transcript, so a session can
/// rehydrate. Every operational method is unused by these tests.
#[derive(Clone, Default)]
pub(super) struct RehydrateFake {
    pub(super) restored: Arc<
        std::sync::Mutex<
            Vec<(
                String,
                String,
                awaken_session_contract::ResolvedSessionResources,
            )>,
        >,
    >,
    pub(super) restored_environments: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    pub(super) restored_runtimes: Arc<std::sync::Mutex<Vec<RestoredRuntime>>>,
    pub(super) delegated: Arc<std::sync::Mutex<Vec<DelegatedRun>>>,
    pub(super) order: Arc<std::sync::Mutex<Vec<&'static str>>>,
    pub(super) committed: Arc<std::sync::Mutex<Option<Vec<Message>>>>,
    pub(super) pending: Arc<std::sync::Mutex<Option<awaken_session_contract::Pending>>>,
    pub(super) ended: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) reject_environment_adoption: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl SessionRuntime for RehydrateFake {
    async fn prepare_session(&self, thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.order.lock().unwrap().push("runtime");
        self.restored_runtimes.lock().unwrap().push((
            thread.to_string(),
            init.runtime,
            0,
            init.environment.network,
            init.environment.sandbox,
        ));
        Ok(())
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

    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        unreachable!()
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.order.lock().unwrap().push("history");
        if let Some(messages) = self.committed.lock().unwrap().clone() {
            return Ok(messages);
        }
        Ok(vec![Message::text(
            awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
            awaken_agent_contract::agent::message::Role::User,
            "hello",
        )])
    }

    async fn pending_tool(
        &self,
        _thread: &str,
    ) -> Result<Option<awaken_session_contract::Pending>, RunError> {
        Ok(self.pending.lock().unwrap().clone())
    }

    async fn delegated_runs(&self, _thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.order.lock().unwrap().push("delegations");
        Ok(self.delegated.lock().unwrap().clone())
    }

    async fn end_session(&self, thread: &str) -> Result<(), RunError> {
        self.ended.lock().unwrap().push(thread.to_string());
        Ok(())
    }

    async fn restore_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        if self
            .reject_environment_adoption
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::internal("sandbox is owned by another runtime"));
        }
        self.order.lock().unwrap().push("environment");
        self.restored_environments.lock().unwrap().push((
            agent.to_string(),
            thread.to_string(),
            binding.to_string(),
        ));
        Ok(())
    }

    fn model(&self) -> String {
        "host-default-model".to_string()
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        _resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.order.lock().unwrap().push("resources");
        self.restored.lock().unwrap().push((
            thread.to_string(),
            workspace_id.to_string(),
            inputs.clone(),
        ));
        Ok(())
    }
}

#[async_trait]
impl awaken_session_contract::McpAttachmentRealizer for RehydrateFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.order.lock().unwrap().push("mcp");
        if let Some(restored) = self
            .restored_runtimes
            .lock()
            .unwrap()
            .iter_mut()
            .find(|restored| restored.0 == request.generation.session_id)
        {
            restored.2 += 1;
        }
        let receipt_fingerprint = request.fingerprint();
        Ok(awaken_session_contract::McpRealizationReceipt {
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
            receipt_fingerprint,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}
