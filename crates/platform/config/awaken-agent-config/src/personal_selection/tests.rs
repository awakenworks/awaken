use super::*;

fn intent(key: &str, generation: u64, selected: bool) -> AgentSelectionIntent {
    AgentSelectionIntent {
        request_key: key.into(),
        expected_generation: generation,
        selected,
    }
}

#[test]
fn selection_is_a_generation_fenced_preference_not_authority() {
    let initial = intent("original", 0, true);
    let AgentSelectionDecision::Apply { state, receipt } =
        decide_agent_selection(None, None, &initial).expect("first preference")
    else {
        panic!("absence is not replay");
    };
    assert_eq!(
        state,
        AgentSelectionState {
            generation: 1,
            selected: true
        }
    );
    assert_eq!(receipt.intent, initial);
    assert_eq!(receipt.applied_generation, 1);
    assert_eq!(
        decide_agent_selection(Some(state), None, &intent("another", 0, false)),
        Err(AgentSelectionError::Conflict {
            current_generation: 1
        })
    );
    let AgentSelectionDecision::Apply { state, .. } =
        decide_agent_selection(Some(state), None, &intent("remove", 1, false)).unwrap()
    else {
        panic!("new removal applies");
    };
    assert!(!state.selected);
    assert_eq!(state.generation, 2);
    assert_eq!(
        decide_agent_selection(Some(state), Some(&receipt), &initial),
        Ok(AgentSelectionDecision::Replay(receipt.clone())),
        "old outcome is recoverable without restoring the old mutable selection"
    );
    assert_eq!(state.generation, 2, "recovery carries no replacement write");
}

#[test]
fn original_selection_key_never_admits_changed_body_or_baseline() {
    let original = intent("fixed-key", 0, true);
    let receipt = AgentSelectionReceipt {
        intent: original.clone(),
        applied_generation: 1,
    };
    for changed in [
        intent("fixed-key", 0, false),
        intent("fixed-key", 1, true),
        intent("new-key", 0, true),
    ] {
        assert_eq!(
            decide_agent_selection(None, Some(&receipt), &changed),
            Err(AgentSelectionError::KeyReuse)
        );
    }
    assert_eq!(
        decide_agent_selection(None, Some(&receipt), &original),
        Ok(AgentSelectionDecision::Replay(receipt)),
        "the immutable receipt precedes mutable current-state interpretation"
    );
}

#[test]
fn verified_identity_coordinates_remain_exact_and_invalid_intents_fail_closed() {
    let person =
        AgentSelectionOwner::new("https://issuer.example/原生".into(), "Account-王敏".into())
            .unwrap();
    assert_eq!(person.issuer(), "https://issuer.example/原生");
    assert_eq!(person.account(), "Account-王敏");
    assert_ne!(
        person,
        AgentSelectionOwner::new("https://other.example".into(), "Account-王敏".into()).unwrap()
    );
    for (issuer, account) in [
        ("", "a"),
        (" issuer", "a"),
        ("issuer", "a\n"),
        ("issuer", ""),
    ] {
        assert_eq!(
            AgentSelectionOwner::new(issuer.into(), account.into()),
            Err(AgentSelectionError::Invalid)
        );
    }
    for key in [String::new(), " a".into(), "a\n".into(), "a".repeat(256)] {
        assert_eq!(
            decide_agent_selection(None, None, &intent(&key, 0, true)),
            Err(AgentSelectionError::Invalid)
        );
    }
    assert_eq!(
        decide_agent_selection(
            Some(AgentSelectionState {
                generation: u64::MAX,
                selected: false
            }),
            None,
            &intent("fixed", u64::MAX, true)
        ),
        Err(AgentSelectionError::GenerationExhausted)
    );
    for field in [
        "owner",
        "issuer",
        "workspace",
        "authority",
        "definition",
        "actor",
    ] {
        let mut body = serde_json::to_value(intent("fixed", 0, true)).unwrap();
        body[field] = serde_json::json!("caller-selected");
        assert!(
            serde_json::from_value::<AgentSelectionIntent>(body).is_err(),
            "unknown {field} cannot select identity or authority"
        );
    }
}
