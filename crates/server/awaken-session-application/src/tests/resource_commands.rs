//! Fail-closed rows of the caller-issued Resource commands. Accepted rows run
//! end to end through the Managed adapter's Resource route suites.

use super::*;
use crate::resource_reconciliation::MEMORY_STORE_CREATE_ONLY;
use crate::{SessionInputRequest, SessionInputTarget, SessionResourceManifestRequest};
use awaken_resource_contract::{
    BindingId, ConfigVersion, FileId, MemoryStoreConfigVersion, ResourceAccess, RetentionPolicy,
};

fn file_request(file: &str, mount: &str) -> SessionInputRequest {
    SessionInputRequest {
        target: SessionInputTarget::File(FileId::from(file)),
        mount_path: mount.into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        derived_memory_mount: false,
    }
}

fn memory_input(binding: &str) -> crate::session_resource::ResolvedInput {
    crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: BindingId::new(binding),
        source: crate::session_resource::ResolvedInputSource::MemoryStore {
            memory_store_id: "memory".into(),
            config: MemoryStoreConfigVersion {
                memory_store_id: "memory".into(),
                version: ConfigVersion::INITIAL,
                retention_policy: RetentionPolicy::default(),
            },
        },
        mount_path: "/mnt/memory/project".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
    }
}

async fn fixture(
    id: &str,
    policy: crate::session_resource::SessionMutationPolicy,
) -> (
    SessionApplication,
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
) {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("Resource command Session store"),
    );
    let mut session = persisted(id, false, "idle");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut session.baseline
    else {
        unreachable!("fixture baseline is frozen")
    };
    baseline.mutation_policy = policy;
    session
        .resources
        .prepare(
            &SessionId::from(id),
            crate::session_resource::ResolvedSessionResources::try_new(
                vec![memory_input("session:memory")],
                Vec::new(),
            )
            .unwrap(),
        )
        .expect("desired MemoryStore input");
    create(repo.as_ref(), session).await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    (app, repo)
}

#[tokio::test]
async fn resource_commands_refuse_before_any_lowering_or_root_change() {
    // Cause/effect rows from the command tables (resource_commands.rs):
    // | Row | Command | Cause | Effect |
    // |---|---|---|---|
    // | A1 | attach | target is a MemoryStore, not a File | rejected |
    // | D1 | detach | no input occupies the binding | NotFound |
    // | D2 | detach | the input is a MemoryStore | rejected, create-only message |
    // | M2 | replace | expected Resource revision differs | Conflict |
    // | M3 | replace | the frozen policy is not Managed | rejected |
    // Every row leaves the exact durable root unchanged.
    let id = "resource-commands";
    let session_id = SessionId::from(id);
    let (app, repo) = fixture(id, crate::session_resource::SessionMutationPolicy::Managed).await;
    let before = repo.get(&session_id).await.unwrap();

    let memory_target =
        SessionInputRequest::derived_memory_store("memory".into(), ResourceAccess::ReadWrite, None);
    assert!(
        matches!(
            app.attach_session_file(&session_id, memory_target).await,
            Err(SessionResourceManifestError::Rejected(error))
                if error.kind == awaken_session_types::RunErrorKind::BadRequest
        ),
        "A1"
    );
    assert!(
        matches!(
            app.detach_session_input(&session_id, &BindingId::new("session:absent"))
                .await,
            Err(SessionResourceManifestError::NotFound)
        ),
        "D1"
    );
    assert!(
        matches!(
            app.detach_session_input(&session_id, &BindingId::new("session:memory"))
                .await,
            Err(SessionResourceManifestError::Rejected(error))
                if error.message == MEMORY_STORE_CREATE_ONLY
        ),
        "D2"
    );
    let request = |expected_resource_revision| SessionResourceManifestRequest {
        inputs: vec![file_request("file", "/workspace/file")],
        expected_resource_revision,
        idempotency_key: None,
        request_fingerprint: "fingerprint".into(),
    };
    assert!(
        matches!(
            app.replace_session_resource_manifest_request(
                &session_id,
                request(Some(before.resources.revision + 1)),
            )
            .await,
            Err(SessionResourceManifestError::Conflict)
        ),
        "M2"
    );
    assert_eq!(
        repo.get(&session_id).await.unwrap(),
        before,
        "A1/D1/D2/M2 root"
    );

    let profiled = "resource-commands-profiled";
    let profiled_id = SessionId::from(profiled);
    let (app, repo) = fixture(
        profiled,
        crate::session_resource::SessionMutationPolicy::FileResources,
    )
    .await;
    let before = repo.get(&profiled_id).await.unwrap();
    assert!(
        matches!(
            app.replace_session_resource_manifest_request(&profiled_id, request(None))
                .await,
            Err(SessionResourceManifestError::Rejected(error))
                if error.message.contains("profiled Session whole-resource manifest")
        ),
        "M3"
    );
    assert_eq!(repo.get(&profiled_id).await.unwrap(), before, "M3 root");
}
