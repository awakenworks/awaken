//! The actual SQLite/PostgreSQL adapters share this owner-isolation and original-outcome table.
use awaken_agent_config::ScopeId;
use awaken_agent_config::personal_selection::PersonalAgentSelectionRepository;
use awaken_agent_config::personal_selection::{
    AgentSelectionError, AgentSelectionIntent, AgentSelectionOwner, AgentSelectionState,
};

pub(super) fn owner(issuer: &str, account: &str) -> AgentSelectionOwner {
    AgentSelectionOwner::new(issuer.into(), account.into()).unwrap()
}

pub(super) fn intent(key: &str, generation: u64, selected: bool) -> AgentSelectionIntent {
    AgentSelectionIntent {
        request_key: key.into(),
        expected_generation: generation,
        selected,
    }
}

pub(super) async fn original_outcome(store: &impl PersonalAgentSelectionRepository) {
    let scope = ScopeId::from("ws");
    let person = owner("https://native.example", "account-a");
    let first = intent("original-key", 0, true);
    assert_eq!(
        store
            .personal_agent_selection(&scope, &person, "agent")
            .await
            .unwrap(),
        None
    );
    let receipt = store
        .set_personal_agent_selection(&scope, &person, "agent", &first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.applied_generation, 1);
    assert_eq!(
        store
            .set_personal_agent_selection(&scope, &person, "agent", &intent("stale", 0, false))
            .await
            .unwrap(),
        Err(AgentSelectionError::Conflict {
            current_generation: 1
        })
    );
    assert_eq!(
        store
            .set_personal_agent_selection(
                &scope,
                &person,
                "agent",
                &intent("original-key", 0, false)
            )
            .await
            .unwrap(),
        Err(AgentSelectionError::KeyReuse)
    );
    let remove = intent("remove-key", 1, false);
    store
        .set_personal_agent_selection(&scope, &person, "agent", &remove)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .set_personal_agent_selection(&scope, &person, "agent", &first)
            .await
            .unwrap()
            .unwrap(),
        receipt
    );
    assert_eq!(
        store
            .personal_agent_selection(&scope, &person, "agent")
            .await
            .unwrap(),
        Some(AgentSelectionState {
            generation: 2,
            selected: false
        }),
        "old replay must not undo later removal"
    );
    assert_eq!(
        store
            .personal_agent_selection_outcome(&scope, &person, "agent", "original-key")
            .await
            .unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(
        store
            .personal_agent_selection_outcome(&scope, &person, "agent", "not-committed")
            .await
            .unwrap(),
        None
    );
    for (scope, person, agent) in [
        (ScopeId::from("ws-b"), person.clone(), "agent"),
        (
            scope.clone(),
            owner("https://other.example", "account-a"),
            "agent",
        ),
        (
            scope.clone(),
            owner("https://native.example", "account-b"),
            "agent",
        ),
        (scope.clone(), person.clone(), "other-agent"),
    ] {
        assert_eq!(
            store
                .personal_agent_selection(&scope, &person, agent)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .personal_agent_selection_outcome(&scope, &person, agent, "original-key")
                .await
                .unwrap(),
            None
        );
        let separate = store
            .set_personal_agent_selection(&scope, &person, agent, &first)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            separate.applied_generation, 1,
            "same key belongs to a distinct verified owner tuple"
        );
    }
}

pub(super) async fn concurrent_cas(
    store: &impl PersonalAgentSelectionRepository,
    other: &impl PersonalAgentSelectionRepository,
) {
    let scope = ScopeId::from("race-ws");
    let person = owner("https://native.example", "account-a");
    let a = intent("request-a", 0, true);
    let b = intent("request-b", 0, false);
    let (left, right) = tokio::join!(
        store.set_personal_agent_selection(&scope, &person, "race", &a),
        other.set_personal_agent_selection(&scope, &person, "race", &b),
    );
    let results = [left.unwrap(), right.unwrap()];
    assert_eq!(results.iter().filter(|row| row.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|row| matches!(
                row,
                Err(AgentSelectionError::Conflict {
                    current_generation: 1
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        store
            .personal_agent_selection(&scope, &person, "race")
            .await
            .unwrap()
            .unwrap()
            .generation,
        1
    );
    let receipts = [
        store
            .personal_agent_selection_outcome(&scope, &person, "race", "request-a")
            .await
            .unwrap(),
        store
            .personal_agent_selection_outcome(&scope, &person, "race", "request-b")
            .await
            .unwrap(),
    ];
    assert_eq!(
        receipts.iter().filter(|receipt| receipt.is_some()).count(),
        1,
        "only the committed winner gets an immutable receipt"
    );
}
