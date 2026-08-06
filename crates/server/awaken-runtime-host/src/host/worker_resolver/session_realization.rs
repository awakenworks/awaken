//! Worker-side projection and effect adapters for the canonical Session
//! realization driver.

use super::*;

struct WorkerProjectionSynchronizer<'a> {
    host: &'a SharedHost,
    claim: Option<&'a awaken_run_ingress::RunClaim>,
    published_snapshot: Option<&'a awaken_runtime_contract::ExecutableAgentSnapshot>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for WorkerProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        lease: &awaken_session_contract::SessionRealizationLease,
        _prepare_session: bool,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.host
            .install_frozen_session_projection(session_id, projection.clone(), self.claim)
            .await
            .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        self.host
            .install_session_realization_lease(session_id, lease.clone());
        if self.host.session_environment(session_id).await.is_none()
            && let Some(binding) = projection.environment.binding()
        {
            let published_snapshot = self.published_snapshot.ok_or_else(|| {
                awaken_session_contract::RunError::classified(
                    "session_environment_recovery_authority_missing",
                    "a cold Worker needs the exact claimed Agent snapshot to adopt a durable Session Environment",
                )
            })?;
            let (adopted, rebuild) = self
                .host
                .adopt_bound_session_environment(
                    session_id,
                    Some(binding),
                    &published_snapshot.resolved_spec.model_binding.provisioning,
                    false,
                )
                .await
                .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
            debug_assert!(!rebuild);
            self.host
                .ctx_for_snapshot_with_sandbox(
                    session_id,
                    Some(&projection.baseline.agent_id),
                    Some(published_snapshot.clone()),
                    adopted,
                )
                .await
                .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        }
        Ok(())
    }
}

pub(super) struct WorkerMcpEffects<'a>(pub(super) &'a SharedHost);

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for WorkerMcpEffects<'_> {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
    {
        self.0.stage_dispatched_mcp(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.publish_dispatched_mcp(generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.drain_dispatched_mcp(generation).await
    }
}

impl HostWorkerResolver {
    pub(crate) async fn realize_application_session(
        host: &SharedHost,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claim: Option<&awaken_run_ingress::RunClaim>,
        published_snapshot: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), awaken_run_ingress::Error> {
        awaken_session_contract::drive_session_realization(
            session_id,
            control,
            &WorkerProjectionSynchronizer {
                host,
                claim,
                published_snapshot,
            },
            &WorkerMcpEffects(host),
            directive,
        )
        .await
        .map_err(|error| Self::execution_error(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::worker_resolver::test_support::{AdoptionModel, test_activation};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: Mutex<Vec<&'static str>>,
        required_runtime: Option<(std::sync::Weak<SharedHost>, String)>,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_contract::StageMcpAttachment,
        ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
        {
            self.calls.lock().unwrap().push("stage");
            let resident = self.required_runtime.as_ref().is_none_or(|(host, thread)| {
                host.upgrade().is_some_and(|host| {
                    host.session_slots
                        .read(thread, |slot| slot.runtime.is_some())
                        .unwrap_or(false)
                })
            });
            if !resident {
                return Err(awaken_session_contract::RunError::classified(
                    "test_session_runtime_missing",
                    "MCP stage ran before the frozen Environment was adopted",
                ));
            }
            Ok(awaken_session_contract::McpRealizationReceipt {
                receipt_fingerprint: request.fingerprint(),
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    struct RecoveryControl {
        projection: awaken_session_contract::FrozenSessionProjection,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RecoveryControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.projection.clone(),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                },
            })
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self.projection.clone(),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Complete,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            Ok(())
        }
    }

    fn frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: serde_json::json!({}),
                    sandbox_provisioning: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
                mcp_authoring: Default::default(),
                agent_id: "agent-a".into(),
                model: "model".into(),
                runtime: None,
                application: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
            },
        );
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            toolsets: Vec::new(),
            mcp: Vec::new(),
        }
    }

    #[tokio::test]
    async fn cold_worker_adopts_the_frozen_environment_before_recovering_stdio_mcp() {
        /* Cold remote-Session recovery cause/effect graph: C1 Control returns a
         * frozen Session with an opaque resident-Environment binding; C2 the new
         * Worker process has no resident Runtime/Environment; C3 the claimed Run
         * carries the exact immutable Agent snapshot; C4 Control requires the
         * active sandbox-stdio MCP generation to be restaged. Effects: E1 adopt
         * the exact bound Environment before MCP stage; E2 never consult current
         * Agent publication; E3 preserve the Sandbox handle and generation; E4 a
         * cold lease-only replay without the exact snapshot fails closed.
         *
         * | Rule | binding | resident | exact snapshot | stdio recovery | Effect |
         * |---|---|---|---|---|---|
         * | R1 | yes | no | yes | yes | E1 + E2 + E3 |
         * | R2 | yes | yes | no | yes | reuse resident (covered by W6) |
         * | R3 | yes | no | no | yes | E4 |
         * | R4 | no | no | yes | no | ordinary frozen resume (covered by O1) |
         */
        let storage = tempfile::tempdir().expect("storage");
        let thread = "cold-frozen-environment";
        let activation = test_activation(thread, "run-cold-frozen-environment");
        let original =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let original_ctx = original
            .ctx_for_snapshot_with_sandbox(
                thread,
                Some("agent-a"),
                Some(activation.snapshot.clone()),
                None,
            )
            .await
            .expect("R1 original Environment");
        let handle = original_ctx
            .env
            .as_ref()
            .expect("R1 eager Environment")
            .handle();
        let binding = serde_json::to_string(&handle).expect("R1 durable binding");
        drop(original_ctx);
        drop(original);

        let stage = awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: thread.into(),
                attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: "worker-a".into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: u64::MAX,
            },
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "recover-browser-1".into(),
            name: "browser".into(),
            target: awaken_session_contract::McpTarget::sandbox_stdio(
                "playwright-mcp",
                vec!["--headless".into()],
            )
            .expect("sandbox stdio target"),
            credential: None,
            prompts_as_skills: false,
            selected_plaintext_holder: None,
        };
        let mut frozen = frozen_projection();
        frozen.environment.set_resident(binding);
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let directive = awaken_session_contract::SessionRealizationDirective {
            projection: frozen.clone(),
            lease,
            action: awaken_session_contract::SessionRealizationAction::Stage {
                prepare_session: true,
                mcp_stages: vec![stage],
            },
        };
        let control = RecoveryControl {
            projection: frozen.clone(),
        };
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let realizer = Arc::new(RecordingMcpRealizer {
            required_runtime: Some((Arc::downgrade(&host), thread.into())),
            ..Default::default()
        });
        let managed =
            crate::ManagedHost::new(host.clone()).with_mcp_attachment_realizer(realizer.clone());
        drop(managed);
        HostWorkerResolver::realize_application_session(
            &host,
            &control,
            thread,
            directive.clone(),
            None,
            Some(&activation.snapshot),
        )
        .await
        .expect("R1 frozen Environment recovery");
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(handle),
            "R1/E1-E3"
        );
        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"],
            "R1/E1"
        );

        let cold = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let error = HostWorkerResolver::realize_application_session(
            &cold, &control, thread, directive, None, None,
        )
        .await
        .expect_err("R3 cold lease-only recovery must fail closed");
        assert!(
            error.to_string().contains("exact claimed Agent snapshot"),
            "R3/E4: {error}"
        );
    }
}
