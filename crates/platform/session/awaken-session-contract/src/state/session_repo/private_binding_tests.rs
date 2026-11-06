use super::*;
use crate::SessionPrivateBinding;

fn binding() -> SessionPrivateBinding {
    SessionPrivateBinding::new("org".into(), "desk".into(), "alice".into(), "ws".into(), 1).unwrap()
}

#[test]
fn private_binding_only_fresh_roots_can_bind_and_metadata_cannot_rebind() {
    let fresh = mutation_tests::session("s", SessionRevision(0));
    assert!(SessionPrivateBinding::of(&fresh).is_none());
    let mut personal = binding().bind_fresh(fresh.clone()).unwrap();
    assert!(binding().bind_fresh(personal.clone()).is_err());
    let mut existing = fresh.clone();
    existing.revision = SessionRevision(1);
    assert!(binding().bind_fresh(existing).is_err());
    let mut observed = fresh.clone();
    observed.created_at_unix_ms = Some(0);
    assert!(binding().bind_fresh(observed).is_err());
    personal
        .metadata
        .insert("enterprise_owner".into(), "mallory".into());
    assert_eq!(SessionPrivateBinding::of(&personal), Some(&binding()));
    assert_eq!(
        fresh.validate_observation_replacement(&personal),
        Err(SessionMutationValidationError::PrivateBindingChanged)
    );
    assert_eq!(
        personal.validate_observation_replacement(&fresh),
        Err(SessionMutationValidationError::PrivateBindingChanged)
    );
    let mut same = personal.clone();
    same.metadata.clear();
    assert_ne!(
        personal.validate_observation_replacement(&same),
        Err(SessionMutationValidationError::PrivateBindingChanged)
    );
    assert_ne!(
        SessionMutationPayload::default_create_idempotency(&fresh),
        SessionMutationPayload::default_create_idempotency(&personal)
    );
}
