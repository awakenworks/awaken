//! T27-C02: one provider instance must not adopt another instance's live
//! Environment just because both mint the same logical identity.
//!
//! Causes: C1 provider instances A and B are distinct physical owners of the
//! same kind; C2 both mint byte-equal `SandboxHandle`s (same `sandbox_id`);
//! C3 B is asked to rebind A's retained Environment.
//!
//! | Rule | rebinding provider | handle | Effect |
//! |---|---|---|---|
//! | F1 | A (own Environment) | equal | rebound, no new create |
//! | F2 | B | differing sandbox_id | Conflict before any adoption |
//! | F3 | B | byte-equal to A's | Conflict: capabilities must come from A's physical owner |
//!
//! Negative control: a logical-id-only check (`handle == handle`,
//! `sandbox_id == sandbox_id`) is executed against the F3 composition and
//! must accept it, proving such a check cannot be the identity oracle.

use super::*;

fn container_provider(
    provider: &Arc<FakeContainerProvider>,
) -> awaken_session_environment_contract::SandboxProviderRoles {
    SandboxEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_provider_roles()
}

fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
    counter.load(Ordering::SeqCst)
}

/// Logical-id-only identity check, the negative control.
fn logical_id_only_accepts(a: &pc::SandboxHandle, b: &pc::SandboxHandle) -> bool {
    a.sandbox_id == b.sandbox_id && a == b
}

#[tokio::test]
async fn foreign_instance_with_differing_identity_is_refused_and_logical_id_check_is_insufficient()
{
    let provider_a = Arc::new(FakeContainerProvider::default());
    let provider_b = Arc::new(FakeContainerProvider::default());
    let configured_a = container_provider(&provider_a);
    let configured_b = container_provider(&provider_b);
    let environment_a = configured_a
        .realizer()
        .create_unattached(&spec())
        .await
        .unwrap();
    let environment_b = configured_b
        .realizer()
        .create_unattached(&spec())
        .await
        .unwrap();
    let handle_a = environment_a.lifecycle().handle();
    let handle_b = environment_b.lifecycle().handle();

    // NC precondition: both instances mint the same logical identity.
    assert_eq!(handle_a, handle_b, "NC handles are byte-equal");
    assert_eq!(
        handle_a.sandbox_id, handle_b.sandbox_id,
        "NC same sandbox_id"
    );
    // NC executed: a logical-id-only check accepts mixing A and B, so it
    // cannot distinguish the physical source.
    assert!(
        logical_id_only_accepts(&handle_a, &handle_b),
        "NC: logical-id-only check accepts the foreign composition"
    );

    // F1 positive: A rebinds its own retained Environment without a create.
    let rebound = configured_a
        .adoption()
        .rebind_retained(
            &spec(),
            &handle_a,
            &environment_a,
            pc::SandboxEffect::Unattached,
        )
        .await
        .expect("F1 own Environment rebinds");
    assert_eq!(rebound.lifecycle().handle(), handle_a, "F1 exact handle");
    assert_eq!(count(&provider_a.creates), 1, "F1 no second create");

    // F2: B with a differing sandbox_id over A's Environment.
    let adopts_b = count(&provider_b.adopts);
    let disposals_a = count(&provider_a.disposals);
    let mut foreign = handle_a.clone();
    foreign.sandbox_id = "another-container".into();
    let error = configured_b
        .adoption()
        .rebind_retained(
            &spec(),
            &foreign,
            &environment_a,
            pc::SandboxEffect::Unattached,
        )
        .await
        .err()
        .expect("F2 differing identity must be refused");
    assert!(
        matches!(error, pc::SandboxError::Conflict(_)),
        "F2 stable Conflict class: {error:?}"
    );
    assert_eq!(count(&provider_b.adopts), adopts_b, "F2 zero adopts on B");
    assert_eq!(count(&provider_b.creates), 1, "F2 zero creates on B");
    assert_eq!(
        count(&provider_a.disposals),
        disposals_a,
        "F2 A disposals unchanged"
    );
}

#[tokio::test]
async fn foreign_instance_with_equal_handle_cannot_rebind_another_providers_environment() {
    let provider_a = Arc::new(FakeContainerProvider::default());
    let provider_b = Arc::new(FakeContainerProvider::default());
    let configured_a = container_provider(&provider_a);
    let configured_b = container_provider(&provider_b);
    let environment_a = configured_a
        .realizer()
        .create_unattached(&spec())
        .await
        .unwrap();
    let environment_b = configured_b
        .realizer()
        .create_unattached(&spec())
        .await
        .unwrap();
    let handle_a = environment_a.lifecycle().handle();
    assert_eq!(
        handle_a,
        environment_b.lifecycle().handle(),
        "same logical id"
    );
    assert!(logical_id_only_accepts(
        &handle_a,
        &environment_b.lifecycle().handle()
    ));

    let adopts_b = count(&provider_b.adopts);
    let creates_b = count(&provider_b.creates);
    let disposals_a = count(&provider_a.disposals);
    let result = configured_b
        .adoption()
        .rebind_retained(
            &spec(),
            &handle_a,
            &environment_a,
            pc::SandboxEffect::Unattached,
        )
        .await;
    assert_eq!(count(&provider_b.adopts), adopts_b, "F3 zero adopts on B");
    assert_eq!(
        count(&provider_b.creates),
        creates_b,
        "F3 zero creates on B"
    );
    assert_eq!(
        count(&provider_a.disposals),
        disposals_a,
        "F3 A disposals unchanged"
    );
    let error = result
        .err()
        .expect("F3 B must not rebind A's Environment under an equal handle");
    assert!(
        matches!(error, pc::SandboxError::Conflict(_)),
        "F3 stable Conflict class: {error:?}"
    );
}
