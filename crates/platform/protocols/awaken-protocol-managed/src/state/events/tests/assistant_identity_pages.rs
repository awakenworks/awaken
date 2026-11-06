//! T31-C01: an assistant response's public id, ordinal and page cursor are a
//! function of its durable Run/Step/response coordinates. Paging, a lost
//! cache and an active peer never renumber it.

use super::*;

const STEP: usize = 4;

fn replica(
    runtime: &RehydrateFake,
    repo: Arc<dyn awaken_session_application::SessionStore>,
) -> ManagedState {
    let session_fixture_clock = awaken_time::system_clock();
    crate::state::test_support::managed_state_from_parts(
        std::sync::Arc::new(runtime.clone()),
        None,
        repo,
        awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
            .execution,
        session_fixture_clock,
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
}

/// One `(event id, event type, next-page cursor)` per event, walking the
/// timeline one event per page.
fn paged(state: &ManagedState, session_id: &str) -> Vec<(String, String, Option<String>)> {
    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..256 {
        let page = state
            .list_events(session_id, cursor.as_deref(), Some(1), false)
            .expect("list one page");
        if page.data.is_empty() {
            break;
        }
        for event in page.data {
            walked.push((
                event.id.clone(),
                event.type_str().to_owned(),
                page.next_page.clone(),
            ));
        }
        match page.next_page {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    walked
}

fn is_assistant(event_type: &str) -> bool {
    event_type == "agent.thinking" || event_type == "agent.message"
}

fn role(event_type: &str) -> &'static str {
    if event_type == "agent.thinking" {
        "agent.thinking"
    } else {
        "agent.message"
    }
}

/// | Rule | read | replica | Effect |
/// |---|---|---|---|
/// | I1 | one event per page | warm | assistant ids carry ordinals 0,0,1,2 from coordinates |
/// | I2 | one event per page | cold restart and active peer | identical ids, order and cursors |
/// | NC | per-read index numbering of the same pages | any | the identity oracle rejects it |
#[tokio::test]
async fn paged_assistant_identity_and_cursors_survive_cache_loss_and_peers() {
    let repo: Arc<dyn awaken_session_application::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    let runtime = RehydrateFake::default();
    let warm = replica(&runtime, repo.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let run = RunId("run-paged-response-identity".into());
    runtime.committed_by_thread.lock().insert(
        session.id.clone(),
        vec![
            Message::new(
                MessageId::assistant_truncated(&run, STEP, 0),
                Role::Assistant,
                vec![
                    ContentBlock::thinking("private partial"),
                    ContentBlock::text("first partial"),
                ],
            ),
            Message::text(
                MessageId::assistant_truncated(&run, STEP, 1),
                Role::Assistant,
                "second partial",
            ),
            Message::text(
                MessageId::assistant(&run, STEP),
                Role::Assistant,
                "final response",
            ),
        ],
    );
    runtime.lifecycle.lock().extend([
        lifecycle(
            1,
            &session.id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &session.id,
            &run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    warm.refresh_committed_events(&session.id).await.unwrap();

    let expected = [
        ("agent.thinking", 0),
        ("agent.message", 0),
        ("agent.message", 1),
        ("agent.message", 2),
    ]
    .map(|(event_type, response)| {
        managed_assistant_event_id(&session.id, &session.id, &run.0, STEP, response, event_type)
    })
    .to_vec();
    // The identity oracle over one walked timeline.
    let identity = |walked: &[(String, String, Option<String>)]| -> Vec<String> {
        walked
            .iter()
            .filter(|(_, event_type, _)| is_assistant(event_type))
            .map(|(id, _, _)| id.clone())
            .collect()
    };

    let warm_walk = paged(&warm, &session.id);
    assert_eq!(identity(&warm_walk), expected, "I1 coordinate-derived ids");
    let full = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    assert_eq!(
        warm_walk
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect::<Vec<_>>(),
        full,
        "I1 paging neither drops nor renumbers an event"
    );

    let restarted = replica(&runtime, repo.clone());
    restarted.ensure_session(&session.id).await.unwrap();
    assert_eq!(paged(&restarted, &session.id), warm_walk, "I2 cold restart");
    let peer = replica(&runtime, repo);
    peer.ensure_session(&session.id).await.unwrap();
    peer.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(paged(&peer, &session.id), warm_walk, "I2 active peer");

    // NC: number each read's assistant events by their index in that read.
    // One event per page restarts the index on every page.
    let per_read_index = warm_walk
        .iter()
        .filter(|(_, event_type, _)| is_assistant(event_type))
        .map(|(_, event_type, _)| {
            managed_assistant_event_id(&session.id, &session.id, &run.0, STEP, 0, role(event_type))
        })
        .collect::<Vec<_>>();
    assert_ne!(
        per_read_index, expected,
        "NC per-read index numbering is rejected by the identity oracle"
    );
}
