mod support;
use awaken_dispatch_contract::ClaimedCommitCommand;

#[tokio::test]
async fn claimed_port_shares_postgres_replay_and_error_laws() {
    let (_, _, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let command = ClaimedCommitCommand {
        claim,
        operation: support::operation("run", "session", 0, 0, "first"),
    };
    awaken_store_conformance::claimed_commit::exact_replay_and_rejections(
        &session, &session, command,
    )
    .await;
}
#[tokio::test]
async fn claimed_port_shares_postgres_concurrent_operation_laws() {
    let (_, _, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let command = ClaimedCommitCommand {
        claim,
        operation: support::operation("run", "session", 0, 0, "first"),
    };
    awaken_store_conformance::claimed_commit::concurrent_operation_has_one_winner(
        &session, &session, command,
    )
    .await;
}
#[tokio::test]
async fn claimed_port_shares_postgres_takeover_laws() {
    let (_, clock, _, session) = support::setup().await;
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    let stale = ClaimedCommitCommand {
        claim,
        operation: support::operation("run", "session", 0, 0, "first"),
    };
    clock.advance(1_000);
    let mut current = stale.clone();
    current.claim = session
        .claim("run", "replacement", 1_000)
        .await
        .unwrap()
        .run_claim();
    awaken_store_conformance::claimed_commit::stale_claim_cannot_publish(
        &session, &session, stale, current,
    )
    .await;
}
