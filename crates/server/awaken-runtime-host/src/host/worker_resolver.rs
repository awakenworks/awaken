//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;
mod application;
mod claimed_dispatch;
#[cfg(test)]
mod dispatched_mcp_tests;
mod resolver;
mod session_realization;
#[cfg(test)]
pub(super) mod test_support;
use application::install_claimed_session_projection;
pub(crate) use resolver::HostWorkerResolver;

#[cfg(test)]
mod tests {
    use super::claimed_dispatch::adopt_bound_sandbox;
    use super::test_support::{
        AdoptionModel, ToggleBindingSink, claim, deferred_environment, prepare_deferred_session,
        test_activation,
    };
    use super::*;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// C1-C3: a cold Worker must derive eager-vs-deferred provisioning only from
    /// the immutable dispatch envelope. Legacy absence remains eager, an exact
    /// on-tool-use projection stays sandbox-free during Brain resolution, and a
    /// malformed projection fails before any Sandbox can be created.
    #[tokio::test]
    async fn cold_worker_runtime_projection_decision_table() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let legacy = claim(&store, "cold-legacy", "run-legacy", "worker-a", now).await;
        resolver
            .worker_for_claimed(&legacy)
            .await
            .expect("C1 legacy projection remains eager");
        assert!(
            host.session_environment("cold-legacy").await.is_some(),
            "C1"
        );

        let runtime = crate::provisioning::encode_session_runtime_envelope(
            deferred_environment(),
            Some(Vec::new()),
            Vec::new(),
        )
        .expect("encode runtime projection");
        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "cold-deferred",
                    "run-deferred",
                ))
                .with_session_runtime(runtime),
            )
            .await
            .expect("enqueue deferred projection");
        let deferred = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim deferred projection")
            .expect("deferred projection available");
        resolver
            .worker_for_claimed(&deferred)
            .await
            .expect("C2 cold Brain resolution stays deferred");
        assert!(
            host.session_environment("cold-deferred").await.is_none(),
            "C2"
        );
        assert!(
            host.session_slots
                .read("cold-deferred", |slot| slot.deferred_executor.is_some()
                    && slot.toolsets.as_ref().is_some_and(Vec::is_empty))
                .unwrap_or(false),
            "C2"
        );

        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "cold-invalid",
                    "run-invalid",
                ))
                .with_session_runtime(awaken_run_ingress::SessionRuntimeEnvelope::new("{")),
            )
            .await
            .expect("enqueue invalid projection");
        let invalid = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim invalid projection")
            .expect("invalid projection available");
        let error = match resolver.worker_for_claimed(&invalid).await {
            Ok(_) => panic!("C3 malformed runtime projection must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid Session runtime projection")
        );
        assert!(
            host.session_environment("cold-invalid").await.is_none(),
            "C3"
        );
    }

    /// D1-D5: durable lazy placement is fenced by the current dispatch claim.
    /// Brain resolution stays sandbox-free; a replacement claim rejects stale
    /// publication; and a crash gap after dispatch binding is repaired by adoption.
    #[tokio::test]
    async fn durable_deferred_sandbox_publication_decision_table() {
        use awaken_run_ingress::{Clock, DispatchQueue};
        use awaken_session_contract::SessionRuntime;

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let sink = Arc::new(ToggleBindingSink {
            fail: std::sync::atomic::AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        });
        let managed = prepare_deferred_session(host.clone(), "durable-lazy").await;
        managed.install_environment_binding_sink(sink.clone());
        let claimed = claim(&store, "durable-lazy", "run-lazy", "worker-a", now).await;
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("D1 resolves Brain worker without Sandbox");
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D1"
        );

        let replacement = store
            .claim("worker-b", 1_000, now + 2_000, &Default::default())
            .await
            .expect("replacement claim")
            .expect("expired claim is recoverable");
        let deferred = host
            .session_slots
            .read("durable-lazy", |slot| slot.deferred_executor.clone())
            .flatten()
            .expect("deferred executor");
        let error = deferred
            .invoke(&awaken_runtime_contract::tool::ToolCall {
                call_id: "stale-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing"}),
            })
            .await
            .expect_err("D3 stale claim is fenced");
        assert!(
            error.to_string().contains("replacement claim"),
            "D3: {error}"
        );
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D3"
        );

        resolver
            .worker_for_claimed(&replacement)
            .await
            .expect("install replacement claim");
        sink.fail.store(true, Ordering::SeqCst);
        let deferred = host
            .session_slots
            .read("durable-lazy", |slot| slot.deferred_executor.clone())
            .flatten()
            .expect("replacement deferred executor");
        let error = deferred
            .invoke(&awaken_runtime_contract::tool::ToolCall {
                call_id: "crash-gap-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing"}),
            })
            .await
            .expect_err("D4 Session binding failure");
        assert!(
            error
                .to_string()
                .contains("injected Session binding failure")
        );
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D4"
        );

        let adopted = store
            .claim("worker-c", 1_000, now + 4_000, &Default::default())
            .await
            .expect("adoption claim")
            .expect("dispatch-bound run is recoverable");
        assert!(adopted.sandbox.is_some(), "D4 dispatch binding survived");
        sink.fail.store(false, Ordering::SeqCst);
        resolver
            .worker_for_claimed(&adopted)
            .await
            .expect("D5 adopts and repairs Session binding");
        assert!(
            host.session_environment("durable-lazy").await.is_some(),
            "D5"
        );
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            2,
            "D4 failure + D5 repair"
        );
    }

    struct CountingProvisioner {
        calls: Arc<AtomicUsize>,
        refreshes: Arc<AtomicUsize>,
        phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    struct RecordingContributor {
        calls: Arc<AtomicUsize>,
        resume_calls: Arc<AtomicUsize>,
        phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
        projection: Arc<std::sync::Mutex<Option<awaken_session_contract::FrozenSessionProjection>>>,
        mcp_stage: Option<awaken_session_contract::StageMcpAttachment>,
        fail_begin: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_stage: std::sync::atomic::AtomicBool,
        required_runtime: Option<(std::sync::Weak<SharedHost>, String)>,
    }

    fn recording_environment() -> awaken_session_contract::EnvironmentSnapshot {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        awaken_session_contract::EnvironmentSnapshot {
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
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_contract::StageMcpAttachment,
        ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
        {
            self.calls.lock().unwrap().push("stage");
            if let Some((host, thread)) = &self.required_runtime {
                let resident = host.upgrade().is_some_and(|host| {
                    host.session_slots
                        .read(thread, |slot| slot.runtime.is_some())
                        .unwrap_or(false)
                });
                if !resident {
                    return Err(awaken_session_contract::RunError::classified(
                        "test_session_runtime_missing",
                        "claimed MCP stage ran before its immutable Agent snapshot opened the Session",
                    ));
                }
            }
            if self.fail_stage.load(Ordering::SeqCst) {
                return Err(awaken_session_contract::RunError::classified(
                    "test_mcp_stage_failed",
                    "test MCP stage failed",
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

    #[tokio::test]
    async fn cold_claimed_stdio_mcp_opens_the_exact_agent_snapshot_before_staging() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        /* Claimed sandbox-stdio recovery cause/effect decision table. Causes:
         * C1 a cold Worker has no process-local Agent publication; C2 the durable
         * Run carries its exact immutable snapshot and non-default backend
         * projection; C3 its Session runtime envelope contains a sandbox-stdio
         * MCP stage; C4 the Session has no resident Runtime/Environment. Effects:
         * E1 open the canonical Session Runtime from the claimed snapshot before
         * MCP staging; E2 stage and publish the exact MCP generation; E3 rebuild
         * the Runtime after publication invalidates its pre-stage projection;
         * E4 never consult or synthesize a second publication source. Rule S1:
         * C1+C2+C3+C4=>E1+E2+E3+E4. The adjacent malformed-envelope and stale-
         * claim rules retain fail-closed coverage for invalid authority. */
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let thread = "cold-stdio-recovery";
        let realizer = Arc::new(RecordingMcpRealizer {
            required_runtime: Some((Arc::downgrade(&host), thread.into())),
            ..Default::default()
        });
        let _managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
        let stage = awaken_session_contract::StageMcpAttachment {
            workspace_id: host.local_workspace().into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: thread.into(),
                attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: "worker-a".into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: u64::MAX,
            },
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "stage-browser-1".into(),
            name: "browser".into(),
            target: awaken_session_contract::McpTarget::sandbox_stdio(
                "playwright-mcp",
                vec!["--headless".into()],
            )
            .expect("sandbox stdio target"),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        let runtime = crate::provisioning::encode_session_runtime_envelope(
            deferred_environment(),
            Some(Vec::new()),
            vec![stage],
        )
        .expect("runtime envelope");
        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    thread,
                    "run-cold-stdio-recovery",
                ))
                .with_session_runtime(runtime),
            )
            .await
            .expect("enqueue");
        let claimed = store
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("claim")
            .expect("claimed Run");

        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("S1 cold stdio recovery");

        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"]
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false),
            "S1/E3"
        );
    }

    #[async_trait::async_trait]
    impl awaken_run_ingress_contract::ClaimedSessionControl for RecordingContributor {
        async fn resume_frozen(
            &self,
            claim: &awaken_run_ingress::RunClaim,
            _session_id: &str,
        ) -> Result<
            Option<awaken_session_contract::SessionRealizationDirective>,
            awaken_run_ingress_contract::ClaimedSessionControlError,
        > {
            self.resume_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.projection.lock().unwrap().clone().map(|projection| {
                awaken_session_contract::SessionRealizationDirective {
                    projection,
                    lease: awaken_session_contract::SessionRealizationLease {
                        owner: claim.owner.clone(),
                        runtime_incarnation: claim.owner.clone(),
                        epoch: claim.epoch,
                        expires_at_unix_ms: u64::MAX,
                    },
                    action: awaken_session_contract::SessionRealizationAction::Complete,
                }
            }))
        }

        async fn contribute(
            &self,
            _claim: &awaken_run_ingress::RunClaim,
            contribution: awaken_session_contract::ApplicationSessionContribution,
        ) -> Result<
            awaken_run_ingress_contract::ClaimedSessionContributionReceipt,
            awaken_run_ingress_contract::ClaimedSessionControlError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.phases.lock().unwrap().push("contribute");
            let input = contribution.input;
            let baseline = awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment: recording_environment(),
                    runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                    mcp_authoring: Default::default(),
                    // The Control projection and the claimed publication name
                    // the same immutable Agent. A different id is a projection
                    // conflict and is covered by the Session projection tests.
                    agent_id: "agent-a".into(),
                    model: "model".into(),
                    runtime: None,
                    application: Some(
                        awaken_session_contract::ApplicationContributionReceipt::from_input(
                            contribution.application_fingerprint,
                            &input,
                        ),
                    ),
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: input.mounts,
                    env: input.env,
                    prompts: input.prompts,
                },
            );
            let mcp = self.mcp_stage.as_ref().map_or_else(Vec::new, |stage| {
                vec![
                    serde_json::from_value(serde_json::json!({
                        "attachment_id": stage.generation.attachment_id,
                        "name": stage.name,
                        "generation": stage.generation.generation,
                        "target": stage.target,
                        "origin": "application",
                        "credential": stage.credential,
                        "selected_plaintext_holder": stage.selected_plaintext_holder,
                        "state": "realizing",
                        "publication_acknowledged": false,
                        "realization": null,
                        "attempts": 1,
                        "last_error": null
                    }))
                    .expect("test MCP projection"),
                ]
            });
            let projection = awaken_session_contract::FrozenSessionProjection {
                workspace_id: "workspace".into(),
                revision: awaken_session_contract::SessionRevision(2),
                baseline,
                environment: Default::default(),
                resource_revision: 0,
                resources: Default::default(),
                mcp,
                toolsets: Vec::new(),
            };
            *self.projection.lock().unwrap() = Some(projection.clone());
            let lease = awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 1,
                expires_at_unix_ms: self
                    .mcp_stage
                    .as_ref()
                    .map_or(u64::MAX, |stage| stage.generation.lease_expires_at_unix_ms),
            };
            Ok(
                awaken_run_ingress_contract::ClaimedSessionContributionReceipt {
                    contribution: awaken_session_contract::ApplicationSessionContributionReceipt {
                        outcome: awaken_session_contract::ApplicationContributionOutcome::Committed,
                        projection: projection.clone(),
                    },
                    realization: awaken_session_contract::SessionRealizationDirective {
                        projection,
                        lease,
                        action: awaken_session_contract::SessionRealizationAction::Stage {
                            prepare_session: true,
                            mcp_stages: self.mcp_stage.clone().into_iter().collect(),
                        },
                    },
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RecordingContributor {
        async fn begin_session_realization(
            &self,
            command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("begin");
            if self.fail_begin.load(Ordering::SeqCst) {
                return Err(awaken_session_contract::SessionRealizationControlFailure::NotReady);
            }
            let projection = self
                .projection
                .lock()
                .unwrap()
                .clone()
                .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
            let mut stage = self
                .mcp_stage
                .clone()
                .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
            stage.generation.lease_expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
            stage.stage_idempotency_key =
                format!("renew:{}", command.target.lease_expires_at_unix_ms);
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection,
                lease: awaken_session_contract::SessionRealizationLease {
                    owner: command.target.owner,
                    runtime_incarnation: command.target.runtime_incarnation,
                    epoch: 1,
                    expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                },
                action: awaken_session_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            })
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("activate");
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("contribution projection"),
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
            self.phases.lock().unwrap().push("acknowledge");
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("contribution projection"),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Complete,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            self.phases.lock().unwrap().push("fail");
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::ApplicationSessionProvisioner for CountingProvisioner {
        async fn prepare(
            &self,
            activation: &RunActivation,
            session_id: &str,
            ownership: Arc<dyn awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier>,
        ) -> Result<
            awaken_session_contract::ApplicationSessionContribution,
            awaken_session_contract::ApplicationSessionProvisionError,
        > {
            ownership.verify_current().await.map_err(|error| {
                awaken_session_contract::ApplicationSessionProvisionError::retryable(
                    error.to_string(),
                )
            })?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.phases.lock().unwrap().push("prepare");
            Ok(awaken_session_contract::ApplicationSessionContribution {
                session_id: session_id.to_owned(),
                application_fingerprint: format!(
                    "application:{}",
                    activation.snapshot.fingerprint.0
                ),
                input: Default::default(),
            })
        }

        async fn refresh_frozen(
            &self,
            _activation: &RunActivation,
            _session_id: &str,
            ownership: Arc<dyn awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier>,
        ) -> Result<
            awaken_session_contract::ApplicationSessionMaterialRefresh,
            awaken_session_contract::ApplicationSessionProvisionError,
        > {
            ownership.verify_current().await.map_err(|error| {
                awaken_session_contract::ApplicationSessionProvisionError::retryable(
                    error.to_string(),
                )
            })?;
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            self.phases.lock().unwrap().push("refresh");
            Ok(awaken_session_contract::ApplicationSessionMaterialRefresh::Refreshed)
        }
    }

    fn ordinary_frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: true,
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

    /// Cause/effect graph: C1 an authenticated remote Run claim is current; C2
    /// its Session is already frozen; C3 no application provisioner is installed;
    /// C4 the ordinary Worker has the claimed Session-control client. C1+C2+C4
    /// cause E1 resume the canonical realization, E2 install the exact baseline
    /// and lease, and E3 create the Worker environment without contribution.
    /// C4 absent is the co-located local-pool row, covered by the cold legacy test.
    ///
    /// | Rule | Claim | Frozen | Provisioner | Control | Effect |
    /// |---|---|---|---|---|---|
    /// | O1 | live | ordinary | absent | present | resume/realize, no contribution |
    /// | O2 | stale | any | any | present | reject before realization |
    /// | O3 | live | preparing application | absent | present | fail closed |
    /// | O4 | live | local pre-realized | absent | absent | local-pool replay |
    /// | O5 | live ordinary Run (no Session pointer) | n/a | absent | present | bypass Session control |
    #[tokio::test]
    async fn ordinary_remote_worker_uses_claimed_session_realization_without_an_application() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        let dispatch = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("in-memory dispatch"),
        );
        let contribution_calls = Arc::new(AtomicUsize::new(0));
        let resume_calls = Arc::new(AtomicUsize::new(0));
        let projection = Arc::new(std::sync::Mutex::new(Some(ordinary_frozen_projection())));
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_dispatch_store(dispatch.clone())
                .with_application_session_control(Arc::new(RecordingContributor {
                    calls: contribution_calls.clone(),
                    resume_calls: resume_calls.clone(),
                    phases: Arc::new(std::sync::Mutex::new(Vec::new())),
                    projection,
                    mcp_stage: None,
                    fail_begin: Arc::new(AtomicBool::new(false)),
                })),
        );
        dispatch
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "ordinary-remote",
                    "run-ordinary-remote",
                ))
                .for_session(awaken_agent_contract::agent::thread::Id(
                    "ordinary-remote".into(),
                )),
            )
            .await
            .expect("O1 enqueue");
        let claimed = dispatch
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("O1 claim")
            .expect("O1 claimed Run");
        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("O1 canonical realization");

        assert_eq!(contribution_calls.load(Ordering::SeqCst), 0, "O1/E3");
        assert_eq!(resume_calls.load(Ordering::SeqCst), 1, "O1/E1");
        assert!(
            host.session_environment("ordinary-remote").await.is_some(),
            "O1/E3"
        );
        assert!(
            host.session_slots
                .read("ordinary-remote", |slot| {
                    slot.baseline.is_some()
                        && slot.realization_lease.as_ref().is_some_and(|lease| {
                            lease.owner == claimed.lease.owner && lease.epoch == claimed.lease.epoch
                        })
                })
                .unwrap_or(false),
            "O1/E1-E2"
        );

        dispatch
            .enqueue(awaken_run_ingress::RunDispatch::new(test_activation(
                "ordinary-run",
                "run-without-session",
            )))
            .await
            .expect("O5 enqueue");
        let claimed = dispatch
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("O5 claim")
            .expect("O5 claimed Run");
        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("O5 ordinary Run bypasses Session realization");
        assert_eq!(resume_calls.load(Ordering::SeqCst), 1, "O5");
    }

    #[tokio::test]
    async fn claimed_application_contribution_is_acknowledged_before_session_realization() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        // Cause graph: live exact claim -> provisioner succeeds -> Control client
        // exists -> contribution receipt carries a frozen projection -> install it
        // before Session environment realization. A missing client must fail closed;
        // the stale-claim table row is generated by the adjacent test.
        //
        // | Rule | Claim live | Provisioner | Contributor | Effect |
        // |---|---|---|---|---|
        // | W1 | T | success | installed, exact Agent | receipt, realization, then material refresh before Run |
        // | W2 | T | success | missing | reject before environment |
        // | W3 | F | - | any | reject before provisioner |
        // | W4 | T | success + initial MCP + legacy dispatch copy | installed | Control alone stages/activates/publishes/acks, then renews |
        // | W5 | T | initial MCP stage fails | installed | fail; no publish/environment |
        // | W6 | T | active MCP lease due | installed | same canonical driver renews generation |
        // | W7 | T | renewal loses Session authority | installed | revoke local projection, preserve durable Environment; Worker remains healthy |
        // | W8 | T | Session already frozen | installed | refresh before adoption/rebuild and again after realization |
        for (rule, install_contributor, with_initial_mcp, fail_mcp_stage) in [
            ("W1", true, false, false),
            ("W2", false, false, false),
            ("W4", true, true, false),
            ("W5", true, true, true),
        ] {
            let storage = tempfile::tempdir().expect("storage");
            let dispatch = Arc::new(
                awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                    .expect("in-memory dispatch"),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let refreshes = Arc::new(AtomicUsize::new(0));
            let contribution_calls = Arc::new(AtomicUsize::new(0));
            let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
            let projection = Arc::new(std::sync::Mutex::new(None));
            let fail_begin = Arc::new(AtomicBool::new(false));
            let host = SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_store_dir(storage.path())
                .with_dispatch_store(dispatch.clone())
                .with_application_session_provisioner(Arc::new(CountingProvisioner {
                    calls: calls.clone(),
                    refreshes: refreshes.clone(),
                    phases: phases.clone(),
                }));
            let mcp_realizer = Arc::new(RecordingMcpRealizer::default());
            mcp_realizer
                .fail_stage
                .store(fail_mcp_stage, Ordering::SeqCst);
            let initial_mcp_expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
                + 1_000;
            let mcp_stage = with_initial_mcp.then(|| {
                let generation = awaken_session_contract::McpGenerationRef {
                    session_id: format!("thread-application-{rule}"),
                    attachment_id: awaken_session_contract::McpAttachmentId("mcp-docs".into()),
                    generation: awaken_session_contract::McpGeneration(1),
                    runtime_incarnation: "worker-a".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: initial_mcp_expiry,
                };
                awaken_session_contract::StageMcpAttachment {
                    workspace_id: "workspace".into(),
                    generation,
                    realization_id: "realize-docs-1".into(),
                    stage_idempotency_key: "stage-docs-1".into(),
                    name: "docs".into(),
                    target: awaken_session_contract::McpTarget::parse_http(
                        "https://mcp.example.test/sse",
                    )
                    .unwrap(),
                    credential: None,
                    prompts_as_skills: false,
                    selected_plaintext_holder: None,
                }
            });
            let dispatched_mcp_stage = mcp_stage.clone();
            let host = if install_contributor {
                host.with_application_session_control(Arc::new(RecordingContributor {
                    calls: contribution_calls.clone(),
                    resume_calls: Arc::new(AtomicUsize::new(0)),
                    phases: phases.clone(),
                    projection,
                    mcp_stage,
                    fail_begin: fail_begin.clone(),
                }))
            } else {
                host
            };
            let host = Arc::new(host);
            let _managed = crate::ManagedHost::new(host.clone())
                .with_mcp_attachment_realizer(mcp_realizer.clone())
                .install_dispatch_session_runtime();
            let thread = format!("thread-application-{rule}");
            let run = format!("run-application-{rule}");
            let mut run_dispatch =
                awaken_run_ingress::RunDispatch::new(test_activation(&thread, &run))
                    .for_session(awaken_agent_contract::agent::thread::Id(thread.clone()));
            if let Some(stage) = dispatched_mcp_stage {
                let runtime = crate::provisioning::encode_session_runtime_envelope(
                    recording_environment(),
                    Some(Vec::new()),
                    vec![stage],
                )
                .expect("legacy application runtime envelope");
                run_dispatch = run_dispatch.with_session_runtime(runtime);
            }
            dispatch.enqueue(run_dispatch).await.expect("enqueue");
            let now = awaken_run_ingress::SystemClock.now_ms();
            let claimed = dispatch
                .claim("worker-a", 30_000, now, &Default::default())
                .await
                .expect("claim")
                .expect("claimed run");
            let resolver = HostWorkerResolver {
                host: Arc::downgrade(&host),
            };
            let result = resolver.worker_for_claimed(&claimed).await;

            if rule == "W1" {
                resolver
                    .worker_for_claimed(&claimed)
                    .await
                    .expect("W8 resumes the frozen projection");
                assert_eq!(calls.load(Ordering::SeqCst), 1, "W8");
                assert_eq!(refreshes.load(Ordering::SeqCst), 3, "W8");
                assert_eq!(contribution_calls.load(Ordering::SeqCst), 1, "W8");
            }

            if rule == "W4" {
                assert_eq!(
                    host.renew_due_session_realizations(
                        initial_mcp_expiry,
                        initial_mcp_expiry + 1_000,
                    )
                    .await
                    .expect("W6"),
                    1,
                    "W6"
                );
                let durable_environment = host
                    .session_environment_handle(&thread)
                    .await
                    .expect("W7 durable Environment before revocation");
                let durable_binding = serde_json::to_string(&durable_environment)
                    .expect("W7 durable Environment binding");
                fail_begin.store(true, Ordering::SeqCst);
                assert_eq!(
                    host.renew_due_session_realizations(
                        initial_mcp_expiry + 1_000,
                        initial_mcp_expiry + 2_000,
                    )
                    .await
                    .expect("W7 isolates the rejected Session renewal"),
                    0,
                    "W7"
                );
                assert!(!host.session_slots.contains(&thread), "W7");
                let (adopted, rebuild) = host
                    .adopt_bound_session_environment(
                        &thread,
                        Some(&durable_binding),
                        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                        false,
                    )
                    .await
                    .expect("W7 preserved Environment remains adoptable");
                assert!(!rebuild, "W7");
                assert_eq!(
                    adopted.expect("W7 adopted Environment").handle(),
                    durable_environment,
                    "W7"
                );
            }

            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(install_contributor),
                "{rule}"
            );
            assert_eq!(
                contribution_calls.load(Ordering::SeqCst),
                usize::from(install_contributor),
                "{rule}"
            );
            assert_eq!(
                refreshes.load(Ordering::SeqCst),
                usize::from(install_contributor && !fail_mcp_stage) + 2 * usize::from(rule == "W1"),
                "{rule}"
            );
            let succeeds = install_contributor && !fail_mcp_stage;
            assert_eq!(result.is_ok(), succeeds, "{rule}");
            let expected_phases: &[&str] = if fail_mcp_stage {
                &["prepare", "contribute", "fail"]
            } else if rule == "W4" {
                &[
                    "prepare",
                    "contribute",
                    "activate",
                    "acknowledge",
                    "refresh",
                    "begin",
                    "activate",
                    "acknowledge",
                    "begin",
                ]
            } else if install_contributor {
                &[
                    "prepare",
                    "contribute",
                    "activate",
                    "acknowledge",
                    "refresh",
                    "refresh",
                    "refresh",
                ]
            } else {
                &[]
            };
            assert_eq!(phases.lock().unwrap().as_slice(), expected_phases, "{rule}");
            let expected_mcp: &[&str] = if fail_mcp_stage {
                &["stage"]
            } else if rule == "W4" {
                &["stage", "publish", "stage", "publish"]
            } else if with_initial_mcp {
                &["stage", "publish"]
            } else {
                &[]
            };
            assert_eq!(
                mcp_realizer.calls.lock().unwrap().as_slice(),
                expected_mcp,
                "{rule}"
            );
            assert_eq!(
                host.session_environment(&thread).await.is_some(),
                succeeds && rule != "W4",
                "{rule}"
            );
        }
    }

    /// Cause/effect rule W3 from the table above: a Session Run with a stale
    /// claim must fail before application preparation, refresh, or Environment
    /// creation. The explicit Session pointer distinguishes it from a plain Run.
    #[tokio::test]
    async fn stale_claim_is_rejected_before_application_provisioning() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        let storage = tempfile::tempdir().expect("storage");
        let dispatch = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("in-memory dispatch"),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_store_dir(storage.path())
                .with_dispatch_store(dispatch.clone())
                .with_application_session_provisioner(Arc::new(CountingProvisioner {
                    calls: calls.clone(),
                    refreshes: refreshes.clone(),
                    phases: phases.clone(),
                })),
        );
        dispatch
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "thread-stale-application",
                    "run-stale-application",
                ))
                .for_session(awaken_agent_contract::agent::thread::Id(
                    "thread-stale-application".into(),
                )),
            )
            .await
            .expect("enqueue");
        let now = awaken_run_ingress::SystemClock.now_ms();
        let mut claimed = dispatch
            .claim("worker-a", 30_000, now, &Default::default())
            .await
            .expect("claim")
            .expect("claimed run");
        claimed.lease.epoch += 1;
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        assert!(resolver.worker_for_claimed(&claimed).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        assert!(phases.lock().unwrap().is_empty());
        assert!(
            host.session_environment("thread-stale-application")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn resource_manifest_must_match_the_durable_execution_scope() {
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let run = RunId("run-scope-mismatch".to_string());
        let request =
            awaken_run_ingress::RunDispatch::new(test_activation("thread-scope-mismatch", &run.0))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-b"),
                ))
                .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                    "workspace-a",
                    r#"{"inputs":[],"skills":[]}"#,
                ));
        let claimed = awaken_run_ingress::Claimed {
            request,
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "worker-a".to_string(),
                expires_ms: 100,
                epoch: 1,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: false,
            pending: Vec::new(),
            recovered: false,
            sandbox: None,
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let error = match resolver.worker_for_claimed(&claimed).await {
            Ok(_) => panic!("scope mismatch must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("outside its execution scope"));
        assert!(
            host.session_environment("thread-scope-mismatch")
                .await
                .is_none(),
            "scope rejection happens before sandbox creation"
        );
    }

    #[tokio::test]
    async fn malformed_resource_manifest_is_rejected_before_sandbox_creation() {
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let run = RunId("run-malformed-resources".to_string());
        let request = awaken_run_ingress::RunDispatch::new(test_activation(
            "thread-malformed-resources",
            &run.0,
        ))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-a"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-a",
            "not-json",
        ));
        let claimed = awaken_run_ingress::Claimed {
            request,
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "worker-a".to_string(),
                expires_ms: 100,
                epoch: 1,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: false,
            pending: Vec::new(),
            recovered: false,
            sandbox: None,
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let error = match resolver.worker_for_claimed(&claimed).await {
            Ok(_) => panic!("malformed manifest must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid Session resource manifest")
        );
        assert!(
            host.session_environment("thread-malformed-resources")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn claimed_worker_installs_then_live_replaces_a_frozen_file_manifest() {
        let storage = tempfile::tempdir().expect("storage");
        let mut raw_host =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        // Cause/effect decision table: R1 no resident Environment + generation 1
        // File => stage before open with path fidelity/read-only enforcement; R2
        // resident Environment + higher empty generation => remove the physical
        // projection before publishing the empty logical manifest. Workdir remains
        // correctly ineligible for R1's immutability requirement.
        raw_host.session_provider =
            crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
                storage.path().join("sandboxes"),
                false,
            );
        let host = Arc::new(raw_host);
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let bytes = b"frozen worker input".to_vec();
        let file_id = host
            .file_application()
            .expect("test composition installs File application")
            .create_uploaded_file(
                "workspace-a",
                "input.bin".into(),
                "application/octet-stream".into(),
                &bytes,
            )
            .await
            .expect("create file")
            .id;
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-a",
            awaken_session_contract::ResolvedSessionResources {
                inputs: vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                    source: awaken_session_contract::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from(file_id.as_str()),
                    },
                    mount_path: "/uploads/input.bin".to_string(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );

        host.install_dispatched_resources("thread-cold-resource", &manifest, None)
            .await
            .expect("install frozen manifest");
        let activation = test_activation("thread-cold-resource", "run-cold-resource");
        host.ctx_for_snapshot_with_sandbox(
            "thread-cold-resource",
            Some("agent-a"),
            Some(activation.snapshot),
            None,
        )
        .await
        .expect("open environment after resource install");

        let mount = host
            .sandbox_spec("thread-cold-resource")
            .mounts
            .into_iter()
            .find(|mount| mount.mount_id == file_id)
            .expect("frozen File mount");
        assert_eq!(mount.mount_path, "/mnt/session/uploads/uploads/input.bin");
        assert_eq!(
            mount.access,
            awaken_provisioning_contract::MountAccess::ReadOnly
        );
        let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = mount.source
        else {
            panic!("frozen File uses the binary-safe carried source")
        };
        assert_eq!(contents, bytes);
        assert_eq!(
            host.thread_resource_manifest("thread-cold-resource"),
            Some(manifest)
        );

        let cleared = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            2,
            awaken_session_contract::ResolvedSessionResources {
                inputs: Vec::new(),
                skills: Some(Vec::new()),
            },
        );
        host.install_dispatched_resources("thread-cold-resource", &cleared, None)
            .await
            .expect("R2 live replacement");
        assert!(
            host.session_environment("thread-cold-resource")
                .await
                .expect("resident environment")
                .list_files("/mnt/session/uploads")
                .await
                .expect("list live files")
                .is_empty(),
            "R2 removes the stale physical projection"
        );
        assert!(
            host.sandbox_spec("thread-cold-resource").mounts.is_empty(),
            "R2 publishes the empty staged projection"
        );
        assert_eq!(
            host.thread_resource_manifest("thread-cold-resource"),
            Some(cleared),
            "R2 publishes generation 2 only after realization"
        );
    }

    /// Claimed Session Resource generation cause/effect decision table.
    /// Causes: C1 prior process-local generation exists; C2 incoming generation
    /// is exact, newer, older, or same-generation/different-value; C3 Workspace
    /// partition is unchanged. Effects: E1 revalidate exact bytes without a
    /// logical replacement; E2 advance to the newer generation; E3 reject stale,
    /// corrupt, or cross-Workspace replacement and retain the prior manifest.
    /// Rules: R1 exact+C3=>E1; R2 newer+C3=>E2; R3 older+C3=>E3;
    /// R4 same-revision/different-value+C3=>E3; R5 !C3=>E3.
    #[tokio::test]
    async fn claimed_worker_advances_only_to_a_newer_resource_generation() {
        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-generation-fence".into()),
            owner: "worker-generation-fence".into(),
            epoch: 1,
        };
        let revision_four = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            4,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-generation-fence", &revision_four, None)
            .await
            .expect("install active generation");

        host.install_dispatched_resources("thread-generation-fence", &revision_four, Some(&claim))
            .await
            .expect("R1 exact replay");

        for rejected in [
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                3,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                4,
                awaken_session_contract::ResolvedSessionResources {
                    inputs: Vec::new(),
                    skills: Some(Vec::new()),
                },
            ),
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-b",
                5,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
        ] {
            let result = host
                .install_dispatched_resources("thread-generation-fence", &rejected, Some(&claim))
                .await;
            assert!(
                result.is_err(),
                "R3/R4/R5 reject non-authoritative replacement: {rejected:?}"
            );
            assert_eq!(
                host.thread_resource_manifest("thread-generation-fence"),
                Some(revision_four.clone())
            );
        }

        let revision_five = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            5,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-generation-fence", &revision_five, Some(&claim))
            .await
            .expect("R2 newer generation");
        assert_eq!(
            host.thread_resource_manifest("thread-generation-fence"),
            Some(revision_five)
        );
    }

    /// Cause/effect decision table for Worker-side File staging:
    /// | Rule | Worker File source | exact claim | local File DB entry | Effect |
    /// |---|---|---|---|---|
    /// | W1 | remote adapter | present | absent | stage returned digest/bytes; later runtime validation does not reopen a local File database |
    /// | W2 | remote adapter | absent | absent | fail closed; no mount |
    #[tokio::test]
    async fn cold_worker_file_staging_uses_only_the_claim_bound_source() {
        struct ClaimFileSource;

        #[async_trait::async_trait]
        impl crate::FileContentSource<awaken_run_ingress::RunClaim> for ClaimFileSource {
            async fn read(
                &self,
                workspace_id: &str,
                file_id: &str,
                claim: Option<&awaken_run_ingress::RunClaim>,
            ) -> Result<Option<(String, Vec<u8>)>, awaken_resource_contract::FileContentSourceError>
            {
                let Some(claim) = claim else {
                    return Ok(None);
                };
                if workspace_id != "workspace-remote"
                    || file_id != "file-remote"
                    || claim.run_id.0 != "run-remote"
                    || claim.owner != "worker-remote"
                    || claim.epoch != 7
                {
                    return Ok(None);
                }
                let bytes = b"remote immutable File".to_vec();
                Ok(Some((awaken_resource_contract::content_id(&bytes), bytes)))
            }
        }

        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_file_content_source(Arc::new(ClaimFileSource)),
        );
        let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-remote",
            awaken_session_contract::ResolvedSessionResources {
                inputs: vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new("remote-file-binding"),
                    source: awaken_session_contract::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-remote"),
                    },
                    mount_path: "/input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-remote".into()),
            owner: "worker-remote".into(),
            epoch: 7,
        };

        host.install_dispatched_resources("remote-file-ok", &manifest, Some(&claim))
            .await
            .expect("W1");
        let mount = host
            .sandbox_spec("remote-file-ok")
            .mounts
            .into_iter()
            .find(|mount| mount.mount_id == "file-remote")
            .expect("W1 exact mount");
        assert!(
            matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::InlineBytes { ref contents, .. }
                    if contents == b"remote immutable File"
            ),
            "W1"
        );
        managed
            .validate_thread_resource_bindings("remote-file-ok")
            .await
            .expect("W1 has no redundant local File catalog check");

        let error = host
            .install_dispatched_resources("remote-file-no-claim", &manifest, None)
            .await
            .expect_err("W2");
        assert!(error.to_string().contains("not found"), "W2: {error}");
        assert!(
            host.sandbox_spec("remote-file-no-claim").mounts.is_empty(),
            "W2"
        );
    }

    #[tokio::test]
    async fn sandbox_binding_is_validated_before_provider_adoption() {
        let storage = tempfile::tempdir().expect("storage");
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        assert!(
            host.adopt_bound_session_environment(
                "thread-a",
                Some("not-json"),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                false,
            )
            .await
            .is_err()
        );

        let wrong = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "local", "thread-b",
        ))
        .unwrap();
        assert!(
            host.adopt_bound_session_environment(
                "thread-a",
                Some(&wrong),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                false,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn recovery_mode_controls_the_production_adoption_seam() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-adoption";
        let first = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let first_ctx = first.ctx_for(thread, None).await.expect("first session");
        let handle = first_ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        drop(first_ctx);
        drop(first);

        let replacement =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let run_id = RunId("run-adoption".into());
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&encoded),
            thread,
            &run_id,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("continuity mode adopts the durable handle");
        assert!(!rebuild);
        assert_eq!(adopted.unwrap().handle(), handle);

        let missing_thread = "thread-missing";
        let missing = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            handle.provider_kind,
            missing_thread,
        ))
        .unwrap();
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&missing),
            missing_thread,
            &run_id,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .expect("rebuild mode may replace a missing sandbox from committed truth");
        assert!(adopted.is_none());
        assert!(rebuild);
        assert!(
            adopt_bound_sandbox(
                &replacement,
                Some(&missing),
                missing_thread,
                &run_id,
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
            )
            .await
            .is_err(),
            "continuity mode fails closed when the bound sandbox is gone"
        );
    }

    #[tokio::test]
    async fn cold_adoption_uses_the_provider_owned_by_frozen_model_provisioning() {
        // Cause/effect graph: C1=HostExecutor/Provider model; C2=BackendOwned
        // model; C3=durable handle exists only below the selected provider root.
        // Effects: E1=adopt from configured Session root; E2=adopt from trusted
        // host-identity root; E3=missing selected root fails continuity.
        //
        // Decision table: A1 C1+C3(configured)=>E1 (covered by
        // recovery_mode_controls...); A2 C2+C3(trusted)=>E2; A3 either model
        // with its selected root absent=>E3 (covered by that test's missing row).
        let storage = tempfile::tempdir().expect("storage");
        let trusted_root = storage.path().join("trusted-local-sandboxes");
        let thread = "thread-backend-owned-adoption";
        let provisioning = awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
            credential: awaken_runtime_contract::CredentialRef {
                id: "local".into(),
                revision: 1,
            },
            model_selection: awaken_runtime_contract::resolved::BackendModelSelection::Default,
            acp: Default::default(),
        };

        let mut first =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        first.backend_owned_session_provider =
            Some(crate::session_environment::SessionEnvironmentProvider::workdir(&trusted_root));
        let created = first
            .backend_owned_session_provider
            .as_ref()
            .expect("trusted provider")
            .create(&first.sandbox_spec(thread))
            .await
            .expect("A2 create trusted environment");
        let handle = created.handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        drop(created);
        drop(first);
        assert!(
            trusted_root.join(thread).exists(),
            "A2 trusted root owns the durable environment"
        );
        assert!(
            !storage.path().join("sandboxes").join(thread).exists(),
            "A2 ordinary provider root cannot satisfy this handle"
        );

        let mut replacement =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        replacement.backend_owned_session_provider =
            Some(crate::session_environment::SessionEnvironmentProvider::workdir(&trusted_root));
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&encoded),
            thread,
            &RunId("run-backend-owned-adoption".into()),
            &provisioning,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("A2 adopt from the provisioning-selected provider");
        assert!(!rebuild, "A2");
        assert_eq!(adopted.expect("A2 adopted").handle(), handle, "A2");
    }

    #[tokio::test]
    async fn a_resident_environment_is_reused_without_a_second_adoption() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-resident-adoption";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let handle = ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();

        let (adopted, rebuild) = adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &RunId("resident-run".into()),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("resident handle is already adopted");

        assert!(adopted.is_none(), "no duplicate environment wrapper");
        assert!(!rebuild);
        assert_eq!(host.session_environment_handle(thread).await, Some(handle));
    }

    #[tokio::test]
    async fn a_dead_resident_environment_fails_continuity_and_is_fenced_before_rebuild() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-dead-resident";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let handle = ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        std::fs::remove_dir_all(storage.path().join("sandboxes").join(thread))
            .expect("terminate local sandbox out of band");

        let run = RunId("dead-resident-run".into());
        assert!(
            adopt_bound_sandbox(
                &host,
                Some(&encoded),
                thread,
                &run,
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
            )
            .await
            .is_err(),
            "continuity never silently replaces a dead resident sandbox"
        );
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(handle.clone()),
            "a failed continuity check does not mutate the owner registry"
        );

        let (adopted, rebuild) = adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &run,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .expect("explicit rebuild may discard the dead resident environment");
        assert!(adopted.is_none());
        assert!(rebuild);
        assert!(host.session_environment(thread).await.is_none());
        assert!(
            !host
                .session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn a_stale_binding_cannot_evict_a_different_resident_environment() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-binding-fence";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let resident = ctx.env.as_ref().expect("eager environment").handle();
        let mut stale = resident.clone();
        stale.extra = Some(serde_json::json!({"generation": "stale"}));
        let encoded = serde_json::to_string(&stale).unwrap();

        assert!(
            adopt_bound_sandbox(
                &host,
                Some(&encoded),
                thread,
                &RunId("stale-binding-run".into()),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
            )
            .await
            .is_err(),
            "rebuild mode cannot override the full-handle fence"
        );
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(resident)
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn an_aba_replacement_with_the_same_handle_survives_stale_discard() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-environment-aba";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        host.ctx_for(thread, None).await.expect("resident session");
        let observed = host
            .session_environment(thread)
            .await
            .expect("observed owner");
        let replacement = Arc::new(
            host.session_provider
                .adopt(&observed.handle())
                .await
                .expect("same-handle replacement"),
        );
        assert_eq!(observed.handle(), replacement.handle());
        assert!(!Arc::ptr_eq(&observed, &replacement));

        host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.environment = Some(replacement.clone());
        });

        assert!(
            !host.discard_session_environment(thread, &observed).await,
            "object identity fences a stale observer even when the handle is reused"
        );
        let current = host
            .session_environment(thread)
            .await
            .expect("replacement kept");
        assert!(Arc::ptr_eq(&current, &replacement));
    }

    #[tokio::test]
    async fn cancellation_resolution_does_not_touch_an_invalid_sandbox_binding() {
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let thread = ThreadId("thread-control-only".to_string());
        let run = RunId("run-control-only".to_string());
        let fingerprint = CatalogFingerprint("control-only-catalog".to_string());
        let activation = RunActivation::new(
            run.clone(),
            thread.clone(),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("control-only-snapshot".to_string()),
                metadata: Default::default(),
                root_agent_id: AgentId("control-only-agent".to_string()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: fingerprint.clone(),
                    instructions: "test".to_string(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", "model", "backend"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint,
            },
            Vec::new(),
        );
        let claimed = awaken_run_ingress::Claimed {
            request: awaken_run_ingress::RunDispatch::new(activation),
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "control-owner".to_string(),
                expires_ms: 100,
                epoch: 2,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: true,
            pending: Vec::new(),
            recovered: false,
            sandbox: Some("this is deliberately not a sandbox handle".to_string()),
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("terminal control bypasses sandbox decoding/adoption");
        assert!(host.session_environment(&thread.0).await.is_none());
        assert!(
            !host
                .session_slots
                .read(&thread.0, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }
}
