//! Host test fixtures shared across the crate's test modules (re-exported by
//! `host` for the crate): the stub model, the resource validator, the remote
//! terminal-cleanup Control double, and the Memory mounter installer.
use crate::host::*;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
use awaken_session_types::SessionId;
use std::collections::BTreeMap;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub(crate) struct TestLiveResourceBindingVerifier;

pub(crate) fn test_resource_validator()
-> Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier> {
    Arc::new(TestLiveResourceBindingVerifier)
}

#[async_trait::async_trait]
impl awaken_resource_contract::LiveResourceBindingVerifier for TestLiveResourceBindingVerifier {
    async fn verify_memory_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        Ok(())
    }

    async fn verify_repository_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        Ok(())
    }
}

/// Test startup adapter for runtime-host's dependency-inverted MemoryMounter
/// port. Production installs `awaken-sandbox-memoryd` from awaken-coordinator.
pub(crate) struct TestMemoryMounter {
    pub(crate) fs: Arc<dyn awaken_memory_store::MemoryRepository>,
}

pub(crate) struct TestMemoryMount {
    pub(crate) heads: Vec<awaken_provisioning_contract::MemoryMaterializationHead>,
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMount for TestMemoryMount {
    fn realization(&self) -> awaken_provisioning_contract::Realization {
        awaken_provisioning_contract::Realization::Copy
    }

    fn materialization_heads(
        &self,
    ) -> Option<Vec<awaken_provisioning_contract::MemoryMaterializationHead>> {
        Some(self.heads.clone())
    }

    async fn teardown(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMounter for TestMemoryMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &std::path::Path,
        _access: awaken_provisioning_contract::MountAccess,
    ) -> awaken_provisioning_contract::MemoryMountOutcome {
        let result: Result<
            Box<dyn awaken_provisioning_contract::MemoryMount>,
            awaken_provisioning_contract::SandboxError,
        > = async {
            std::fs::create_dir_all(host_path).map_err(|error| {
                awaken_provisioning_contract::SandboxError::backend("mount", error)
            })?;
            let mut heads = Vec::new();
            for entry in self
                .fs
                .list(store_id, "/")
                .await
                .map_err(awaken_provisioning_contract::SandboxError::from)?
            {
                let Some(memory) = self
                    .fs
                    .get_by_path(store_id, &entry.path)
                    .await
                    .map_err(awaken_provisioning_contract::SandboxError::from)?
                else {
                    continue;
                };
                heads.push(awaken_provisioning_contract::MemoryMaterializationHead {
                    path: memory.path.clone(),
                    id: memory.id.clone(),
                    version: memory.version,
                    content_sha256: memory.content_sha256.clone(),
                });
                let path = host_path.join(memory.path.trim_start_matches('/'));
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        awaken_provisioning_contract::SandboxError::backend("mount", error)
                    })?;
                }
                std::fs::write(path, memory.content.unwrap_or_default()).map_err(|error| {
                    awaken_provisioning_contract::SandboxError::backend("mount", error)
                })?;
            }
            heads.sort_by(|left, right| left.path.cmp(&right.path));
            Ok(Box::new(TestMemoryMount { heads })
                as Box<dyn awaken_provisioning_contract::MemoryMount>)
        }
        .await;
        awaken_provisioning_contract::MemoryMountOutcome::from_no_effect(result)
    }

    async fn recover_unknown_mount(
        &self,
        _: &str,
        _: &std::path::Path,
        _: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    async fn reconcile_recovered_copy(
        &self,
        reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let _ = (reference, evidence, files, access);
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "recovered Memory copy reconciliation is outside this fixture",
        ))
    }
}

pub(crate) fn install_test_memory_mounter(host: &SharedHost) {
    host.install_memory_mounter(Arc::new(TestMemoryMounter {
        fs: host.memory_repository(),
    }));
}

/// The main assistant answers plainly; the memory extractor (identified by its
/// system instructions) saves one memory then reports done.
pub(crate) struct MemoryHostModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryHostModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        let output = if is_extractor {
            if request.messages.iter().any(|m| m.role == Role::Tool) {
                AssistantOutput::text("saved 1 memory")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({
                        "name": "user prefs",
                        "content": "user likes rust",
                    }),
                }])
            }
        } else {
            AssistantOutput::text("ok")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

pub(crate) fn user(text: &str) -> Vec<Message> {
    vec![Message::text(MessageId("u1".into()), Role::User, text)]
}

#[derive(Default)]
pub(crate) struct RemoteTerminalCleanupControl {
    pub(crate) assignments: Mutex<
        std::collections::VecDeque<
            awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        >,
    >,
    pub(crate) session: Mutex<Option<awaken_session_state_contract::PersistedSession>>,
    pub(crate) preparations: Mutex<Vec<awaken_session_state_contract::SessionCleanupPreparation>>,
    pub(crate) disposals: Mutex<Vec<awaken_session_state_contract::SessionCleanupDisposalReceipt>>,
    pub(crate) publication_receipts:
        Mutex<Vec<awaken_session_state_contract::SessionRepositoryPublicationReceipt>>,
    pub(crate) publication_rejections:
        Mutex<Vec<awaken_session_state_contract::SessionRepositoryPublicationRejection>>,
    pub(crate) events: Mutex<Vec<String>>,
    pub(crate) disposal_response_loss_once: AtomicBool,
    pub(crate) authorization_barrier:
        Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    pub(crate) terminal_work_block_for: Mutex<Option<String>>,
    pub(crate) terminal_work_started: tokio::sync::Notify,
    pub(crate) terminal_work_release: tokio::sync::Notify,
    pub(crate) terminal_work_seen: Mutex<Vec<String>>,
    pub(crate) terminal_work_fail_for: Mutex<Option<String>>,
    pub(crate) terminal_work_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    pub(crate) terminal_work_active: AtomicUsize,
    pub(crate) max_terminal_work_active: AtomicUsize,
    pub(crate) claim_failure_on_call: Mutex<Option<usize>>,
    pub(crate) renewal_failure:
        Mutex<Option<awaken_session_realization_contract::SessionRealizationControlFailure>>,
    pub(crate) claim_targets:
        Mutex<Vec<awaken_session_realization_contract::SessionRealizationTarget>>,
    pub(crate) renewal_sessions: Mutex<Vec<String>>,
    pub(crate) renewal_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    pub(crate) renewal_response_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    pub(crate) renewal_response_ready: tokio::sync::Notify,
    pub(crate) renewal_delay_ms: Mutex<std::collections::HashMap<String, u64>>,
    pub(crate) renewals_active: AtomicUsize,
    pub(crate) max_renewals_active: AtomicUsize,
    pub(crate) renewals:
        Mutex<BTreeMap<String, awaken_session_resource_contract::SessionRealizationLease>>,
    pub(crate) authority: Mutex<
        Option<(
            awaken_session_realization_contract::FrozenSessionProjection,
            awaken_session_resource_contract::SessionRealizationLease,
        )>,
    >,
}

pub(crate) struct ActiveControlCallGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveControlCallGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionRealizationControl
    for RemoteTerminalCleanupControl
{
    async fn begin_session_realization(
        &self,
        command: awaken_session_realization_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        let _ = command;
        Err(self.renewal_failure.lock().unwrap().clone().unwrap_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        ))
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_realization_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationLeaseReceipt,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        use std::sync::atomic::Ordering;

        let active = self.renewals_active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveControlCallGuard(&self.renewals_active);
        self.max_renewals_active.fetch_max(active, Ordering::SeqCst);
        self.renewal_sessions
            .lock()
            .unwrap()
            .push(command.session_id.to_string());
        let gate = self.renewal_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire_owned()
                .await
                .expect("renewal gate remains open")
                .forget();
        }
        if let Some(failure) = self.renewal_failure.lock().unwrap().clone() {
            return Err(failure);
        }
        let delay = self
            .renewal_delay_ms
            .lock()
            .unwrap()
            .get(command.session_id.as_str())
            .copied();
        if let Some(delay) = delay {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let response = {
            let mut renewals = self.renewals.lock().unwrap();
            renewals.get_mut(command.session_id.as_str()).map(|current| {
                if !awaken_session_realization_contract::realization_lease_generation_authorizes(
                    current, &command.asserted_lease,
                ) || command.requested_expires_at_unix_ms < current.expires_at_unix_ms {
                    Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
                } else {
                    current.expires_at_unix_ms = command.requested_expires_at_unix_ms;
                    Ok(crate::session_slot::test_lease_receipt(current.clone()))
                }
            })
        };
        if let Some(response) = response {
            // Gate the already accepted immutable response, not the request:
            // later Control mutations cannot rewrite this response in flight.
            let gate = self.renewal_response_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                self.renewal_response_ready.notify_one();
                gate.acquire_owned().await.unwrap().forget();
            }
            return response;
        }

        let (projection, current) = self.authority.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &current,
            &command.asserted_lease,
        ) || command.requested_expires_at_unix_ms < current.expires_at_unix_ms
        {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let mut renewed = current;
        renewed.expires_at_unix_ms = command.requested_expires_at_unix_ms;
        let mut session = self.session.lock().unwrap();
        let session = session
            .as_mut()
            .filter(|session| session.session_id == command.session_id)
            .ok_or(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            )?;
        if !session.realization.as_ref().is_some_and(|lease| {
            awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease,
                &command.asserted_lease,
            )
        }) {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        session.realization = Some(renewed.clone());
        *self.authority.lock().unwrap() = Some((projection, renewed.clone()));
        Ok(crate::session_slot::test_lease_receipt(renewed))
    }

    async fn activate_session_realization(
        &self,
        _command: awaken_session_realization_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn acknowledge_session_realization(
        &self,
        _command: awaken_session_realization_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_realization_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalCleanupQueue
    for RemoteTerminalCleanupControl
{
    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_realization_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionTerminalCleanupAssignment>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        let call = {
            let mut targets = self.claim_targets.lock().unwrap();
            targets.push(target);
            targets.len()
        };
        if *self.claim_failure_on_call.lock().unwrap() == Some(call) {
            return Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    "injected terminal claim failure".into(),
                ),
            );
        }
        let assignment = self.assignments.lock().unwrap().pop_front();
        if let Some(assignment) = &assignment {
            *self.authority.lock().unwrap() = Some((
                assignment.projection.clone(),
                assignment.authority.lease.clone(),
            ));
        }
        Ok(assignment)
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionTerminalCleanupWork>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.events.lock().unwrap().push("cleanup:poll".into());
        self.terminal_work_seen
            .lock()
            .unwrap()
            .push(session_id.to_string());
        let active = self.terminal_work_active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveControlCallGuard(&self.terminal_work_active);
        self.max_terminal_work_active
            .fetch_max(active, Ordering::SeqCst);
        let gate = self.terminal_work_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire_owned()
                .await
                .expect("terminal work gate remains open")
                .forget();
        }
        let blocked =
            self.terminal_work_block_for.lock().unwrap().as_deref() == Some(session_id.as_str());
        if blocked {
            self.terminal_work_started.notify_one();
            self.terminal_work_release.notified().await;
        }
        if self.terminal_work_fail_for.lock().unwrap().as_deref() == Some(session_id.as_str()) {
            return Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    "injected terminal work failure".into(),
                ),
            );
        }
        if self
            .renewals
            .lock()
            .unwrap()
            .contains_key(session_id.as_str())
        {
            return Ok(None);
        }
        let Some(session) = self.session.lock().unwrap().clone() else {
            return Ok(None);
        };
        let (projection, current) = self.authority.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        if session.session_id != session_id
            || session.realization.as_ref() != Some(&current)
            || !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &current, lease,
            )
        {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let action = session.terminal_cleanup_work_action().map_err(|error| {
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                error.to_string(),
            )
        })?;
        let Some(action) = action else {
            return Ok(None);
        };
        Ok(Some(
            awaken_session_realization_contract::SessionTerminalCleanupWork {
                assignment: awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                    session_id: session_id.clone(),
                    projection,
                    authority: crate::session_slot::test_lease_receipt(current),
                },
                action,
            },
        ))
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalPreparationControl
    for RemoteTerminalCleanupControl
{
    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        let authority = self.authority.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &authority.1,
            &effect.lease,
        ) {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let inherited_provider_disposal = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)?
            .authorize_terminal_cleanup_effect(effect)
            .map_err(|error| {
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        let authorization =
            awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                authority.0.workspace_id,
                inherited_provider_disposal,
            )?;
        let barrier = self.authorization_barrier.lock().unwrap().clone();
        if let Some((started, proceed)) = barrier {
            started.notify_one();
            proceed.notified().await;
        }
        Ok(authorization)
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
        preparation: awaken_session_state_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        let (projection, current) = self.authority.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &current, lease,
        ) {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let thread_id = preparation.effect.command.thread_id.clone();
        let mut session = self.session.lock().unwrap();
        let session = session.as_mut().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        let mut candidate = session.clone();
        let admitted = match candidate.record_terminal_cleanup_preparation(
            &projection.workspace_id,
            lease,
            preparation.clone(),
            None,
        ) {
            Ok(_) => Ok(candidate),
            Err(
                awaken_session_state_contract::SessionCleanupError::RepositoryPreparationReceiptMismatch,
            ) => {
                let repository_preparation =
                    awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
                        &preparation.effect.command.session_id,
                        &projection.workspace_id,
                        &session.resources,
                    )
                    .map_err(|error| {
                        awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                            error.to_string(),
                        )
                    })?;
                let mut candidate = session.clone();
                candidate
                    .record_terminal_cleanup_preparation(
                        &projection.workspace_id,
                        lease,
                        preparation.clone(),
                        Some(repository_preparation),
                    )
                    .map(|_| candidate)
            }
            Err(error) => Err(error),
        }
        .map_err(|error| {
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(error.to_string())
        })?;
        *session = admitted;
        self.events
            .lock()
            .unwrap()
            .push(format!("cleanup:prepared:{thread_id}"));
        self.preparations.lock().unwrap().push(preparation);
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalDisposalControl
    for RemoteTerminalCleanupControl
{
    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_realization_contract::SessionRealizationControlFailure> {
        let authority = self.authority.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        let exact = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|session| {
                session
                    .authorize_terminal_cleanup_disposal_effect(&authority.0.workspace_id, effect)
                    .is_ok()
            });
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &authority.1,
            &effect.lease,
        ) || !exact
        {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        Ok(authority.0.workspace_id)
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
        receipt: awaken_session_state_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        let current = self
            .authority
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, lease)| lease.clone())
            .ok_or(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            )?;
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &current, lease,
        ) {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let workspace_id = self
            .authority
            .lock()
            .unwrap()
            .as_ref()
            .map(|(projection, _)| projection.workspace_id.clone())
            .ok_or(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            )?;
        self.session
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)?
            .record_terminal_cleanup_disposal(
                &workspace_id,
                lease,
                receipt.clone(),
                "remote-terminal-test",
            )
            .map_err(|error| {
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        self.events.lock().unwrap().push("cleanup:disposed".into());
        self.disposals.lock().unwrap().push(receipt);
        if self
            .disposal_response_loss_once
            .swap(false, Ordering::SeqCst)
        {
            // Model an aggregate disposal CAS whose transport response is lost.
            // The next canonical poll returns Completed (`None`); no Worker-local
            // receipt cache is involved.
            return Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    "injected disposal response loss".into(),
                ),
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl
    for RemoteTerminalCleanupControl
{
    async fn terminal_repository_publication_command(
        &self,
        session_id: &SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.events.lock().unwrap().push("publication:poll".into());
        let (projection, current) = self.authority.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        let session = self.session.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        if session.session_id != session_id
            || session.realization.as_ref() != Some(&current)
            || !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &current, lease,
            )
        {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        session
            .terminal_cleanup
            .publication_command(session_id)
            .map(|command| {
                command.map(|command| {
                    awaken_session_realization_contract::SessionRepositoryPublicationProjection {
                        workspace_id: projection.workspace_id,
                        command,
                        current_lease: current,
                    }
                })
            })
            .map_err(|error| {
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
        receipt: awaken_session_state_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        let current = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|session| session.realization.clone())
            .ok_or(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            )?;
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &current, lease,
        ) {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        self.session
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(awaken_session_realization_contract::SessionRealizationControlFailure::NotReady)?
            .terminal_cleanup
            .record_repository_publication_receipt(session_id, receipt.clone())
            .map_err(|error| {
                awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        self.events
            .lock()
            .unwrap()
            .push("publication:receipt".into());
        self.publication_receipts.lock().unwrap().push(receipt);
        Ok(())
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
        rejection: awaken_session_state_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        let current = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|session| session.realization.clone())
            .ok_or(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            )?;
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &current, lease,
        ) {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        {
            let mut root = self.session.lock().unwrap();
            let session = root.as_mut().ok_or(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            )?;
            if session.session_id != *session_id {
                return Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::NotFound,
                );
            }
            session
                .record_repository_publication_rejection(rejection.clone())
                .map_err(|error| {
                    awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                        error.to_string(),
                    )
                })?;
        }
        self.events
            .lock()
            .unwrap()
            .push("publication:rejection".into());
        self.publication_rejections.lock().unwrap().push(rejection);
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_contract::ClaimedSessionRealizationControl
    for RemoteTerminalCleanupControl
{
    async fn resume_frozen(
        &self,
        _claim: &awaken_dispatch::RunClaim,
        _session_id: &str,
        _session_work_authority: Option<&awaken_session_work_contract::SessionWorkAuthorityReceipt>,
        _continuation: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRealizationDirective>,
        awaken_dispatch_contract::ClaimedSessionControlError,
    > {
        Ok(None)
    }
}

impl SharedHost {
    /// The Workdir provider rooted where this Host's Session provider realizes
    /// sandboxes, carrying the installed Memory mounter: the fixture a test
    /// uses to pre-create a physical Session sandbox.
    pub(crate) fn test_local_provider(&self) -> awaken_sandbox_local::LocalProvider {
        let root = match &self.store_dir {
            Some(dir) => dir.join("sandboxes"),
            None => crate::acp_backend::session_sandbox_base(&self.deployment),
        };
        let provider = awaken_sandbox_local::LocalProvider::new(root);
        if let Some(mounter) = self.memory_mounter() {
            provider.install_memory_mounter(mounter);
        }
        provider
    }
}
