//! T15-C02: credential resolution blocked at a barrier while the attempt's
//! authority is revoked or its owner changes.
//!
//! Production entrypoint: `PinnedCredentialMaterializer::materialize_claimed_remote`
//! over the real Vault local material authority. Only collaborators are fakes:
//! the SecretStore (gated, counting reads), the ownership verifier (mutable,
//! owner-aware) and the realization recorder (counting receipts).
//!
//! Decision table (the barrier is inside the single SecretStore read):
//! | rule | authority change             | expected effect                        |
//! | W0   | none                         | material + one receipt + one read      |
//! | W1   | Lost/swap/unavailable before | Err, zero reads, zero receipts         |
//! | W2   | Lost while read blocked      | Err, zero receipts (current authority) |
//! | W3   | owner swapped while blocked  | Err, zero receipts                     |
//! | W4   | Unavailable while blocked    | Err, zero receipts                     |
//! W2-W4 also require the in-flight read to stay owned by the call and to be
//! observed to completion exactly once (never abandoned, never re-issued).
//! Both Provider and A2A exercise the same claim-bound sequence. W5-W7 change
//! authority while an already committed receipt waits for acknowledgement:
//! retain that receipt, observe the acknowledgement once, and release no material.
//!
//! Negative control (executed): replaying cached successful material after
//! revocation must be detected by the same oracle check.

use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use super::*;
use awaken_credential_contract::CredentialRealizationReceipt;
use awaken_runtime_contract::{AttemptOwnershipError, AttemptOwnershipVerifier};

/// SecretStore whose `get` can be held at a barrier after it was entered.
struct GatedSecrets {
    inner: InMemorySecretStore,
    reads: AtomicUsize,
    completed_reads: AtomicUsize,
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl GatedSecrets {
    fn new() -> Self {
        Self {
            inner: InMemorySecretStore::new(),
            reads: AtomicUsize::new(0),
            completed_reads: AtomicUsize::new(0),
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn completed_reads(&self) -> usize {
        self.completed_reads.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl SecretStore for GatedSecrets {
    async fn put(
        &self,
        reference: &SecretRef,
        secret: RedactedString,
    ) -> Result<(), CredentialError> {
        self.inner.put(reference, secret).await
    }

    async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.acquire().await.expect("gate open").forget();
        }
        let value = self.inner.get(reference).await;
        self.completed_reads.fetch_add(1, Ordering::SeqCst);
        value
    }

    async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
        self.inner.delete(reference).await
    }
}

/// Owner-aware verifier: the attempt is current only while the authority's
/// current owner is the attempt's owner and the authority is reachable.
struct OwnerAuthority {
    attempt_owner: String,
    current_owner: StdMutex<String>,
    revoked: AtomicBool,
    unavailable: AtomicBool,
}

impl OwnerAuthority {
    fn new(owner: &str) -> Self {
        Self {
            attempt_owner: owner.into(),
            current_owner: StdMutex::new(owner.into()),
            revoked: AtomicBool::new(false),
            unavailable: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl AttemptOwnershipVerifier for OwnerAuthority {
    async fn verify_current(&self) -> Result<(), AttemptOwnershipError> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(AttemptOwnershipError::Unavailable("authority down".into()));
        }
        if self.revoked.load(Ordering::SeqCst)
            || *self.current_owner.lock().unwrap() != self.attempt_owner
        {
            return Err(AttemptOwnershipError::Lost);
        }
        Ok(())
    }
}

struct CountingRecorder {
    receipts: StdMutex<Vec<CredentialRealizationReceipt>>,
    acknowledged: AtomicUsize,
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Default for CountingRecorder {
    fn default() -> Self {
        Self {
            receipts: StdMutex::new(Vec::new()),
            acknowledged: AtomicUsize::new(0),
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl CountingRecorder {
    fn receipts(&self) -> usize {
        self.receipts.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialRealizationRecorder for CountingRecorder {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), awaken_credential_contract::CredentialRealizationRecordError> {
        self.receipts.lock().unwrap().push(receipt);
        if self.armed.load(Ordering::SeqCst) {
            self.entered.notify_one();
            self.release
                .acquire()
                .await
                .expect("receipt gate open")
                .forget();
        }
        self.acknowledged.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

const KINDS: [CredentialRealizationKind; 2] = [
    CredentialRealizationKind::WorkerRelay,
    CredentialRealizationKind::WorkerProviderAdapter,
];

struct Fixture {
    kind: CredentialRealizationKind,
    secrets: Arc<GatedSecrets>,
    materializer: PinnedCredentialMaterializer,
    candidate: ResolvedModelCandidate,
    bindings: Vec<awaken_credential_contract::AttemptCredentialBinding>,
}

async fn fixture(kind: CredentialRealizationKind) -> Fixture {
    let (provider, usage, purpose) = match kind {
        CredentialRealizationKind::WorkerRelay => (
            "https://agent.example",
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            awaken_agent_contract::credential::CredentialPurpose::RemoteAgentAuthorization,
        ),
        CredentialRealizationKind::WorkerProviderAdapter => (
            "anthropic",
            CredentialUsage::ProviderAdapter,
            awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
        ),
        _ => unreachable!("the table selects exactly the two claimed adapters"),
    };
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(GatedSecrets::new());
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "workspace-a".into(),
            kind: CredentialKind::Vault,
            provider_id: Some(provider.into()),
            env_key: None,
            secret: Some(RedactedString::new("remote-token")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let holder = selected_holder();
    let access = CredentialAccess::new(
        CredentialRef {
            id: source.id.0,
            revision: 1,
        },
        CredentialMaterialSource::ControlPlaneReference,
        usage,
        CredentialExecutionPolicy::self_hosted_provider(),
    )
    .with_target(awaken_agent_contract::credential::CredentialTarget::new(
        purpose, provider,
    ));
    let candidate = match kind {
        CredentialRealizationKind::WorkerRelay => ResolvedModelCandidate::try_remote(
            ModelBinding::parse("", "", "a2a:https://agent.example").expect("valid binding"),
            "workspace-a",
            Some(access),
            "sha256:card",
        ),
        CredentialRealizationKind::WorkerProviderAdapter => ResolvedModelCandidate::try_provider(
            ModelBinding::parse("anthropic", "model", "native").expect("valid binding"),
            "anthropic@1",
            "endpoint@1",
            "workspace-a",
            Some(access),
            InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                api_dialect: "anthropic_messages".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: "model".into(),
                processing_placement: None,
            },
        ),
        _ => unreachable!("the table selects exactly the two claimed adapters"),
    }
    .expect("coherent claimed credential candidate");
    let capabilities = CredentialRealizationCapabilities {
        holders: [holder.clone()].into_iter().collect(),
        material_sources: [CredentialMaterialSource::ControlPlaneReference]
            .into_iter()
            .collect(),
        realization_kinds: [kind].into_iter().collect(),
        ..Default::default()
    };
    let bindings = awaken_credential_contract::compile_candidate_credential_bindings(
        &[&candidate],
        Some(&holder),
        &capabilities,
        3,
        0,
    )
    .unwrap();
    let materializer = pinned_vault(credentials, secrets.clone(), awaken_time::system_clock());
    Fixture {
        kind,
        secrets,
        materializer,
        candidate,
        bindings,
    }
}

impl Fixture {
    async fn materialize(
        &self,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<RedactedString>, String> {
        match self.kind {
            CredentialRealizationKind::WorkerRelay => {
                self.materializer
                    .materialize_claimed_remote(&self.candidate, context)
                    .await
            }
            CredentialRealizationKind::WorkerProviderAdapter => {
                self.materializer
                    .materialize_claimed_provider(&self.candidate, context, self.kind)
                    .await
            }
            _ => unreachable!("the table selects exactly the two claimed adapters"),
        }
    }
}

fn context(
    fixture: &Fixture,
    authority: Arc<OwnerAuthority>,
    recorder: Arc<dyn awaken_credential_contract::CredentialRealizationRecorder>,
) -> awaken_runtime_contract::RuntimeRunContext {
    awaken_runtime_contract::RuntimeRunContext::new()
        .with_ownership(authority)
        .with_credential_realization(
            awaken_credential_contract::AttemptCredentialRealization::new(
                fixture.bindings.clone(),
                recorder,
            ),
        )
}

/// Observed effects of one materialization call whose authority was no
/// longer current when the call finished.
struct RevokedObservation {
    outcome: Result<Option<RedactedString>, String>,
    new_reads: usize,
    new_receipts: usize,
    expected_error: String,
}

/// The T15-C02 oracle for a call whose authority is not current: refuse with
/// an ownership error, release no material and record no realization.
fn check_rejected_under_current_authority(observed: &RevokedObservation) -> Result<(), String> {
    match &observed.outcome {
        Ok(Some(_)) => return Err("material was returned under revoked authority".into()),
        Ok(None) => return Err("revoked call reported an anonymous success".into()),
        Err(error) if error != &observed.expected_error => {
            return Err(format!("revoked call failed for another reason: {error}"));
        }
        Err(_) => {}
    }
    if observed.new_receipts != 0 {
        return Err(format!(
            "{} realization receipt(s) recorded under revoked authority",
            observed.new_receipts
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Change {
    Revoke,
    SwapOwner,
    Unavailable,
}

const CHANGES: [Change; 3] = [Change::Revoke, Change::SwapOwner, Change::Unavailable];

fn expected_ownership_error(change: Change) -> String {
    let cause = match change {
        Change::Revoke | Change::SwapOwner => AttemptOwnershipError::Lost,
        Change::Unavailable => AttemptOwnershipError::Unavailable("authority down".into()),
    };
    awaken_runtime_contract::execution::Error::Execution(format!(
        "Run attempt no longer owns external execution: {cause}"
    ))
    .to_string()
}

fn apply(authority: &OwnerAuthority, change: Change) {
    match change {
        Change::Revoke => authority.revoked.store(true, Ordering::SeqCst),
        Change::SwapOwner => *authority.current_owner.lock().unwrap() = "worker-b".into(),
        Change::Unavailable => authority.unavailable.store(true, Ordering::SeqCst),
    }
}

/// Run one materialization with the read held at the barrier, apply `change`
/// once the read has been entered, assert the read is still in flight and
/// owned by the call (no early return, no abandonment), then release it.
async fn materialize_with_change_during_read(
    kind: CredentialRealizationKind,
    change: Change,
) -> RevokedObservation {
    let fixture = fixture(kind).await;
    let authority = Arc::new(OwnerAuthority::new("worker-a"));
    let recorder = Arc::new(CountingRecorder::default());
    let context = context(&fixture, authority.clone(), recorder.clone());
    let reads_before = fixture.secrets.reads();
    let completed_before = fixture.secrets.completed_reads();
    fixture.secrets.armed.store(true, Ordering::SeqCst);
    let finished = AtomicBool::new(false);
    let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(
            async {
                let outcome = fixture.materialize(&context).await;
                finished.store(true, Ordering::SeqCst);
                outcome
            },
            async {
                fixture.secrets.entered.notified().await;
                apply(&authority, change);
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    !finished.load(Ordering::SeqCst),
                    "{change:?}: the in-flight read must stay owned by the call until it settles"
                );
                assert_eq!(
                    fixture.secrets.completed_reads(),
                    completed_before,
                    "{change:?}: read still in flight at the barrier"
                );
                assert_eq!(
                    recorder.receipts(),
                    0,
                    "{change:?}: no receipt before the read"
                );
                fixture.secrets.release.add_permits(1);
            }
        )
    })
    .await
    .expect("barrier-controlled materialization finishes");
    let new_reads = fixture.secrets.reads() - reads_before;
    assert_eq!(
        new_reads, 1,
        "{change:?}: exactly one read, never re-issued"
    );
    assert_eq!(
        fixture.secrets.completed_reads() - completed_before,
        1,
        "{change:?}: the original read was observed to completion"
    );
    RevokedObservation {
        outcome,
        new_reads,
        new_receipts: recorder.receipts(),
        expected_error: expected_ownership_error(change),
    }
}

#[tokio::test]
async fn t15_c02_revocation_before_resolution_and_cached_replay_are_rejected() {
    for kind in KINDS {
        // W0 healthy row: material plus exactly one receipt and one read.
        let fixture = fixture(kind).await;
        let authority = Arc::new(OwnerAuthority::new("worker-a"));
        let recorder = Arc::new(CountingRecorder::default());
        let ctx = context(&fixture, authority.clone(), recorder.clone());
        let reads_before = fixture.secrets.reads();
        let cached = fixture
            .materialize(&ctx)
            .await
            .expect("W0 current authority")
            .expect("W0 credential-bearing candidate");
        assert_eq!(cached.expose_secret(), "remote-token", "W0");
        assert_eq!(fixture.secrets.reads(), reads_before + 1, "W0 one read");
        assert_eq!(recorder.receipts(), 1, "W0 one receipt");

        // W1: authority changes before the call -> production rejects with zero
        // new reads and zero new receipts.
        for change in CHANGES {
            let authority = Arc::new(OwnerAuthority::new("worker-a"));
            let recorder = Arc::new(CountingRecorder::default());
            let ctx = context(&fixture, authority.clone(), recorder.clone());
            apply(&authority, change);
            let reads_before = fixture.secrets.reads();
            let observed = RevokedObservation {
                outcome: fixture.materialize(&ctx).await,
                new_reads: fixture.secrets.reads() - reads_before,
                new_receipts: recorder.receipts(),
                expected_error: expected_ownership_error(change),
            };
            check_rejected_under_current_authority(&observed)
                .unwrap_or_else(|violation| panic!("W1 {change:?}: {violation}"));
            assert_eq!(observed.new_reads, 0, "W1 {change:?}: no secret read");
        }

        // Negative control, executed: a caller that replays the cached successful
        // material after revocation (instead of re-entering the production
        // boundary) must be detected by the same oracle check.
        let replay = RevokedObservation {
            outcome: Ok(Some(cached.clone())),
            new_reads: 0,
            new_receipts: 0,
            expected_error: expected_ownership_error(Change::Revoke),
        };
        let violation = check_rejected_under_current_authority(&replay)
            .expect_err("NC: cached-success replay must violate the oracle");
        assert!(violation.contains("revoked authority"), "NC: {violation}");
        // The production boundary, re-entered on the same (now revoked) attempt
        // that previously succeeded, does not serve the earlier success.
        apply(&authority, Change::Revoke);
        let reads_before = fixture.secrets.reads();
        let receipts_before = recorder.receipts();
        let observed = RevokedObservation {
            outcome: fixture.materialize(&ctx).await,
            new_reads: fixture.secrets.reads() - reads_before,
            new_receipts: recorder.receipts() - receipts_before,
            expected_error: expected_ownership_error(Change::Revoke),
        };
        check_rejected_under_current_authority(&observed)
            .unwrap_or_else(|violation| panic!("NC production re-entry: {violation}"));
        assert_eq!(
            observed.new_reads, 0,
            "NC production re-entry reads nothing"
        );
    }
}

#[tokio::test]
async fn t15_c02_blocked_resolution_keeps_its_single_in_flight_read_observed() {
    // Oracle 1 (observation responsibility) for W2-W4: while the read is held
    // at the barrier and authority changes, the call keeps the original read
    // outstanding, observes it to completion exactly once, and never
    // re-issues it (asserted inside `materialize_with_change_during_read`).
    for kind in KINDS {
        for change in CHANGES {
            let observed = materialize_with_change_during_read(kind, change).await;
            assert_eq!(observed.new_reads, 1, "{kind:?}/{change:?}");
        }
    }
}

#[tokio::test]
async fn t15_c02_revocation_during_blocked_resolution_rejects_material_and_receipt() {
    // W2-W4 x Provider/A2A: a completed read under changed authority must
    // produce no receipt or material, while retaining the original read owner.
    for kind in KINDS {
        for change in CHANGES {
            let observed = materialize_with_change_during_read(kind, change).await;
            check_rejected_under_current_authority(&observed)
                .unwrap_or_else(|violation| panic!("{kind:?}/{change:?}: {violation}"));
        }
    }
}

#[tokio::test]
async fn t15_c02_receipt_acknowledgement_never_releases_material_after_authority_changes() {
    // W5-W7 x Provider/A2A: C1 the exact receipt committed while ownership was
    // current; C2 its acknowledgement is in flight; C3 revoke/swap/unavailable
    // occurs before that acknowledgement returns. E1 finish the original call
    // exactly once; E2 retain the already committed receipt; E3 refuse material.
    // This is a different persistence boundary from W2-W4's zero-receipt rows.
    for kind in KINDS {
        for change in CHANGES {
            let fixture = fixture(kind).await;
            let authority = Arc::new(OwnerAuthority::new("worker-a"));
            let recorder = Arc::new(CountingRecorder::default());
            recorder.armed.store(true, Ordering::SeqCst);
            let ctx = context(&fixture, authority.clone(), recorder.clone());
            let reads_before = fixture.secrets.reads();
            let finished = AtomicBool::new(false);
            let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(
                    async {
                        let result = fixture.materialize(&ctx).await;
                        finished.store(true, Ordering::SeqCst);
                        result
                    },
                    async {
                        recorder.entered.notified().await;
                        assert_eq!(
                            recorder.receipts(),
                            1,
                            "W5-W7 commit preceded authority change"
                        );
                        assert_eq!(
                            fixture.secrets.reads(),
                            reads_before + 1,
                            "W5-W7 one settled read"
                        );
                        apply(&authority, change);
                        for _ in 0..16 {
                            tokio::task::yield_now().await;
                        }
                        assert!(
                            !finished.load(Ordering::SeqCst),
                            "W5-W7 acknowledgement remains owned"
                        );
                        assert_eq!(recorder.acknowledged.load(Ordering::SeqCst), 0);
                        recorder.release.add_permits(1);
                    }
                )
            })
            .await
            .expect("receipt barrier settles");
            assert_eq!(
                outcome.unwrap_err(),
                expected_ownership_error(change),
                "{kind:?}/{change:?}"
            );
            assert_eq!(recorder.receipts(), 1, "W5-W7 no duplicate or rollback");
            assert_eq!(
                recorder.acknowledged.load(Ordering::SeqCst),
                1,
                "W5-W7 one acknowledgement"
            );
            assert_eq!(
                fixture.secrets.reads(),
                reads_before + 1,
                "W5-W7 no re-issued material read"
            );
        }
    }
}

#[tokio::test]
async fn current_authority_materializes_each_claimed_route_exactly_once() {
    for kind in KINDS {
        let fixture = fixture(kind).await;
        let authority = Arc::new(OwnerAuthority::new("worker-a"));
        let recorder = Arc::new(CountingRecorder::default());
        let ctx = context(&fixture, authority, recorder.clone());
        let reads_before = fixture.secrets.reads();
        let secret = fixture.materialize(&ctx).await.unwrap().unwrap();
        assert_eq!(secret.expose_secret(), "remote-token", "{kind:?}");
        assert_eq!(fixture.secrets.reads() - reads_before, 1, "{kind:?}");
        assert_eq!(recorder.receipts(), 1, "{kind:?}");
    }
}

struct AwaitedRecorder {
    committed: CountingRecorder,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialRealizationRecorder for AwaitedRecorder {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), awaken_credential_contract::CredentialRealizationRecordError> {
        // This receipt commits while ownership is current. The gate models
        // awaiting its response, not a new write after revocation.
        awaken_credential_contract::CredentialRealizationRecorder::record(&self.committed, receipt)
            .await?;
        self.entered.notify_one();
        self.release
            .acquire()
            .await
            .expect("owned response gate")
            .forget();
        Ok(())
    }
}

#[tokio::test]
async fn ownership_loss_during_receipt_response_never_releases_plaintext_or_repeats_the_effect() {
    for kind in KINDS {
        for change in [
            None,
            Some(Change::Revoke),
            Some(Change::SwapOwner),
            Some(Change::Unavailable),
        ] {
            let fixture = fixture(kind).await;
            let authority = Arc::new(OwnerAuthority::new("worker-a"));
            let recorder = Arc::new(AwaitedRecorder {
                committed: CountingRecorder::default(),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Semaphore::new(0),
            });
            let ctx = context(&fixture, authority.clone(), recorder.clone());
            let reads_before = fixture.secrets.reads();
            let completed_before = fixture.secrets.completed_reads();
            let finished = AtomicBool::new(false);
            let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(
                    async {
                        let result = fixture.materialize(&ctx).await;
                        finished.store(true, Ordering::SeqCst);
                        result
                    },
                    async {
                        recorder.entered.notified().await;
                        assert_eq!(
                            recorder.committed.receipts(),
                            1,
                            "receipt predates ownership change"
                        );
                        if let Some(change) = change {
                            apply(&authority, change);
                        }
                        assert!(
                            !finished.load(Ordering::SeqCst),
                            "must observe the original recorder response"
                        );
                        recorder.release.add_permits(1);
                    }
                )
            })
            .await
            .expect("owned recorder response settles");
            if let Some(change) = change {
                assert_eq!(
                    outcome.unwrap_err(),
                    expected_ownership_error(change),
                    "{kind:?}/{change:?}"
                );
            } else {
                assert_eq!(
                    outcome.unwrap().unwrap().expose_secret(),
                    "remote-token",
                    "{kind:?}"
                );
            }
            assert_eq!(
                recorder.committed.receipts(),
                1,
                "retain the one pre-revocation commit"
            );
            assert_eq!(
                fixture.secrets.reads() - reads_before,
                1,
                "no repeated materialization"
            );
            assert_eq!(
                fixture.secrets.completed_reads() - completed_before,
                1,
                "original read completed"
            );
        }
    }
}
