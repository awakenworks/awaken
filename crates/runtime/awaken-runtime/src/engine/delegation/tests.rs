use super::*;
use awaken_agent_contract::agent::state::Action;
use awaken_agent_contract::audit::model_request::TokenUsage;
use awaken_runtime_contract::delegation::{ChildRunResultInbox, DelegationResultError};

#[test]
fn cached_child_lookup_preserves_the_complete_inbox_value_and_producer_law() {
    // CL1 absent inbox => None, no state change. CL2 recorded exact full value
    // => cloned value, no state change. CL3 exact producer retry => Duplicate;
    // changed text/model usage => Conflict; wrong child => ChildRunMismatch.
    // The lookup is not a second publisher and cannot repair or truncate data.
    let id = DelegationId::for_parent_call(&RunId("parent".into()), "call");
    let mut store = Store::new();
    assert_eq!(cached_child_run_result(&store, &id), Ok(None), "CL1");
    assert!(store.is_empty(), "CL1");
    let mut usage = ThreadUsage::default();
    usage.record(
        "model-a",
        TokenUsage {
            prompt_tokens: 2,
            completion_tokens: 3,
            cache_read_tokens: 5,
            cache_creation_tokens: 7,
        },
    );
    usage.record(
        "model-b",
        TokenUsage {
            prompt_tokens: 11,
            completion_tokens: 13,
            cache_read_tokens: 17,
            cache_creation_tokens: 19,
        },
    );
    let result = ChildRunResult {
        child_run_id: id.child_run_id(),
        text: "complete reply".into(),
        usage,
    };
    let mut inbox = ChildRunResultInbox::default();
    assert_eq!(
        inbox.record(id.clone(), result.clone()),
        Ok(ResultRecord::Recorded)
    );
    assert_eq!(
        inbox.record(id.clone(), result.clone()),
        Ok(ResultRecord::Duplicate),
        "CL3"
    );
    let mut changed = result.clone();
    changed.text.push('!');
    assert_eq!(
        inbox.record(id.clone(), changed),
        Err(DelegationResultError::Conflict),
        "CL3"
    );
    let mut changed = result.clone();
    changed
        .usage
        .by_model
        .get_mut("model-b")
        .unwrap()
        .cache_creation_tokens += 1;
    assert_eq!(
        inbox.record(id.clone(), changed),
        Err(DelegationResultError::Conflict),
        "CL3"
    );
    let mut changed = result.clone();
    changed.child_run_id = RunId("another-child".into());
    assert_eq!(
        inbox.record(id.clone(), changed),
        Err(DelegationResultError::ChildRunMismatch),
        "CL3"
    );
    store.apply(&PendingChildRunResults::write(&inbox));
    let before = store.clone();
    assert_eq!(
        cached_child_run_result(&store, &id),
        Ok(Some(result)),
        "CL2"
    );
    assert_eq!(store, before, "CL2: no write or consume");
}

#[test]
fn cached_child_lookup_rejects_shape_and_identity_drift_without_mutation() {
    // CL4 malformed persisted inbox => original StateError with address, no
    // mutation. CL5 structurally typed but wrong-child historical envelope =>
    // StateError from the inbox's original producer identity law, no repair or
    // relabelling. Cold reads cannot turn invalid history into a cache hit.
    let id = DelegationId::for_parent_call(&RunId("parent".into()), "call");
    let mut command = PendingChildRunResults::write(&ChildRunResultInbox::default());
    command.action = Action::Set(serde_json::json!({"pending": false}));
    let mut store = Store::rebuild(&[command.clone()]);
    let before = store.clone();
    let error = cached_child_run_result(&store, &id).unwrap_err();
    assert_eq!(error.key, PendingChildRunResults::KEY, "CL4");
    assert_eq!(error.scope, PendingChildRunResults::SCOPE, "CL4");
    assert_eq!(store, before, "CL4: malformed state is retained");
    let result = ChildRunResult {
        child_run_id: RunId("historical-wrong-child".into()),
        text: "old payload".into(),
        usage: ThreadUsage::default(),
    };
    command.action = Action::Set(serde_json::json!({"pending": {(id.0.clone()): result}}));
    store = Store::rebuild(&[command]);
    let before = store.clone();
    let error = cached_child_run_result(&store, &id).unwrap_err();
    assert_eq!(error.key, PendingChildRunResults::KEY, "CL5");
    assert!(error.detail.contains("stable child Run"), "CL5");
    assert_eq!(store, before, "CL5: no repair, consume or qualification");
}
