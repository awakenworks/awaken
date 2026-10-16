// @formal surface exclude=test_support_crate
// @formal owner crates/stores/awaken-session-store/tests/session_repo_conformance.rs
// @formal reason: Test-only SessionStore suite execution; every backend keeps the same rules and assertions.
//! Bounded execution of the shared SessionStore rules, separate from their domain assertions.

use super::{
    absent_id_reads_none, concurrent_root_cas_has_one_authoritative_winner,
    create_receipt_decision_table, credential_source_dependency_decision_table,
    deleted_event_admission_decision_table, environment_state_is_atomic_and_non_destructive,
    global_environment_phase_count_has_no_recovery_batch_limit,
    lifecycle_outbox_tracks_every_committed_transition, ownership_is_one_atomic_repository_fact,
    pending_resource_activation_index_is_durable, root_cas_decision_table, save_get_round_trips,
    save_is_idempotent_upsert, scan_query_decision_table,
    tombstone_requires_hidden_completed_cleanup,
    vault_reference_index_returns_only_live_scoped_sessions,
    worker_realization_owner_scan_decision_table,
};

pub async fn run_suite<R, F>(fresh: impl Fn() -> F)
where
    R: awaken_session_repository_contract::SessionStore + 'static,
    F: std::future::Future<Output = R>,
{
    // Construct each table's future inside its callback. This bounds the native
    // debug poll frame as the shared suite grows, including on Windows test
    // threads. Each table retains its own fresh backend and complete assertions.
    type Case<R> =
        for<'a> fn(&'a R) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
    type Race<R> =
        fn(std::sync::Arc<R>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
    let cases: &[Case<R>] = &[
        |repo| Box::pin(save_get_round_trips(repo)),
        |repo| Box::pin(absent_id_reads_none(repo)),
        |repo| Box::pin(save_is_idempotent_upsert(repo)),
        |repo| Box::pin(ownership_is_one_atomic_repository_fact(repo)),
        |repo| Box::pin(environment_state_is_atomic_and_non_destructive(repo)),
        |repo| {
            Box::pin(global_environment_phase_count_has_no_recovery_batch_limit(
                repo,
            ))
        },
        |repo| {
            Box::pin(vault_reference_index_returns_only_live_scoped_sessions(
                repo,
            ))
        },
        |repo| Box::pin(scan_query_decision_table(repo)),
        |repo| Box::pin(worker_realization_owner_scan_decision_table(repo)),
        |repo| Box::pin(credential_source_dependency_decision_table(repo)),
        |repo| Box::pin(lifecycle_outbox_tracks_every_committed_transition(repo)),
        |repo| Box::pin(pending_resource_activation_index_is_durable(repo)),
        |repo| Box::pin(tombstone_requires_hidden_completed_cleanup(repo)),
        |repo| Box::pin(deleted_event_admission_decision_table(repo)),
    ];
    for case in cases {
        let repo = fresh().await;
        case(&repo).await;
    }
    let root_cas: Case<R> = |repo| Box::pin(root_cas_decision_table(repo));
    let create_receipt: Case<R> = |repo| Box::pin(create_receipt_decision_table(repo));
    let concurrent_root_cas: Race<R> =
        |repo| Box::pin(concurrent_root_cas_has_one_authoritative_winner(repo));
    let cas_repo = fresh().await;
    root_cas(&cas_repo).await;
    create_receipt(&fresh().await).await;
    concurrent_root_cas(std::sync::Arc::new(fresh().await)).await;
}
