//! T05-C02 (回原提交回执): an update replay returns the ORIGINAL command
//! receipt after a later independent root write, and a retry that reuses the
//! key with a changed request fingerprint is refused.
//!
//! Oracles (groups JSON order):
//! - O0 the replay returns the earlier command's original `committed_revision`
//!   and receipt;
//! - O1 a retry that changes the fingerprint is rejected.
//!
//! Negative control: an implementation that rebuilds the receipt from the
//! current root must fail. It is executed: the same `is_original_receipt`
//! checker is run against a receipt rebuilt from the current root revision and
//! must reject it, and a later independent root write guarantees the current
//! revision really differs from the original command revision.
//!
//! | Row | Key | Fingerprint | Schedule | Effect |
//! |---|---|---|---|---|
//! | F1 | k | A | first | applied, receipt R |
//! | F2 | k | A | after a later root write | replay R, not current revision |
//! | F3 | k | B | sequential after F1 | IdempotencyMismatch, root unchanged |
//! | F4 | k2 | B vs A | B paused before its CAS while A commits | IdempotencyMismatch, root unchanged |
//! | NC | - | - | receipt rebuilt from the current root | rejected by the F2 checker |
use crate::{SessionFieldUpdate, SessionUpdateCommand, SessionUpdateError, SessionUpdateOutcome};
use awaken_session_contract::SessionRepositoryReads as _;
use awaken_session_types::SessionId;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

fn title_command(title: &str, key: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        title: Some(SessionFieldUpdate::Replace(title.into())),
        metadata: None,
        budget: None,
        tools: None,
        mcp_update: None,
        idempotency_key: Some(key.into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&title),
        if_match: None,
    }
}

/// O0 checker: a replay carries the original command coordinate, never a
/// coordinate derived from the root it reloaded.
fn is_original_receipt(replay: &SessionUpdateOutcome, original: &SessionUpdateOutcome) -> bool {
    !replay.command_applied
        && replay.command_revision == original.command_revision
        && replay.session.revision >= replay.command_revision
}

async fn fixture(
    id: &SessionId,
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<super::super::FaultingSessionRepository>,
    Arc<super::super::RecordingEnvironmentSource>,
    crate::SessionApplication,
) {
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("fingerprint repository"),
    );
    super::super::create(
        durable.as_ref(),
        super::super::persisted(id.as_str(), false, "idle"),
    )
    .await;
    let repo = Arc::new(super::super::FaultingSessionRepository::new(
        durable.clone(),
    ));
    let environments = Arc::new(super::super::RecordingEnvironmentSource::default());
    let app = super::super::application(repo.clone(), environments.clone());
    (durable, repo, environments, app)
}

#[tokio::test]
async fn update_replay_returns_the_original_receipt_and_refuses_a_changed_fingerprint() {
    let id = SessionId::from("fingerprint-retry");
    let (durable, _repo, environments, app) = fixture(&id).await;

    // F1: the original command.
    let original = app
        .update_commands
        .update_session(&id, title_command("first", "shared"))
        .await
        .expect("F1 original command");
    assert!(original.command_applied, "F1 applied");
    assert_eq!(
        original.command_revision, original.session.revision,
        "F1 receipt is the committing revision"
    );

    // A later, independent root write advances the current revision.
    let before_authoring = durable.get(&id).await.unwrap();
    let mut later = before_authoring.clone();
    later.metadata.insert("later".into(), "write".into());
    let advanced = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            awaken_store_conformance::session_store::record_update(&before_authoring, later, 0),
            "independent-after-fingerprint-command",
            Vec::new(),
        )
        .await
        .expect("independent root write");
    assert!(advanced.transitioned(), "the later root write applied");
    let current = durable.get(&id).await.unwrap();
    assert!(
        current.revision > original.command_revision,
        "the current root is a distinct, newer coordinate"
    );

    // F2/O0: the exact retry replays the original receipt.
    let replay = app
        .update_commands
        .update_session(&id, title_command("first", "shared"))
        .await
        .expect("F2 exact retry");
    assert!(is_original_receipt(&replay, &original), "F2/O0 {replay:?}");
    assert_eq!(replay.command_revision, original.command_revision, "F2/O0");
    assert_eq!(
        replay.session.revision, current.revision,
        "F2 current root reloaded"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        current,
        "F2 no root mutation"
    );

    // NC: the receipt an implementation would rebuild from the current root.
    let mut rebuilt = replay.clone();
    rebuilt.command_revision = replay.session.revision;
    assert!(
        !is_original_receipt(&rebuilt, &original),
        "NC: a receipt rebuilt from the current root is detected"
    );

    // F3/O1: the same key with a changed fingerprint is refused, root unchanged.
    let changed = app
        .update_commands
        .update_session(&id, title_command("changed", "shared"))
        .await;
    assert!(
        matches!(changed, Err(SessionUpdateError::IdempotencyMismatch)),
        "F3/O1 {changed:?}"
    );
    let after = durable.get(&id).await.unwrap();
    assert_eq!(after, current, "F3/O1 no root mutation");
    assert_eq!(after.title.as_deref(), Some("first"), "F3/O1 title kept");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        0,
        "metadata commands never start execution"
    );
}

#[tokio::test]
async fn a_changed_fingerprint_racing_the_original_commit_is_refused_at_the_cas() {
    // F4: both requests pass the receipt fast path before either commits; the
    // changed request is held at the real repository CAS while the original
    // commits the same key. On release the durable receipt refuses it.
    let id = SessionId::from("fingerprint-race");
    let (durable, repo, _environments, app) = fixture(&id).await;
    let app = Arc::new(app);
    let pause = repo.pause_next_mutation("managed:update-command:");
    let delayed_app = app.clone();
    let delayed_id = id.clone();
    let delayed = tokio::spawn(async move {
        delayed_app
            .update_commands
            .update_session(&delayed_id, title_command("changed", "racing"))
            .await
    });
    tokio::time::timeout(WAIT, pause.entered.notified())
        .await
        .expect("F4 changed request reaches the CAS");
    let winner = app
        .update_commands
        .update_session(&id, title_command("original", "racing"))
        .await
        .expect("F4 original commits");
    assert!(winner.command_applied, "F4 winner applied");
    let committed = durable.get(&id).await.unwrap();
    pause.release.notify_one();
    let refused = tokio::time::timeout(WAIT, delayed)
        .await
        .expect("F4 delayed request completes")
        .expect("F4 task joins");
    assert!(
        matches!(refused, Err(SessionUpdateError::IdempotencyMismatch)),
        "F4/O1 {refused:?}"
    );
    let after = durable.get(&id).await.unwrap();
    assert_eq!(after, committed, "F4/O1 refused candidate never commits");
    assert_eq!(after.title.as_deref(), Some("original"), "F4/O1");
    let replay = app
        .update_commands
        .update_session(&id, title_command("original", "racing"))
        .await
        .expect("F4 winner replay");
    assert!(is_original_receipt(&replay, &winner), "F4/O0 {replay:?}");
}
