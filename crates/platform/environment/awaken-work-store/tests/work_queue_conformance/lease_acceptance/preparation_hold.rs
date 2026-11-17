//! A Run-free preparation hands back only the Session Work it still holds
//! (ADR-0106), on every Work backend, through two independent handles.
//!
//! The Session root decides "release" from a read that cannot see the Work
//! store. Between that decision and the hand-back, the same Worker's Run may
//! acquire the Work. The hand-back is one store compare-and-release under the
//! lock every acquisition takes, so that Run keeps its Work.
//!
//! Causes: C1 a preparation claimed the Work (`acquire_preparation_session`);
//! C2 between the decision and the hand-back, on the other handle, the same
//! Worker's Run acquired it (`acquire_session`), a Realization renewal renewed
//! it (`renew_session`), or the preparation re-claimed it; C3 the hand-back
//! names this owner or another. Effects: E1 Work stopped, `true`; E2 Work
//! unchanged (same owner and epoch, still active), `false`.
//!
//! | Rule | claimed by  | in the gap               | hand-back owner | effect |
//! |------|-------------|--------------------------|-----------------|--------|
//! | H1   | preparation | nothing                  | same            | E1     |
//! | H1r  | preparation | Realization renewal      | same            | E1     |
//! | H1p  | preparation | the preparation re-claims| same            | E1     |
//! | H2   | preparation | the same Worker's Run    | same            | E2     |
//! | H3   | preparation | nothing                  | another         | E2     |
//! | H4   | preparation | already handed back      | same            | E2     |
//! | H6   | a Run       | nothing                  | same            | E2     |

use super::*;
use awaken_executable_environment_contract::WorkQueueError;

const OWNER: &str = "preparing-worker:boot";

#[derive(Clone, Copy, Debug)]
enum Gap {
    Nothing,
    RealizationRenewal,
    PreparationReclaim,
    RunAcquisition,
    AlreadyHandedBack,
}

/// One row on `pair`: returns whether the hand-back released, and the
/// Work's state afterwards.
async fn hand_back(
    pair: &Pair,
    row: &str,
    claimed_by_preparation: bool,
    gap: Gap,
    hand_back_owner: &str,
) -> Result<(bool, WorkState), WorkQueueError> {
    let env = format!("prep-{row}");
    let session = SessionId::from(format!("prep-session-{row}"));
    let work_id = pair.a.enqueue_session(&env, &session).await?;
    let claimed = if claimed_by_preparation {
        pair.a
            .acquire_preparation_session(&env, &session, OWNER)
            .await?
    } else {
        pair.a.acquire_session(&env, &session, OWNER).await?
    }
    .unwrap_or_else(|| panic!("{} {row} C1 claim", pair.label));
    // The Session root decided "release" here; the gap runs on the other handle.
    match gap {
        Gap::Nothing => {}
        Gap::RealizationRenewal => {
            pair.b.renew_session(&env, &session, OWNER).await?;
        }
        Gap::PreparationReclaim => {
            pair.b
                .acquire_preparation_session(&env, &session, OWNER)
                .await?;
        }
        Gap::RunAcquisition => {
            let run = pair
                .b
                .acquire_session(&env, &session, OWNER)
                .await?
                .unwrap_or_else(|| panic!("{} {row} C2 the Run acquires", pair.label));
            assert_eq!(
                (run.work_id(), run.owner(), run.epoch()),
                (claimed.work_id(), claimed.owner(), claimed.epoch()),
                "{} {row} C2 the Run renews the very lease the preparation claimed",
                pair.label
            );
        }
        Gap::AlreadyHandedBack => {
            assert!(
                pair.b
                    .release_preparation_session(&env, &session, OWNER)
                    .await?,
                "{} {row} C2 first hand-back",
                pair.label
            );
        }
    }
    let released = pair
        .a
        .release_preparation_session(&env, &session, hand_back_owner)
        .await?;
    let state = pair
        .a
        .get(&env, &work_id)
        .await?
        .unwrap_or_else(|| panic!("{} {row} Work row", pair.label))
        .state;
    if matches!(gap, Gap::RunAcquisition) {
        let current = pair
            .b
            .renew_session(&env, &session, OWNER)
            .await?
            .unwrap_or_else(|| panic!("{} {row} E2 the Run still holds its Work", pair.label));
        assert_eq!(
            (current.owner(), current.epoch()),
            (OWNER, claimed.epoch()),
            "{} {row} E2 same owner and epoch",
            pair.label
        );
        assert!(
            pair.b.release_session(&current).await?,
            "{} {row} the Run's own settlement still releases",
            pair.label
        );
    }
    Ok((released, state))
}

async fn preparation_hold_table(pair: Pair) {
    for (row, by_preparation, gap, owner, released, state) in [
        ("h1", true, Gap::Nothing, OWNER, true, WorkState::Stopped),
        (
            "h1r",
            true,
            Gap::RealizationRenewal,
            OWNER,
            true,
            WorkState::Stopped,
        ),
        (
            "h1p",
            true,
            Gap::PreparationReclaim,
            OWNER,
            true,
            WorkState::Stopped,
        ),
        (
            "h2",
            true,
            Gap::RunAcquisition,
            OWNER,
            false,
            WorkState::Active,
        ),
        (
            "h3",
            true,
            Gap::Nothing,
            "another-worker:boot",
            false,
            WorkState::Active,
        ),
        (
            "h4",
            true,
            Gap::AlreadyHandedBack,
            OWNER,
            false,
            WorkState::Stopped,
        ),
        ("h6", false, Gap::Nothing, OWNER, false, WorkState::Active),
    ] {
        let observed = hand_back(&pair, row, by_preparation, gap, owner)
            .await
            .unwrap_or_else(|error| panic!("{} {row}: {error}", pair.label));
        assert_eq!(observed, (released, state), "{} {row}", pair.label);
    }
    pair.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preparation_hands_back_only_work_no_run_acquired_on_volatile_and_file_sqlite() {
    preparation_hold_table(Pair::volatile()).await;
    preparation_hold_table(Pair::sqlite_file().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_preparation_hands_back_only_work_no_run_acquired() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    preparation_hold_table(Pair::postgres(&url).await).await;
}
