// @formal entrypoint memory_extraction.unknown_execution_never_reenters_retry
//! Out-of-band memory extraction: the process-startup wiring.
//!
//! The persistence half — the `write_memory` tool, the extractor agent's config
//! and prompts, the file store, and bounded recall — lives in `awaken-ext-memory`
//! (a bounded context). This module wires those onto the host's aux-agent
//! substrate: it runs the extractor as an ordinary sub-agent through
//! the shared Agent Run substrate, fire-and-forget via [`BackgroundRuns`], triggered by
//! the host after a Run, and reads memories back for recall.

use awaken_credential_contract::{
    AttemptCredentialBinding, AttemptCredentialRealization, CredentialRealizationProfile,
    CredentialRealizationReceipt, CredentialRealizationRecordError, CredentialRealizationRecorder,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::{future::Future, pin::Pin, time::Duration};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::transcript::TranscriptSnapshot;
use awaken_ext_builtin_tools::erase;
use awaken_ext_memory::{
    EXTRACT_PROMPT, MemoryExtractionController, MemoryExtractionDriver, MemoryExtractionError,
    MemoryExtractionIntent, MemoryExtractionMutation, MemoryExtractionOutcome,
    MemoryExtractionRepository, MemoryExtractorSnapshot, MemoryMutationReceipt, MemoryStoreHandle,
    MemoryTerminalExtractionRequest, WriteMemoryTool, accepts_memory_content, sanitize_stem,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
#[cfg(test)]
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::agent_runner::{AgentRunError, run_configured_agent_with_id};
use crate::background::{BackgroundRuns, BackgroundWorkClass};
use crate::store::HostCommit;

mod selector;
mod terminal_observer;
pub(crate) use crate::memory_binding::{MemoryBinding, PlatformMemoryHandle};
pub(crate) use selector::AgentSelector;

static EXTRACTION_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

mod captured_writes;
use captured_writes::{CapturedMemoryWrites, writes_from_committed_agent};

/// Host-level extraction/selection capability. It owns the auxiliary-agent
/// machinery but deliberately owns no MemoryStore identity or content handle.
/// A Session's realization projects a [`MemoryBinding`] from its resolved
/// resource manifest; [`MemoryRuntime::bound`] pairs it with this runtime.
pub struct MemoryRuntime {
    llm: Arc<dyn LlmExecutor>,
    /// The Host's runtime-only inference materialization, when its
    /// composition installed one (`HostDependencies::with_inference_materializer`).
    inference_materializer:
        Option<Arc<dyn awaken_credential_materializer::InferenceExecutorMaterializer>>,
    /// Fresh sandboxes for extraction/selection Runs, from the process
    /// composition's `HostDependencies::with_auxiliary_sandboxes`.
    provider: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
    background: Arc<BackgroundRuns>,
    claim_owner: String,
    extractions: Arc<dyn MemoryExtractionRepository>,
    /// The Host's process clock extraction claims and receipts are stamped on.
    clock: Arc<dyn awaken_time::WallClock>,
}

/// One realized Session MemoryStore binding paired with the host's extraction
/// runtime: recall reads through the binding, extraction runs on the runtime.
#[derive(Clone)]
pub(crate) struct BoundMemory {
    runtime: Arc<MemoryRuntime>,
    binding: Arc<MemoryBinding>,
}

/// Adapter from the Memory bounded context's durable claim to the common
/// attempt-credential SPIs. It owns no materialization logic: it only proves
/// the exact extraction claim and stores the common secret-free receipt back in
/// that same aggregate.
struct MemoryExtractionCredentialAuthority {
    repository: Arc<dyn MemoryExtractionRepository>,
    /// The Host's process clock the claim lease is judged on.
    clock: Arc<dyn awaken_time::WallClock>,
    intent_id: String,
    owner: String,
    generation: u64,
    bindings: Vec<AttemptCredentialBinding>,
}

impl MemoryExtractionCredentialAuthority {
    async fn current(&self) -> Result<MemoryExtractionIntent, String> {
        let intent = self
            .repository
            .get_extraction(&self.intent_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Memory extraction credential claim disappeared".to_string())?;
        let now = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        if intent.claim_owner.as_deref() != Some(self.owner.as_str())
            || intent.claim_generation != self.generation
            || intent
                .lease_expires_at_unix_ms
                .is_none_or(|expires| expires <= now)
        {
            return Err("Memory extraction credential claim is stale".into());
        }
        Ok(intent)
    }
}

#[async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for MemoryExtractionCredentialAuthority {
    async fn verify_current(&self) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        self.current()
            .await
            .map(|_| ())
            .map_err(|_| awaken_runtime_contract::AttemptOwnershipError::Lost)
    }
}

#[async_trait]
impl CredentialRealizationRecorder for MemoryExtractionCredentialAuthority {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), CredentialRealizationRecordError> {
        awaken_credential_contract::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
        // The extraction intent is SDK state; it carries the receipt as an
        // opaque token fenced by claim epoch and candidate (ADR-0086 §7).
        let receipt = serde_json::to_value(&receipt)
            .and_then(awaken_ext_memory::CredentialRealizationToken::new)
            .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
        for _ in 0..4 {
            let mut intent = self
                .current()
                .await
                .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
            let expected_revision = intent.revision;
            intent
                .record_credential_realization(
                    &self.owner,
                    self.generation,
                    awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
                    receipt.clone(),
                )
                .map_err(|error| CredentialRealizationRecordError(error.to_string()))?;
            if intent.revision == expected_revision {
                // The aggregate already contains this exact receipt. Its
                // idempotent command deliberately does not bump revision, so
                // issuing a CAS that requires expected+1 would manufacture a
                // conflict on the second model step.
                return Ok(());
            }
            match self
                .repository
                .compare_and_swap_extraction(expected_revision, intent)
                .await
            {
                Ok(()) => return Ok(()),
                Err(MemoryExtractionError::RevisionConflict(_)) => continue,
                Err(error) => {
                    return Err(CredentialRealizationRecordError(error.to_string()));
                }
            }
        }
        Err(CredentialRealizationRecordError(
            "Memory extraction credential receipt CAS remained contended".into(),
        ))
    }
}

impl MemoryRuntime {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
        background: Arc<BackgroundRuns>,
        extractions: Arc<dyn MemoryExtractionRepository>,
        clock: Arc<dyn awaken_time::WallClock>,
        inference_materializer: Option<
            Arc<dyn awaken_credential_materializer::InferenceExecutorMaterializer>,
        >,
    ) -> Self {
        Self {
            clock,
            llm,
            inference_materializer,
            provider,
            background,
            claim_owner: format!(
                "memory-extractor:{}:{}",
                std::process::id(),
                EXTRACTION_OWNER_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            extractions,
        }
    }

    pub(crate) fn bound(self: &Arc<Self>, binding: Arc<MemoryBinding>) -> BoundMemory {
        BoundMemory {
            runtime: self.clone(),
            binding,
        }
    }

    /// Await every in-flight extraction started by any bound Session.
    pub async fn drain(&self, timeout: Duration) -> bool {
        self.background.drain(timeout).await
    }

    pub(crate) fn background(&self) -> Arc<BackgroundRuns> {
        self.background.clone()
    }

    fn materialize_extractor(
        &self,
        intent: &MemoryExtractionIntent,
    ) -> Result<(Arc<dyn LlmExecutor>, RuntimeRunContext), String> {
        let snapshot = &intent.extractor;
        if let Some(materializer) = self.inference_materializer.as_ref() {
            let holder = CredentialRealizationProfile::self_hosted_native().inference_holder;
            let model = &snapshot.agent.resolved_spec.model_binding;
            let bindings = awaken_credential_contract::compile_candidate_credential_bindings(
                &[model],
                Some(&holder),
                &materializer.credential_realization_capabilities(),
                intent.claim_generation,
                awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
            )
            .map_err(|error| error.to_string())?;
            let owner = intent
                .claim_owner
                .clone()
                .ok_or_else(|| "Memory extractor has no durable claim owner".to_string())?;
            let authority = Arc::new(MemoryExtractionCredentialAuthority {
                repository: self.extraction_repository(),
                clock: self.clock.clone(),
                intent_id: intent.intent_id.clone(),
                owner,
                generation: intent.claim_generation,
                bindings: bindings.clone(),
            });
            let context = RuntimeRunContext::new()
                .with_ownership(authority.clone())
                .with_credential_realization(AttemptCredentialRealization::new(
                    bindings, authority,
                ));
            return materializer
                .materialize_pinned(model, &context)
                .map(|executor| (executor, context))
                .ok_or_else(|| {
                    format!(
                        "published model candidate `{}` is unavailable",
                        model.binding().identity().1
                    )
                });
        }
        matches!(
            snapshot.agent.resolved_spec.model_binding.provisioning(),
            awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
        )
        .then(|| (self.llm.clone(), RuntimeRunContext::new()))
        .ok_or_else(|| {
            "published model candidate requires an installed credential materializer".into()
        })
    }

    pub(crate) fn extraction_repository(&self) -> Arc<dyn MemoryExtractionRepository> {
        self.extractions.clone()
    }

    fn extraction_controller(&self) -> MemoryExtractionController {
        MemoryExtractionController::new(
            self.extraction_repository(),
            self.claim_owner.clone(),
            self.clock.clone(),
        )
    }
}

impl BoundMemory {
    pub(crate) fn binding(&self) -> &Arc<MemoryBinding> {
        &self.binding
    }

    async fn bind_recovery(&self, commit: Arc<HostCommit>) -> Result<bool, MemoryExtractionError> {
        self.binding
            .attach_extraction_commit(commit)
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?;
        self.reconcile().await
    }

    /// Durably enqueue one extraction keyed by the terminal commit, then drive it
    /// asynchronously. Returning from this method means the intent is persistent,
    /// not that extraction has completed.
    pub async fn trigger(
        &self,
        terminal_commit_id: &str,
        committed: TranscriptSnapshot,
        extractor: MemoryExtractorSnapshot,
    ) -> Result<(), MemoryExtractionError> {
        self.runtime
            .extraction_controller()
            .enqueue_terminal(MemoryTerminalExtractionRequest {
                workspace_id: self.binding.workspace_id.clone(),
                session_id: self.binding.session_thread_id.clone(),
                terminal_run_id: terminal_commit_id.to_string(),
                memory_store_id: self.binding.memory_store_id.clone(),
                memory_config_version: self.binding.memory_config_version,
                committed_transcript: committed,
                extractor,
            })
            .await?;
        self.reconcile().await?;
        Ok(())
    }

    /// RunResume every non-terminal intent for this exact physical Session and
    /// frozen resource binding. Invoked after enqueue and after parent Session
    /// rehydration, so a process crash cannot lose a delegated child's remaining
    /// extraction/store/receipt work.
    pub async fn reconcile(&self) -> Result<bool, MemoryExtractionError> {
        if self
            .binding
            .extraction_commit()
            .map_err(|error| MemoryExtractionError::Storage(error.to_string()))?
            .is_none()
        {
            return Ok(false);
        }
        let controller = self.runtime.extraction_controller();
        if !controller.has_recoverable(self).await? {
            return Ok(false);
        }
        let bound = self.clone();
        self.runtime
            .background
            .spawn(
                BackgroundWorkClass::ExternalDurable {
                    durable_intent_id: format!(
                        "memory-extraction:{}",
                        self.binding.session_thread_id
                    ),
                },
                async move {
                    bound.drive_recoverable().await;
                },
            )
            .await;
        Ok(true)
    }

    async fn drive_recoverable(&self) {
        self.runtime
            .extraction_controller()
            .drive_recoverable(self)
            .await;
    }

    async fn extract_mutations(&self, intent: &MemoryExtractionIntent) -> MemoryExtractionOutcome {
        let capture = Arc::new(CapturedMemoryWrites::default());
        let mut seed = intent.transcript.clone();
        seed.push(Message {
            id: MessageId(format!("{}-mem-prompt", intent.terminal_commit_id)),
            role: Role::User,
            content: vec![ContentBlock::text(
                intent
                    .extractor
                    .extraction_prompt
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(EXTRACT_PROMPT),
            )],
        });
        let catalog = AgentCatalog::new().with_agent(intent.extractor.agent.clone());
        let (executor, context) = match self.runtime.materialize_extractor(intent) {
            Ok(materialized) => materialized,
            Err(reason) => return MemoryExtractionOutcome::Retryable { reason },
        };
        let tool = erase(WriteMemoryTool::from_handle(capture.clone()));
        let commit = match self.binding.extraction_commit() {
            Ok(Some(commit)) => commit,
            Err(error) => {
                return MemoryExtractionOutcome::Retryable {
                    reason: error.to_string(),
                };
            }
            Ok(None) => {
                return MemoryExtractionOutcome::Retryable {
                    reason: "Memory extraction is waiting for the Session commit/history binding"
                        .to_string(),
                };
            }
        };
        let context = context
            .with_commit(commit.clone() as Arc<dyn awaken_agent_contract::CommitCoordinator>)
            .with_reader(commit.clone() as Arc<dyn CommittedThreadView>);
        let auxiliary_thread_id = intent.auxiliary_thread_id();
        let auxiliary_sandboxes = self.runtime.provider.clone();
        if let Err(error) = run_configured_agent_with_id(
            &catalog,
            crate::agent_runner::AgentRunSandbox::Fresh(auxiliary_sandboxes.as_ref()),
            executor,
            self.runtime.clock.clone(),
            &intent.extractor.agent.root_agent_id.0,
            &auxiliary_thread_id,
            RunId(intent.auxiliary_run_id()),
            seed,
            vec![tool],
            context,
        )
        .await
        {
            return classify_extraction_run_error(
                intent,
                commit.run_state(&RunId(intent.auxiliary_run_id())),
                error,
            );
        }
        let mut writes = capture.take();
        if writes.is_empty() {
            writes = writes_from_committed_agent(
                &commit.committed_messages(&ThreadId(auxiliary_thread_id)),
            );
        }
        match self.binding.platform().plan_mutations(writes).await {
            Ok(mutations) => MemoryExtractionOutcome::Mutations(mutations),
            Err(reason) => MemoryExtractionOutcome::Retryable { reason },
        }
    }

    /// A live-validating Memory handle shared with the recall plugin. Returning
    /// the underlying data-plane handle would let an already-bound Session bypass
    /// a later suspend/archive transition.
    pub(crate) fn store(&self) -> Arc<dyn MemoryStoreHandle> {
        self.binding.clone()
    }
}

#[async_trait]
impl MemoryExtractionDriver for BoundMemory {
    fn accepts(&self, intent: &MemoryExtractionIntent) -> bool {
        self.binding.matches_intent(intent)
    }

    async fn validate_binding(&self, _intent: &MemoryExtractionIntent) -> Result<(), String> {
        self.binding
            .validate_live_resource()
            .await
            .map_err(|_| "MemoryStore is missing, suspended, archived, or deleted".to_string())
    }

    async fn extract(&self, intent: &MemoryExtractionIntent) -> MemoryExtractionOutcome {
        self.extract_mutations(intent).await
    }

    async fn apply(
        &self,
        _intent: &MemoryExtractionIntent,
        mutation: &MemoryExtractionMutation,
    ) -> Result<MemoryMutationReceipt, String> {
        self.binding.platform().apply_mutation(mutation).await
    }
}

fn classify_extraction_run_error(
    intent: &MemoryExtractionIntent,
    observed_state: Option<RunState>,
    error: AgentRunError,
) -> MemoryExtractionOutcome {
    let reason = error.to_string();
    match error {
        AgentRunError::Configuration(_) => MemoryExtractionOutcome::PermanentFailure { reason },
        AgentRunError::Provisioning(_) => MemoryExtractionOutcome::Retryable { reason },
        AgentRunError::Runtime(_) => match observed_state {
            Some(RunState::Ended(EndCause::Indeterminate)) | None | Some(RunState::Running) => {
                MemoryExtractionOutcome::ExecutionIndeterminate {
                    auxiliary_run_id: intent.auxiliary_run_id(),
                    reason,
                }
            }
            Some(RunState::Ended(_)) | Some(RunState::Awaiting) => {
                MemoryExtractionOutcome::PermanentFailure { reason }
            }
        },
    }
}

impl crate::RunAssembly {
    /// The Workspace-scoped Skill aggregate port shared by HTTP authoring and
    /// runtime activation. `None` means this host has no durable Skill plane.
    #[cfg(any(test, feature = "test-support"))]
    pub fn skill_store(&self) -> Option<Arc<dyn awaken_resource_contract::SkillStore>> {
        self.tools.skills.store_handle()
    }

    /// Replace the complete set of standard mounted MemoryStore bindings. No
    /// automatic recall/extraction binding is inferred from collection order.
    #[cfg(test)]
    pub(crate) fn register_thread_memory_bindings(
        &self,
        thread: &str,
        bindings: std::collections::HashMap<String, Arc<MemoryBinding>>,
    ) {
        self.installed_sessions.install(thread);
        let realizations = self.installed_sessions.realizations();
        realizations
            .update_realization(thread, |r| {
                r.staging_mut().memory_bindings = bindings;
                r.staging_mut().memory = None;
            })
            .expect("installed Sessions");
    }

    pub(crate) fn memory_for_thread(&self, thread: &str) -> Option<BoundMemory> {
        let realizations = self.installed_sessions.realizations();
        realizations
            .read_realization(thread, |r| r.staging().memory.clone())
            .expect("session runtime slots mutex poisoned")
            .flatten()
            .map(|binding| self.resources.memory.bound(binding))
    }

    /// Attach the one physical Session commit authority to every writable
    /// MemoryStore binding restored from its frozen manifest, then resume any
    /// matching durable intent. Recovery is resource-binding-owned rather than
    /// dependent on whether the parent Agent selects the Memory plugin: a child
    /// may have selected extraction even when its parent did not.
    pub(crate) fn bind_thread_memory_recovery<'a>(
        &'a self,
        session_thread: &'a str,
        commit: Arc<HostCommit>,
    ) -> Pin<Box<impl Future<Output = Result<usize, crate::HostError>> + Send + 'a>> {
        // Keep extraction recovery's typed state out of enclosing context polls,
        // including resident-cache hits during the next Run of the same Session.
        Box::pin(async move {
            let realizations = self.installed_sessions.realizations();
            // A database-less execution Worker owns only the frozen Memory data-plane
            // binding. The Coordinator's guarded settlement observer is the sole
            // owner of remote terminal observation and its durable extraction outbox;
            // probing the Worker's fail-closed repository here would duplicate that
            // authority and prevent an otherwise valid claimed Runtime from starting.
            if !self.authority.observes_terminal_memory_locally() {
                return Ok(0);
            }
            let bindings = realizations
                .read_realization(session_thread, |r| {
                    let mut bindings: Vec<Arc<MemoryBinding>> = Vec::new();
                    for memory in r
                        .staging()
                        .memory_bindings
                        .values()
                        .cloned()
                        .chain(r.staging().memory.iter().cloned())
                    {
                        if !bindings
                            .iter()
                            .any(|existing| Arc::ptr_eq(existing, &memory))
                        {
                            bindings.push(memory);
                        }
                    }
                    bindings
                })?
                .unwrap_or_default();
            let mut scheduled = 0;
            for binding in bindings {
                if binding.extraction_enabled
                && self
                    .resources
                    .memory
                    .bound(binding)
                    .bind_recovery(commit.clone())
                    .await
                    .map_err(|error| {
                        crate::HostError::internal(format!(
                            "recover Memory extraction for physical Session `{session_thread}`: {error}"
                        ))
                    })?
            {
                scheduled += 1;
            }
            }
            Ok(scheduled)
        })
    }

    /// Install one already-resolved MemoryStore binding for a thread. This is an
    /// ACL/startup seam for embedders: ownership and authorization must have
    /// completed before calling it; the Runtime Host receives only the opaque store
    /// id, pinned config, and maximum access.
    pub fn bind_resolved_memory(
        &self,
        thread: &str,
        workspace_id: &str,
        config: &awaken_resource_contract::MemoryStoreConfigVersion,
        access: awaken_resource_contract::ResourceAccess,
        resource_validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    ) {
        let writable = access == awaken_resource_contract::ResourceAccess::ReadWrite;
        let handle = Arc::new(PlatformMemoryHandle::from_roles(
            self.resources.memory_stores.clone(),
            config.memory_store_id.to_string(),
            writable,
        ));
        let binding = MemoryBinding::new(
            thread,
            workspace_id,
            handle,
            Some(resource_validator),
            config,
            writable,
        );
        // The embedder seam installs the Session it binds (the host's install
        // path); the realization view then only changes the installed slot.
        self.installed_sessions.update(thread, |_| ());
        self.realization_executor
            .staging()
            .register_thread_memory(thread, Some(Arc::new(binding)))
            .expect("session runtime slots mutex poisoned");
    }
}

#[cfg(test)]
mod tests;
