//! Publication selection without constructing a Host or executing a provider.
use super::*;
use crate::installed_session::InstalledSessions;
use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{PublishedAgentSnapshotSource, StaticPublishedAgentSnapshots};
use std::sync::{Barrier, Mutex};

fn publication(agent: &str, revision: u64) -> ExecutableAgentSnapshot {
    let mut snapshot = ExecutableAgentSnapshot::builder(agent)
        .resolved_model(ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::parse("provider", "model", "native")
                .expect("valid publication model fixture"),
        ))
        .build();
    snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
    snapshot.metadata.source.revision = revision;
    snapshot.recompute_fingerprint().unwrap();
    snapshot
}

fn baseline(revision: Option<u64>) -> awaken_session_configuration_contract::SessionBaseline {
    awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "publication-environment".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "publication-environment-v1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization:
                    awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
            },
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent-a".into(),
            agent_revision: revision,
            model: "provider/model".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    )
}

struct Publications {
    values: StaticPublishedAgentSnapshots,
    reads: Mutex<Vec<(String, Option<u64>)>>,
}

impl PublishedAgentSnapshotSource for Publications {
    fn current(&self, workspace: &str, agent: &AgentId) -> Option<ExecutableAgentSnapshot> {
        self.reads.lock().unwrap().push((workspace.into(), None));
        self.values.current(workspace, agent)
    }

    fn exact(
        &self,
        workspace: &str,
        fingerprint: &awaken_runtime_contract::resolved::CatalogFingerprint,
    ) -> Option<ExecutableAgentSnapshot> {
        self.values.exact(workspace, fingerprint)
    }

    fn at_revision(
        &self,
        workspace: &str,
        agent: &AgentId,
        revision: u64,
    ) -> Option<ExecutableAgentSnapshot> {
        self.reads
            .lock()
            .unwrap()
            .push((workspace.into(), Some(revision)));
        self.values.at_revision(workspace, agent, revision)
    }
}

#[test]
fn publication_selection_uses_frozen_coordinates_and_selected_workspace_without_a_host() {
    // Causes: canonical/legacy coordinates; source/delivered/missing publication;
    // exact/wrong Agent, revision and backend; registered/fallback Workspace.
    // Each denial must precede any provider effect (this component owns none).
    for rule in [
        "exact",
        "legacy",
        "delivered",
        "agent",
        "revision",
        "backend",
        "missing_required",
        "optional_missing",
        "orphan_backend",
        "workspace",
    ] {
        let sessions = Arc::new(InstalledSessions::default());
        let source = Arc::new(Publications {
            values: StaticPublishedAgentSnapshots::try_new([
                publication("agent-a", 2),
                publication("agent-a", 3),
            ])
            .unwrap(),
            reads: Mutex::new(Vec::new()),
        });
        let missing = matches!(
            rule,
            "delivered" | "missing_required" | "optional_missing" | "orphan_backend"
        );
        let selection = SessionPublicationSelection::new(
            sessions.realizations(),
            "local-workspace".into(),
            (!missing).then(|| source.clone() as Arc<dyn PublishedAgentSnapshotSource>),
        );
        sessions
            .replace_frozen_with("session", |next| {
                next.agent_id = Some("agent-a".into());
                if !matches!(rule, "legacy" | "delivered" | "orphan_backend") {
                    let mut frozen_baseline = baseline(Some(2));
                    if rule == "missing_required" {
                        frozen_baseline.runtime_placement =
                            awaken_session_types::SessionRuntimePlacement::Worker;
                    }
                    next.baseline = Some(frozen_baseline);
                }
                if rule == "workspace" {
                    next.workspace = Some("session-workspace".into());
                }
                if matches!(rule, "backend" | "orphan_backend") {
                    next.backend_ref = Some("other".into());
                }
            })
            .expect("installed Sessions");
        let delivered = match rule {
            "delivered" => Some(publication("agent-a", 2)),
            "revision" => Some(publication("agent-a", 3)),
            _ => None,
        };
        let asserted = (rule == "agent").then_some("other-agent");
        let before = sessions
            .realizations()
            .frozen("session")
            .expect("installed Sessions");
        let result = selection.resolve_session_publication("session", asserted, delivered);
        let admitted = matches!(
            rule,
            "exact" | "legacy" | "delivered" | "optional_missing" | "workspace"
        );
        assert_eq!(result.is_ok(), admitted, "{rule}: {result:?}");
        assert!(
            Arc::ptr_eq(
                &before,
                &sessions
                    .realizations()
                    .frozen("session")
                    .expect("installed Sessions")
            ),
            "{rule}: a read never publishes"
        );
        if let Ok((workspace, agent, selected, candidate)) = result {
            assert_eq!(agent, "agent-a", "{rule}");
            assert_eq!(
                workspace,
                if rule == "workspace" {
                    "session-workspace"
                } else {
                    "local-workspace"
                }
            );
            assert_eq!(
                selected
                    .as_ref()
                    .map(|value| value.metadata.source.revision),
                match rule {
                    "optional_missing" => None,
                    "legacy" => Some(3),
                    _ => Some(2),
                },
                "{rule}"
            );
            assert_eq!(candidate.is_some(), rule != "optional_missing", "{rule}");
        }
        let reads = source.reads.lock().unwrap();
        if missing || rule == "agent" || rule == "revision" {
            assert!(
                reads.is_empty(),
                "{rule}: supplied publication never consults a newer head"
            );
        } else {
            assert_eq!(
                reads.as_slice(),
                &[(
                    if rule == "workspace" {
                        "session-workspace"
                    } else {
                        "local-workspace"
                    }
                    .into(),
                    (rule != "legacy").then_some(2),
                )],
                "{rule}"
            );
        }
    }
}

#[test]
fn publication_retention_is_one_atomic_installed_session_write_and_rejects_competing_candidates() {
    // R1 same publication is idempotent; R2 missing input cannot erase it;
    // R3 two distinct concurrent publications have one winner and one denial;
    // R4 no projection fields are lost while replacing its immutable value.
    let sessions = Arc::new(InstalledSessions::default());
    sessions
        .replace_frozen_with("session", |next| next.workspace = Some("tenant".into()))
        .expect("installed Sessions");
    let selection = Arc::new(SessionPublicationSelection::new(
        sessions.realizations(),
        "local-workspace".into(),
        None,
    ));
    let ready = Arc::new(Barrier::new(3));
    let workers: Vec<_> = [2, 3]
        .into_iter()
        .map(|revision| {
            let selection = selection.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                let candidate = publication("agent-a", revision);
                ready.wait();
                let accepted = selection
                    .retain_session_publication("session", Some(&candidate))
                    .is_ok();
                (candidate, accepted)
            })
        })
        .collect();
    ready.wait();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|(_, accepted)| *accepted).count(), 1);
    let winner = &outcomes.iter().find(|(_, accepted)| *accepted).unwrap().0;
    selection
        .retain_session_publication("session", Some(winner))
        .unwrap();
    selection
        .retain_session_publication("session", None)
        .unwrap();
    let frozen = sessions
        .realizations()
        .frozen("session")
        .expect("installed Sessions");
    assert_eq!(frozen.published_snapshot.as_ref(), Some(winner));
    assert_eq!(frozen.workspace.as_deref(), Some("tenant"));
    assert_eq!(
        selection
            .projected_session_model_provisioning("session", None)
            .unwrap(),
        *winner.resolved_spec.model_binding.provisioning()
    );
}

#[tokio::test]
async fn local_cold_task_reservation_uses_its_done_origin_bundle_without_current_catalog_reads() {
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::{RunId, ThreadId};
    use awaken_dispatch_contract::{
        DispatchClaims as _, DispatchEnqueue as _, DispatchSettlement as _,
        SessionRunAdmissionReads as _,
    };
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
    use awaken_session_execution_contract::ChildRunCoordination as _;
    // L1 ordinary None-version delegate was frozen at v1; today's source is v2
    // or unavailable; original queued Run is Done. L2 the real Local adapter
    // starts with no Runtime/Environment. => original v1 bundle through ctx
    // plan/resources/assembly/install AND the same reservation compiler, with
    // ZERO current/exact catalog calls and no Environment creation. L3 all
    // inherited restrictions/model/content-owner survive; current DenyAll is
    // an intersection. This bounded compiler test supplies trusted admission
    // data directly; held-Task authentication is owned by the real HTTP cases.
    for unavailable in [false, true] {
        for deny in [false, true] {
            let thread = format!("cold-task-source-{unavailable}-{deny}");
            let mut root = publication("agent-a", 2);
            root.resolved_spec
                .plugin_ids
                .push(awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.into());
            root.resolved_spec.plugin_config.agent.delegates.push(
                awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                    agent_id: AgentId("delegate".into()),
                    source_revision: None,
                    recursive_self: false,
                },
            );
            root.resolved_spec
                .model_candidates
                .push(ResolvedModelCandidate::host(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "provider",
                        "other-model",
                        "native",
                    )
                    .expect("fixed alternate model fixture"),
                ));
            root.recompute_fingerprint().unwrap();
            let v1 = publication("delegate", 1);
            let v2 = publication("delegate", 2);
            let initial = StaticPublishedAgentSnapshots::try_new([v1.clone()]).unwrap();
            let frozen = awaken_session_run_delivery::RUN_PUBLICATIONS
                .freeze(&root, Some(&initial), "workspace")
                .unwrap();
            assert_eq!(frozen, vec![v1.clone()], "L1 original None selection");
            let live = Arc::new(Publications {
                values: StaticPublishedAgentSnapshots::try_new(if unavailable {
                    Vec::new()
                } else {
                    vec![root.clone(), v2]
                })
                .unwrap(),
                reads: Mutex::new(Vec::new()),
            });
            let queue = Arc::new(awaken_dispatch::MemoryDispatchStore::new(
                awaken_time::system_clock(),
            ));
            let origin_run = RunId(format!("{thread}-origin"));
            let inherited = awaken_agent_contract::toolset::InheritedToolsets::with_caller(
                Vec::new(),
                Some(
                    &awaken_agent_contract::toolset::InheritedToolsets::with_caller(
                        Vec::new(),
                        None,
                    ),
                ),
            );
            let activation = RunActivation::new(
                origin_run.clone(),
                ThreadId(thread.clone()),
                root.clone(),
                vec![Message::text(
                    MessageId("old-source-input".into()),
                    Role::User,
                    "old input must not replay",
                )],
            )
            .with_inherited_toolsets(Some(inherited.clone()))
            .with_model_ref_override(Some("other-model".into()))
            .with_data_subject(Some(awaken_runtime_contract::DataSubjectId(
                "source-content-owner".into(),
            )));
            // L0 freeze intrinsic Task codec demand after the original inputs
            // and publications; Local claim and L1-L3 observations stay unchanged.
            let original = awaken_dispatch_contract::RunDispatch::new(activation.clone())
                .with_agent_publications(frozen.clone())
                .with_worker_codec_requirements();
            queue.enqueue(original).await.unwrap();
            let claim = queue
                .claim_run(&origin_run, "original-worker", 30_000, &Default::default())
                .await
                .unwrap()
                .unwrap();
            queue
                .settle(
                    &origin_run,
                    claim.lease.epoch(),
                    awaken_dispatch_contract::DispatchOutcome::Done,
                    &[],
                )
                .await
                .unwrap();
            let roots = crate::host::tests::ManagedSessionRootFixture::new().await;
            let host = roots
                .configure(
                    crate::SessionExecutorBuilder::ephemeral(
                        Arc::new(crate::no_model::NoModelConfiguredExecutor),
                        "stub",
                    )
                    .await
                    .with_agent_publications(live.clone())
                    .with_ephemeral_dispatch(Arc::new(
                        awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(
                            queue.clone(),
                        ),
                    )),
                )
                .build()
                .unwrap();
            // L2 the Managed Task plan requires a live background role even
            // though reservation does not invoke it. Retain the same strict
            // unused authority so any unexpected callback still fails the test.
            let application = crate::test_application_bindings::fresh();
            crate::test_application_bindings::select_background(&host, application.clone())
                .unwrap();
            let mut projection = crate::host::empty_frozen_projection_for_snapshot(
                host.local_workspace(),
                crate::host::eager_environment(),
                &root,
            );
            projection.agent_publication = Some(root);
            roots
                .install_projection(&host, &thread, projection)
                .await
                .unwrap();
            assert_eq!(
                host.run_assembly
                    .installed_sessions
                    .read(&thread, |installed| (
                        installed.realization.environment().owner.is_resident(),
                        installed.run.context.is_none()
                    ))
                    .unwrap(),
                Some((false, true)),
                "L2 cold precondition after setup"
            );
            live.reads.lock().unwrap().clear();
            let operation = "task-attention-operation";
            let run = awaken_session_execution_contract::session_run_id(
                &thread.as_str().into(),
                operation,
            );
            let message = Message::text(
                MessageId("new-task-attention".into()),
                Role::System,
                "Background task needs input",
            );
            let command = awaken_session_execution_contract::AdmitSessionRun {
                session_id:thread.as_str().into(), agent_id:"agent-a".into(), operation_id:operation.into(),run_id:run.clone(),messages:vec![message.clone()],
                data_subject_id:None,traceparent:None,
                execution_requirements:awaken_session_execution_contract::SessionRunExecutionRequirements { tool_capability_narrowing:if deny {ToolCapabilityNarrowing::DenyAll}else{ToolCapabilityNarrowing::Configured}, ..Default::default() },
                replacement:awaken_session_types::SessionRunReplacement::PreservePrior,
                private_task_input:Some(serde_json::from_value(serde_json::json!({"version":1,"observations":[],"tenure":{"thread":thread,"partition":{"kind":"thread"},"run":origin_run.0,"task":"task","holder":"holder","epoch":1}})).unwrap()),
            };
            assert_eq!(
                host.coordination
                    .reserve_session_run(command.clone())
                    .await
                    .unwrap(),
                awaken_session_execution_contract::SessionRunReservation::Reserved,
                "L2 real Local reserve"
            );
            let awaken_dispatch_contract::SessionRunAdmissionObservation::Live(reserved) =
                queue.observe_session_run_admission(&run).await.unwrap()
            else {
                panic!("L2 reserved carrying Run");
            };
            assert_eq!(
                reserved.agent_publications, frozen,
                "L1 None did not switch to published v2"
            );
            assert_eq!(
                reserved.activation.snapshot, activation.snapshot,
                "L1 original source snapshot"
            );
            assert_eq!(
                reserved.activation.inherited_toolsets(),
                Some(&inherited),
                "L3 complete chain retained"
            );
            assert_eq!(
                reserved.activation.tool_capability_narrowing(),
                if deny {
                    ToolCapabilityNarrowing::DenyAll
                } else {
                    ToolCapabilityNarrowing::ConfiguredWithInheritedToolsets
                },
                "L3 current ceiling intersects"
            );
            assert_eq!(
                reserved.activation.model_ref_override, activation.model_ref_override,
                "L3 model selector"
            );
            assert_eq!(
                reserved.activation.data_subject_id, activation.data_subject_id,
                "L3 actor/content owner"
            );
            assert_eq!(
                reserved.activation.input,
                vec![message],
                "L3 old input not replayed"
            );
            assert_eq!(
                reserved.session_command_fingerprint,
                Some(command.command_fingerprint()),
                "L3 original command owner"
            );
            assert!(
                live.reads.lock().unwrap().is_empty(),
                "L1/L2 zero catalog reads, including cold ctx"
            );
            assert_eq!(
                host.run_assembly
                    .installed_sessions
                    .read(&thread, |installed| (
                        installed.realization.environment().owner.is_resident(),
                        installed.run.context.is_none()
                    ))
                    .unwrap(),
                Some((false, true)),
                "L2 no Environment/new executable context"
            );
        }
    }
}
