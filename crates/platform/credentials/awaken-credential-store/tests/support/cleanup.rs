// @formal evidence credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
//! Shared cleanup-fence conformance, called by every repository backend.

use super::{CredentialError, CredentialMutationIntent, CredentialRepo, source};

pub async fn source_cleanup_rechecks_durable_truth(repo: &dyn CredentialRepo) {
    // Cause/effect: C1 exact abort WAL; C2 current Source still equals before;
    // C3 a delayed original request; C4 a newer intent occupies the same key.
    // R1 C1+!C2 rejects abort (including C3) and completion, retaining WAL;
    // R2 C1+C2 permits exact abort replay and completion; R3 C4 rejects an old
    // completion without retiring the newer fact. A repository put simulates
    // the committed-state interference; no external SecretStore IO is needed.
    let before = source("cred:abort-replay-fence", "ws");
    repo.put(before.clone()).await.unwrap();
    let mut after = before.clone();
    after.version = 2;
    let intent =
        CredentialMutationIntent::prepare(Some(before.clone()), after.clone(), 1_000).unwrap();
    repo.begin_mutation(intent.clone(), 1_000).await.unwrap();
    let abort = repo.abort_mutation(&intent).await.unwrap();
    repo.put(after.clone()).await.unwrap();
    for request in [&intent, &abort] {
        assert!(
            matches!(
                repo.abort_mutation(request).await,
                Err(CredentialError::MutationConflict(_))
            ),
            "R1 abort replay must recheck unpublished truth"
        );
    }
    assert!(
        matches!(
            repo.complete_mutation(&abort).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R1 completion"
    );
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        vec![abort.clone()],
        "R1 retained"
    );
    repo.put(before.clone()).await.unwrap();
    assert_eq!(repo.abort_mutation(&intent).await.unwrap(), abort, "R2");
    repo.complete_mutation(&abort).await.unwrap();
    after.version = 3;
    let next = CredentialMutationIntent::prepare(Some(before), after, 1_000).unwrap();
    repo.begin_mutation(next.clone(), 1_000).await.unwrap();
    assert!(
        matches!(
            repo.complete_mutation(&abort).await,
            Err(CredentialError::MutationConflict(_))
        ),
        "R3 late completion"
    );
    assert_eq!(
        repo.pending_mutations().await.unwrap(),
        vec![next.clone()],
        "R3 retained"
    );
    let next_abort = repo.abort_mutation(&next).await.unwrap();
    repo.complete_mutation(&next_abort).await.unwrap();
}
