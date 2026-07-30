//! Application-owned, claim-bound additions to one Session environment.
//!
//! The host remains the sole owner of Session realization. An embedding
//! application may prepare mounts, environment values, prompt context, and MCP
//! servers after a dispatch is claimed, but the result is staged into the same
//! Session slot and realized by the same Native/ACP backend path.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::RunState;
use awaken_run_ingress::{RunClaim, WorkerIdentity};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{ExecutorCapabilities, RunAttemptExecutor};
use awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier;

/// Frozen application additions for one claimed Session.
///
/// `fingerprint` is the application's stable identity for the complete plan.
/// Re-delivery of the same plan is idempotent; a different plan cannot mutate an
/// already-bound Session and fails closed.
#[derive(Clone)]
pub struct ApplicationSessionPlan {
    pub fingerprint: String,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    pub mcp_inputs: Vec<serde_json::Value>,
    pub network_restriction: Option<awaken_protocol_managed::SessionNetworkPolicy>,
}

impl ApplicationSessionPlan {
    #[must_use]
    pub fn empty(fingerprint: impl Into<String>) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            mcp_inputs: Vec::new(),
            network_restriction: None,
        }
    }

    /// Author one URL-based MCP input without requiring the embedding Worker
    /// application to depend on Managed wire or JSON crates. This is only a
    /// boundary value constructor: the Managed application compiler remains the
    /// sole owner of URL canonicalization, precedence, credential selection, and
    /// attachment generation allocation.
    #[must_use]
    pub fn with_mcp_url(mut self, name: impl Into<String>, url: impl Into<String>) -> Self {
        self.mcp_inputs.push(serde_json::json!({
            "name": name.into(),
            "type": "url",
            "url": url.into(),
        }));
        self
    }

    /// Author one URL-based MCP input pinned to an exact credential revision.
    /// The contribution remains secret-free: Control resolves the reference
    /// through the existing Session MCP credential path and freezes the selected
    /// plaintext holder before Runtime realization.
    #[must_use]
    pub fn with_mcp_url_credential(
        mut self,
        name: impl Into<String>,
        url: impl Into<String>,
        credential: awaken_runtime_contract::CredentialRef,
    ) -> Self {
        self.mcp_inputs.push(serde_json::json!({
            "name": name.into(),
            "type": "url",
            "url": url.into(),
            "credential_source_id": credential.id,
            "credential_revision": credential.revision,
        }));
        self
    }

    /// Restrict the Session network policy using the existing neutral
    /// provisioning vocabulary. The Managed anti-corruption boundary converts
    /// it into a frozen Session fact and remains the sole owner of safe
    /// intersection with the Control-authored Environment policy.
    #[must_use]
    pub fn with_network_restriction(
        mut self,
        restriction: awaken_provisioning_contract::NetworkPolicy,
    ) -> Self {
        self.network_restriction = Some(match restriction {
            awaken_provisioning_contract::NetworkPolicy::Unrestricted => {
                awaken_protocol_managed::SessionNetworkPolicy::Unrestricted
            }
            awaken_provisioning_contract::NetworkPolicy::Allowlist { hosts } => {
                awaken_protocol_managed::SessionNetworkPolicy::Allowlist { hosts }
            }
            awaken_provisioning_contract::NetworkPolicy::None => {
                awaken_protocol_managed::SessionNetworkPolicy::None
            }
        });
        self
    }

    fn into_contribution(
        self,
        session_id: String,
    ) -> Result<awaken_protocol_managed::ApplicationSessionContribution, ApplicationSessionError>
    {
        let mounts = self
            .mounts
            .into_iter()
            .map(|mount| serde_json::to_value(mount).map_err(ApplicationSessionError::from_error))
            .collect::<Result<Vec<_>, _>>()?;
        let env = self
            .env
            .into_iter()
            .map(|value| serde_json::to_value(value).map_err(ApplicationSessionError::from_error))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(awaken_protocol_managed::ApplicationSessionContribution {
            session_id,
            application_fingerprint: self.fingerprint,
            input: awaken_protocol_managed::ApplicationSessionInput {
                mounts,
                env,
                prompts: self.prompts,
                mcp_inputs: self.mcp_inputs,
                network_restriction: self.network_restriction,
            },
        })
    }
}

/// Failure while an application projects a claimed Run into Session additions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationSessionError(String);

impl ApplicationSessionError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn from_error(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

impl std::fmt::Display for ApplicationSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ApplicationSessionError {}

/// Claim-time application projection port.
///
/// The ownership verifier is neutral runtime wiring. Implementations should
/// check it around credential materialization or any other external operation;
/// the host also checks before and after the complete projection.
#[async_trait::async_trait]
pub trait ApplicationSessionProvisioner: Send + Sync {
    async fn prepare(
        &self,
        activation: &RunActivation,
        ownership: Arc<dyn AttemptOwnershipVerifier>,
    ) -> Result<ApplicationSessionPlan, ApplicationSessionError>;
}

#[cfg(test)]
mod application_plan_tests {
    use super::*;

    #[test]
    fn exact_application_mcp_credential_is_secret_free() {
        let plan = ApplicationSessionPlan::empty("flow-plan").with_mcp_url_credential(
            "flow",
            "http://flow.invalid/mcp",
            awaken_runtime_contract::CredentialRef {
                id: "run-credential".into(),
                revision: 3,
            },
        );
        assert_eq!(
            plan.mcp_inputs,
            [serde_json::json!({
                "name": "flow",
                "type": "url",
                "url": "http://flow.invalid/mcp",
                "credential_source_id": "run-credential",
                "credential_revision": 3,
            })]
        );
        assert!(!plan.mcp_inputs[0].to_string().contains("Bearer"));
    }
}

/// Result of the claim-fenced contribution and initial realization assignment.
#[derive(Clone)]
pub struct ApplicationSessionControlReceipt {
    pub contribution: awaken_protocol_managed::ApplicationSessionContributionReceipt,
    pub realization: awaken_protocol_managed::SessionRealizationDirective,
}

/// Worker-side outbound port to the authenticated Coordinator-owned Session
/// application service. Contribution and realization phases cannot be wired to
/// different authorities.
#[async_trait::async_trait]
pub trait ApplicationSessionControlClient:
    awaken_protocol_managed::SessionRealizationControl + Send + Sync
{
    async fn contribute(
        &self,
        session_id: &str,
        claim: &RunClaim,
        plan: ApplicationSessionPlan,
    ) -> Result<ApplicationSessionControlReceipt, ApplicationSessionError>;
}

/// Standard client using the same registered identity-bound Worker transport as
/// lifecycle, dispatch, recovery, and claimed commits.
pub struct WorkerControlApplicationSessionClient {
    control: crate::WorkerControlClient,
    identity: WorkerIdentity,
}

impl WorkerControlApplicationSessionClient {
    #[must_use]
    pub fn new(control: crate::WorkerControlClient, identity: WorkerIdentity) -> Self {
        Self { control, identity }
    }
}

#[async_trait::async_trait]
impl ApplicationSessionControlClient for WorkerControlApplicationSessionClient {
    async fn contribute(
        &self,
        session_id: &str,
        claim: &RunClaim,
        plan: ApplicationSessionPlan,
    ) -> Result<ApplicationSessionControlReceipt, ApplicationSessionError> {
        let contribution = plan.into_contribution(session_id.to_string())?;
        self.control
            .contribute_application(&self.identity, claim, contribution)
            .await
            .map_err(ApplicationSessionError::new)
    }
}

#[async_trait::async_trait]
impl awaken_protocol_managed::SessionRealizationControl for WorkerControlApplicationSessionClient {
    async fn begin_session_realization(
        &self,
        command: awaken_protocol_managed::BeginSessionRealization,
    ) -> Result<
        awaken_protocol_managed::SessionRealizationDirective,
        awaken_protocol_managed::SessionRealizationControlFailure,
    > {
        self.control
            .begin_session_realization(&self.identity, command)
            .await
            .map_err(awaken_protocol_managed::SessionRealizationControlFailure::Unavailable)
    }

    async fn activate_session_realization(
        &self,
        command: awaken_protocol_managed::ActivateSessionRealization,
    ) -> Result<
        awaken_protocol_managed::SessionRealizationDirective,
        awaken_protocol_managed::SessionRealizationControlFailure,
    > {
        self.control
            .activate_session_realization(&self.identity, command)
            .await
            .map_err(awaken_protocol_managed::SessionRealizationControlFailure::Unavailable)
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_protocol_managed::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_protocol_managed::SessionRealizationDirective,
        awaken_protocol_managed::SessionRealizationControlFailure,
    > {
        self.control
            .acknowledge_session_realization(&self.identity, command)
            .await
            .map_err(awaken_protocol_managed::SessionRealizationControlFailure::Unavailable)
    }

    async fn fail_session_realization(
        &self,
        command: awaken_protocol_managed::FailSessionRealization,
    ) -> Result<(), awaken_protocol_managed::SessionRealizationControlFailure> {
        self.control
            .fail_session_realization(&self.identity, command)
            .await
            .map_err(awaken_protocol_managed::SessionRealizationControlFailure::Unavailable)
    }
}

/// The single Session-baseline prompt projection boundary for foreground,
/// durable, Native, ACP, and A2A attempts.
///
/// A contribution may freeze after a durable dispatch was authored, so mutating
/// only Host `pending_system` state cannot affect that already-serialized
/// activation. Wrapping the authoritative attempt router keeps one mechanism for
/// every topology. Deterministic message ids make a retried uncommitted attempt
/// byte-for-byte stable; committed history prevents later turns from reinjecting
/// the baseline.
pub(crate) struct SessionPromptAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    prompts: Vec<String>,
}

impl SessionPromptAttemptExecutor {
    pub(crate) fn new(inner: Arc<dyn RunAttemptExecutor>, prompts: Vec<String>) -> Self {
        Self { inner, prompts }
    }

    fn project(
        &self,
        mut activation: RunActivation,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> RunActivation {
        let first_turn = context
            .reader
            .as_ref()
            .is_none_or(|reader| reader.committed_messages(&activation.thread_id).is_empty());
        if !first_turn || self.prompts.is_empty() {
            return activation;
        }
        let already_present = |prompt: &str| {
            activation.input.iter().any(|message| {
                message.role == Role::System
                    && message.content.iter().any(|content| {
                        matches!(
                            content,
                            awaken_agent_contract::agent::content::ContentBlock::Text { text }
                                if text == prompt
                        )
                    })
            })
        };
        let mut projected = self
            .prompts
            .iter()
            .enumerate()
            .filter(|(_, prompt)| !already_present(prompt))
            .map(|(index, prompt)| {
                Message::text(
                    MessageId(format!(
                        "session-baseline:{}:{index}",
                        activation.thread_id.0
                    )),
                    Role::System,
                    prompt.clone(),
                )
            })
            .collect::<Vec<_>>();
        projected.append(&mut activation.input);
        activation.input = projected;
        activation
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for SessionPromptAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.inner
            .execute(self.project(activation, &context), context)
            .await
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        self.inner.capabilities()
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for SessionPromptAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: awaken_runtime_contract::resume::ResumeCommand,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.inner
            .resume(self.project(activation, &context), command, context)
            .await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        self.inner.cancel(activation, context).await
    }
}

/// Loads selected Skills and bounded Memory recall into ACP's request-only
/// context. Native execution keeps using its existing Skill tools and
/// `BeforeInference` Memory hook; this adapter only bridges the external backend
/// through the neutral `RuntimeRunContext` field.
pub(crate) struct AcpContextAttemptExecutor {
    inner: Arc<dyn RunAttemptExecutor>,
    skills: Option<Arc<dyn awaken_ext_skills::SkillRegistry>>,
    memory: Option<awaken_ext_memory::MemoryRecall>,
    session_id: String,
}

impl AcpContextAttemptExecutor {
    pub(crate) fn new(
        inner: Arc<dyn RunAttemptExecutor>,
        skills: Option<Arc<dyn awaken_ext_skills::SkillRegistry>>,
        memory: Option<awaken_ext_memory::MemoryRecall>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            skills,
            memory,
            session_id: session_id.into(),
        }
    }

    async fn load_context(
        &self,
        activation: &RunActivation,
        context: &mut awaken_runtime_contract::RuntimeRunContext,
    ) {
        if !activation
            .snapshot
            .resolved_spec
            .model_binding
            .backend_ref
            .starts_with("acp:")
        {
            return;
        }
        if let Some(skills) = &self.skills {
            let loaded = skills
                .list()
                .into_iter()
                .filter(|skill| skill.model_invocable)
                .map(|skill| {
                    awaken_ext_skills::render_backend_context(&skill, Some(&self.session_id))
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            if !loaded.is_empty() {
                context.request_context.push(Message::text(
                    MessageId(format!("acp-skills:{}", activation.run_id.0)),
                    Role::System,
                    loaded,
                ));
            }
        }
        if let Some(memory) = &self.memory
            && let Some(recalled) = memory.context(&activation.input).await
        {
            context.request_context.push(Message::text(
                MessageId(format!("acp-memory:{}", activation.run_id.0)),
                Role::System,
                recalled,
            ));
        }
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::execution::RunExecutor for AcpContextAttemptExecutor {
    async fn execute(
        &self,
        activation: RunActivation,
        mut context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.load_context(&activation, &mut context).await;
        self.inner.execute(activation, context).await
    }

    fn capabilities(&self) -> ExecutorCapabilities {
        self.inner.capabilities()
    }
}

#[async_trait::async_trait]
impl RunAttemptExecutor for AcpContextAttemptExecutor {
    async fn resume(
        &self,
        activation: RunActivation,
        command: awaken_runtime_contract::resume::ResumeCommand,
        mut context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<RunState> {
        self.load_context(&activation, &mut context).await;
        self.inner.resume(activation, command, context).await
    }

    async fn cancel(
        &self,
        activation: RunActivation,
        context: awaken_runtime_contract::RuntimeRunContext,
    ) -> awaken_runtime_contract::execution::Result<()> {
        self.inner.cancel(activation, context).await
    }
}

#[cfg(test)]
mod acp_context_tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::execution::{Error, RunExecutor};
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    struct UnusedExecutor;

    #[async_trait::async_trait]
    impl RunExecutor for UnusedExecutor {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<RunState, Error> {
            panic!("load_context test never executes the inner adapter")
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for UnusedExecutor {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: awaken_runtime_contract::resume::ResumeCommand,
            _context: awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<RunState, Error> {
            panic!("load_context test never resumes the inner adapter")
        }
    }

    fn activation(backend: &str) -> RunActivation {
        RunActivation::new(
            RunId("run-1".into()),
            ThreadId("session-1".into()),
            ExecutableAgentSnapshot::builder("agent")
                .model(ModelBinding::new("provider", "model", backend))
                .build(),
            vec![Message::text(
                MessageId("user-1".into()),
                Role::User,
                "current request",
            )],
        )
    }

    /// Cause/effect graph:
    /// C1 ACP backend, C2 selected model-invocable Skill, C3 non-empty Memory
    /// -> E1 one Skill context and E2 one bounded Memory context; a Native
    /// backend (C1=false) -> E3 no adapter context because its existing Skill
    /// tools and BeforeInference hook remain authoritative.
    ///
    /// | Rule | ACP | Skill | Memory | Context messages |
    /// | A1 | T | T | T | skill + memory |
    /// | A2 | F | T | T | empty |
    #[tokio::test]
    async fn acp_loads_selected_skills_and_memory_as_request_only_context() {
        let skill = awaken_ext_skills::SkillSpec::new(
            "review",
            "Review",
            "Review carefully",
            "Use ${SESSION_ID} and inspect the evidence.",
        );
        let skills: Arc<dyn awaken_ext_skills::SkillRegistry> =
            Arc::new(awaken_ext_skills::InMemorySkillRegistry::from_specs([
                skill,
            ]));
        let root = std::env::temp_dir().join(format!(
            "awaken-acp-memory-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let memory = awaken_ext_memory::MemoryDir::new(root);
        memory
            .write("preference", "Prefer concise answers.")
            .expect("seed memory");
        let loader = AcpContextAttemptExecutor::new(
            Arc::new(UnusedExecutor),
            Some(skills),
            Some(awaken_ext_memory::MemoryRecall::new(
                Arc::new(memory),
                awaken_ext_memory::RecallBounds::default(),
            )),
            "session-1",
        );

        let acp = activation("acp:codex");
        let durable_input = acp.input.clone();
        let mut acp_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&acp, &mut acp_context).await;
        assert_eq!(acp_context.request_context.len(), 2, "A1");
        assert!(
            acp_context.request_context[0]
                .text_content()
                .contains("Use session-1"),
            "A1 skill template"
        );
        assert!(
            acp_context.request_context[1]
                .text_content()
                .contains("Prefer concise answers"),
            "A1 memory"
        );
        assert_eq!(acp.input, durable_input, "request context is non-durable");

        let native = activation("genai");
        let mut native_context = awaken_runtime_contract::RuntimeRunContext::default();
        loader.load_context(&native, &mut native_context).await;
        assert!(native_context.request_context.is_empty(), "A2");
    }
}

impl crate::SharedHost {
    pub(crate) fn install_session_realization_lease(
        &self,
        session_id: &str,
        lease: awaken_protocol_managed::SessionRealizationLease,
    ) {
        self.session_slots
            .update(session_id, |slot| slot.realization_lease = Some(lease));
    }

    /// Renew every active MCP projection approaching expiry through the same
    /// Control phase protocol used for initial creation and hot replacement.
    /// One failure aborts the batch so a Worker heartbeat cannot claim healthy
    /// custody while any owned route lost its authority.
    pub async fn renew_due_session_realizations(
        &self,
        renew_before_unix_ms: u64,
        requested_expiry_unix_ms: u64,
    ) -> Result<usize, crate::HostError> {
        let due = self
            .session_slots
            .realization_leases()
            .into_iter()
            .filter(|(_, lease)| lease.expires_at_unix_ms <= renew_before_unix_ms)
            .collect::<Vec<_>>();
        if due.is_empty() {
            return Ok(0);
        }
        let control = self.application_session_control.as_ref().ok_or_else(|| {
            crate::HostError::internal(
                "active application Session projection has no Control renewal client",
            )
        })?;
        for (session_id, lease) in &due {
            let directive = control
                .begin_session_realization(awaken_protocol_managed::BeginSessionRealization {
                    session_id: session_id.clone(),
                    target: awaken_protocol_managed::SessionRealizationTarget {
                        owner: lease.owner.clone(),
                        runtime_incarnation: lease.runtime_incarnation.clone(),
                        lease_expires_at_unix_ms: requested_expiry_unix_ms,
                        renew_existing_lease: true,
                    },
                })
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
            crate::host::HostWorkerResolver::realize_application_session(
                self, control, session_id, directive,
            )
            .await
            .map_err(|error| crate::HostError::internal(error.to_string()))?;
        }
        Ok(due.len())
    }

    /// Revoke every process-local Session projection after Worker authority is
    /// no longer provable. Visibility is removed and environments/routes are
    /// disposed through the same terminal Host path; no credential-bearing
    /// projection remains available while Control ownership is unknown.
    pub async fn revoke_all_session_realizations(&self) -> Result<usize, crate::HostError> {
        let session_ids = self.session_slots.session_ids();
        let mut revoked = 0;
        let mut first_error = None;
        for session_id in session_ids {
            match self.end_session(&session_id).await {
                Ok(()) => revoked += 1,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(revoked),
        }
    }

    pub(crate) async fn install_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_protocol_managed::FrozenSessionProjection,
    ) -> Result<(), crate::HostError> {
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::HostError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        if projection.baseline.application.is_none() {
            return Err(crate::HostError::internal(
                "application contribution returned a baseline without its durable receipt",
            ));
        }
        let has_mcp_projection = projection.mcp.iter().any(|attachment| {
            !matches!(
                attachment.state,
                awaken_protocol_managed::McpAttachmentState::Removed
                    | awaken_protocol_managed::McpAttachmentState::Failed
            )
        });
        let baseline = decode_baseline_projection(&projection.baseline)?;

        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten()
        {
            if existing.fingerprint != baseline.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Session baseline"
                )));
            }
            self.session_slots.update(thread, |slot| {
                slot.has_mcp_projection = has_mcp_projection;
            });
            return Ok(());
        }

        let occupied = self.session_slots.read(thread, |slot| {
            (
                slot.runtime.is_some() || slot.environment.is_some(),
                slot.resources.mounts.clone(),
            )
        });
        let (is_realized, built_in_mounts) = occupied.unwrap_or_else(|| (false, Vec::new()));
        if is_realized {
            return Err(crate::HostError::internal(format!(
                "thread {thread} was realized before its frozen Session baseline"
            )));
        }

        validate_baseline_projection(&baseline, &built_in_mounts)?;
        if projection.resources != awaken_protocol_managed::ResolvedSessionResources::default() {
            let manifest = awaken_protocol_managed::SessionResourceManifest::new(
                projection.workspace_id.clone(),
                projection.resources,
            );
            self.install_dispatched_resources(thread, &manifest)
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
        }
        self.session_slots.update(thread, |slot| {
            slot.baseline = Some(baseline);
            slot.has_mcp_projection = has_mcp_projection;
        });
        self.install_environment_projection(thread, &projection.baseline.environment)?;
        self.register_thread_workspace(thread, &projection.workspace_id);
        self.register_thread_model(thread, &projection.baseline.execution_model_ref);
        if let Some(backend_ref) = &projection.baseline.runtime {
            self.register_thread_backend_projection(thread, backend_ref);
        }
        self.register_thread_delegates(thread, projection.baseline.delegate_ids);
        Ok(())
    }

    pub(crate) fn install_environment_projection(
        &self,
        thread: &str,
        environment: &awaken_protocol_managed::EnvironmentSnapshot,
    ) -> Result<(), crate::HostError> {
        let projection = decode_environment_projection(environment);
        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.environment_projection.clone())
            .flatten()
        {
            return if existing.fingerprint == projection.fingerprint {
                Ok(())
            } else {
                Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Environment"
                )))
            };
        }
        self.session_slots.update(thread, |slot| {
            slot.environment_projection = Some(projection)
        });
        Ok(())
    }

    pub(crate) fn thread_session_mounts(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::MountRequirement> {
        self.session_slots
            .read(thread, |slot| {
                let mut mounts = slot.resources.mounts.clone();
                if let Some(baseline) = &slot.baseline {
                    mounts.extend(baseline.mounts.clone());
                }
                mounts
            })
            .unwrap_or_default()
    }

    pub(crate) fn thread_session_env(
        &self,
        thread: &str,
    ) -> Vec<awaken_provisioning_contract::EnvVar> {
        self.session_slots
            .read(thread, |slot| {
                slot.baseline
                    .as_ref()
                    .map(|baseline| baseline.env.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    pub(crate) fn thread_session_prompts(&self, thread: &str) -> Vec<String> {
        self.session_slots
            .read(thread, |slot| {
                let mut prompts = slot.resources.prompts.clone();
                if let Some(baseline) = &slot.baseline {
                    prompts.extend(baseline.prompts.clone());
                }
                prompts
            })
            .unwrap_or_default()
    }
}

fn decode_baseline_projection(
    baseline: &awaken_protocol_managed::SessionBaseline,
) -> Result<crate::session_slot::FrozenBaselineRuntimeProjection, crate::HostError> {
    let mounts = baseline
        .mounts
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                crate::HostError::internal(format!(
                    "frozen Session baseline has an invalid mount: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let env = baseline
        .env
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| {
                crate::HostError::internal(format!(
                    "frozen Session baseline has an invalid environment value: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(crate::session_slot::FrozenBaselineRuntimeProjection {
        fingerprint: baseline.fingerprint.clone(),
        mounts,
        env,
        prompts: baseline.prompts.clone(),
    })
}

fn decode_environment_projection(
    environment: &awaken_protocol_managed::EnvironmentSnapshot,
) -> crate::session_slot::FrozenEnvironmentRuntimeProjection {
    let network = match &environment.network {
        awaken_protocol_managed::SessionNetworkPolicy::Unrestricted => {
            awaken_provisioning_contract::NetworkPolicy::Unrestricted
        }
        awaken_protocol_managed::SessionNetworkPolicy::Allowlist { hosts } => {
            awaken_provisioning_contract::NetworkPolicy::Allowlist {
                hosts: hosts.clone(),
            }
        }
        awaken_protocol_managed::SessionNetworkPolicy::None => {
            awaken_provisioning_contract::NetworkPolicy::None
        }
    };
    let package_config = &environment.packages;
    let packages = awaken_provisioning_contract::PackageRequirements {
        managers: [
            ("apt", &package_config.apt),
            ("cargo", &package_config.cargo),
            ("gem", &package_config.gem),
            ("go", &package_config.go),
            ("npm", &package_config.npm),
            ("pip", &package_config.pip),
        ]
        .into_iter()
        .filter(|(_, packages)| !packages.is_empty())
        .map(|(manager, packages)| (manager.to_string(), packages.clone()))
        .collect(),
    };
    let sandbox =
        awaken_provisioning_contract::SandboxOverride::from_config_value(&environment.sandbox)
            .and_then(|mut sandbox| {
                // EnvironmentSnapshot.network is the sole reachability authority. Old
                // retained blobs may still contain the pre-normalization sandbox.network
                // field; ignoring it is fail-stable and prevents a late widening override.
                sandbox.network = None;
                (!sandbox.is_empty()).then_some(sandbox)
            });
    crate::session_slot::FrozenEnvironmentRuntimeProjection {
        fingerprint: environment.config_fingerprint.clone(),
        network,
        packages,
        sandbox,
        provisioning: environment.sandbox_provisioning,
        credential_realization: environment.credential_realization.clone(),
    }
}

fn validate_baseline_projection(
    baseline: &crate::session_slot::FrozenBaselineRuntimeProjection,
    built_in_mounts: &[awaken_provisioning_contract::MountRequirement],
) -> Result<(), crate::HostError> {
    let mut mount_ids: HashSet<&str> = built_in_mounts
        .iter()
        .map(|mount| mount.mount_id.as_str())
        .collect();
    let mut mount_paths: HashSet<&str> = built_in_mounts
        .iter()
        .map(|mount| mount.mount_path.as_str())
        .collect();
    for mount in &baseline.mounts {
        if mount.mount_id.trim().is_empty()
            || mount.mount_path.trim().is_empty()
            || !mount_ids.insert(&mount.mount_id)
            || !mount_paths.insert(&mount.mount_path)
        {
            return Err(crate::HostError::internal(
                "frozen Session baseline has an empty or conflicting mount",
            ));
        }
    }

    let mut env_names = HashSet::new();
    for env in &baseline.env {
        if env.name.trim().is_empty() || !env_names.insert(env.name.as_str()) {
            return Err(crate::HostError::internal(
                "frozen Session baseline has an empty or duplicate environment variable",
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod network_policy_tests {
    use super::*;

    /// Cause graph: each neutral provisioning policy has exactly one Session
    /// representation; no value is widened or interpreted by the Worker
    /// application. The Managed baseline compiler remains the next authority.
    ///
    /// | Rule | Provisioning input | Session contribution fact |
    /// | N1 | Unrestricted | Unrestricted |
    /// | N2 | Allowlist(a,b) | Allowlist(a,b), byte-faithful |
    /// | N3 | None | None |
    #[test]
    fn application_network_policy_mapping_is_total_and_lossless() {
        let cases = [
            (
                awaken_provisioning_contract::NetworkPolicy::Unrestricted,
                awaken_protocol_managed::SessionNetworkPolicy::Unrestricted,
            ),
            (
                awaken_provisioning_contract::NetworkPolicy::Allowlist {
                    hosts: vec!["A.example".into(), "b.example".into()],
                },
                awaken_protocol_managed::SessionNetworkPolicy::Allowlist {
                    hosts: vec!["A.example".into(), "b.example".into()],
                },
            ),
            (
                awaken_provisioning_contract::NetworkPolicy::None,
                awaken_protocol_managed::SessionNetworkPolicy::None,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                ApplicationSessionPlan::empty("network-policy")
                    .with_network_restriction(input)
                    .network_restriction,
                Some(expected),
            );
        }
    }

    /// Package projection cause graph: the exact frozen Environment package
    /// vectors become one neutral manager map; empty managers disappear, values
    /// and ordering remain exact, and no protocol DTO reaches provisioning.
    #[test]
    fn environment_packages_project_losslessly_to_the_provisioning_contract() {
        let environment = awaken_protocol_managed::EnvironmentSnapshot {
            environment_id: "env_packages".into(),
            revision: awaken_protocol_managed::EnvironmentRevision(3),
            config_fingerprint: awaken_protocol_managed::EnvironmentFingerprint("fp".into()),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            packages: awaken_protocol_managed::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                pip: vec!["httpx==0.28".into()],
                ..Default::default()
            },
            network: awaken_protocol_managed::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let projected = decode_environment_projection(&environment);
        assert_eq!(
            projected.packages.managers,
            [
                ("npm".into(), vec!["tsx@4".into()]),
                ("pip".into(), vec!["httpx==0.28".into()]),
            ]
            .into_iter()
            .collect()
        );
        assert!(!projected.packages.managers.contains_key("apt"));
    }
}
