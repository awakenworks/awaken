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
use awaken_run_ingress::RunClaim;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{ExecutorCapabilities, RunAttemptExecutor};

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
            Arc::new(awaken_ext_skills::FixedSkillRegistry::from_specs([skill]));
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
        lease: awaken_session_contract::SessionRealizationLease,
    ) {
        self.session_slots
            .update(session_id, |slot| slot.realization_lease = Some(lease));
    }

    /// Renew every active MCP projection approaching expiry through the same
    /// Control phase protocol used for initial creation and hot replacement.
    /// A failed renewal revokes that Session's process-local projection before
    /// the batch continues. Session authority is narrower than Worker registry
    /// authority: an expired or terminal application Run must not fence unrelated
    /// in-flight Sessions from the same Worker incarnation.
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
        let mut renewed = 0;
        for (session_id, lease) in &due {
            let renewal = async {
                let directive = control
                    .begin_session_realization(awaken_session_contract::BeginSessionRealization {
                        session_id: session_id.clone(),
                        target: awaken_session_contract::SessionRealizationTarget {
                            owner: lease.owner.clone(),
                            runtime_incarnation: lease.runtime_incarnation.clone(),
                            lease_expires_at_unix_ms: requested_expiry_unix_ms,
                            renew_existing_lease: true,
                        },
                    })
                    .await
                    .map_err(|error| crate::HostError::internal(error.to_string()))?;
                let realization = self.session_slots.realization_lock(session_id);
                let Ok(_realization) = realization.try_lock() else {
                    // Control has extended only the same owner/incarnation/epoch.
                    // Preserve that authority locally, but never start a second
                    // effect driver. The active driver compares exact generation
                    // fences at Activate/Acknowledge and catches up before it can
                    // report completion.
                    self.install_session_realization_lease(session_id, directive.lease.clone());
                    return Ok::<bool, crate::HostError>(true);
                };
                crate::host::HostWorkerResolver::drive_application_session(
                    self,
                    control.as_ref(),
                    session_id,
                    directive,
                    None,
                    None,
                    false,
                )
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
                Ok::<bool, crate::HostError>(true)
            }
            .await;
            match renewal {
                Ok(true) => renewed += 1,
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "Session realization renewal lost authority for `{session_id}`; revoking only that Session: {error}"
                    );
                    let _ = self.interrupt(session_id).await;
                    self.revoke_session_realization(session_id).await;
                }
            }
        }
        Ok(renewed)
    }

    /// Remove one process-local realization after its continuing authority is
    /// no longer provable. This is not a terminal Session edge: preserve the
    /// durable Sandbox so the next authorized Worker can adopt it, while local
    /// processes, routes, credentials, and runtime references are discarded.
    async fn revoke_session_realization(&self, session_id: &str) -> bool {
        let Some(lifecycle) = self
            .session_slots
            .read(session_id, |slot| slot.lifecycle.clone())
        else {
            return false;
        };
        let _lifecycle = lifecycle.lock().await;
        self.stop_session_mcp_processes(session_id).await;
        let Some(slot) = self.session_slots.remove(session_id) else {
            return false;
        };
        if let Some(environment) = slot
            .environment
            .or_else(|| slot.runtime.and_then(|runtime| runtime.env.clone()))
        {
            environment.stop_bound_processes().await;
        }
        if let Some(relay) = self.mcp_relay.get() {
            relay.remove_routes(session_id);
        }
        true
    }

    /// Revoke every process-local Session projection after Worker authority is
    /// no longer provable. Durable Session environments remain available for
    /// adoption; only terminal Session lifecycle commands may dispose them.
    pub async fn revoke_all_session_realizations(&self) -> Result<usize, crate::HostError> {
        let session_ids = self.session_slots.session_ids();
        let mut revoked = 0;
        for session_id in session_ids {
            if self.revoke_session_realization(&session_id).await {
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    /// Cancel every run currently backed by a process-local Session projection.
    ///
    /// Worker authority loss must stop in-flight tool processes before local
    /// realization revocation detaches their process bindings. Otherwise a native
    /// process can continue writing after this Worker has dropped authority.
    pub async fn interrupt_all_session_runs(&self) -> usize {
        let session_ids = self.session_slots.session_ids();
        let mut interrupted = 0;
        for session_id in session_ids {
            if self.interrupt(&session_id).await.is_ok() {
                interrupted += 1;
            }
        }
        interrupted
    }

    /// Install the protocol-neutral Runtime coordinates derived from one frozen
    /// Session projection. Managed local preparation and claimed Worker replay
    /// share this single lowering path; neither may independently reconstruct a
    /// different workspace, Agent, model, backend, Environment, or tool surface.
    pub(crate) fn project_session_init(
        &self,
        thread: &str,
        init: &awaken_session_contract::SessionInit,
    ) -> Result<(), crate::HostError> {
        // Validate the only fallible coordinate before publishing the remaining
        // fields, so a conflicting Environment cannot leave a partial update.
        self.install_environment_projection(thread, &init.environment)?;
        self.register_thread_workspace(thread, &init.workspace_id);
        self.register_thread_agent_projection(thread, &init.agent_id);
        if let Some(model) = &init.model {
            self.register_thread_model(thread, model);
        }
        if let Some(backend_ref) = &init.runtime {
            self.register_thread_backend_projection(thread, backend_ref);
        }
        self.register_thread_delegates(thread, init.delegate_ids.clone());
        self.session_slots.update(thread, |slot| {
            slot.agent_id = Some(init.agent_id.clone());
            slot.toolsets = init.toolsets.clone();
        });
        Ok(())
    }

    pub(crate) async fn install_frozen_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        claim: Option<&RunClaim>,
    ) -> Result<(), crate::HostError> {
        if projection.baseline.fingerprint.0.trim().is_empty() {
            return Err(crate::HostError::internal(
                "frozen Session baseline fingerprint must not be empty",
            ));
        }
        let has_mcp_projection = projection.mcp.iter().any(|attachment| {
            !matches!(
                attachment.state,
                awaken_session_contract::McpAttachmentState::Removed
                    | awaken_session_contract::McpAttachmentState::Failed
            )
        });
        let baseline = decode_baseline_projection(&projection.baseline)?;
        let init = projection.session_init();

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
            // The baseline is immutable, but a remote Resource verification is
            // authorized by the current dispatch claim. Re-stage the exact
            // manifest on every claimed replay so Repository checks never retain
            // a prior lease epoch. `install_dispatched_resources` owns generation
            // equality/fencing and does not realize an existing environment.
            if projection.resources != awaken_session_contract::ResolvedSessionResources::default()
            {
                let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
                    projection.workspace_id.clone(),
                    projection.resource_revision,
                    projection.resources.clone(),
                );
                self.install_dispatched_resources(thread, &manifest, claim)
                    .await
                    .map_err(|error| crate::HostError::internal(error.to_string()))?;
            }
            self.project_session_init(thread, &init)?;
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
        self.project_session_init(thread, &init)?;
        if projection.resources != awaken_session_contract::ResolvedSessionResources::default() {
            let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
                projection.workspace_id.clone(),
                projection.resource_revision,
                projection.resources,
            );
            self.install_dispatched_resources(thread, &manifest, claim)
                .await
                .map_err(|error| crate::HostError::internal(error.to_string()))?;
        }
        self.session_slots.update(thread, |slot| {
            slot.baseline = Some(baseline);
            slot.has_mcp_projection = has_mcp_projection;
        });
        Ok(())
    }

    pub(crate) fn install_environment_projection(
        &self,
        thread: &str,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) -> Result<(), crate::HostError> {
        let projection = crate::provisioning::project_environment(environment);
        if let Some(existing) = self
            .session_slots
            .read(thread, |slot| slot.environment_projection.clone())
            .flatten()
        {
            if existing.fingerprint != projection.fingerprint {
                return Err(crate::HostError::internal(format!(
                    "thread {thread} is already bound to a different frozen Environment"
                )));
            }
            self.session_slots.update(thread, |slot| {
                slot.environment_snapshot = Some(environment.clone())
            });
            return Ok(());
        }
        self.session_slots.update(thread, |slot| {
            slot.environment_projection = Some(projection);
            slot.environment_snapshot = Some(environment.clone());
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
    baseline: &awaken_session_contract::SessionBaseline,
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
        agent_id: baseline.agent_id.clone(),
        mounts,
        env,
        prompts: baseline.prompts.clone(),
    })
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
    /// Cause/effect graph: a Worker authors the canonical Session input directly;
    /// an exact credential reference is retained, no plaintext secret is added,
    /// and the network restriction is not translated by Runtime Host.
    ///
    /// | Rule | Credential ref | Network input | Effect |
    /// |---|---|---|---|
    /// | C1 | exact id/revision | allowlist | byte-faithful, secret-free input |
    #[test]
    fn canonical_application_contribution_is_secret_free_and_lossless() {
        let contribution = awaken_session_contract::ApplicationSessionContribution {
            session_id: "session".into(),
            application_fingerprint: "flow-plan".into(),
            input: awaken_session_contract::ApplicationSessionInput {
                mcp_inputs: vec![serde_json::json!({
                    "name": "flow",
                    "type": "url",
                    "url": "http://flow.invalid/mcp",
                    "credential_source_id": "run-credential",
                    "credential_revision": 3,
                })],
                network_restriction: Some(
                    awaken_session_contract::SessionNetworkPolicy::Allowlist {
                        hosts: vec!["A.example".into(), "b.example".into()],
                    },
                ),
                ..Default::default()
            },
        };
        assert_eq!(
            contribution.input.network_restriction,
            Some(awaken_session_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["A.example".into(), "b.example".into()],
            })
        );
        assert_eq!(contribution.input.mcp_inputs[0]["credential_revision"], 3);
        assert!(
            !contribution.input.mcp_inputs[0]
                .to_string()
                .contains("Bearer")
        );
    }

    /// Package projection cause/effect decision table: R1 an unprepared exact
    /// Environment projects package managers losslessly and no image override;
    /// R2 a prepared immutable image suppresses startup package installation and
    /// becomes the sole sandbox image override. Empty managers disappear and no
    /// protocol DTO reaches provisioning.
    #[test]
    fn environment_packages_project_losslessly_to_the_provisioning_contract() {
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env_packages".into(),
            revision: awaken_session_contract::EnvironmentRevision(3),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("fp".into()),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            packages: awaken_session_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                pip: vec!["httpx==0.28".into()],
                ..Default::default()
            },
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let projected = crate::provisioning::project_environment(&environment);
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
        assert_eq!(
            projected.packages.resolution_id.as_deref(),
            Some("env_packages:3")
        );

        let mut prepared = environment;
        prepared.prepared_image = Some("registry/awaken@sha256:prepared".into());
        let projected = crate::provisioning::project_environment(&prepared);
        assert!(projected.packages.is_empty(), "R2");
        assert!(matches!(
            projected.sandbox.and_then(|value| value.environment),
            Some(awaken_provisioning_contract::EnvironmentKind::Image { reference })
                if reference == "registry/awaken@sha256:prepared"
        ));
    }
}
