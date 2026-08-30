//! Claim-fenced Memory Coordinator tests over real HTTP.

use awaken_run_ingress_testkit::worker_http as support;

use std::sync::Arc;

use awaken_memory_store::{MemErr, MemoryRepository as _};
use awaken_provisioning_contract::{MemoryMaterializationEvidence, MemoryMaterializationHead};
use awaken_resource_contract::{
    ChangeMemoryStoreState, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
    RegisterMemoryStore, ResourceAccess, ResourceAdministration as _, ResourceState,
};
use awaken_resource_worker_http::{
    HttpMemoryRepository, WorkerMemoryService, worker_memory_router,
};
use awaken_resource_worker_http::{
    memory_materialization_reference, terminal_memory_materialization_reference,
};
use awaken_run_ingress::{DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch};
use awaken_session_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, SessionRealizationControl, SessionRealizationControlFailure,
    SessionRealizationDirective, SessionTerminalMemoryIntent, SessionTerminalMemoryTarget,
};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

fn memory_input(
    binding: &str,
    store: &str,
    access: ResourceAccess,
) -> awaken_session_contract::ResolvedInput {
    awaken_session_contract::ResolvedInput {
        binding_id: awaken_resource_contract::BindingId::new(binding),
        source: awaken_session_contract::ResolvedInputSource::MemoryStore {
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

fn create_store(catalog: &awaken_resource_application::RegistryApplication, id: &str) {
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
        .expect("register test MemoryStore");
}

struct ExactTerminalMemoryControl {
    workspace_id: String,
    intent: SessionTerminalMemoryIntent,
    current_effect: awaken_session_contract::SessionTerminalCleanupEffect,
}

#[async_trait::async_trait]
impl SessionRealizationControl for ExactTerminalMemoryControl {
    async fn begin_session_realization(
        &self,
        _command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "test control has no realization driver".into(),
        ))
    }

    async fn activate_session_realization(
        &self,
        _command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "test control has no realization driver".into(),
        ))
    }

    async fn acknowledge_session_realization(
        &self,
        _command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "test control has no realization driver".into(),
        ))
    }

    async fn fail_session_realization(
        &self,
        _command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(SessionRealizationControlFailure::Invalid(
            "test control has no realization driver".into(),
        ))
    }

    async fn authorize_terminal_memory_intent(
        &self,
        intent: &SessionTerminalMemoryIntent,
    ) -> Result<SessionTerminalMemoryTarget, SessionRealizationControlFailure> {
        if intent != &self.intent
            || self.current_effect.command != intent.effect().command
            || !awaken_session_contract::realization_lease_generation_authorizes(
                &self.current_effect.lease,
                &intent.effect().lease,
            )
            || !awaken_session_contract::realization_lease_is_live_at(
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
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
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
    ));
    create_store(&catalog, "memory-rw");
    create_store(&catalog, "memory-ro");
    let resources = awaken_session_contract::ResolvedSessionResources::try_new(
        vec![
            memory_input("rw", "memory-rw", ResourceAccess::ReadWrite),
            memory_input("ro", "memory-ro", ResourceAccess::ReadOnly),
        ],
        Vec::new(),
    )
    .unwrap();
    let dispatch = Arc::new(MemoryDispatchStore::new());
    dispatch
        .enqueue(
            RunDispatch::new(support::activation("memory"))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-memory"),
                ))
                .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                    "workspace-memory",
                    serde_json::to_string(&resources).unwrap(),
                )),
        )
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-memory").await;
    let claimed = dispatch
        .claim(
            &identity.lease_owner(),
            60_000,
            support::unix_now_ms(),
            &Default::default(),
        )
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
    assert_eq!(snapshot.len(), 1, "M1");
    let created = client.create(&rw, "/new.md", "v1").await.expect("M2");
    let updated = client
        .update(&rw, &created.id, "v2", &created.content_sha256)
        .await
        .expect("M2");
    assert_eq!(updated.content.as_deref(), Some("v2"), "M2");

    let conflict = client
        .update(&rw, &created.id, "stale", &created.content_sha256)
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
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    assert!(client.snapshot_heads(&rw).await.is_err(), "M6");
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
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new());
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
    ));
    create_store(&catalog, "memory-terminal");

    let dispatch = Arc::new(MemoryDispatchStore::new());
    dispatch
        .enqueue(RunDispatch::new(support::activation("terminal-memory")))
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-terminal-memory").await;
    let claimed = dispatch
        .claim(
            &identity.lease_owner(),
            60_000,
            support::unix_now_ms(),
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    let stale_run_claim = RunClaim::from(&claimed.lease);
    dispatch
        .settle(
            &stale_run_claim.run_id,
            stale_run_claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();

    let input = memory_input("terminal", "memory-terminal", ResourceAccess::ReadWrite);
    let materialization = MemoryMaterializationEvidence::new(
        "memory-terminal",
        input.mount_path.clone(),
        vec![
            MemoryMaterializationHead {
                path: note_a.path.clone(),
                id: note_a.id.clone(),
                content_sha256: note_a.content_sha256.clone(),
            },
            MemoryMaterializationHead {
                path: conflict_a.path.clone(),
                id: conflict_a.id.clone(),
                content_sha256: conflict_a.content_sha256.clone(),
            },
        ],
    )
    .unwrap();
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        awaken_session_contract::SessionCleanupCommand::new(
            "session-terminal-memory",
            "session-terminal-memory",
            "cleanup-root",
        ),
        awaken_session_contract::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 9,
            expires_at_unix_ms: u64::MAX,
        },
    );
    let intent = awaken_session_contract::terminal_memory_reconciliation_intent(
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
        )
        .with_session_control(Arc::new(ExactTerminalMemoryControl {
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
        .update(&reference, &note_a.id, "B", &note_a.content_sha256)
        .await
        .expect("T1 A→B");
    let history_after_first = repository
        .list_versions("memory-terminal")
        .await
        .unwrap()
        .len();
    assert_eq!(history_after_first, history_before + 1, "T1");

    let replay = client
        .update(&reference, &note_a.id, "B", &note_a.content_sha256)
        .await
        .expect("T2 response-loss replay");
    assert_eq!(replay.version, note_b.version, "T2");
    assert_eq!(
        repository
            .list_versions("memory-terminal")
            .await
            .unwrap()
            .len(),
        history_after_first,
        "T2 no duplicate history"
    );

    let conflict_c = repository
        .update(
            "memory-terminal",
            &conflict_a.id,
            "C",
            &conflict_a.content_sha256,
        )
        .await
        .unwrap();
    assert!(
        matches!(
            client
                .update(
                    &reference,
                    &conflict_a.id,
                    "B-conflict",
                    &conflict_a.content_sha256,
                )
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
                &conflict_c.id,
                "not-authorized-from-C",
                &conflict_c.content_sha256,
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
    let expired_intent = awaken_session_contract::terminal_memory_reconciliation_intent(
        &input,
        &materialization,
        &expired_effect,
    )
    .unwrap();
    let expired_reference = terminal_memory_materialization_reference(&expired_intent).unwrap();
    let renewed_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        expired_effect.command.clone(),
        awaken_session_contract::SessionRealizationLease {
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
        )
        .with_session_control(Arc::new(ExactTerminalMemoryControl {
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
        )
        .with_session_control(Arc::new(ExactTerminalMemoryControl {
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

    let successor_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        expired_effect.command.clone(),
        awaken_session_contract::SessionRealizationLease {
            epoch: expired_effect.lease.epoch + 1,
            expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
            ..expired_effect.lease.clone()
        },
    );
    let successor_intent = awaken_session_contract::terminal_memory_reconciliation_intent(
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
        )
        .with_session_control(Arc::new(ExactTerminalMemoryControl {
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

    let altered_materialization =
        MemoryMaterializationEvidence::new("memory-terminal", input.mount_path.clone(), Vec::new())
            .unwrap();
    let altered_intent = awaken_session_contract::terminal_memory_reconciliation_intent(
        &input,
        &altered_materialization,
        &effect,
    )
    .unwrap();
    let altered_reference = terminal_memory_materialization_reference(&altered_intent).unwrap();
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
