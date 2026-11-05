//! Native owner decisions over the real Session repository, before any content.
use super::*;
use crate::{SessionAccessError, SessionQueries, SessionRootReads};
use awaken_session_contract::{LifecycleOutbox, SessionPrivateBinding, SessionRepositoryReads};

fn binding() -> SessionPrivateBinding {
    SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "workspace".into(),
        1,
    )
    .unwrap()
}

#[tokio::test]
async fn private_binding_owner_matrix_refuses_workspace_admin_and_filters_collection() {
    // Original Account/application/Org/Workspace/revision all agree -> proof.
    // Vary each one independently, omit private binding, or name a missing id
    // -> identical NotFound. Workspace authority alone never upgrades a caller.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    create(repo.as_ref(), persisted("legacy", false, "idle")).await;
    create(
        repo.as_ref(),
        binding()
            .bind_fresh(persisted("personal", false, "idle"))
            .unwrap(),
    )
    .await;
    let executor =
        crate::SessionExecutor::colocated(Arc::new(NoopRuntime), Arc::new(NoopMcpRealizer));
    let queries = SessionQueries::new(repo.clone(), &executor, None);
    let id = SessionId::from("personal");
    assert!(
        queries
            .authorize("workspace", &SessionId::from("legacy"))
            .await
            .is_ok()
    );
    assert_eq!(
        queries.authorize("workspace", &id).await,
        Err(SessionAccessError::NotFound)
    );
    let access = queries.authorize_private(&binding(), &id).await.unwrap();
    assert_eq!(access.session_id(), &id);
    assert_eq!(access.owner_scope(), "workspace");
    assert_eq!(
        queries
            .authorize_private(&binding(), &SessionId::from("legacy"))
            .await,
        Err(SessionAccessError::NotFound)
    );
    assert_eq!(
        queries
            .authorize_private(&binding(), &SessionId::from("missing"))
            .await,
        Err(SessionAccessError::NotFound)
    );
    for field in [
        "org_id",
        "application_id",
        "account_id",
        "workspace_id",
        "application_binding_revision",
    ] {
        let mut changed = serde_json::to_value(binding()).unwrap();
        changed[field] = if field == "application_binding_revision" {
            serde_json::json!(2)
        } else {
            serde_json::json!("other")
        };
        let changed: SessionPrivateBinding = serde_json::from_value(changed).unwrap();
        assert_eq!(
            queries.authorize_private(&changed, &id).await,
            Err(SessionAccessError::NotFound),
            "{field}"
        );
    }
    let visible = queries.sessions_by_owner("workspace").await.unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].session_id.as_str(), "legacy");
    assert_eq!(
        SessionPrivateBinding::of(&repo.get(&id).await.unwrap()),
        Some(&binding())
    );
    assert!(repo.pending_lifecycle().await.unwrap().is_empty());
}
