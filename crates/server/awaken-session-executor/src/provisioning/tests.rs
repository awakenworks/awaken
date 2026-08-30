
use super::*;
use crate::host::SharedHost;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
use awaken_sandbox_local::LocalProvider;

/// The logical path a staged resource realizes under, recovered from a projected
/// pc mount (`.mnt/<logical>`) so the registry assertions stay resource-oriented.
fn logical_of(m: &pc::MountRequirement) -> &str {
    m.mount_path.strip_prefix(".mnt/").unwrap_or(&m.mount_path)
}

/// Whether a projected Workdir spec denies tool egress.
fn denies(spec: &pc::SandboxSpec) -> bool {
    spec.deny_tool_egress
}

struct NoLlm;
#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        unreachable!("provisioning bookkeeping never calls the model")
    }
}

fn host() -> SharedHost {
    SharedHost::new(Arc::new(NoLlm), "test")
}

#[derive(Default)]
struct RecordingPublicationRealizer {
    calls: std::sync::Mutex<usize>,
    mismatched_receipt: bool,
}

#[async_trait::async_trait]
impl pc::RepositoryRealizer for RecordingPublicationRealizer {
    async fn realize_repository(
        &self,
        _plan: &pc::RepositoryRealizationPlan,
        _credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        unreachable!("publication helper never realizes a repository")
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        _credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        *self.calls.lock().unwrap() += 1;
        let mut receipt = pc::RepositoryPublicationReceipt::new(plan, expectation);
        if self.mismatched_receipt {
            receipt.repository_id.push_str("-wrong");
        }
        Ok(receipt)
    }
}

fn root_terminal_cleanup_command(
    session_id: &str,
) -> awaken_session_contract::SessionCleanupCommand {
    let mut operation = awaken_session_contract::SessionCleanupOperation::default();
    assert!(operation.request(session_id));
    operation.freeze_targets(session_id, [], 0, 0).unwrap();
    operation.command_for(session_id, session_id).unwrap()
}

#[test]
fn session_and_housekeeping_filesystem_continuity_are_distinct() {
    /* Continuity cause/effect table.
     * Causes: C1 the spec realizes the canonical Session environment; C2
     * the spec realizes a disposable child/probe environment. Effects: E1
     * request retained writable state for DurableRequest recovery; E2
     * request ephemeral state and therefore no configured continuation PVC.
     * Rules: SC1 C1=>E1; SC2 C2=>E2. The typed field participates in the
     * capacity identity, so the two requests cannot share warm capacity.
     */
    assert_eq!(
        host().sandbox_spec("session").filesystem_continuity,
        pc::FilesystemContinuity::Retained,
        "SC1"
    );
    assert_eq!(
        agent_run_sandbox_spec("probe").filesystem_continuity,
        pc::FilesystemContinuity::Ephemeral,
        "SC2"
    );
    assert_ne!(
        pc::SandboxCapacityShapeId::from_spec(&host().sandbox_spec("session")),
        pc::SandboxCapacityShapeId::from_spec(&agent_run_sandbox_spec("probe")),
        "SC1/SC2"
    );
}

/// A resource mount realized read-only under `.mnt/<logical>`.
fn resource_mount(logical: &str) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: format!("id-{logical}"),
        source: pc::MountSource::InlineBytes {
            contents: format!("content of {logical}").into_bytes(),
            content_hash: None,
        },
        mount_path: format!(".mnt/{logical}"),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    }
}

fn repository_activation(logical: &str) -> RepositoryActivation {
    RepositoryActivation {
        plan: pc::RepositoryRealizationPlan {
            repository_id: format!("id-{logical}"),
            mount_path: logical.to_string(),
            source_remote_url: "https://example.invalid/x.git".to_string(),
            transport_url: "https://example.invalid/x.git".to_string(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        },
        credential_pin: None,
    }
}

#[test]
fn register_replaces_the_threads_staged_set() {
    let host = host();
    host.register_thread_resources(
        "t",
        StagedResources {
            mounts: vec![resource_mount("a.md"), resource_mount("b.md")],
            prompts: vec!["first".into()],
            memory_prompts: Vec::new(),
            binding_checks: Vec::new(),
            repositories: vec![repository_activation("repo-a")],
        },
    );
    // A second register REPLACES (correct at create time, before any first Run).
    host.register_thread_resources(
        "t",
        StagedResources {
            mounts: vec![resource_mount("c.md")],
            prompts: vec!["second".into()],
            memory_prompts: Vec::new(),
            binding_checks: Vec::new(),
            repositories: Vec::new(),
        },
    );

    assert_eq!(host.sandbox_spec("t").mounts.len(), 1, "old mounts dropped");
    assert_eq!(host.thread_session_prompts("t"), vec!["second".to_string()]);
    assert!(
        host.thread_repository_activations("t").is_empty(),
        "old repository activation dropped"
    );
}

#[test]
fn sandbox_spec_carries_deny_egress_and_the_staged_mounts() {
    // Cause graph: a frozen restriction plus a provider without strict network
    // isolation selects the existing Workdir wrapper, not an unsupported
    // admission requirement.
    // | Rule | restriction | strict provider | network | deny wrapper |
    // | W1 | absent | no | unrestricted | no |
    // | W2 | none | no | unrestricted | yes |
    let host = host();
    // No registration and no egress: shared network, no mounts.
    let bare = host.sandbox_spec("t");
    assert!(!denies(&bare));
    assert!(bare.mounts.is_empty());

    host.install_environment_projection(
        "t",
        &awaken_session_contract::EnvironmentSnapshot {
            environment_id: "environment".into(),
            revision: awaken_session_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                "environment-1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::None,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        },
    )
    .expect("freeze test Environment");
    host.register_thread_resources(
        "t",
        StagedResources {
            mounts: vec![resource_mount("notes.md")],
            ..Default::default()
        },
    );
    let spec = host.sandbox_spec("t");
    assert!(denies(&spec), "the thread's deny-egress policy is carried");
    assert_eq!(
        spec.network,
        pc::NetworkPolicy::Unrestricted,
        "Workdir does not claim strict network isolation in admission"
    );
    assert_eq!(spec.mounts.len(), 1);
    assert_eq!(logical_of(&spec.mounts[0]), "notes.md");
}

#[tokio::test]
async fn repository_workspace_rejects_a_provider_with_split_tool_and_process_paths() {
    // Cause/effect graph: C1 Session has/has-not a Repository; C2 provider
    // has/has-not tool transparency plus sandbox-absolute path fidelity.
    // Effects: E1 repository demand is promoted to Namespace requirements;
    // E2 a Workdir provider is rejected before environment creation/clone;
    // E3 a no-repository Workdir Session retains its supported local tier.
    //
    // | Rule | Repository | Provider path contract | Effect |
    // |---|---|---|---|
    // | W1 | yes | split Workdir paths | E1+E2 |
    // | W2 | no | split Workdir paths | E3 |
    // | W3 | yes | transparent/fidelitous | admitted (covered by the real Namespace Hand test) |
    // Constraint/invariant: SandboxSpec plus provider capabilities are the
    // single admission authority; no Bash command rewriting or alias mount
    // is introduced as a second path mapping.
    let host = host();
    let local = crate::session_environment::SessionEnvironmentProvider::workdir(
        tempfile::tempdir().unwrap().path(),
    );
    let bare = host.sandbox_spec("bare");
    assert_eq!(bare.isolation, pc::IsolationClass::Workdir, "W2/E3");

    host.register_thread_resources(
        "repository-session",
        StagedResources {
            repositories: vec![repository_activation("repo")],
            ..Default::default()
        },
    );
    let repository_spec = host.sandbox_spec("repository-session");
    assert_eq!(
        repository_spec.isolation,
        pc::IsolationClass::Namespace,
        "W1/E1"
    );
    let error = match host
        .create_session_environment(&local, &repository_spec)
        .await
    {
        Ok(_) => panic!("W1/E2 Workdir cannot offer one /workspace path"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("one sandbox-absolute workspace path"),
        "W1/E2: {}",
        error.message
    );
}

#[tokio::test]
async fn realize_thread_repositories_fails_closed_on_an_unsafe_path() {
    // Managed repository Skill cause/effect rule R9. C1 the frozen mount is
    // unsafe (the provider rejects it before Git) or C2 repository
    // realization returns an error. E1 Session activation fails before
    // Skill-root selection, snapshot, or prompt publication. This existing
    // realization boundary is the only clone/fetch owner; Skill discovery
    // never adds another transport path. Decision rows R9a=C1=>E1 and
    // R9b=C2=>E1 exercise both causes through that same boundary.
    struct FailingRepositoryRealizer;

    #[async_trait::async_trait]
    impl pc::RepositoryRealizer for FailingRepositoryRealizer {
        async fn realize_repository(
            &self,
            _plan: &pc::RepositoryRealizationPlan,
            _credential: Option<&pc::RepositoryHttpBasicCredential>,
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::new("clone failed"))
        }

        async fn publish_repository(
            &self,
            _plan: &pc::RepositoryRealizationPlan,
            _expectation: &pc::RepositoryPublicationExpectation,
            _credential: Option<&pc::RepositoryHttpBasicCredential>,
        ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
            unreachable!("R9 tests realization only")
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let env = crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(tmp.path())
            .create_sandbox(&agent_run_sandbox_spec("s"))
            .await
            .unwrap(),
    );
    let host = host();
    host.register_thread_resources(
        "t",
        StagedResources {
            repositories: vec![RepositoryActivation {
                plan: pc::RepositoryRealizationPlan {
                    repository_id: "repo-escape".into(),
                    mount_path: "../escape".into(),
                    source_remote_url: "https://example.invalid/x.git".into(),
                    transport_url: "https://example.invalid/x.git".into(),
                    initial_branch: None,
                    initial_commit: None,
                    access: pc::MountAccess::ReadWrite,
                },
                credential_pin: None,
            }],
            ..Default::default()
        },
    );
    let err = host.realize_thread_repositories("t", &env).await;
    assert!(
        err.is_err(),
        "R9a unsafe repo mount must abort Session start"
    );

    host.register_thread_resources(
        "clone-failure",
        StagedResources {
            repositories: vec![repository_activation("repo")],
            ..Default::default()
        },
    );
    let error = host
        .realize_thread_repositories("clone-failure", &FailingRepositoryRealizer)
        .await
        .expect_err("R9b clone failure must abort Session start");
    assert!(error.message.contains("clone failed"), "R9b/E1: {error:?}");
}

#[tokio::test]
async fn explicit_repository_publication_uses_one_frozen_activation_and_verifies_receipt() {
    /* Publication-helper cause/effect table.
     * Causes: C1 activation is read/write; C2 expectation has a valid exact
     * coordinate; C3 adapter receipt is canonical. Effects: E1 invoke exactly
     * one Realizer effect; E2 return the verified receipt; E3 reject before
     * any adapter call; E4 reject mismatched evidence. Rules: H1 C1+C2+C3 =>
     * E1+E2; H2 !C1=>E3; H3 !C2=>E3; H4 C1+C2+!C3=>E1+E4. The caller supplies
     * the activation compiled from its frozen ResolvedInput; no thread Resource
     * lookup or publish-all loop exists in this helper.
     */
    let host = host();
    let repository = repository_activation("repo");
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        expected_prior_commit: None,
    };
    let realizer = RecordingPublicationRealizer::default();
    let receipt = host
        .publish_repository_activation("thread", &repository, &[], &realizer, &expectation, None)
        .await
        .expect("H1");
    receipt.verify(&repository.plan, &expectation).unwrap();
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H1/E1");

    let readonly = RepositoryActivation {
        plan: pc::RepositoryRealizationPlan {
            access: pc::MountAccess::ReadOnly,
            ..repository.plan.clone()
        },
        credential_pin: None,
    };
    let error = host
        .publish_repository_activation("thread", &readonly, &[], &realizer, &expectation, None)
        .await
        .expect_err("H2");
    assert!(
        matches!(
            error,
            RepositoryPublicationActivationError::Failed(error)
                if error.message.contains("read-only")
        ),
        "H2/E3"
    );
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H2 no effect");

    let invalid = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: "short".into(),
        expected_prior_commit: None,
    };
    host.publish_repository_activation("thread", &repository, &[], &realizer, &invalid, None)
        .await
        .expect_err("H3");
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H3 no effect");

    let mismatched = RecordingPublicationRealizer {
        mismatched_receipt: true,
        ..Default::default()
    };
    let error = host
        .publish_repository_activation("thread", &repository, &[], &mismatched, &expectation, None)
        .await
        .expect_err("H4");
    assert!(
        matches!(
            error,
            RepositoryPublicationActivationError::Failed(error)
                if error.message.contains("does not match")
        ),
        "H4/E4"
    );
    assert_eq!(*mismatched.calls.lock().unwrap(), 1, "H4/E1");
}

#[tokio::test]
async fn reverse_channels_are_safe_noops_without_a_live_session() {
    let host = host();
    // Stage a repo, but never create a session for the thread: reverse channels
    // must early-return (no live env), not panic. Memory write-through is owned
    // by the MemoryMount guard and therefore has no Host-side reverse channel.
    host.register_thread_resources(
        "t",
        StagedResources {
            repositories: vec![repository_activation("r")],
            ..Default::default()
        },
    );
    host.harvest_thread_skills("t").await.unwrap(); // no env / no store → no persist
    assert!(
        host.harvest_thread_artifacts("t")
            .await
            .unwrap()
            .receipts
            .is_empty()
    );
    assert!(
        host.harvest_thread_artifacts("never-seen")
            .await
            .unwrap()
            .receipts
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_release_harvests_outputs_idempotently_before_sandbox_disposal() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_session_contract::SessionRuntime;
    // Cause/effect decision table:
    // R1 output present + live Sandbox => harvest creates one scoped File.
    // R2 identical retry => same File id, no duplicate manifest row/reference.
    // R3 terminal release => Sandbox gone while File metadata/bytes remain.
    let storage = tempfile::tempdir().unwrap();
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    host.register_thread_workspace("session-artifacts", "workspace-a");
    let spec = agent_run_sandbox_spec("session-artifacts");
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(storage.path())
            .create_sandbox(&spec)
            .await
            .unwrap(),
    ));
    host.install_test_resident_session_environment("session-artifacts", environment.clone());
    let output = storage
        .path()
        .join("session-artifacts")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"durable report").unwrap();

    let first = host
        .harvest_thread_artifacts("session-artifacts")
        .await
        .unwrap();
    let retry = host
        .harvest_thread_artifacts("session-artifacts")
        .await
        .unwrap();
    assert_eq!(first.receipts.len(), 1);
    assert_eq!(retry.receipts[0].record.id, first.receipts[0].record.id);
    assert!(first.receipts[0].record.id.starts_with("file_"));
    assert!(first.receipts[0].record.downloadable);

    crate::ManagedHost::new(host.clone())
        .execute_terminal_cleanup(root_terminal_cleanup_command("session-artifacts"))
        .await
        .unwrap();
    assert!(
        host.session_environment("session-artifacts")
            .await
            .is_none()
    );
    let records = host
        .file_application()
        .expect("test startup installs File application")
        .list("workspace-a", Some("session-artifacts"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1, "terminal retry remains idempotent");
    assert_eq!(
        host.file_application()
            .expect("test startup installs File application")
            .bytes("workspace-a", &records[0].id)
            .await
            .unwrap()
            .unwrap()
            .1,
        b"durable report"
    );
}

#[tokio::test]
async fn artifact_harvest_has_one_file_authority_and_no_magic_bundle_gate() {
    // Cause/effect decision table: C1 an output path happens to be named
    // `skill-export/change.patch`; C2 no manifest or checksum sibling exists;
    // C3 the same immutable bytes are harvested again. Effects: E1 the File
    // aggregate publishes the exact bytes without interpreting the directory;
    // E2 terminal harvest is not blocked by a second bundle-completion rule;
    // E3 replay returns the same File identity. R1 C1+C2=>E1+E2;
    // R2 C1+C2+C3=>E1+E2+E3.
    let storage = tempfile::tempdir().unwrap();
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    host.register_thread_workspace("plain-files", "workspace-a");
    let spec = agent_run_sandbox_spec("plain-files");
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(storage.path())
            .create_sandbox(&spec)
            .await
            .unwrap(),
    ));
    host.install_test_resident_session_environment("plain-files", environment);
    let output = storage
        .path()
        .join("plain-files")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("skill-export")
        .join("change.patch");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"diff --git a/a b/a\n").unwrap();

    let first = host
        .harvest_thread_artifacts("plain-files")
        .await
        .expect("R1/E1+E2");
    let replay = host
        .harvest_thread_artifacts("plain-files")
        .await
        .expect("R2/E1+E2+E3");
    assert_eq!(first.receipts.len(), 1, "R1/E1");
    assert_eq!(replay.receipts.len(), 1, "R2/E3");
    assert_eq!(
        replay.receipts[0].record.id, first.receipts[0].record.id,
        "R2/E3"
    );
}

struct FailingFileCatalog;

use awaken_resource_contract::CreateFileRecordOutcome;

#[async_trait::async_trait]
impl FileCatalog for FailingFileCatalog {
    async fn create_file(
        &self,
        _record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        Err(FileCatalogError::Storage("injected catalog failure".into()))
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(None)
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Ok(Vec::new())
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(None)
    }

    async fn active_size_bytes(&self, _workspace_id: &str) -> Result<u64, FileCatalogError> {
        Ok(0)
    }
}

#[tokio::test]
async fn terminal_harvest_failure_preserves_the_sandbox_for_retry() {
    use awaken_session_contract::SessionRuntime;

    // Test design. Causes: R4 has terminal output present while its durable
    // catalog write fails. Effects: end_session fails and preserves both
    // Environment and output for retry. Constraint/Invariant: sandbox disposal
    // follows successful durable harvest, never precedes it. Decision rule:
    // execute R4 and require failure with zero disposal.
    let storage = tempfile::tempdir().unwrap();
    let mut raw_host = SharedHost::new(Arc::new(NoLlm), "test");
    let catalog = Arc::new(FailingFileCatalog);
    raw_host.file_catalog = catalog.clone();
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        raw_host.file_store(),
        catalog,
        raw_host
            .resource_reclamation()
            .expect("test lifecycle repository"),
    ));
    raw_host = raw_host.with_file_application(
        application.clone(),
        Arc::new(
            awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
        ),
        Arc::new(awaken_resource_application::ApplicationArtifactPublisher::new(application)),
    );
    let host = Arc::new(raw_host);
    let spec = agent_run_sandbox_spec("session-harvest-failure");
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(storage.path())
            .create_sandbox(&spec)
            .await
            .unwrap(),
    ));
    host.install_test_resident_session_environment("session-harvest-failure", environment);
    let output = storage
        .path()
        .join("session-harvest-failure")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"retry me").unwrap();

    let error = crate::ManagedHost::new(host.clone())
        .execute_terminal_cleanup(root_terminal_cleanup_command("session-harvest-failure"))
        .await
        .unwrap_err();
    assert!(error.message.contains("injected catalog failure"));
    assert!(
        host.session_environment("session-harvest-failure")
            .await
            .is_some()
    );
    assert_eq!(std::fs::read(output).unwrap(), b"retry me");
}

#[tokio::test]
async fn harvest_persists_an_agent_authored_skill_to_the_durable_catalog() {
    // Hermes-style self-authoring (ADR-0036 D6/D8): a skill the agent writes under the
    // workspace this run must be harvested into the durable catalog so the next session
    // delivers it — the skill analogue of memory write-back.
    let dir = std::env::temp_dir().join(format!("awaken-skillharvest-{}", std::process::id()));
    let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));

    // A real sandbox env with a skill authored under the workspace `skills/` dir.
    let base = dir.join("sbx");
    let env = crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(&base)
            .create_sandbox(&agent_run_sandbox_spec("t"))
            .await
            .unwrap(),
    );
    let skill_dir = base.join("t").join("skills").join("notes");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: authored this run\n---\nremember to hydrate",
    )
    .unwrap();

    // The catalog is empty until the run-authored skill is harvested; after harvest it
    // holds the skill, addressable for delivery to the next session.
    assert!(
        host.skills
            .definitions(host.local_workspace())
            .await
            .unwrap()
            .is_empty()
    );
    host.persist_authored_skills(host.local_workspace(), &env)
        .await
        .unwrap();
    let ids = host
        .skills
        .definitions(host.local_workspace())
        .await
        .unwrap()
        .into_iter()
        .map(|definition| definition.id)
        .collect::<Vec<_>>();
    assert!(
        ids.iter().any(|id| id.as_str().contains("notes")),
        "the authored skill must be persisted to the durable catalog: {ids:?}"
    );

    env.dispose().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_durable_skill_is_advertised_by_a_resolvable_catalog_id() {
    // Cause/effect rule V1: C1 a Skill is persisted into the versioned
    // catalog => E1 Managed advertisement contains its stable resource id
    // and E2 the corresponding frozen bytes remain loadable. Constraint:
    // display names and host-static specs cannot satisfy E2, so they cannot
    // enter this Managed id set. The static exclusion row is covered by
    // `managed_session_folds_builtins_into_the_agent_toolset`.
    let dir = std::env::temp_dir().join(format!("awaken-skillid-{}", std::process::id()));
    let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));
    host.skills
        .persist_authored(
            host.local_workspace(),
            "Greeter",
            "---\nname: Greeter\ndescription: hi\n---\nsay hi",
        )
        .await;

    let cid = "Greeter".to_string();
    assert_eq!(
        host.skills.managed_ids_in(host.local_workspace()),
        vec![cid.clone()],
        "Managed advertisement contains only the version-backed catalog id"
    );

    let version = host
        .skills
        .cache_snapshot_in(host.local_workspace())
        .into_iter()
        .find(|version| version.skill_id.as_str() == cid)
        .expect("the advertised resource id must resolve to the Skill version");
    assert!(version.skill_md().unwrap().ends_with(b"say hi"));
    assert!(
        host.skills
            .cache_snapshot_in(host.local_workspace())
            .into_iter()
            .find(|version| version.skill_id.as_str() == "skill_deadbeefdeadbeef")
            .is_none()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
