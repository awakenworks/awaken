//! Claim-fenced Memory Coordinator tests over real HTTP.

use awaken_dispatch_testkit::worker_http as support;

use std::sync::Arc;

use awaken_memory_store::{MemErr, MemoryRepository as _};
use awaken_provisioning_contract::{MemoryMaterializationEvidence, MemoryMaterializationHead};
use awaken_resource_contract::{
    ChangeMemoryStoreState, ConfigVersion, MemoryMaterializationReferenceEncoder,
    MemoryStoreConfigVersion, MemoryStoreDefinition, RegisterMemoryStore, ResourceAccess,
    ResourceAdministration as _, ResourceState,
};
use awaken_resource_worker_http::{
    HttpMemoryRepository, WorkerMemoryService, worker_memory_router,
};
use awaken_resource_worker_http::{
    memory_materialization_reference, terminal_memory_materialization_reference,
};
use awaken_dispatch::{DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch};
use awaken_session_realization_contract::{
    SessionRealizationControlFailure, SessionResourceMaterializationIntent,
};
use awaken_session_state_contract::{SessionTerminalMemoryIntent, SessionTerminalMemoryTarget};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

fn memory_input(
    binding: &str,
    store: &str,
    access: ResourceAccess,
) -> awaken_session_resource_contract::ResolvedInput {
    awaken_session_resource_contract::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::new(binding),
        source: awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: store.into(),
            config: MemoryStoreConfigVersion {
                memory_store_id: store.into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: format!("/memory/{binding}"),
        access,
        instructions: None,
    }
}

async fn create_store(catalog: &awaken_resource_application::RegistryApplication, id: &str) {
    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: id.into(),
                workspace_id: "workspace-memory".into(),
                name: id.into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: MemoryStoreConfigVersion {
                memory_store_id: id.into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        })
        .await
        .expect("register test MemoryStore");
}

struct ExactTerminalMemoryControl {
    workspace_id: String,
    intent: SessionTerminalMemoryIntent,
    current_effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
}

// Keep File/Skill renewal wire coverage beside Memory to reuse the same exact
// Control double. No transport-private Session or alternate expiry authority.
#[tokio::test]
async fn immutable_file_and_skill_reads_follow_live_root_renewal_over_http() {
    // Decision table: C1 registered Worker and exact target; C2 asserted lease
    // already expired; C3 current root is same-generation renewed or expired;
    // C4 endpoint is File or Skill. R1=C1+C2+renewed(C3)+C4 => exact bytes;
    // R2=C1+C2+expired(C3)+C4 => 409 with no bytes. The real Session-root
    // matrix separately covers owner/epoch/revision/target substitution; here
    // the actual HTTP routers must reach Control instead of rejecting C2 first.
    use awaken_file_store::FileStore as _;
    use awaken_resource_contract::{FileCatalog as _, FileContentSource as _, FileReadPurpose};
    use awaken_resource_worker_http::{
        HttpFileContentSource, HttpSkillBundleSource, WorkerFileContentService,
        WorkerSkillBundleService, worker_file_content_router, worker_skill_bundle_router,
    };
    use awaken_session_configuration_contract::SkillBundleSource as _;
    use awaken_skill_store::SkillStore as _;
    let (directory, identity) = support::ready_worker("worker-renewed-resources").await;
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        epoch: 7,
        expires_at_unix_ms: 1,
    };
    let file_store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let digest = file_store.put(b"exact input").await.unwrap();
    file_store
        .create_file(awaken_resource_contract::FileRecord {
            id: "renewed-file".into(),
            workspace_id: "workspace-memory".into(),
            blob_id: digest,
            filename: "input.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: 11,
            created_at: "2026-09-06T00:00:00Z".into(),
            expires_at: None,
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
    let file_application = Arc::new(awaken_resource_application::FileApplication::new(
        file_store.clone(),
        file_store,
        Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap()),
        awaken_time::system_clock(),
    ));
    let mut file_input = memory_input("file", "unused", ResourceAccess::ReadOnly);
    file_input.source = awaken_session_resource_contract::ResolvedInputSource::File {
        file_id: "renewed-file".into(),
    };
    let file_intent = SessionResourceMaterializationIntent::try_new(
        "renewed-session",
        1,
        file_input,
        lease.clone(),
    )
    .unwrap();
    let skill_store = Arc::new(awaken_skill_store::InMemorySkillStore::new());
    let files = vec![awaken_skill_store::SkillBundleFile {
        path: "SKILL.md".into(),
        content: b"exact skill".to_vec(),
        executable: false,
    }];
    let version = awaken_skill_store::SkillVersion {
        id: "renewed-skill-v1".into(),
        skill_id: "renewed-skill".into(),
        version: 1,
        name: "renewed-skill".into(),
        description: "test".into(),
        directory: "/skills/test".into(),
        bundle_sha256: awaken_skill_store::bundle_sha256(&files),
        files,
        created_unix_nanos: 1,
    };
    skill_store
        .create(
            awaken_skill_store::SkillDefinition {
                id: version.skill_id.clone(),
                workspace_id: "workspace-memory".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            version.clone(),
        )
        .await
        .unwrap();
    let binding = awaken_session_resource_contract::ResolvedSkillBinding {
        kind: awaken_agent_contract::AgentSkillKind::Custom,
        skill_id: version.skill_id.to_string(),
        version: 1,
        bundle_sha256: version.bundle_sha256.clone(),
    };
    let skill_intent = SessionResourceMaterializationIntent::try_new_skill(
        "renewed-session",
        1,
        binding.clone(),
        lease.clone(),
    )
    .unwrap();
    for current_expiry in [u64::MAX, 1] {
        let control = |intent| {
            Arc::new(ExactResourceControl {
                workspace_id: "workspace-memory".into(),
                intent,
                current_lease: awaken_session_resource_contract::SessionRealizationLease {
                    expires_at_unix_ms: current_expiry,
                    ..lease.clone()
                },
            })
        };
        let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
        let file_service = WorkerFileContentService::new(
            Arc::new(
                awaken_resource_application::ApplicationFileContentSource::new(
                    file_application.clone(),
                ),
            ),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            awaken_time::system_clock(),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(control(file_intent.clone()));
        let skill_service = WorkerSkillBundleService::new(
            Arc::new(awaken_resource_application::StoreSkillBundleSource::new(
                skill_store.clone(),
            )),
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
            awaken_time::system_clock(),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(control(skill_intent.clone()));
        let address = support::serve(
            worker_file_content_router(Arc::new(file_service))
                .merge(worker_skill_bundle_router(Arc::new(skill_service))),
        )
        .await;
        let upstream =
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone());
        let file = HttpFileContentSource::new(upstream.clone())
            .read(
                "workspace-memory",
                "renewed-file",
                &FileReadPurpose::SessionResource,
                Some(
                    &awaken_dispatch::ResourceOperationFence::SessionMaterialization(
                        file_intent.clone(),
                    ),
                ),
            )
            .await;
        let skill = HttpSkillBundleSource::new(upstream)
            .load(
                "workspace-memory",
                &binding,
                Some(
                    &awaken_dispatch::ResourceOperationFence::SessionMaterialization(
                        skill_intent.clone(),
                    ),
                ),
            )
            .await;
        if current_expiry == u64::MAX {
            assert_eq!(file.expect("R1 File").unwrap().bytes, b"exact input");
            assert_eq!(skill.expect("R1 Skill").unwrap(), version);
        } else {
            assert!(file.expect_err("R2 File").to_string().contains("409"));
            assert!(skill.expect_err("R2 Skill").to_string().contains("409"));
        }
    }
}

struct ExactResourceControl {
    workspace_id: String,
    intent: SessionResourceMaterializationIntent,
    current_lease: awaken_session_resource_contract::SessionRealizationLease,
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionResourceMaterializationControl
    for ExactResourceControl
{
    async fn authorize_resource_materialization_intent(
        &self,
        intent: &SessionResourceMaterializationIntent,
    ) -> Result<String, SessionRealizationControlFailure> {
        if intent.session_id() != self.intent.session_id()
            || intent.resource_revision() != self.intent.resource_revision()
            || intent.target() != self.intent.target()
            || !awaken_session_realization_contract::realization_lease_authorizes(
                &self.current_lease,
                intent.lease(),
                support::unix_now_ms(),
            )
        {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        Ok(self.workspace_id.clone())
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalMemoryControl
    for ExactTerminalMemoryControl
{
    async fn authorize_terminal_memory_intent(
        &self,
        intent: &SessionTerminalMemoryIntent,
    ) -> Result<SessionTerminalMemoryTarget, SessionRealizationControlFailure> {
        if intent != &self.intent
            || self.current_effect.command != intent.effect().command
            || !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &self.current_effect.lease,
                &intent.effect().lease,
            )
            || !awaken_session_realization_contract::realization_lease_is_live_at(
                self.current_effect.lease.expires_at_unix_ms,
                support::unix_now_ms(),
            )
        {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        SessionTerminalMemoryTarget::from_authorized_root(self.workspace_id.clone(), intent.clone())
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))
    }
}

/// Cause/effect decision table:
/// | Rule | identity/claim | frozen binding | access | CAS base | Effect |
/// |---|---|---|---|---|---|
/// | M1 | current/live | exact store/config | read-write | n/a | return one atomic snapshot |
/// | M2 | current/live | exact store/config | read-write | current | create/update canonical repository |
/// | M3 | current/live | exact store/config | read-write | stale | typed conflict; preserve durable head |
/// | M4 | current/live | exact store/config | read-only | n/a | deny write before repository mutation |
/// | M5 | current/live | wrong Workspace | any | n/a | deny before repository access |
/// | M6 | current/stale | exact store/config | any | n/a | reject after claim settlement |
/// | M7 | current/live | exact store/config | read-write | n/a | reject rename outside the snapshot/CAS boundary |
/// | M8 | current/live | archived store | any | n/a | typed not-active denial; no repository read |
#[tokio::test]
async fn memory_snapshot_and_writeback_are_exact_claim_and_cas_fenced() {
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    repository
        .create("memory-rw", "/seed.md", "seed")
        .await
        .unwrap();
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open test Resource Registry"),
    );
    let catalog = Arc::new(awaken_resource_application::RegistryApplication::new(
        storage,
        awaken_time::system_clock(),
    ));
    create_store(&catalog, "memory-rw").await;
    create_store(&catalog, "memory-ro").await;
    let resources = awaken_session_resource_contract::ResolvedSessionResources::try_new(
        vec![
            memory_input("rw", "memory-rw", ResourceAccess::ReadWrite),
            memory_input("ro", "memory-ro", ResourceAccess::ReadOnly),
        ],
        Vec::new(),
    )
    .unwrap();
    let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    dispatch
        .enqueue(
            RunDispatch::new(support::activation("memory"))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-memory"),
                ))
                .with_session_resources(awaken_dispatch::SessionResourceEnvelope::new(
                    "workspace-memory",
                    serde_json::to_string(&resources).unwrap(),
                )),
        )
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-memory").await;
    let claimed = dispatch
        .claim(&identity.lease_owner(), 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let claim = RunClaim::from(&claimed.lease);
    let service = Arc::new(WorkerMemoryService::new(
        repository.clone(),
        catalog.clone(),
        dispatch.clone(),
        Arc::new(HeaderWorkerAuthenticator),
        directory,
        awaken_time::system_clock(),
    ));
    let address = support::serve(worker_memory_router(service)).await;
    let client = Arc::new(HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    ));
    let rw = memory_materialization_reference(
        "workspace-memory",
        "memory-rw",
        ConfigVersion::INITIAL,
        ResourceAccess::ReadWrite,
        &claim,
    )
    .unwrap();

    let snapshot = client.snapshot_heads(&rw).await.expect("M1");
    // RC-W1 Run RW is still NOT terminal retention authority.
    let proposed = awaken_resource_contract::MemoryCopyConflict {
        path: "/new.md".into(),
        base: None,
        content: Some("candidate".into()),
    };
    assert!(
        client
            .preserve_copy_conflict(&rw, &proposed)
            .await
            .unwrap_err()
            .to_string()
            .contains("403")
    );
    assert!(
        repository
            .list_copy_conflicts("memory-rw")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(snapshot.len(), 1, "M1");
    let created = client.create(&rw, "/new.md", "v1").await.expect("M2");
    let updated = client
        .update(&rw, &created.generation(), "v2")
        .await
        .expect("M2");
    assert_eq!(updated.content.as_deref(), Some("v2"), "M2");

    let conflict = client
        .update(&rw, &created.generation(), "stale")
        .await
        .expect_err("M3");
    assert!(matches!(conflict, MemErr::Conflict { .. }), "M3");
    assert_eq!(
        repository
            .get_by_path("memory-rw", "/new.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "M3"
    );

    // MA attribution: C1 the caller states an author on a write that crosses
    // this transport; C2 the caller states none. Effects: E1 the author reaches
    // the backing repository's version history unchanged; E2 an unattributed
    // write records no author, so the field says what happened rather than
    // being filled in. Before the writeback operation carried an author there
    // was no field at all, and every attributed write over this transport
    // arrived anonymous.
    let actor = awaken_resource_contract::MemoryActor::UserActor {
        user_id: "user-11".into(),
    };
    let attributed = client
        .create_as(&rw, "/attributed.md", "v1", Some(&actor))
        .await
        .expect("MA/C1 create");
    client
        .update_head_as(&rw, &attributed.generation(), "v2", None, Some(&actor))
        .await
        .expect("MA/C1 update");
    client
        .create(&rw, "/anonymous.md", "v1")
        .await
        .expect("MA/C2 create");
    let versions = repository.list_versions("memory-rw").await.unwrap();
    let authors = |path: &str| -> Vec<Option<awaken_resource_contract::MemoryActor>> {
        versions
            .iter()
            .filter(|version| version.path == path)
            .map(|version| version.created_by.clone())
            .collect()
    };
    assert_eq!(
        authors("/attributed.md"),
        vec![Some(actor.clone()), Some(actor.clone())],
        "MA/E1 both attributed writes reached history with their author"
    );
    assert_eq!(
        authors("/anonymous.md"),
        vec![None],
        "MA/E2 an unattributed write records no author"
    );

    // MP-RUN: unlike terminal requests, a Run reference has no captured heads.
    // The Copy client must transmit its expected path; a rename between client
    // snapshot and server transaction must not turn this into ID-following IO.
    let copied = client.create(&rw, "/copy.md", "base").await.unwrap();
    client
        .update_materialized(&rw, "/copy.md", &copied.generation(), "edit")
        .await
        .unwrap();
    repository
        .rename("memory-rw", "/copy.md", "/copy-moved.md")
        .await
        .unwrap();
    let heads = repository.snapshot_heads("memory-rw").await.unwrap();
    let history = repository.list_versions("memory-rw").await.unwrap();
    assert!(matches!(
        client
            .update_materialized(&rw, "/copy.md", &copied.generation(), "edit")
            .await,
        Err(MemErr::Conflict { .. })
    ));
    assert_eq!(repository.snapshot_heads("memory-rw").await.unwrap(), heads);
    assert_eq!(
        repository.list_versions("memory-rw").await.unwrap(),
        history
    );

    let ro = memory_materialization_reference(
        "workspace-memory",
        "memory-ro",
        ConfigVersion::INITIAL,
        ResourceAccess::ReadOnly,
        &claim,
    )
    .unwrap();
    assert!(client.create(&ro, "/denied.md", "no").await.is_err(), "M4");
    assert!(
        repository
            .get_by_path("memory-ro", "/denied.md")
            .await
            .unwrap()
            .is_none(),
        "M4"
    );

    let wrong_workspace = memory_materialization_reference(
        "workspace-other",
        "memory-rw",
        ConfigVersion::INITIAL,
        ResourceAccess::ReadWrite,
        &claim,
    )
    .unwrap();
    assert!(client.snapshot_heads(&wrong_workspace).await.is_err(), "M5");

    assert!(
        client.rename(&rw, "/seed.md", "/renamed.md").await.is_err(),
        "M7"
    );

    catalog
        .change_memory_store_state(ChangeMemoryStoreState {
            workspace_id: "workspace-memory".into(),
            id: "memory-rw".into(),
            state: ResourceState::Archived,
        })
        .await
        .expect("archive test MemoryStore");
    let archived = client.snapshot_heads(&rw).await.expect_err("M8");
    assert!(
        archived.to_string().contains("not active"),
        "M8: {archived}"
    );

    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_dispatch::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    assert!(client.snapshot_heads(&rw).await.is_err(), "M6");
}

/// Session-realization Memory cause/effect graph: C1 the authenticated Worker
/// owns the asserted realization generation; C2 Control matches the complete
/// intent to the pending Session root and its live renewed lease; C3 the
/// operation is snapshot or write. The assertion's original expiry is already
/// past: only Control can observe its same-generation renewal, so the HTTP
/// identity boundary must not reject it before that authoritative check.
/// Effects: E1 one read-only snapshot through the canonical Memory repository;
/// E2 stale generation conflict; E3 foreign Worker denial; E4 all writes denied
/// before repository mutation. Registry validation remains after root authority.
///
/// | Rule | C1 | C2 | C3 | Effect |
/// |---|---|---|---|---|
/// | R1 | yes | yes | snapshot | E1 |
/// | R2 | yes | no | any | E2 |
/// | R3 | no | any | any | E3 |
/// | R4 | yes | yes | write | E4 |
#[tokio::test]
async fn realization_memory_is_read_only_and_fenced_by_worker_and_session_root() {
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    repository
        .create("memory-realization", "/seed.md", "seed")
        .await
        .expect("seed realization Memory");
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open realization Resource Registry"),
    );
    let catalog = Arc::new(awaken_resource_application::RegistryApplication::new(
        storage,
        awaken_time::system_clock(),
    ));
    create_store(&catalog, "memory-realization").await;
    let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let (directory, identity) = support::ready_worker("worker-realization-memory").await;
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        epoch: 13,
        expires_at_unix_ms: 1,
    };
    let input = memory_input(
        "realization",
        "memory-realization",
        ResourceAccess::ReadOnly,
    );
    let intent = SessionResourceMaterializationIntent::try_new(
        "session-realization-memory",
        3,
        input.clone(),
        lease.clone(),
    )
    .expect("exact realization Memory intent");
    let service = Arc::new(
        WorkerMemoryService::new(
            repository.clone(),
            catalog,
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
            directory,
            awaken_time::system_clock(),
        )
        .with_materialization_control(Arc::new(ExactResourceControl {
            workspace_id: "workspace-memory".into(),
            intent: intent.clone(),
            current_lease: awaken_session_resource_contract::SessionRealizationLease {
                expires_at_unix_ms: u64::MAX,
                ..lease.clone()
            },
        })),
    );
    let address = support::serve(worker_memory_router(service)).await;
    let client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );
    let encoder = awaken_resource_worker_http::HttpMemoryMaterializationReferenceEncoder;
    let reference = encoder
        .encode(
            "workspace-memory",
            "memory-realization",
            ConfigVersion::INITIAL,
            ResourceAccess::ReadOnly,
            &intent,
        )
        .expect("encode exact realization Memory reference");
    let heads = client
        .snapshot_heads(&reference)
        .await
        .expect("R1/E1 exact snapshot");
    assert_eq!(heads.len(), 1, "R1/E1");
    // RC-W2 realization authority may read but cannot retain candidates.
    let proposed = awaken_resource_contract::MemoryCopyConflict {
        path: "/new.md".into(),
        base: None,
        content: Some("candidate".into()),
    };
    assert!(
        client
            .preserve_copy_conflict(&reference, &proposed)
            .await
            .unwrap_err()
            .to_string()
            .contains("403")
    );
    assert!(
        repository
            .list_copy_conflicts("memory-realization")
            .await
            .unwrap()
            .is_empty()
    );

    let write_error = client
        .create(&reference, "/must-not-exist.md", "forbidden")
        .await
        .expect_err("R4/E4 realization reference is snapshot-only");
    assert!(
        write_error.to_string().contains("403"),
        "R4/E4: {write_error}"
    );
    assert!(
        repository
            .get_by_path("memory-realization", "/must-not-exist.md")
            .await
            .expect("read repository after denied write")
            .is_none(),
        "R4/E4 no mutation"
    );

    let stale_intent = SessionResourceMaterializationIntent::try_new(
        "session-realization-memory",
        4,
        input,
        lease,
    )
    .expect("stale generation remains structurally valid");
    let stale_reference = encoder
        .encode(
            "workspace-memory",
            "memory-realization",
            ConfigVersion::INITIAL,
            ResourceAccess::ReadOnly,
            &stale_intent,
        )
        .expect("encode stale realization Memory reference");
    let stale_error = client
        .snapshot_heads(&stale_reference)
        .await
        .expect_err("R2/E2 stale root generation");
    assert!(
        stale_error.to_string().contains("409"),
        "R2/E2: {stale_error}"
    );

    let foreign_client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
            awaken_worker_contract::WorkerIdentity::new("foreign-worker", "foreign-boot", 1),
        ),
    );
    let foreign_error = foreign_client
        .snapshot_heads(&reference)
        .await
        .expect_err("R3/E3 foreign Worker");
    assert!(
        foreign_error.to_string().contains("403"),
        "R3/E3: {foreign_error}"
    );
}

/// Terminal Memory cause/effect decision table:
/// | Rule | Worker/effect | root input+binding | original A | live head | Effect |
/// |---|---|---|---|---|---|
/// | T1 | current/live | exact | exact | A | A→B through canonical CAS |
/// | T2 | current/live replay | exact | exact | B | same B/version; no new history |
/// | T3 | current/live | exact | exact | concurrent C | typed conflict; preserve C |
/// | T4 | current/live | exact | caller asserts C | C | reject before repository mutation |
/// | T5 | old Run claim settled | exact terminal effect | exact | any | terminal path remains authorized |
/// | T6 | exact Worker, asserted expired >20s | same generation renewed/live | exact | any | mutation succeeds through current root |
/// | T7 | exact Worker, asserted expired >20s | same generation expired | exact | any | 409 before repository mutation |
/// | T8 | exact Worker, foreign generation / foreign Worker | predecessor current / any | exact | any | 409 / 403 before mutation |
/// | T9 | current/live | root mismatch or no root control | any | any | reject before repository mutation |
/// Constraints: v2 carries no RunClaim or caller Workspace; the root-returned
/// target and Registry binding are checked before the first Memory operation.
/// The Worker transport reuses the one terminal-generation temporal rule; the
/// Control root remains the only current lease clock and Memory owns no timer.
#[tokio::test]
async fn terminal_memory_uses_root_target_and_original_heads_after_run_settlement() {
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let note_a = repository
        .create("memory-terminal", "/note.md", "A")
        .await
        .unwrap();
    let conflict_a = repository
        .create("memory-terminal", "/conflict.md", "A-conflict")
        .await
        .unwrap();
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open test Resource Registry"),
    );
    let catalog = Arc::new(awaken_resource_application::RegistryApplication::new(
        storage,
        awaken_time::system_clock(),
    ));
    create_store(&catalog, "memory-terminal").await;

    let dispatch = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    dispatch
        .enqueue(RunDispatch::new(support::activation("terminal-memory")))
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-terminal-memory").await;
    let claimed = dispatch
        .claim(&identity.lease_owner(), 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let stale_run_claim = RunClaim::from(&claimed.lease);
    dispatch
        .settle(
            &stale_run_claim.run_id,
            stale_run_claim.epoch,
            awaken_dispatch::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    let input = memory_input("terminal", "memory-terminal", ResourceAccess::ReadWrite);
    let materialization = MemoryMaterializationEvidence::new(
        "memory-terminal",
        awaken_provisioning_contract::memory_materialization_mount_path(&input.mount_path),
        vec![
            MemoryMaterializationHead {
                path: note_a.path.clone(),
                id: note_a.id.clone(),
                version: note_a.version,
                content_sha256: note_a.content_sha256.clone(),
            },
            MemoryMaterializationHead {
                path: conflict_a.path.clone(),
                id: conflict_a.id.clone(),
                version: conflict_a.version,
                content_sha256: conflict_a.content_sha256.clone(),
            },
        ],
    )
    .unwrap();
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        awaken_session_state_contract::SessionCleanupCommand::new(
            &awaken_session_types::SessionId::from("session-terminal-memory"),
            "session-terminal-memory",
            "cleanup-root",
        ),
        awaken_session_resource_contract::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 9,
            expires_at_unix_ms: u64::MAX,
        },
    );
    let intent = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &input,
        &materialization,
        &effect,
    )
    .unwrap();
    let service = Arc::new(
        WorkerMemoryService::new(
            repository.clone(),
            catalog.clone(),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
            awaken_time::system_clock(),
        )
        .with_terminal_memory_control(Arc::new(ExactTerminalMemoryControl {
            workspace_id: "workspace-memory".into(),
            intent: intent.clone(),
            current_effect: effect.clone(),
        })),
    );
    let address = support::serve(worker_memory_router(service)).await;
    let client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );
    let reference = terminal_memory_materialization_reference(&intent).unwrap();

    let history_before = repository
        .list_versions("memory-terminal")
        .await
        .unwrap()
        .len();
    let note_b = client
        .update(&reference, &note_a.generation(), "B")
        .await
        .expect("T1 A→B");
    let history_after_first = repository
        .list_versions("memory-terminal")
        .await
        .unwrap()
        .len();
    assert_eq!(history_after_first, history_before + 1, "T1");

    // T2: the frozen original generation cannot be treated as a successful
    // response-loss retry after B committed. Without an operation receipt, the
    // safe result is Conflict; current-token retry belongs to a live caller that
    // holds that token, while terminal recovery retains its candidate.
    assert!(matches!(
        client.update(&reference, &note_a.generation(), "B").await,
        Err(MemErr::Conflict { current }) if *current == note_b
    ));
    assert_eq!(
        repository
            .list_versions("memory-terminal")
            .await
            .unwrap()
            .len(),
        history_after_first,
        "T2 no duplicate history"
    );

    // MP-HTTP: exact terminal A fence + content-preserving rename after B =>
    // both the old wire client (no expected_path) and Copy client reject. Even
    // equal candidate bytes at the new path cannot authorize an acknowledgement.
    repository
        .rename("memory-terminal", "/note.md", "/moved.md")
        .await
        .unwrap();
    let moved_heads = repository.snapshot_heads("memory-terminal").await.unwrap();
    let moved_history = repository.list_versions("memory-terminal").await.unwrap();
    for pinned in [false, true] {
        let result = if pinned {
            client
                .update_materialized(&reference, "/note.md", &note_a.generation(), "B")
                .await
        } else {
            client.update(&reference, &note_a.generation(), "B").await
        };
        assert!(
            matches!(result, Err(MemErr::Conflict { .. })),
            "MP-HTTP pinned={pinned}"
        );
    }
    assert_eq!(
        repository.snapshot_heads("memory-terminal").await.unwrap(),
        moved_heads
    );
    assert_eq!(
        repository.list_versions("memory-terminal").await.unwrap(),
        moved_history
    );

    // RC-W3 terminal exact original path/generation => one durable candidate on
    // repeated/concurrent requests. RC-W4 altered path/generation or omitted base
    // for an original path => 403 before persistence. Heads/history stay exact.
    let proposed = awaken_resource_contract::MemoryCopyConflict {
        path: note_a.path.clone(),
        base: Some(awaken_resource_contract::MemoryCopyBase {
            id: note_a.id.clone(),
            version: note_a.version,
            content_sha256: note_a.content_sha256.clone(),
        }),
        content: Some("losing proposal".into()),
    };
    let (first, retry) = tokio::join!(
        client.preserve_copy_conflict(&reference, &proposed),
        client.preserve_copy_conflict(&reference, &proposed),
    );
    first.unwrap();
    retry.unwrap();
    assert_eq!(
        repository
            .list_copy_conflicts("memory-terminal")
            .await
            .unwrap(),
        vec![proposed.clone()]
    );
    // RC-W3/R5 transport row. Cause: the exact terminal reference is accepted
    // and one candidate is durably retained. Effect: the remote repository
    // returns that typed candidate exactly; it must not substitute an empty
    // list or surface a missing-capability error. The denied RW/realization
    // rows above cover the two non-terminal authority partitions.
    assert_eq!(
        client
            .list_copy_conflicts(&reference)
            .await
            .expect("R5 typed copy-conflict list crosses the HTTP adapter"),
        vec![proposed.clone()],
        "R5 the HTTP projection preserves every candidate field"
    );
    for axis in ["path", "id", "sha", "base"] {
        let mut bad = proposed.clone();
        match axis {
            "path" => bad.path = "/elsewhere".into(),
            "id" => bad.base.as_mut().unwrap().id = "foreign".into(),
            "sha" => bad.base.as_mut().unwrap().content_sha256 = "b".repeat(64),
            _ => bad.base = None,
        }
        assert!(
            client
                .preserve_copy_conflict(&reference, &bad)
                .await
                .unwrap_err()
                .to_string()
                .contains("403"),
            "RC-W4 {axis}"
        );
    }
    assert_eq!(
        repository.snapshot_heads("memory-terminal").await.unwrap(),
        moved_heads
    );
    assert_eq!(
        repository.list_versions("memory-terminal").await.unwrap(),
        moved_history
    );
    assert_eq!(
        repository
            .list_copy_conflicts("memory-terminal")
            .await
            .unwrap(),
        vec![proposed.clone()]
    );

    let conflict_c = repository
        .update("memory-terminal", &conflict_a.generation(), "C")
        .await
        .unwrap();
    assert!(
        matches!(
            client
                .update(&reference, &conflict_a.generation(), "B-conflict",)
                .await,
            Err(MemErr::Conflict { .. })
        ),
        "T3"
    );
    assert_eq!(
        repository
            .get_by_path("memory-terminal", "/conflict.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("C"),
        "T3 preserve C"
    );

    assert!(
        client
            .update(
                &reference,
                &conflict_c.generation(),
                "not-authorized-from-C",
            )
            .await
            .is_err(),
        "T4"
    );
    assert_eq!(
        repository
            .get_by_path("memory-terminal", "/conflict.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("C"),
        "T4 no mutation"
    );

    let foreign_client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
            awaken_worker_contract::WorkerIdentity::new("foreign-worker", "foreign-boot", 1),
        ),
    );
    assert!(
        foreign_client
            .create(&reference, "/foreign.md", "must-not-exist")
            .await
            .is_err(),
        "T8 foreign Worker"
    );

    let mut expired_effect = effect.clone();
    let now_unix_ms = support::unix_now_ms();
    expired_effect.lease.expires_at_unix_ms = now_unix_ms.saturating_sub(20_001);
    let expired_intent = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &input,
        &materialization,
        &expired_effect,
    )
    .unwrap();
    let expired_reference = terminal_memory_materialization_reference(&expired_intent).unwrap();
    let renewed_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        expired_effect.command.clone(),
        awaken_session_resource_contract::SessionRealizationLease {
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..expired_effect.lease.clone()
        },
    );
    let renewed_service = Arc::new(
        WorkerMemoryService::new(
            repository.clone(),
            catalog.clone(),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
            awaken_time::system_clock(),
        )
        .with_terminal_memory_control(Arc::new(ExactTerminalMemoryControl {
            workspace_id: "workspace-memory".into(),
            intent: expired_intent.clone(),
            current_effect: renewed_effect.clone(),
        })),
    );
    let renewed_address = support::serve(worker_memory_router(renewed_service)).await;
    let renewed_client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{renewed_address}"))
            .with_worker_identity(identity.clone()),
    );
    renewed_client
        .create(
            &expired_reference,
            "/renewed-terminal.md",
            "current generation authorizes old assertion",
        )
        .await
        .expect("T6 expired assertion uses the current same-generation renewal");
    assert_eq!(
        repository
            .get_by_path("memory-terminal", "/renewed-terminal.md")
            .await
            .unwrap()
            .and_then(|record| record.content),
        Some("current generation authorizes old assertion".into()),
        "T6 mutation is owned by the renewed current root"
    );

    let expired_current_service = Arc::new(
        WorkerMemoryService::new(
            repository.clone(),
            catalog.clone(),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
            awaken_time::system_clock(),
        )
        .with_terminal_memory_control(Arc::new(ExactTerminalMemoryControl {
            workspace_id: "workspace-memory".into(),
            intent: expired_intent.clone(),
            current_effect: expired_effect.clone(),
        })),
    );
    let expired_current_address =
        support::serve(worker_memory_router(expired_current_service)).await;
    let expired_current_client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{expired_current_address}"))
            .with_worker_identity(identity.clone()),
    );
    let expired_error = expired_current_client
        .create(&expired_reference, "/expired.md", "must-not-exist")
        .await
        .expect_err("T7 expired current root has no Memory authority");
    assert!(
        expired_error.to_string().contains("409"),
        "T7 expired current root uses the canonical conflict status: {expired_error}"
    );

    let successor_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        expired_effect.command.clone(),
        awaken_session_resource_contract::SessionRealizationLease {
            epoch: expired_effect.lease.epoch + 1,
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..expired_effect.lease.clone()
        },
    );
    let successor_intent = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &input,
        &materialization,
        &successor_effect,
    )
    .unwrap();
    let successor_reference = terminal_memory_materialization_reference(&successor_intent).unwrap();
    let successor_service = Arc::new(
        WorkerMemoryService::new(
            repository.clone(),
            catalog.clone(),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            directory.clone(),
            awaken_time::system_clock(),
        )
        .with_terminal_memory_control(Arc::new(ExactTerminalMemoryControl {
            workspace_id: "workspace-memory".into(),
            intent: successor_intent,
            current_effect: renewed_effect,
        })),
    );
    let successor_address = support::serve(worker_memory_router(successor_service)).await;
    let successor_client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{successor_address}"))
            .with_worker_identity(identity.clone()),
    );
    let successor_error = successor_client
        .create(
            &successor_reference,
            "/foreign-generation.md",
            "must-not-exist",
        )
        .await
        .expect_err("T8 successor generation cannot use predecessor renewal");
    assert!(
        successor_error.to_string().contains("409"),
        "T8 successor generation uses the canonical conflict status: {successor_error}"
    );

    let altered_materialization = MemoryMaterializationEvidence::new(
        "memory-terminal",
        awaken_provisioning_contract::memory_materialization_mount_path(&input.mount_path),
        Vec::new(),
    )
    .unwrap();
    let altered_intent = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &input,
        &altered_materialization,
        &effect,
    )
    .unwrap();
    let altered_reference = terminal_memory_materialization_reference(&altered_intent).unwrap();
    // RC-W5 availability/ownership are checked even for an already-retained
    // exact replay: foreign worker, expired current lease, successor generation,
    // or altered materialization may never obtain a successful cleanup ack.
    for (denied_client, denied_reference) in [
        (&foreign_client, &reference),
        (&expired_current_client, &expired_reference),
        (&successor_client, &successor_reference),
        (&client, &altered_reference),
    ] {
        assert!(
            denied_client
                .preserve_copy_conflict(denied_reference, &proposed)
                .await
                .is_err(),
            "RC-W5"
        );
    }
    assert_eq!(
        repository
            .list_copy_conflicts("memory-terminal")
            .await
            .unwrap(),
        vec![proposed]
    );
    assert!(
        client
            .create(&altered_reference, "/root-mismatch.md", "must-not-exist")
            .await
            .is_err(),
        "T9 root mismatch"
    );

    let no_control = Arc::new(WorkerMemoryService::new(
        repository.clone(),
        catalog,
        dispatch,
        Arc::new(HeaderWorkerAuthenticator),
        directory,
        awaken_time::system_clock(),
    ));
    let no_control_address = support::serve(worker_memory_router(no_control)).await;
    let no_control_client = HttpMemoryRepository::new(
        WorkerUpstream::new(format!("http://{no_control_address}")).with_worker_identity(identity),
    );
    assert!(
        no_control_client
            .create(&reference, "/no-control.md", "must-not-exist")
            .await
            .is_err(),
        "T9 no root control"
    );
    for path in [
        "/foreign.md",
        "/expired.md",
        "/foreign-generation.md",
        "/root-mismatch.md",
        "/no-control.md",
    ] {
        assert!(
            repository
                .get_by_path("memory-terminal", path)
                .await
                .unwrap()
                .is_none(),
            "T7-T9 no mutation: {path}"
        );
    }
}
