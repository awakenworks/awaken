// @formal surface exclude=test_only
// @formal owner crates/platform/resources/awaken-resource-application/src/files.rs
// @formal reason: Exercises admitted File views, original command identity and interrupted deletion recovery; this is regression evidence, not a formal proof.
//! Private input metadata must never become ordinary Workspace content.

use super::tests::application;
use awaken_resource_contract::PrivateContentBinding;

fn binding(account: &str) -> PrivateContentBinding {
    PrivateContentBinding::new(
        "org".into(),
        "desk".into(),
        account.into(),
        "workspace-a".into(),
        1,
    )
    .unwrap()
}

#[cfg(test)]
#[tokio::test]
async fn admitted_private_views_share_original_authorities_and_separate_command_recovery() {
    let app = application().await;
    let alice = super::admitted_private_file_view(&app, binding("alice"));
    let bob = super::admitted_private_file_view(&app, binding("bob"));
    let create = |name: &str| (name.to_owned(), "text/plain".to_owned());
    let (name, mime) = create("input.txt");
    let a = alice
        .create_uploaded_file_with_expiry_and_idempotency(
            "workspace-a",
            name,
            mime,
            b"private",
            None,
            Some("same-command".into()),
        )
        .await
        .unwrap();
    let (name, mime) = create("input.txt");
    assert_eq!(
        alice
            .create_uploaded_file_with_expiry_and_idempotency(
                "workspace-a",
                name,
                mime,
                b"private",
                None,
                Some("same-command".into())
            )
            .await
            .unwrap(),
        a,
        "V1: original command"
    );
    let (name, mime) = create("input.txt");
    let b = bob
        .create_uploaded_file_with_expiry_and_idempotency(
            "workspace-a",
            name,
            mime,
            b"private",
            None,
            Some("same-command".into()),
        )
        .await
        .unwrap();
    assert_ne!(
        a.id, b.id,
        "V2: same external key in one S does not select another person"
    );
    assert_eq!(
        a.blob_id, b.blob_id,
        "V2: physical dedup retains independent logical identity"
    );
    assert!(app.get("workspace-a", &a.id).await.unwrap().is_none(), "V3");
    assert!(
        bob.bytes("workspace-a", &a.id).await.unwrap().is_none(),
        "V3"
    );
    assert_eq!(
        alice.bytes("workspace-a", &a.id).await.unwrap().unwrap().1,
        b"private",
        "V3"
    );
    for provenance in [
        PrivateContentBinding::new(
            "other-org".into(),
            "desk".into(),
            "alice".into(),
            "workspace-a".into(),
            1,
        )
        .unwrap(),
        PrivateContentBinding::new(
            "org".into(),
            "other-app".into(),
            "alice".into(),
            "workspace-a".into(),
            1,
        )
        .unwrap(),
        binding("bob"),
        PrivateContentBinding::new(
            "org".into(),
            "desk".into(),
            "alice".into(),
            "other-space".into(),
            1,
        )
        .unwrap(),
        PrivateContentBinding::new(
            "org".into(),
            "desk".into(),
            "alice".into(),
            "workspace-a".into(),
            2,
        )
        .unwrap(),
    ] {
        let workspace = provenance.workspace_id().to_owned();
        let view = super::admitted_private_file_view(&app, provenance);
        assert!(
            view.get("workspace-a", &a.id).await.unwrap().is_none(),
            "V4"
        );
        assert!(
            view.bytes("workspace-a", &a.id).await.unwrap().is_none(),
            "V4"
        );
        assert!(
            view.delete("workspace-a", &a.id, 1)
                .await
                .unwrap()
                .is_none(),
            "V4: no cross-binding effects"
        );
        let own = view
            .create_uploaded_file_with_expiry_and_idempotency(
                &workspace,
                "input.txt".into(),
                "text/plain".into(),
                b"private",
                None,
                Some("same-command".into()),
            )
            .await
            .unwrap();
        assert_ne!(
            own.id, a.id,
            "V4: every binding gets its own original command identity"
        );
    }
    assert_eq!(
        alice.list("workspace-a", None).await.unwrap(),
        vec![a.clone()],
        "V5: filter before pagination"
    );
    assert_eq!(
        bob.list("workspace-a", None).await.unwrap(),
        vec![b.clone()],
        "V5"
    );
    assert!(
        app.list("workspace-a", None).await.unwrap().is_empty(),
        "V5"
    );
    let ordinary = app
        .create_uploaded_file_with_expiry_and_idempotency(
            "workspace-a",
            "input.txt".into(),
            "text/plain".into(),
            b"private",
            None,
            Some("same-command".into()),
        )
        .await
        .unwrap();
    assert_ne!(
        ordinary.id, a.id,
        "V5: ordinary command identity stays separate"
    );
    assert!(
        alice
            .get("workspace-a", &ordinary.id)
            .await
            .unwrap()
            .is_none(),
        "V5: no legacy adoption"
    );
    assert!(
        alice
            .create_artifact(&super::tests::artifact_publication(
                "output.txt",
                b"output",
                None
            ))
            .await
            .is_err(),
        "V6: input view cannot mint Session output ownership"
    );
    assert!(
        alice
            .create_uploaded_file(
                "wrong-space",
                "input.txt".into(),
                "text/plain".into(),
                b"private"
            )
            .await
            .is_err(),
        "V6: exact Workspace before effects"
    );
    alice
        .delete("workspace-a", &a.id, 2)
        .await
        .unwrap()
        .unwrap();
    let (name, mime) = create("input.txt");
    assert!(
        alice
            .create_uploaded_file_with_expiry_and_idempotency(
                "workspace-a",
                name,
                mime,
                b"private",
                None,
                Some("same-command".into())
            )
            .await
            .is_err(),
        "V7: no revival after delete"
    );
    assert!(
        alice.get("workspace-a", &a.id).await.unwrap().is_none(),
        "V7"
    );
    assert_eq!(
        bob.bytes("workspace-a", &b.id).await.unwrap().unwrap().1,
        b"private",
        "V7: independent owner remains"
    );
}

#[cfg(test)]
#[tokio::test]
async fn ordinary_file_commands_cannot_read_list_or_delete_private_input() {
    let app = application().await;
    let public = app
        .create_uploaded_file(
            "workspace-a",
            "public.txt".into(),
            "text/plain".into(),
            b"public",
        )
        .await
        .unwrap();
    let binding = PrivateContentBinding::new(
        "org".into(),
        "desk".into(),
        "alice".into(),
        "workspace-a".into(),
        1,
    )
    .unwrap();
    let mut value = serde_json::to_value(&public).unwrap();
    value["id"] = "file_private_input".into();
    value["filename"] = "private.txt".into();
    value["private_binding"] = serde_json::to_value(binding).unwrap();
    value["harvest_key"] = "private-command".into();
    let private = serde_json::from_value(value).unwrap();
    app.catalog.create_file(private).await.unwrap();
    assert!(
        matches!(
            app.create_uploaded_file_with_expiry_and_idempotency(
                "workspace-a",
                "private.txt".into(),
                "text/plain".into(),
                b"public",
                None,
                Some("private-command".into()),
            )
            .await,
            Err(awaken_resource_contract::ResourcePurgeError::IdempotencyConflict(_))
        ),
        "P1: ordinary retry cannot adopt private provenance"
    );

    assert!(
        app.get("workspace-a", "file_private_input")
            .await
            .unwrap()
            .is_none(),
        "P1"
    );
    assert!(
        app.bytes("workspace-a", "file_private_input")
            .await
            .unwrap()
            .is_none(),
        "P1"
    );
    for records in [
        app.list("workspace-a", None).await.unwrap(),
        app.list_including_deleted("workspace-a", None)
            .await
            .unwrap(),
    ] {
        assert_eq!(
            records,
            vec![public.clone()],
            "P2: filter before caller pagination"
        );
    }
    assert!(
        app.delete("workspace-a", "file_private_input", 1)
            .await
            .unwrap()
            .is_none(),
        "P3"
    );
    let retained = app
        .catalog
        .get_file("workspace-a", "file_private_input", true)
        .await
        .unwrap()
        .unwrap();
    assert!(!retained.deleted, "P3: denial precedes lifecycle writes");
    assert_eq!(
        app.store.get(&public.blob_id).await.unwrap(),
        Some(b"public".to_vec()),
        "P3: shared bytes remain"
    );
    assert_eq!(
        app.bytes("workspace-a", &public.id)
            .await
            .unwrap()
            .unwrap()
            .1,
        b"public",
        "P4: ordinary compatibility"
    );
}

#[cfg(test)]
#[tokio::test]
async fn private_input_delete_recovers_the_original_durable_intent_after_reopen() {
    use awaken_resource_contract::{
        ResourceKind, ResourcePurgePreparation, ResourcePurgeRepository, ResourceReferenceIndex,
    };
    use std::sync::Arc;
    async fn open(
        path: &str,
    ) -> (
        super::FileApplication,
        Arc<awaken_resource_store::SqliteResourceStore>,
    ) {
        let files = Arc::new(
            awaken_file_store::sqlite::SqliteFileStore::open(path)
                .await
                .unwrap(),
        );
        let lifecycle = Arc::new(
            awaken_resource_store::SqliteResourceStore::open(path)
                .await
                .unwrap(),
        );
        (
            super::FileApplication::new(
                files.clone(),
                files,
                lifecycle.clone(),
                awaken_time::system_clock(),
            ),
            lifecycle,
        )
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("private-delete.sqlite")
        .to_string_lossy()
        .into_owned();
    let (app, lifecycle) = open(&path).await;
    let private = super::admitted_private_file_view(&app, binding("alice"));
    let file = private
        .create_uploaded_file(
            "workspace-a",
            "a.txt".into(),
            "text/plain".into(),
            b"shared",
        )
        .await
        .unwrap();
    let other = app
        .create_uploaded_file(
            "workspace-b",
            "b.txt".into(),
            "text/plain".into(),
            b"shared",
        )
        .await
        .unwrap();
    let sql = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER private_delete_fault BEFORE DELETE ON resource_lifecycle_references BEGIN SELECT RAISE(ABORT, 'private cleanup interrupted'); END;")
        .execute(&sql).await.unwrap();
    assert!(
        private
            .delete("workspace-a", &file.id, 7)
            .await
            .unwrap_err()
            .to_string()
            .contains("private cleanup interrupted"),
        "D1: interrupt after original intent and tombstone"
    );
    let intent_id = format!("purge:File:workspace-a:{}", file.id);
    let intent = ResourcePurgeRepository::get(lifecycle.as_ref(), &intent_id)
        .await
        .unwrap()
        .unwrap();
    sqlx::raw_sql("DROP TRIGGER private_delete_fault")
        .execute(&sql)
        .await
        .unwrap();
    sql.close().await;
    drop(private);
    drop(app);
    drop(lifecycle);
    let (app, lifecycle) = open(&path).await;
    let wrong = awaken_resource_contract::ResourcePurgeIntent::new(
        "purge:File:workspace-a:wrong",
        "file-delete:workspace-a:wrong",
        intent.target.clone(),
        None,
        7,
        7,
    )
    .unwrap();
    ResourcePurgePreparation::prepare(&app, &wrong)
        .await
        .unwrap();
    assert_eq!(
        lifecycle
            .references_for_resource(ResourceKind::File, &file.blob_id)
            .await
            .unwrap()
            .len(),
        2,
        "D2: a shared blob or guessed command grants no cleanup"
    );
    ResourcePurgePreparation::prepare(&app, &intent)
        .await
        .unwrap();
    assert_eq!(
        lifecycle
            .references_for_resource(ResourceKind::File, &file.blob_id)
            .await
            .unwrap(),
        vec![super::logical_file_reference(&other)],
        "D3: recovery removes only the original logical reference"
    );
    let retained = app
        .catalog
        .get_file("workspace-a", &file.id, true)
        .await
        .unwrap()
        .unwrap();
    assert!(retained.deleted, "D3");
    assert_eq!(
        retained.private_binding, file.private_binding,
        "D3: retain original provenance"
    );
    assert_eq!(
        ResourcePurgeRepository::get(lifecycle.as_ref(), &intent_id)
            .await
            .unwrap(),
        Some(intent),
        "D3: original intent is never reset"
    );
    assert_eq!(
        app.bytes("workspace-b", &other.id)
            .await
            .unwrap()
            .unwrap()
            .1,
        b"shared",
        "D4: independent owner and shared bytes survive"
    );
}
