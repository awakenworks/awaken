use super::*;
use awaken_session_types::SessionRecoveryQuarantine;
use std::collections::VecDeque;
use std::sync::Mutex;

type Page = (
    Option<&'static str>,
    Result<SessionScan, SessionRepositoryError>,
);

struct ScriptedRepository(Mutex<VecDeque<Page>>);

#[async_trait::async_trait]
impl SessionRepository for ScriptedRepository {
    async fn scan(
        &self,
        query: SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        let SessionScanQuery::Reconcilable { after } = query else {
            panic!("Worker demand must read the canonical reconciliation index")
        };
        let (expected, page) = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra scan");
        assert_eq!(after.map(|cursor| cursor.session_id().as_str()), expected);
        page
    }

    async fn create(
        &self,
        _: &str,
        _: PersistedSession,
        _: IdempotencyRecord,
        _: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        panic!("capacity reads must not create Sessions")
    }
    async fn replay_create(
        &self,
        _: &str,
        _: &SessionId,
        _: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        panic!("capacity reads must not admit commands")
    }
    async fn commit(
        &self,
        _: &str,
        _: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        panic!("capacity reads must not mutate Sessions")
    }
    async fn get(&self, _: &SessionId) -> Result<PersistedSession, SessionRepositoryError> {
        panic!("scan already contains authoritative roots")
    }
    async fn owner(&self, _: &SessionId) -> Result<String, SessionRepositoryError> {
        panic!("scan already contains authoritative owners")
    }
    async fn idempotency_receipt(
        &self,
        _: &SessionId,
        _: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        panic!("capacity reads do not replay commands")
    }
    async fn deleted_event_admissions(
        &self,
        _: &str,
        _: &SessionId,
    ) -> Result<Option<DeletedSessionEventAdmissions>, SessionRepositoryError> {
        panic!("capacity reads do not admit Events")
    }
    async fn count_environment_phase(
        &self,
        _: SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        panic!("capacity reads must not substitute another index")
    }
}

fn scripted(pages: impl IntoIterator<Item = Page>) -> ScriptedRepository {
    ScriptedRepository(Mutex::new(pages.into_iter().collect()))
}

fn page(next: Option<&str>) -> SessionScan {
    SessionScan {
        next_cursor: next.map(SessionScanCursor::after_session_id),
        ..Default::default()
    }
}

#[tokio::test]
async fn demand_reads_every_page_before_proving_an_empty_index() {
    // P1 an empty index returns zero; P2 intermediate empty healthy pages
    // cannot prove absence until the authoritative cursor reaches its end.
    for pages in [
        vec![(None, Ok(page(None)))],
        vec![
            (None, Ok(page(Some("a")))),
            (Some("a"), Ok(page(Some("b")))),
            (Some("b"), Ok(page(None))),
        ],
    ] {
        let repository = scripted(pages);
        assert_eq!(repository.worker_demand().await, Ok(0));
        assert!(repository.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn demand_rejects_repeated_and_rewound_cursors() {
    // P3 repeated/descending cursors cannot spin forever or manufacture an
    // apparently complete capacity count. Both are corrupt authority evidence.
    for next in ["b", "a"] {
        let repository = scripted([
            (None, Ok(page(Some("b")))),
            (Some("b"), Ok(page(Some(next)))),
        ]);
        assert_eq!(
            repository.worker_demand().await,
            Err(SessionRepositoryError::Corrupt(
                "Session reconciliation cursor did not advance".into()
            )),
        );
    }
}

#[tokio::test]
async fn demand_preserves_read_failures_and_quarantine_after_a_partial_scan() {
    // P4 an unavailable/corrupt later page is an error, never zero demand;
    // P5 quarantine evidence is fail-closed even when healthy rows are empty.
    let mut quarantined = page(None);
    quarantined.quarantined.push(SessionRecoveryQuarantine {
        session_id: "broken-root".into(),
        reason: "invalid aggregate".into(),
    });
    for (last, expected) in [
        (
            Err(SessionRepositoryError::AuthorityUnavailable(
                "read failed".into(),
            )),
            SessionRepositoryError::AuthorityUnavailable("read failed".into()),
        ),
        (
            Err(SessionRepositoryError::Corrupt("row failed".into())),
            SessionRepositoryError::Corrupt("row failed".into()),
        ),
        (
            Ok(quarantined),
            SessionRepositoryError::Corrupt(
                "Session reconciliation scan contains quarantined roots".into(),
            ),
        ),
    ] {
        let repository = scripted([(None, Ok(page(Some("a")))), (Some("a"), last)]);
        assert_eq!(repository.worker_demand().await, Err(expected));
    }
}
