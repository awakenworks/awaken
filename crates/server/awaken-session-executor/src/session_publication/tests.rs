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
