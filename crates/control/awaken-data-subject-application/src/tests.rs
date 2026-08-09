use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct InMemoryDataSubjectRepo {
    subjects: Mutex<BTreeMap<String, DataSubject>>,
    erasures: Mutex<BTreeMap<String, ErasureProgress>>,
}

impl InMemoryDataSubjectRepo {
    fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DataSubjectRepo for InMemoryDataSubjectRepo {
    async fn create(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let mut subjects = self.subjects.lock().expect("test subjects");
        if subjects.contains_key(&subject.id.0) {
            return Err(DataSubjectError::AlreadyExists(subject.id.0));
        }
        subjects.insert(subject.id.0.clone(), subject);
        Ok(())
    }

    async fn compare_and_swap(
        &self,
        expected_revision: u64,
        subject: DataSubject,
    ) -> Result<(), DataSubjectError> {
        let mut subjects = self.subjects.lock().expect("test subjects");
        match subjects.get(&subject.id.0) {
            Some(current)
                if current.revision == expected_revision
                    && expected_revision.checked_add(1) == Some(subject.revision) =>
            {
                subjects.insert(subject.id.0.clone(), subject);
                Ok(())
            }
            _ => Err(DataSubjectError::Conflict(subject.id.0)),
        }
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        self.subjects
            .lock()
            .expect("test subjects")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| DataSubjectError::NotFound(id.0.clone()))
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        Ok(self
            .subjects
            .lock()
            .expect("test subjects")
            .values()
            .filter(|subject| subject.org == org)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl ErasureJobRepo for InMemoryDataSubjectRepo {
    async fn load(&self, id: &DataSubjectId) -> Result<Option<ErasureProgress>, DataSubjectError> {
        Ok(self
            .erasures
            .lock()
            .expect("test erasures")
            .get(&id.0)
            .cloned())
    }

    async fn compare_and_swap_progress(
        &self,
        id: &DataSubjectId,
        expected_revision: Option<u64>,
        progress: &ErasureProgress,
    ) -> Result<(), DataSubjectError> {
        if !progress.follows(expected_revision) {
            return Err(DataSubjectError::Conflict(id.0.clone()));
        }
        let mut erasures = self.erasures.lock().expect("test erasures");
        if erasures.get(&id.0).map(|value| value.revision) != expected_revision {
            return Err(DataSubjectError::Conflict(id.0.clone()));
        }
        erasures.insert(id.0.clone(), progress.clone());
        Ok(())
    }
}
use awaken_runtime_contract::{DataSubjectConsentSource, DataSubjectResolver};

fn granted(purpose: Purpose) -> ConsentGrant {
    ConsentGrant {
        purpose,
        status: ConsentStatus::Granted,
        basis: LawfulBasis::Consent,
        granted_at: 1,
        version: "v1".into(),
    }
}

// DS3: a Pending (in-flight, not-yet-a-permit) grant ceils to Structured, the
// same as no grant at all — only an active `Granted` grant lifts to Full.
#[test]
fn consent_ceiling_pending_grant_is_structured() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(ConsentGrant {
        status: ConsentStatus::Pending,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured
    );
}

// FAIL-CLOSED: if the aggregate is deserialized from a legacy/tampered blob
// that holds BOTH a Withdrawn and a Granted grant for one purpose (a state the
// writer never produces), the read must honour the withdrawal — otherwise a
// withdrawn subject leaks as `Full` (a GDPR breach). `consents` is a pub field,
// so we can construct that broken state directly, as a bad blob would.
#[test]
fn consent_ceiling_withdrawal_vetoes_a_stale_granted_duplicate() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.consents.push(granted(Purpose::TelemetryContent));
    s.consents.push(ConsentGrant {
        status: ConsentStatus::Withdrawn,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured,
        "a Withdrawn grant must veto a coexisting stale Granted grant"
    );
    // order-independent: withdrawal first, grant second, same verdict.
    let mut s2 = DataSubject::new(DataSubjectId("dsub_2".into()), "org_1", 0);
    s2.consents.push(ConsentGrant {
        status: ConsentStatus::Withdrawn,
        ..granted(Purpose::TelemetryContent)
    });
    s2.consents.push(granted(Purpose::TelemetryContent));
    assert_eq!(
        s2.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured
    );
}

// upsert (b): at most one grant per purpose, even across several purposes —
// re-upserting one purpose supersedes only that purpose's grant.
#[test]
fn upsert_keeps_at_most_one_grant_per_purpose() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    s.upsert_consent(granted(Purpose::EvalRecording));
    assert_eq!(s.consents.len(), 2, "two distinct purposes coexist");
    // Re-upsert TelemetryContent as Withdrawn: supersedes only that purpose.
    s.upsert_consent(ConsentGrant {
        status: ConsentStatus::Withdrawn,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(s.consents.len(), 2, "still at most one per purpose");
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured,
        "superseded to Withdrawn"
    );
    assert_eq!(
        s.consent_ceiling(Purpose::EvalRecording),
        ContentCapture::Full,
        "the other purpose is untouched"
    );
}

#[test]
fn consent_ceiling_is_full_only_with_an_active_grant() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured
    );
    s.upsert_consent(granted(Purpose::TelemetryContent));
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Full
    );
    // A different purpose is unaffected.
    assert_eq!(
        s.consent_ceiling(Purpose::EvalRecording),
        ContentCapture::Structured
    );
}

#[test]
fn mark_erased_withdraws_grants_but_keeps_them_as_audit() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    s.mark_erased(999);
    // The grant is RETAINED (accountability proof), just withdrawn.
    assert_eq!(s.consents.len(), 1);
    assert_eq!(s.consents[0].status, ConsentStatus::Withdrawn);
    assert_eq!(s.erased_at, Some(999));
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured
    );
}

#[test]
fn upsert_supersedes_same_purpose_and_keeps_withdrawal() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    s.upsert_consent(ConsentGrant {
        status: ConsentStatus::Withdrawn,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(s.consents.len(), 1);
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured
    );
}

// upsert transition (Pending → Granted): completing an in-flight enrollment
// supersedes the Pending grant and lifts the ceiling to Full.
#[test]
fn upsert_pending_then_granted_completes_enrollment() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(ConsentGrant {
        status: ConsentStatus::Pending,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured,
        "Pending does not permit"
    );
    s.upsert_consent(granted(Purpose::TelemetryContent));
    assert_eq!(s.consents.len(), 1, "still at most one per purpose");
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Full,
        "enrollment completed → Full"
    );
}

// upsert transition (Withdrawn → Granted): a re-consent after withdrawal
// supersedes the withdrawal and restores the Full ceiling.
#[test]
fn upsert_reconsent_after_withdrawal_restores_full() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(ConsentGrant {
        status: ConsentStatus::Withdrawn,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Structured
    );
    // Re-consent with a fresh (newer) version.
    s.upsert_consent(ConsentGrant {
        version: "v2".into(),
        granted_at: 2,
        ..granted(Purpose::TelemetryContent)
    });
    assert_eq!(
        s.consents.len(),
        1,
        "withdrawal was superseded, not appended"
    );
    assert_eq!(s.consents[0].version, "v2");
    assert_eq!(
        s.consent_ceiling(Purpose::TelemetryContent),
        ContentCapture::Full
    );
}

// mark_erased on a subject with NO grants: stamps erased_at + updated_at and
// leaves the (empty) consent set as-is — no residue, and safe to call.
#[test]
fn mark_erased_without_consents_stamps_and_leaves_no_residue() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.mark_erased(500);
    assert!(s.consents.is_empty(), "no grants to withdraw");
    assert_eq!(s.erased_at, Some(500));
    assert_eq!(s.updated_at, 500, "updated_at advances to erasure time");
}

// mark_erased withdraws grants of EVERY prior status (Granted and Pending
// alike) — erasure leaves no live-or-pending consent behind.
#[test]
fn mark_erased_withdraws_every_status_including_pending() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    s.upsert_consent(ConsentGrant {
        status: ConsentStatus::Pending,
        ..granted(Purpose::EvalRecording)
    });
    s.mark_erased(700);
    assert_eq!(s.consents.len(), 2, "both grants retained as audit proof");
    assert!(
        s.consents
            .iter()
            .all(|g| g.status == ConsentStatus::Withdrawn),
        "Granted AND Pending both flip to Withdrawn"
    );
    assert_eq!(s.updated_at, 700);
}

// mark_erased is idempotent: a second erasure keeps grants Withdrawn and just
// re-stamps the timestamps (no residue accumulates, no state flips back).
#[test]
fn mark_erased_is_idempotent() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    s.mark_erased(100);
    s.mark_erased(200);
    assert_eq!(s.consents.len(), 1);
    assert_eq!(s.consents[0].status, ConsentStatus::Withdrawn);
    assert_eq!(s.erased_at, Some(200), "re-stamped to the latest erasure");
    assert_eq!(s.updated_at, 200);
}

// DataSubject::new default state: no grants, created_at == updated_at, and
// neither the external id nor the erasure stamp is set.
#[test]
fn new_subject_defaults() {
    let s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 42);
    assert!(s.consents.is_empty());
    assert_eq!(s.created_at, 42);
    assert_eq!(s.updated_at, 42, "created == updated on a fresh subject");
    assert_eq!(s.erased_at, None);
    assert_eq!(s.external_id, None);
}

// Wire shapes: ConsentStatus / LawfulBasis are snake_case; LawfulBasis has a
// Consent default and legitimate_interest is the multi-word case.
#[test]
fn consent_status_and_lawful_basis_wire_are_snake_case() {
    assert_eq!(
        serde_json::to_string(&ConsentStatus::Withdrawn).unwrap(),
        "\"withdrawn\""
    );
    assert_eq!(
        serde_json::from_str::<ConsentStatus>("\"pending\"").unwrap(),
        ConsentStatus::Pending
    );
    assert_eq!(
        serde_json::to_string(&LawfulBasis::LegitimateInterest).unwrap(),
        "\"legitimate_interest\""
    );
    assert_eq!(LawfulBasis::default(), LawfulBasis::Consent);
}

// DataSubject serde: None external_id/erased_at are omitted from the wire, a
// grant round-trips, and a grant JSON lacking `basis` defaults to Consent.
#[test]
fn data_subject_serde_omits_absent_optionals_and_defaults_basis() {
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("external_id"), "None external_id omitted");
    assert!(!json.contains("erased_at"), "None erased_at omitted");
    // Full round-trip preserves the grant.
    let back: DataSubject = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);

    // A grant lacking `basis` deserializes to the Consent default.
    let grant: ConsentGrant = serde_json::from_str(
        r#"{"purpose":"telemetry_content","status":"granted","granted_at":1,"version":"v1"}"#,
    )
    .unwrap();
    assert_eq!(grant.basis, LawfulBasis::Consent);
}

// Error Display strings are the operator-facing wire text.
#[test]
fn data_subject_error_display_messages() {
    assert_eq!(
        DataSubjectError::NotFound("dsub_1".into()).to_string(),
        "data_subject not found: dsub_1"
    );
    assert_eq!(
        DataSubjectError::Storage("disk full".into()).to_string(),
        "storage: disk full"
    );
}

// Resolver erase with NO content erasers registered: still marks the subject
// erased (retained + stamped) and returns a zero-count receipt.
#[tokio::test]
async fn resolver_erase_without_erasers_stamps_subject_and_returns_zero() {
    let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    repo.create(s).await.unwrap();

    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo.clone());
    let id = DataSubjectId("dsub_1".into());
    let receipt = resolver.erase(&id).await.unwrap();
    assert_eq!(receipt.records_removed, 0, "no erasers → nothing removed");

    let after = repo.get(&id).await.expect("subject retained");
    assert!(after.erased_at.is_some(), "still stamped without erasers");
    assert_eq!(after.consents[0].status, ConsentStatus::Withdrawn);
}

// Resolver erase of a subject UNKNOWN to the repo still fans content removal
// out to the erasers (fail-open on content) and does not error on the absent
// subject record.
#[tokio::test]
async fn resolver_erase_unknown_subject_counts_eraser_removals() {
    use awaken_runtime_contract::ContentEraser;

    struct FakeEraser(usize);
    #[async_trait]
    impl ContentEraser for FakeEraser {
        async fn erase_subject(
            &self,
            _s: &DataSubjectId,
        ) -> Result<usize, awaken_runtime_contract::ErasureError> {
            Ok(self.0)
        }
    }

    let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo).with_target(
        ErasureTarget::Coordinator,
        std::sync::Arc::new(FakeEraser(6)),
    );
    // Subject was never put(); erasers still remove its orphaned content.
    let receipt = resolver
        .erase(&DataSubjectId("orphan".into()))
        .await
        .unwrap();
    assert_eq!(
        receipt.records_removed, 6,
        "content erased even with no record"
    );
}

#[tokio::test]
async fn erasure_retry_resumes_after_the_last_durable_checkpoint() {
    use awaken_runtime_contract::{ContentEraser, ErasureError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEraser {
        calls: std::sync::Arc<AtomicUsize>,
        removed: usize,
        fail_first: bool,
    }
    #[async_trait]
    impl ContentEraser for CountingEraser {
        async fn erase_subject(&self, _s: &DataSubjectId) -> Result<usize, ErasureError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && call == 0 {
                Err(ErasureError("transient delete failure".into()))
            } else {
                Ok(self.removed)
            }
        }
    }

    // Causes: C1 Coordinator succeeds, C2 Resources fails before its
    // checkpoint, C3 the caller retries. Effects: E1 persist Coordinator's
    // stable target checkpoint and count, E2 surface the first failure, E3
    // skip Coordinator on retry, E4 retry Resources and return the sum.
    // Constraint: target identity is the bounded-context enum, never adapter
    // insertion order. Decision rule R1 = C1+C2+C3 -> E1+E2+E3+E4.
    let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    let first_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let second_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo.clone())
        .with_target(
            ErasureTarget::Coordinator,
            std::sync::Arc::new(CountingEraser {
                calls: first_calls.clone(),
                removed: 2,
                fail_first: false,
            }),
        )
        .with_target(
            ErasureTarget::Resources,
            std::sync::Arc::new(CountingEraser {
                calls: second_calls.clone(),
                removed: 3,
                fail_first: true,
            }),
        );
    let subject = DataSubjectId("resume-me".into());

    assert!(resolver.erase(&subject).await.is_err());
    let receipt = resolver.erase(&subject).await.expect("retry resumes");
    assert_eq!(receipt.records_removed, 5);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 2);
    assert!(repo.load(&subject).await.unwrap().unwrap().complete);
}

#[tokio::test]
async fn concurrent_erasure_replays_effect_but_counts_one_checkpoint() {
    use awaken_runtime_contract::{ContentEraser, ErasureError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RacingIdempotentEraser {
        barrier: tokio::sync::Barrier,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ContentEraser for RacingIdempotentEraser {
        async fn erase_subject(&self, _: &DataSubjectId) -> Result<usize, ErasureError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait().await;
            // A production eraser replays this subject's durable receipt.
            Ok(5)
        }
    }

    // Cause/effect graph: C1 two replicas load an absent checkpoint; C2 both
    // complete the same idempotent target effect before either checkpoint;
    // C3 both attempt create-CAS. Effects: E1 one checkpoint wins; E2 the
    // loser reloads instead of adding the receipt again; E3 both callers
    // receive the same terminal receipt; E4 revision/target set are monotonic.
    // Decision rule R1=C1+C2+C3 -> E1+E2+E3+E4. Physical effect replay is
    // allowed by the port contract; logical double count/regression is not.
    let repo = Arc::new(InMemoryDataSubjectRepo::new());
    let subject = DataSubjectId("concurrent-erasure".into());
    repo.create(DataSubject::new(subject.clone(), "org", 0))
        .await
        .unwrap();
    let eraser = Arc::new(RacingIdempotentEraser {
        barrier: tokio::sync::Barrier::new(2),
        calls: AtomicUsize::new(0),
    });
    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo.clone())
        .with_target(ErasureTarget::Coordinator, eraser.clone());

    let (left, right) = tokio::join!(resolver.erase(&subject), resolver.erase(&subject));
    assert_eq!(left.unwrap().records_removed, 5, "E3");
    assert_eq!(right.unwrap().records_removed, 5, "E3");
    assert_eq!(eraser.calls.load(Ordering::SeqCst), 2, "C2 replay");
    let progress = repo.load(&subject).await.unwrap().unwrap();
    assert!(progress.complete, "E4");
    assert_eq!(progress.records_removed, 5, "E2");
    assert_eq!(
        progress.completed_targets,
        vec![ErasureTarget::Coordinator],
        "E4"
    );
}

#[tokio::test]
async fn resolver_reads_grant_and_erases() {
    let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    repo.create(s).await.unwrap();

    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo);
    let id = DataSubjectId("dsub_1".into());
    assert_eq!(
        resolver
            .consent_ceiling(&id, Purpose::TelemetryContent)
            .await,
        ContentCapture::Full
    );
    // Unknown subject → Structured.
    assert_eq!(
        resolver
            .consent_ceiling(&DataSubjectId("nope".into()), Purpose::TelemetryContent)
            .await,
        ContentCapture::Structured
    );
    // Erase removes the record → resolves back to Structured.
    resolver.erase(&id).await.unwrap();
    assert_eq!(
        resolver
            .consent_ceiling(&id, Purpose::TelemetryContent)
            .await,
        ContentCapture::Structured
    );
}

// FAIL-CLOSED: a backend erasure/accountability failure surfaces as an error, not
// a success receipt. GDPR Art. 17 erasure fans out to content erasers and writes an
// accountability stamp; `ContentEraser::erase_subject` and `erase` now return a
// `Result`, so a failing content DELETE and a failing accountability PUT each
// propagate — the caller can no longer mistake a total no-op for a real erasure.
#[tokio::test]
async fn resolver_erase_surfaces_backend_failures() {
    use awaken_runtime_contract::{ContentEraser, ErasureError};

    // A content backend whose DELETE errored: it now reports the failure through
    // the `Result` channel instead of collapsing to an ambiguous `0`.
    struct FailingEraser;
    #[async_trait]
    impl ContentEraser for FailingEraser {
        async fn erase_subject(&self, _s: &DataSubjectId) -> Result<usize, ErasureError> {
            Err(ErasureError("content DELETE failed".into()))
        }
    }

    // A repo whose accountability write (`put`) always fails. `get` succeeds so the
    // resolver reaches the accountability-write branch.
    struct FailingPutRepo;
    #[async_trait]
    impl DataSubjectRepo for FailingPutRepo {
        async fn create(&self, _subject: DataSubject) -> Result<(), DataSubjectError> {
            Err(DataSubjectError::Storage("disk full".into()))
        }
        async fn compare_and_swap(
            &self,
            _expected_revision: u64,
            _subject: DataSubject,
        ) -> Result<(), DataSubjectError> {
            Err(DataSubjectError::Storage("disk full".into()))
        }
        async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
            Ok(DataSubject::new(id.clone(), "org_1", 0))
        }
        async fn list(&self, _org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
            Ok(Vec::new())
        }
    }

    // (1) A failing content eraser surfaces before any accountability stamp — an
    // unerased subject must never be reported as erased.
    let jobs = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    let with_failing_eraser =
        RepoDataSubjectResolver::new(std::sync::Arc::new(FailingPutRepo), jobs.clone())
            .with_target(
                ErasureTarget::Coordinator,
                std::sync::Arc::new(FailingEraser),
            );
    let err = with_failing_eraser
        .erase(&DataSubjectId("dsub_1".into()))
        .await
        .expect_err("a failing content DELETE must surface, not return a success receipt");
    assert!(err.to_string().contains("content DELETE failed"));

    // (2) With content erasure clean but the accountability write failing, the
    // failed audit stamp still surfaces as an error (no success receipt).
    struct CleanEraser;
    #[async_trait]
    impl ContentEraser for CleanEraser {
        async fn erase_subject(&self, _s: &DataSubjectId) -> Result<usize, ErasureError> {
            Ok(0)
        }
    }
    let with_failing_put = RepoDataSubjectResolver::new(std::sync::Arc::new(FailingPutRepo), jobs)
        .with_target(ErasureTarget::Coordinator, std::sync::Arc::new(CleanEraser));
    let err = with_failing_put
        .erase(&DataSubjectId("dsub_1".into()))
        .await
        .expect_err("a failing accountability write must surface as an error");
    assert!(err.to_string().contains("accountability write failed"));
}

#[tokio::test]
async fn erase_fans_out_and_sums_removed_counts() {
    use awaken_runtime_contract::ContentEraser;

    struct FakeEraser(usize);
    #[async_trait]
    impl ContentEraser for FakeEraser {
        async fn erase_subject(
            &self,
            _s: &DataSubjectId,
        ) -> Result<usize, awaken_runtime_contract::ErasureError> {
            Ok(self.0)
        }
    }

    let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    repo.create(DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0))
        .await
        .unwrap();
    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo)
        .with_target(
            ErasureTarget::Coordinator,
            std::sync::Arc::new(FakeEraser(2)),
        )
        .with_target(ErasureTarget::Resources, std::sync::Arc::new(FakeEraser(3)));

    let receipt = resolver
        .erase(&DataSubjectId("dsub_1".into()))
        .await
        .unwrap();
    assert_eq!(receipt.records_removed, 5, "sums across content stores");
}

// E4: the resolver's erase fans the removal out across every eraser (sum),
// then RETAINS the subject record as accountability proof — stamping
// `erased_at`, withdrawing its grants, and so ceiling any purpose to Structured.
#[tokio::test]
async fn resolver_erase_fans_out_and_stamps_retained_subject() {
    use awaken_runtime_contract::ContentEraser;

    struct FakeEraser(usize);
    #[async_trait]
    impl ContentEraser for FakeEraser {
        async fn erase_subject(
            &self,
            _s: &DataSubjectId,
        ) -> Result<usize, awaken_runtime_contract::ErasureError> {
            Ok(self.0)
        }
    }

    let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
    let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
    s.upsert_consent(granted(Purpose::TelemetryContent));
    repo.create(s).await.unwrap();

    let resolver = RepoDataSubjectResolver::new(repo.clone(), repo.clone())
        .with_target(
            ErasureTarget::Coordinator,
            std::sync::Arc::new(FakeEraser(4)),
        )
        .with_target(ErasureTarget::Resources, std::sync::Arc::new(FakeEraser(3)));
    let id = DataSubjectId("dsub_1".into());

    let receipt = resolver.erase(&id).await.unwrap();
    assert_eq!(receipt.records_removed, 7, "fan-out sum across erasers");

    // Subject record is retained (Art. 5(2)/7(1)) with an erased_at stamp and
    // its grants withdrawn — not deleted.
    let after = repo.get(&id).await.expect("subject retained after erase");
    assert!(after.erased_at.is_some(), "erased_at stamped");
    assert_eq!(after.consents.len(), 1, "grant kept as audit proof");
    assert_eq!(after.consents[0].status, ConsentStatus::Withdrawn);

    // Any purpose now ceils to Structured (no live consent survives erasure).
    assert_eq!(
        resolver
            .consent_ceiling(&id, Purpose::TelemetryContent)
            .await,
        ContentCapture::Structured
    );
    assert_eq!(
        resolver.consent_ceiling(&id, Purpose::EvalRecording).await,
        ContentCapture::Structured
    );
}

struct FixedClock(std::sync::atomic::AtomicI64);

impl FixedClock {
    fn new(now: i64) -> Self {
        Self(std::sync::atomic::AtomicI64::new(now))
    }

    fn set(&self, now: i64) {
        self.0.store(now, std::sync::atomic::Ordering::SeqCst);
    }
}

impl DataSubjectClock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test]
async fn profile_commands_are_org_fenced_and_patch_one_aggregate() {
    // Cause/effect graph: C1 org={owner,other}; C2 operation={create,get,
    // list,patch,no-op,consent}; C3 patch fields={metadata delete/upsert,
    // relationship,trust grant,name}. Effects: E1 create one durable subject;
    // E2 owner reads/lists it; E3 other org reads and writes see NotFound;
    // E4 all patches land on that subject and advance one revision; E5 a
    // no-op does not advance or create a parallel aggregate.
    // Decision table: R1 owner+create/get/list -> E1+E2; R2 other+get -> E3;
    // R3 other+consent -> E3+E5; R4 owner+mixed patch -> E4;
    // R5 owner+empty patch -> E5.
    let repo = Arc::new(InMemoryDataSubjectRepo::new());
    let clock = Arc::new(FixedClock::new(100));
    let application = DataSubjectApplication::with_clock(repo, vec![7; 32], clock.clone()).unwrap();
    let profile = application
        .create_user_profile(CreateUserProfileCommand {
            org: "org_a".into(),
            metadata: BTreeMap::from([("drop".into(), "1".into())]),
            relationship: UserProfileRelationship::External,
            external_id: Some("external-a".into()),
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(
        application.list_user_profiles("org_a").await.unwrap().len(),
        1
    );
    assert!(matches!(
        application.get_user_profile("org_b", &profile.id).await,
        Err(DataSubjectApplicationError::NotFound)
    ));
    assert!(matches!(
        application
            .grant_consent_for_purpose(
                "org_b",
                &profile.id,
                Purpose::TelemetryContent,
                "test".into(),
            )
            .await,
        Err(DataSubjectApplicationError::NotFound)
    ));

    clock.set(200);
    let updated = application
        .update_user_profile(
            "org_a",
            &profile.id,
            UpdateUserProfileCommand {
                metadata: Some(BTreeMap::from([
                    ("drop".into(), String::new()),
                    ("keep".into(), "2".into()),
                ])),
                relationship: Some(UserProfileRelationship::Resold),
                trust_grants: Some(BTreeMap::from([(
                    "calendar".into(),
                    UserProfileTrustGrant {
                        status: UserProfileTrustGrantStatus::Pending,
                    },
                )])),
                external_id: None,
                name: Some(Some("Alice".into())),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.revision, 1, "R4");
    assert_eq!(updated.updated_at, 200);
    assert!(!updated.metadata.contains_key("drop"));
    assert_eq!(updated.metadata["keep"], "2");
    assert_eq!(updated.relationship, UserProfileRelationship::Resold);
    assert_eq!(updated.name.as_deref(), Some("Alice"));
    assert_eq!(
        updated.trust_grants["calendar"].status,
        UserProfileTrustGrantStatus::Pending
    );
    let unchanged = application
        .update_user_profile("org_a", &profile.id, UpdateUserProfileCommand::default())
        .await
        .unwrap();
    assert_eq!(unchanged.revision, 1, "R5");
}

#[tokio::test]
async fn enrollment_hmac_and_expiry_decision_table() {
    // Causes: C1 signature={exact,tampered}; C2 time={before,after expiry};
    // C3 subject={present,missing}; C4 action={inspect,accept,replay}.
    // Effects: E1 exact+live token exposes both purposes; E2 tampered/expired
    // fails closed; E3 missing cannot mint; E4 accept grants both purposes;
    // E5 replay is idempotent. Rules R1 exact+before+present -> E1+E4+E5;
    // R2 tampered -> E2; R3 after -> E2; R4 missing -> E3.
    let repo = Arc::new(InMemoryDataSubjectRepo::new());
    let clock = Arc::new(FixedClock::new(1_000));
    let application =
        DataSubjectApplication::with_clock(repo, vec![9; 32], clock.clone()).expect("strong key");
    let profile = application
        .create_user_profile(CreateUserProfileCommand {
            org: "org".into(),
            metadata: BTreeMap::new(),
            relationship: UserProfileRelationship::External,
            external_id: None,
            name: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        application.mint_enrollment("org", "missing").await,
        Err(DataSubjectApplicationError::NotFound)
    ));
    let ticket = application
        .mint_enrollment("org", &profile.id)
        .await
        .unwrap();
    let token = ticket.url.strip_prefix("/enroll/").unwrap();
    let inspected = application.inspect_enrollment(token).unwrap();
    assert_eq!(inspected.purposes.len(), 2, "R1");
    assert!(matches!(
        application.inspect_enrollment(&format!("{token}x")),
        Err(DataSubjectApplicationError::InvalidEnrollment)
    ));
    let accepted = application.accept_enrollment(token).await.unwrap();
    assert_eq!(accepted.grants.len(), 2);
    let revision = application
        .get_user_profile("org", &profile.id)
        .await
        .unwrap()
        .revision;
    application.accept_enrollment(token).await.unwrap();
    assert_eq!(
        application
            .get_user_profile("org", &profile.id)
            .await
            .unwrap()
            .revision,
        revision,
        "R1 replay"
    );
    clock.set(ticket.expires_at + 1);
    assert!(matches!(
        application.inspect_enrollment(token),
        Err(DataSubjectApplicationError::InvalidEnrollment)
    ));
}

#[tokio::test]
async fn storage_outage_never_masquerades_as_missing_consent() {
    struct FailingRepo;
    #[async_trait]
    impl DataSubjectRepo for FailingRepo {
        async fn create(&self, _: DataSubject) -> Result<(), DataSubjectError> {
            Err(DataSubjectError::Storage("offline".into()))
        }
        async fn compare_and_swap(&self, _: u64, _: DataSubject) -> Result<(), DataSubjectError> {
            Err(DataSubjectError::Storage("offline".into()))
        }
        async fn get(&self, _: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
            Err(DataSubjectError::Storage("offline".into()))
        }
        async fn list(&self, _: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
            Err(DataSubjectError::Storage("offline".into()))
        }
    }

    // Causes: C1 read result={NotFound,Storage}; C2 decision asks for Full.
    // Effects: E1 NotFound safely caps to Structured; E2 Storage surfaces and
    // cannot be reported as a valid no-consent decision. R1 is covered by the
    // enrollment/profile tests; R2 C1=Storage+C2 -> E2 is exercised here.
    let application = DataSubjectApplication::new(Arc::new(FailingRepo), vec![3; 32]).unwrap();
    assert!(matches!(
        application
            .capture_decision("org", "id", ContentCapture::Full, ContentCapture::Full)
            .await,
        Err(DataSubjectApplicationError::Repository(
            DataSubjectError::Storage(_)
        ))
    ));
}

#[tokio::test]
async fn stale_profile_writer_retries_without_losing_consent() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct InjectConsentConflict {
        inner: Arc<InMemoryDataSubjectRepo>,
        injected: AtomicBool,
    }

    #[async_trait]
    impl DataSubjectRepo for InjectConsentConflict {
        async fn create(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
            self.inner.create(subject).await
        }
        async fn compare_and_swap(
            &self,
            expected_revision: u64,
            subject: DataSubject,
        ) -> Result<(), DataSubjectError> {
            if !self.injected.swap(true, Ordering::SeqCst) {
                let mut subjects = self.inner.subjects.lock().expect("test subjects");
                let current = subjects.get_mut(&subject.id.0).expect("subject exists");
                current.upsert_consent(granted(Purpose::TelemetryContent));
                current.revision = expected_revision
                    .checked_add(1)
                    .expect("injected revision remains representable");
                return Err(DataSubjectError::Conflict(subject.id.0));
            }
            self.inner
                .compare_and_swap(expected_revision, subject)
                .await
        }
        async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
            self.inner.get(id).await
        }
        async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
            self.inner.list(org).await
        }
    }

    // Causes: C1 profile writer reads revision N; C2 consent writer commits
    // revision N+1; C3 profile CAS is stale. Effects: E1 stale CAS is rejected;
    // E2 application reloads; E3 final N+2 contains both consent and profile
    // patch. Decision rule R1=C1+C2+C3 -> E1+E2+E3 (lost update forbidden).
    let inner = Arc::new(InMemoryDataSubjectRepo::new());
    let repo = Arc::new(InjectConsentConflict {
        inner: inner.clone(),
        injected: AtomicBool::new(false),
    });
    let application = DataSubjectApplication::new(repo, vec![4; 32]).unwrap();
    let profile = application
        .create_user_profile(CreateUserProfileCommand {
            org: "org".into(),
            metadata: BTreeMap::new(),
            relationship: UserProfileRelationship::External,
            external_id: None,
            name: None,
        })
        .await
        .unwrap();
    let updated = application
        .update_user_profile(
            "org",
            &profile.id,
            UpdateUserProfileCommand {
                name: Some(Some("preserved".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name.as_deref(), Some("preserved"));
    let aggregate = inner.get(&DataSubjectId(profile.id)).await.unwrap();
    assert_eq!(aggregate.consents.len(), 1, "E3");
    assert_eq!(aggregate.revision, 2, "E3");
}

#[tokio::test]
async fn revision_exhaustion_fails_instead_of_saturating() {
    // Causes: C1 stored revision=u64::MAX; C2 mutation changes a field.
    // Effects: E1 checked increment reports RevisionExhausted; E2 no CAS and
    // no silent saturated revision that would make future writers indistinguishable.
    // Decision rule R1=C1+C2 -> E1+E2.
    let repo = Arc::new(InMemoryDataSubjectRepo::new());
    let mut subject = DataSubject::new(DataSubjectId("uprof_max".into()), "org", 0);
    subject.revision = u64::MAX;
    repo.create(subject).await.unwrap();
    let application = DataSubjectApplication::new(repo, vec![5; 32]).unwrap();
    assert!(matches!(
        application
            .update_user_profile(
                "org",
                "uprof_max",
                UpdateUserProfileCommand {
                    name: Some(Some("change".into())),
                    ..Default::default()
                }
            )
            .await,
        Err(DataSubjectApplicationError::Repository(
            DataSubjectError::RevisionExhausted(_)
        ))
    ));
}
