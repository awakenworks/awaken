//! Decoded replacements must preserve native private ownership inside root CAS.
use super::*;
use awaken_session_contract::SessionPrivateBinding;

fn binding() -> SessionPrivateBinding {
    SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "ws_a".into(),
        1,
    )
    .unwrap()
}

#[tokio::test]
async fn sqlite_private_binding_survives_reopen_and_create_replay_without_foreign_partition_effects()
 {
    let (_directory, path) = temp_database("private-owner.db");
    let repo = SqliteManagedSessionRepository::open(&path).await.unwrap();
    let seed = binding().bind_fresh(sample("personal-reopen")).unwrap();
    let identity = SessionMutationPayload::default_create_idempotency(&seed);
    assert!(matches!(
        repo.create("foreign", seed.clone(), identity.clone(), Vec::new())
            .await,
        Err(SessionRepositoryError::InvalidMutation(_))
    ));
    assert!(matches!(
        repo.get(&seed.session_id).await,
        Err(SessionRepositoryError::NotFound)
    ));
    let first = repo
        .create("ws_a", seed.clone(), identity.clone(), Vec::new())
        .await
        .unwrap();
    assert!(matches!(
        first,
        awaken_session_contract::SessionCreateResult::Applied(_)
    ));
    let original = repo.get(&seed.session_id).await.unwrap();
    close_repository(repo).await;
    let reopened = SqliteManagedSessionRepository::open(&path).await.unwrap();
    assert_eq!(reopened.get(&seed.session_id).await.unwrap(), original);
    assert_eq!(reopened.owner(&seed.session_id).await.unwrap(), "ws_a");
    let replay = reopened
        .create("ws_a", seed, identity, Vec::new())
        .await
        .unwrap();
    assert_eq!(
        replay,
        awaken_session_contract::SessionCreateResult::Replayed(original)
    );
    assert!(reopened.pending_lifecycle().await.unwrap().is_empty());
    close_repository(reopened).await;
}

pub(super) async fn replacement_table<R: awaken_session_application::SessionStore>(
    repo: &R,
    prefix: &str,
) {
    // Causes: original private/legacy, each coordinate, remove/add, same owner.
    // Effects: only equality admits; refusal leaves root, receipt and outbox
    // intact, so a corrected original-key request can still commit.
    for (index, field) in [
        "org_id",
        "application_id",
        "account_id",
        "workspace_id",
        "application_binding_revision",
        "remove",
        "add",
        "same",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("{prefix}-private-{index}");
        let seed = if field == "add" {
            sample(&id)
        } else {
            binding().bind_fresh(sample(&id)).unwrap()
        };
        let before = create_fixture(repo, "ws_a", seed, Vec::new()).await;
        let mut json = serde_json::to_value(&before).unwrap();
        match field {
            "remove" => {
                json.as_object_mut().unwrap().remove("private_binding");
            }
            "add" => {
                json["private_binding"] = serde_json::to_value(binding()).unwrap();
            }
            "same" => {}
            "application_binding_revision" => {
                json["private_binding"][field] = serde_json::json!(2);
            }
            _ => {
                json["private_binding"][field] = serde_json::json!("other");
            }
        }
        let candidate: PersistedSession = serde_json::from_value(json).unwrap();
        let facts = repo.pending_lifecycle().await.unwrap();
        let make = |root| {
            let payload = SessionMutationPayload::Replace(root);
            SessionMutation {
                expected_revision: before.revision,
                idempotency: IdempotencyRecord {
                    key: format!("private-update:{id}"),
                    payload_hash: payload.stable_hash(),
                },
                payload,
                lifecycle_facts: Vec::new(),
            }
        };
        let result = repo.commit("ws_a", make(candidate)).await;
        if field == "same" {
            assert!(matches!(
                result.unwrap(),
                SessionMutationResult::Applied { .. }
            ));
        } else {
            assert!(
                matches!(result, Err(SessionRepositoryError::InvalidMutation(_))),
                "{field}: {result:?}"
            );
            assert_eq!(repo.get(&before.session_id).await.unwrap(), before);
            assert_eq!(repo.pending_lifecycle().await.unwrap(), facts);
            assert!(matches!(
                repo.commit("ws_a", make(before.clone())).await.unwrap(),
                SessionMutationResult::Applied { .. }
            ));
        }
        let durable = repo.get(&before.session_id).await.unwrap();
        assert_eq!(
            awaken_session_contract::SessionPrivateBinding::of(&durable),
            awaken_session_contract::SessionPrivateBinding::of(&before)
        );
    }
}
