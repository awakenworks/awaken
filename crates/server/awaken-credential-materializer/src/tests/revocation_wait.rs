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
//! W2-W4's final denial is a product gap: ownership is verified before the
//! read but not after it (claimed_materialization.rs, materialize_claimed_remote),
//! so that reproducer is ignored.
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

#[derive(Default)]
struct CountingRecorder(StdMutex<Vec<CredentialRealizationReceipt>>);

impl CountingRecorder {
    fn receipts(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialRealizationRecorder for CountingRecorder {
    async fn record(
        &self,
        receipt: CredentialRealizationReceipt,
    ) -> Result<(), awaken_credential_contract::CredentialRealizationRecordError> {
        self.0.lock().unwrap().push(receipt);
        Ok(())
    }
}

struct Fixture {
    secrets: Arc<GatedSecrets>,
    materializer: PinnedCredentialMaterializer,
    candidate: ResolvedModelCandidate,
    bindings: Vec<awaken_credential_contract::AttemptCredentialBinding>,
}

async fn fixture() -> Fixture {
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(GatedSecrets::new());
    let source = enter_credential(
        CredentialCreateParams {
            workspace_id: "workspace-a".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("https://agent.example".into()),
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
    let candidate = ResolvedModelCandidate::try_remote(
        ModelBinding::parse("", "", "a2a:https://agent.example").expect("valid binding"),
        "workspace-a",
        Some(
            CredentialAccess::new(
                CredentialRef {
                    id: source.id.0,
                    revision: 1,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::self_hosted_provider(),
            )
            .with_target(awaken_credential_contract::CredentialTarget::new(
                awaken_credential_contract::CredentialPurpose::RemoteAgentAuthorization,
                "https://agent.example",
            )),
        ),
        "sha256:card",
    )
    .expect("coherent remote credential candidate");
    let capabilities = CredentialRealizationCapabilities {
        holders: [holder.clone()].into_iter().collect(),
        material_sources: [CredentialMaterialSource::ControlPlaneReference]
            .into_iter()
            .collect(),
        realization_kinds: [CredentialRealizationKind::WorkerRelay]
            .into_iter()
            .collect(),
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
        secrets,
        materializer,
        candidate,
        bindings,
    }
}

fn context(
    fixture: &Fixture,
    authority: Arc<OwnerAuthority>,
    recorder: Arc<CountingRecorder>,
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
}

/// The T15-C02 oracle for a call whose authority is not current: refuse with
/// an ownership error, release no material and record no realization.
fn check_rejected_under_current_authority(observed: &RevokedObservation) -> Result<(), String> {
    match &observed.outcome {
        Ok(Some(_)) => return Err("material was returned under revoked authority".into()),
        Ok(None) => return Err("revoked call reported an anonymous success".into()),
        Err(error)
            if !(error.contains("attempt ownership was lost")
                || error.contains("authority down")) =>
        {
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
async fn materialize_with_change_during_read(change: Change) -> RevokedObservation {
    let fixture = fixture().await;
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
                let outcome = fixture
                    .materializer
                    .materialize_claimed_remote(&fixture.candidate, &context)
                    .await;
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
    }
}

#[tokio::test]
async fn t15_c02_revocation_before_resolution_and_cached_replay_are_rejected() {
    // W0 healthy row: material plus exactly one receipt and one read.
    let fixture = fixture().await;
    let authority = Arc::new(OwnerAuthority::new("worker-a"));
    let recorder = Arc::new(CountingRecorder::default());
    let ctx = context(&fixture, authority.clone(), recorder.clone());
    let reads_before = fixture.secrets.reads();
    let cached = fixture
        .materializer
        .materialize_claimed_remote(&fixture.candidate, &ctx)
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
            outcome: fixture
                .materializer
                .materialize_claimed_remote(&fixture.candidate, &ctx)
                .await,
            new_reads: fixture.secrets.reads() - reads_before,
            new_receipts: recorder.receipts(),
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
        outcome: fixture
            .materializer
            .materialize_claimed_remote(&fixture.candidate, &ctx)
            .await,
        new_reads: fixture.secrets.reads() - reads_before,
        new_receipts: recorder.receipts() - receipts_before,
    };
    check_rejected_under_current_authority(&observed)
        .unwrap_or_else(|violation| panic!("NC production re-entry: {violation}"));
    assert_eq!(
        observed.new_reads, 0,
        "NC production re-entry reads nothing"
    );
}

#[tokio::test]
async fn t15_c02_blocked_resolution_keeps_its_single_in_flight_read_observed() {
    // Oracle 1 (observation responsibility) for W2-W4: while the read is held
    // at the barrier and authority changes, the call keeps the original read
    // outstanding, observes it to completion exactly once, and never
    // re-issues it (asserted inside `materialize_with_change_during_read`).
    for change in CHANGES {
        let observed = materialize_with_change_during_read(change).await;
        assert_eq!(observed.new_reads, 1, "{change:?}");
    }
}

#[tokio::test]
#[ignore = "product gap: claimed materialization does not re-verify ownership after the awaited secret read, so revocation during resolution still returns material and records a receipt"]
async fn t15_c02_revocation_during_blocked_resolution_rejects_material_and_receipt() {
    for change in CHANGES {
        let observed = materialize_with_change_during_read(change).await;
        check_rejected_under_current_authority(&observed)
            .unwrap_or_else(|violation| panic!("{change:?}: {violation}"));
    }
}
